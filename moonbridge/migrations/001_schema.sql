-- =============================================================
-- moonbridge — консолидированная схема (SQLite).
--
-- Zero-config: файл БД и все таблицы создаются сами при первом запуске.
-- Всё идемпотентно (CREATE IF NOT EXISTS / INSERT OR IGNORE) — применяется
-- на каждом старте. Новые поля ордеров ядра заводятся авто-колонками в
-- рантайме (storage::orders, PRAGMA table_info + ALTER ADD COLUMN).
--
-- JSON хранится как TEXT (SQLite JSON1: json_extract/json_set).
-- Времена — TEXT (CURRENT_TIMESTAMP) либо INTEGER unix (поля ядра).
-- BOOLEAN — INTEGER 0/1.
-- =============================================================

CREATE TABLE IF NOT EXISTS modules (
    name            TEXT    PRIMARY KEY,
    description     TEXT    NOT NULL,
    category        TEXT    NOT NULL,                 -- storage | control | analytics | ...
    dependencies    TEXT    NOT NULL DEFAULT '[]',    -- JSON-массив имён модулей
    default_enabled INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS servers (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    ip         TEXT,                                  -- берётся из ключа (бэкфилл)
    port       INTEGER,
    token      TEXT,                                  -- MoonProto base64 key
    modules    TEXT    NOT NULL DEFAULT '{}',         -- JSON-объект флагов модулей
    note       TEXT,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS app_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    description TEXT,
    module     TEXT REFERENCES modules(name),
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS strategies (
    id           INTEGER PRIMARY KEY,
    server_id    INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    moonbot_id   INTEGER NOT NULL,                    -- signed int64 (Delphi), join к orders.strategyid
    name         TEXT    NOT NULL,
    signal_type  TEXT,
    checked      INTEGER NOT NULL DEFAULT 0,
    is_active    INTEGER NOT NULL DEFAULT 0,
    strategy_ver INTEGER,
    last_date_ms INTEGER,
    created_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (server_id, moonbot_id)
);
CREATE INDEX IF NOT EXISTS strategies_name ON strategies (server_id, name);

CREATE TABLE IF NOT EXISTS strategy_versions (
    id           INTEGER PRIMARY KEY,
    strategy_id  INTEGER NOT NULL REFERENCES strategies(id) ON DELETE CASCADE,
    version_date TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    raw_data     TEXT    NOT NULL,                    -- JSON полного дампа полей
    UNIQUE (strategy_id, version_date)
);
CREATE INDEX IF NOT EXISTS strategy_versions_date ON strategy_versions (strategy_id, version_date DESC);

-- Типизированная репликация БД отчёта Orders из MoonProto (client.reports().sync).
-- Ключ строки ядра — newRecID (стабильный replication-id). Так как один файл БД
-- держит несколько серверов, ключ составной (server_id, newRecID). Колонки заводит
-- сам поток репликации по схеме ядра (storage::orders::apply_schema, ALTER ADD
-- COLUMN по ReportSchema); ниже — только серверный дискриминатор + часто
-- запрашиваемые поля (hub/отчёты, индексы), остальные добавляются авто-колонками.
-- Имена ядра PascalCase (BuyDate, ProfitBTC, …) — SQLite сопоставляет их без учёта
-- регистра, поэтому предобъявленные lowercase-колонки сливаются с полями ядра.
-- NB: старый поток Event::ClosedSellOrderReport (готовый SQL, ключ db_id) удалён —
-- смешивать db_id и newRecID в одной БД нельзя.
CREATE TABLE IF NOT EXISTS orders (
    server_id       INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    newRecID        INTEGER NOT NULL,
    coin            TEXT,
    buydate         INTEGER, closedate       INTEGER,
    spentbtc        REAL,    gainedbtc       REAL,    profitbtc    REAL,
    status          INTEGER, sellreason      TEXT,    strategyid   INTEGER,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (server_id, newRecID)
);
CREATE INDEX IF NOT EXISTS orders_strategy ON orders (server_id, strategyid);
CREATE INDEX IF NOT EXISTS orders_coin ON orders (coin);
-- Курсор репликации отдельной таблицей НЕ храним: по контракту reports.md он всегда
-- MAX(newRecID)+1 локальной реплики (страницы ack'аются последовательно). Считаем его
-- на старте из самой orders (storage::orders::resume_cursor).

-- Очередь исходящих команд (Python-модули пишут pending, moonbridge отправляет).
CREATE TABLE IF NOT EXISTS commands (
    id         INTEGER PRIMARY KEY,
    server_id  INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL,
    payload    TEXT    NOT NULL DEFAULT '{}',         -- JSON
    status     TEXT    NOT NULL DEFAULT 'pending',    -- pending | sent | failed
    result     TEXT,
    error      TEXT,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    sent_at    TEXT
);
CREATE INDEX IF NOT EXISTS commands_pending ON commands (server_id, id) WHERE status = 'pending';

-- Ошибки ядра, извлечённые из логов — ОДНА строка на сигнатуру
-- (server_id+coin+ret_code+order_id): first_seen/prev_seen/last_seen/count, а не
-- на каждое повторение. Повтор той же ошибки только обновляет строку (без
-- дробления на эпизоды по паузе). Полная посекундная история — в сырых
-- лог-файлах. Основа для модуля logwatch (он решает «закончилась/возобновилась»).
CREATE TABLE IF NOT EXISTS core_errors (
    id          INTEGER PRIMARY KEY,
    server_id   INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    category    TEXT,               -- exchange_error | rate_limit | order_fail | action_fail
    coin        TEXT,
    task_id     INTEGER,
    order_id    TEXT,               -- большие id → TEXT
    cid         TEXT,               -- client order id
    ret_code    INTEGER,            -- код ошибки биржи
    ret_msg     TEXT,               -- сообщение биржи
    sig         TEXT NOT NULL,      -- сигнатура (dedup-ключ)
    first_seen  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    prev_seen   TEXT,               -- предпоследнее появление (интервал = last_seen − prev_seen)
    last_seen   TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    count       INTEGER NOT NULL DEFAULT 1,
    line        TEXT,               -- сама строка ошибки (первая)
    context     TEXT,               -- N строк лога «до» (действие перед ошибкой)
    resolved    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS core_errors_server ON core_errors (server_id, last_seen DESC);
CREATE INDEX IF NOT EXISTS core_errors_sig ON core_errors (server_id, sig, last_seen DESC);
CREATE INDEX IF NOT EXISTS core_errors_coin ON core_errors (server_id, coin);
-- purge чистит `WHERE last_seen < …` по всем серверам — нужен индекс по одному last_seen.
CREATE INDEX IF NOT EXISTS core_errors_last ON core_errors (last_seen);

-- ── Сид каталога модулей ──────────────────────────────────────────
INSERT OR IGNORE INTO modules (name, description, category, dependencies, default_enabled) VALUES
 ('listener_strategies', 'Приём snapshot-ов стратегий → strategies + strategy_versions.', 'storage', '[]', 1),
 ('listener_orders',     'Типизированная репликация БД отчёта Orders (MoonProto reports.sync).', 'storage', '[]', 1),
 ('listener_logs',       'Серверные логи ядра → дневные файлы logs/<дата>_<server>.log.', 'storage', '[]', 0),
 ('commander',           'Отправка команд в ядро через MoonProto (дренаж commands).',     'control', '[]', 0);

-- ── Сид runtime-настроек ──────────────────────────────────────────
INSERT OR IGNORE INTO app_settings (key, value, description, module) VALUES
 ('connect_timeout_secs', '15',  'Таймаут подключения к ядру (сек).',                NULL),
 ('event_poll_ms',        '500', 'Период опроса событий в session-thread (мс).',     NULL),
 ('reconnect_backoff_initial_secs', '5',   'Стартовая пауза (сек) перед пересозданием клиента после ConnectFailed.', NULL),
 ('reconnect_backoff_max_secs',     '300', 'Потолок экспоненциального backoff реконнекта (сек).',                   NULL),
 ('strategies_ignore_keys',
  'Active,LastEditDate,ReportToTelegram,ReportTradesToTelegram,SoundAlert,SoundKind,KeepAlert,SilentNoCharts,AddToChart,KeepInChart,DontKeepOrdersOnChart,UseCustomColors,OrderLineKind,SellOrderColor,BuyOrderColor,DontWriteLog,DebugLog,Comment',
  'Поля стратегии, изменение которых НЕ создаёт новую версию (косметика/статус). CSV. PreventWorkingUntil НЕ включать — иначе sgStop/sgStart не даст версию.',
  'listener_strategies'),
 ('strategies_resync_secs', '300',
  'Период (сек) фонового ре-синка снапшота стратегий — heartbeat, освежает strategies.updated_at (живость для гейтов risk/cloner/generator). 0 = только по Event::Strat.',
  'listener_strategies'),
 ('orders_history_depth',  'all', 'Глубина первичной репликации отчёта ордеров: all | server | <дней>.', 'listener_orders'),
 ('commands_poll_ms',     '500', 'Период дренажа очереди commands (мс).',            'commander'),
 ('logs_retention_days',  '14',  'Сколько дней хранить файлы логов ядра (0 = вечно).', 'listener_logs'),
 ('errors_enabled',       'true','Извлекать ошибки из логов ядра в таблицу core_errors.', 'listener_logs'),
 ('errors_context_lines', '6',   'Сколько строк лога «до» сохранять как контекст ошибки.', 'listener_logs'),
 ('errors_retention_days','90',  'Сколько дней хранить строки ошибок (по last_seen).', 'listener_logs'),
 ('errors_purge_interval_hours','6','Как часто (часов) чистить старые строки ошибок и лог-файлы по ретеншену.', 'listener_logs');

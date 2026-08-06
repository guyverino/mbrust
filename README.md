# moonbridge — open platform host

**Публичная бесплатная платформа-скелет.** Любой ставит себе на сервер: принимает
и отправляет данные с MoonBot-ядер через **MoonProto**, хранит всё в локальном
**SQLite-файле** (создаётся сам, ни сервера, ни юзера), даёт Telegram-оболочку с
базовыми командами. Вся бизнес-логика (AI-анализ, генератор стратегий, cloner,
риск) — в **отдельных приватных модулях**; нет модуля → его фичи просто не активны.

**Установка = 2 строки** в `.env` (`TELEGRAM_BOT_TOKEN` + `ADMIN_CHAT_IDS`).
БД-файл и схема создаются автоматически, серверы добавляются командой бота.

Это архитектура **open-core**: открытое ядро сбора/отправки + приватные модули-ценность.

## Состав платформы (public)

| Компонент       | Язык   | Роль                                                                 |
|-----------------|--------|---------------------------------------------------------------------|
| `moonbridge/`   | Rust   | Граница MoonProto ↔ SQLite: приём (стратегии/ордера) + отправка команд. |
| `hub/`          | Python | Telegram-оболочка + реестр модулей + базовые команды. |

Приватные модули (отдельные репозитории, Python) интегрируются **только через
общий SQLite-файл**: читают собранные данные, пишут «намерения» в очередь `commands`.

## moonbridge — что делает

- **Приём**: подключение к N ядрам параллельно (один OS-thread = одно ядро),
  handshake/шифрование/reconnect внутри `moonproto`. Пишет:
  - `strategies` + `strategy_versions` (история по `LastEditDate`, raw-дамп в JSON-тексте);
  - `orders` — **типизированная репликация БД отчёта Orders** (`client.reports().sync`,
    поток `Event::Report`, page-based с backpressure): схема ядра заводит авто-колонки,
    typed upsert/delete по `newRecID`, открытые сделки — `check_open_rows`; курсор =
    `MAX(newRecID)+1` из самой `orders`.
- **Отправка**: дренит таблицу `commands` (Python пишет туда намерения) и шлёт
  через MoonProto (`src/send.rs`: order_cancel, sell_order, strategy_set_checked, …).
- **Модульность через БД**: `modules` (каталог) + `servers.modules` (JSON-текст
  per-server флаги) + `app_settings.module`. Модули: `listener_strategies`,
  `listener_orders`, `commander`.

## Шов: общий SQLite-файл

- **moonbridge** владеет и мигрирует: `servers`, `strategies`, `strategy_versions`,
  `orders`, `app_settings`, `modules`, `commands`.
- **Приватные модули** добавляют СВОИ таблицы своими миграциями (`ai_*`,
  `generator_*`, …) — те же `modules`/`app_settings`, новые строки.

## Быстрый старт

```bash
cp .env.example .env          # вписать TELEGRAM_BOT_TOKEN + ADMIN_CHAT_IDS

# 1. Приём/отправка (Rust) — создаёт data/moonbridge.db и всю схему сам
cargo run --release -p moonbridge

# 2. Telegram-бот (Python)
pip install -r hub/requirements.txt
python -m hub.main
```

Дальше в боте: `/addserver <имя> <ключ>` — ip/port/transport берутся из ключа.
Никакой установки БД: SQLite-файл создаётся сам. Серверы и параметры — в БД.

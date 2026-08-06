# moonbridge

Rust-демон — граница **MoonProto ↔ SQLite**. Подключается к MoonBot-ядрам по
протоколу [`Moonbot-Tech/MoonProtoBeta`](https://github.com/Moonbot-Tech/MoonProtoBeta),
**принимает** стратегии/ордера/логи и **отправляет** команды, храня всё в
локальном SQLite-файле. Ни торговой логики, ни аналитики — чистый мост.
Бизнес-логика живёт в приватных модулях поверх БД (см. корневой README).

## Что делает

- **Приём** (по ядру = один OS-thread): handshake/шифрование/reconnect внутри
  `moonproto`. Пишет:
  - `strategies` + `strategy_versions` (история по `LastEditDate`, JSON-дамп полей);
  - `orders` — **типизированная репликация БД отчёта Orders** (`client.reports().sync`,
    поток `Event::Report`, page-based): схема ядра (append-only) заводит авто-колонки,
    догонка идёт страницами с backpressure (`page_applied` после коммита транзакции),
    typed upsert/delete строк по `newRecID`, открытые сделки — `check_open_rows`;
    курсор = `MAX(newRecID)+1` (из самой `orders`), ключ строки составной `(server_id, newRecID)`;
  - серверные логи ядра → дневные файлы `logs/<дата>_<server>.log` (не в БД).
- **Отправка**: дренит таблицу `commands` (Python-модули пишут туда намерения) и
  шлёт через MoonProto (`src/send.rs`).
- **Модульность через БД**: `modules` + `servers.modules` (JSON) + `app_settings`.

## Запуск

```bash
cp ../.env.example ../.env   # TELEGRAM_BOT_TOKEN + ADMIN_CHAT_IDS (для hub)
cargo run --release          # создаёт data/moonbridge.db и схему сам
```

Конфиг — из окружения: `MOONBRIDGE_DB` (дефолт `data/moonbridge.db`),
`MOONBRIDGE_LOG_DIR` (дефолт `logs`). Серверы добавляются через бота hub
(`/addserver <имя> <ключ>`): ip/port/transport берутся из ключа.

## Структура

```
moonbridge/
├── migrations/001_schema.sql   вся схема (идемпотентно)
└── src/
    ├── main.rs        entry point + оркестратор (thread на сервер)
    ├── config.rs      конфиг из env + DbServer/ServerModules
    ├── db.rs          rusqlite open + схема + load_servers
    ├── settings.rs    app_settings TTL-cache
    ├── applog.rs      дневной файловый лог ядра
    ├── session.rs     MoonProto event-loop (приём) + дренаж команд (отправка)
    ├── send.rs        маппинг команда → вызов MoonProto
    ├── commands.rs    дренаж очереди commands
    └── storage/       запись приёма (orders — авто-колонки, strategies — версии)
```

## Лицензия

MIT (см. [LICENSE](LICENSE)).

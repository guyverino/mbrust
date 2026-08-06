# hub — Telegram-оболочка + реестр модулей (public)

Python-часть платформы. Даёт Telegram-бота, базовые команды и **систему
модулей**: приватные модули (отдельные репозитории) доустанавливаются в
`hub/modules/<name>/` и подключаются автоматически. Нет модуля → его фичи не
активны (в `/modules` он не показывается вовсе; ошибка импорта → `🔒 locked`).

Говорит с ядрами **не напрямую**, а через общий SQLite-файл: читает данные,
которые пишет `moonbridge`, и кладёт команды в очередь `commands` (moonbridge
отправляет их через MoonProto).

## Запуск

```bash
pip install -r hub/requirements.txt
# .env: DB_HOST/PORT/USER/PASSWORD/NAME, TELEGRAM_BOT_TOKEN, ADMIN_CHAT_IDS
python -m hub.main          # из корня mb_ai_rust
```

Базовые команды: `/status`, `/servers`, `/modules`.

## Как сделать модуль

Приватный репо = пакет в `hub/modules/<name>/`, экспортирующий `MODULE`
(экземпляр `hub.sdk.Module`). См. шаблон `hub/modules/example/`.

```python
from hub.sdk import Module, HubServices

class MyModule(Module):
    name = "cloner"
    description = "..."
    depends_on = ["listener_strategies"]

    def migrations(self): return ["CREATE TABLE IF NOT EXISTS ..."]
    def router(self): ...                 # aiogram.Router (опц.)
    def background_tasks(self, hub): ...   # список корутин (опц.)
    async def setup(self, hub): ...        # инициализация

MODULE = MyModule()
```

Сервисы из `hub` (в `setup`/задачах): `hub.db`, `hub.settings`,
`hub.commands.enqueue(server_id, kind, payload)`, `await hub.notify(text)`.

Поддерживаемые `kind` команд и формат payload — в
[`moonbridge/src/send.rs`](../moonbridge/src/send.rs).

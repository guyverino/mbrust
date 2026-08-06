"""Module SDK — контракт, который реализуют приватные модули.

Публичный hub задаёт этот интерфейс; приватный модуль (отдельный репо, Python-
пакет в `hub/modules/<name>/`) экспортирует объект `MODULE: Module`. Если пакета
нет — фича просто не активируется («не доступное не запустится»).

Что hub даёт модулю (`HubServices`):
  * db       — доступ к SQLite (общий коннект);
  * settings — app_settings TTL-кэш;
  * commands — enqueue(server_id, kind, payload) → очередь → moonbridge шлёт в ядро;
  * notify   — отправка текста в Telegram админам.

Что модуль даёт hub:
  * name / depends_on          — для каталога `modules` и проверки зависимостей;
  * migrations()               — SQL своих таблиц (применяются при загрузке);
  * router()                   — aiogram.Router с командами/кнопками модуля;
  * background_tasks(hub)      — корутины фоновых циклов (auto-analysis и т.п.);
  * setup(hub)                 — инициализация после подключения сервисов.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Awaitable, Callable

if TYPE_CHECKING:  # избегаем жёсткой зависимости на aiogram в самом контракте
    from aiogram import Router

    from hub.core.db import Db
    from hub.core.settings import Settings
    from hub.services.commands import Commands


class HubServices:
    """Сервисы платформы, передаются модулю в setup() и background_tasks().

    * db / settings / commands — данные, настройки, очередь команд;
    * proposals — поток «предложить → одобрить → отправить» (с авто-апрувом);
    * alerts    — уведомления с дедупом/сводками;
    * notify    — прямая отправка текста админам.
    """

    def __init__(self, db: "Db", settings: "Settings", commands: "Commands",
                 notify: Callable[[str], Awaitable[None]],
                 proposals=None, alerts=None, commander=None):
        self.db = db
        self.settings = settings
        self.commands = commands
        self._notify = notify
        self.proposals = proposals
        self.alerts = alerts
        # commander — совместимый шов старого MoonbotCommander (имя стратегии→id,
        # действие → очередь commands). Для переноса модулей mb_ai.
        self.commander = commander

    async def notify(self, text: str, reply_markup=None) -> None:
        await self._notify(text, reply_markup)


class Module:
    """Базовый класс модуля. Приватный модуль наследует и переопределяет нужное."""

    name: str = ""
    depends_on: list[str] = []
    description: str = ""
    category: str = "module"

    def migrations(self) -> list[str]:
        """SQL-строки для своих таблиц. Применяются идемпотентно при загрузке."""
        return []

    def router(self) -> "Router | None":
        """aiogram.Router модуля (команды/кнопки) либо None."""
        return None

    def menu_entry(self) -> "tuple[str, str] | None":
        """Кнопка модуля в ГЛАВНОМ меню бота: (label, callback_data) либо None.

        callback_data обрабатывает роутер самого модуля (см. router()). Базовое
        меню лишь дорисовывает кнопку активных модулей, у которых это задано.
        """
        return None

    def background_tasks(self, hub: HubServices) -> list[Awaitable[None]]:
        """Корутины фоновых циклов модуля (будут запущены через asyncio)."""
        return []

    async def setup(self, hub: HubServices) -> None:
        """Инициализация после подключения сервисов. По умолчанию — ничего."""
        return None

"""Сборка и запуск hub: Telegram-оболочка + загрузка модулей.

Порядок: DB → сервисы → discover модулей → активация (каталог+миграции+setup)
→ include роутеров (base + модули) → запуск фоновых задач → polling.
"""

import asyncio
import logging
import os

from aiogram import BaseMiddleware, Bot, Dispatcher

from hub.base_handlers import build_base_router
from hub.config import Config
from hub.features import botcmd, reports
from hub.core.db import Db
from hub.core.settings import Settings
from hub.registry import activate, check_dependencies, discover
from hub.sdk import HubServices
from hub.services.alerts import Alerts
from hub.services.commander import Commander
from hub.services.commands import Commands
from hub.services.proposals import Proposals
from hub.services.proposals import build_router as build_proposals_router

logger = logging.getLogger(__name__)


class AdminOnly(BaseMiddleware):
    """Пускает только ADMIN_CHAT_IDS (если список задан)."""

    async def __call__(self, handler, event, data):
        if Config.ADMIN_CHAT_IDS:
            uid = getattr(getattr(event, "from_user", None), "id", None)
            if uid not in Config.ADMIN_CHAT_IDS:
                return None
        return await handler(event, data)


async def main() -> None:
    if not Config.TELEGRAM_BOT_TOKEN:
        logger.error("TELEGRAM_BOT_TOKEN не задан в .env")
        return

    db = Db.from_env()

    # Бутстрап базовой схемы: любой из процессов (hub/moonbridge) может создать
    # БД первым. Идемпотентно (CREATE IF NOT EXISTS).
    schema = os.path.join(
        os.path.dirname(__file__), "..", "moonbridge", "migrations", "001_schema.sql"
    )
    if os.path.exists(schema):
        with open(schema, encoding="utf-8") as f:
            db.executescript(f.read())

    settings = Settings(db)
    commands = Commands(db)

    bot = Bot(token=Config.TELEGRAM_BOT_TOKEN)
    dp = Dispatcher()
    dp.message.middleware(AdminOnly())
    dp.callback_query.middleware(AdminOnly())

    async def notify(text: str, reply_markup=None) -> None:
        for chat_id in Config.ADMIN_CHAT_IDS:
            try:
                await bot.send_message(chat_id, text, reply_markup=reply_markup)
            except Exception as e:  # noqa: BLE001
                logger.warning("notify to %s failed: %s", chat_id, e)

    # Shared-примитивы платформы.
    alerts = Alerts(notify, settings)
    proposals = Proposals(db, settings, commands, bot)
    proposals.ensure_schema()

    hub = HubServices(
        db=db, settings=settings, commands=commands, notify=notify,
        proposals=proposals, alerts=alerts, commander=Commander(db, commands),
    )

    # Загрузка модулей.
    loaded = discover()
    check_dependencies(loaded)
    active = await activate(db, hub, loaded)

    # Роутеры: базовый + базовые фичи (отчёты/команды) + proposals + модульные.
    dp.include_router(build_base_router(hub, loaded))
    dp.include_router(reports.build_router(hub))
    dp.include_router(botcmd.build_router(hub))
    dp.include_router(build_proposals_router(proposals))
    for l in active:
        r = l.module.router()
        if r is not None:
            dp.include_router(r)
            logger.info("router included: %s", l.name)

    # Фоновые задачи модулей. Ссылки держим обязательно: event loop хранит
    # task слабо, и задача без ссылки может быть собрана GC на полпути; упавшую
    # задачу без done-callback никто бы не заметил до самого выхода процесса.
    def _log_task_crash(task: asyncio.Task) -> None:
        if not task.cancelled() and task.exception() is not None:
            logger.error("фоновая задача модуля упала: %r", task.exception())

    bg_tasks: list[asyncio.Task] = []
    for l in active:
        for coro in l.module.background_tasks(hub):
            task = asyncio.create_task(coro)
            task.add_done_callback(_log_task_crash)
            bg_tasks.append(task)

    logger.info(
        "hub started: %d активных модулей, %d locked",
        len(active),
        sum(1 for l in loaded if not l.active),
    )
    await dp.start_polling(bot)

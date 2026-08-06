"""Точка входа hub. Запуск: `python -m hub.main` (из корня mb_ai_rust)."""

import asyncio
import logging

from hub import bot
from hub.config import Config


def _setup_logging():
    logging.basicConfig(
        level=getattr(logging, Config.LOG_LEVEL.upper(), logging.INFO),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )


def run():
    _setup_logging()
    asyncio.run(bot.main())


if __name__ == "__main__":
    run()

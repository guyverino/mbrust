"""Конфиг hub — из окружения (единый `.env` в корне, общий с moonbridge).

Обязательны только 2 строки: TELEGRAM_BOT_TOKEN + ADMIN_CHAT_IDS.
БД — тот же SQLite-файл, что пишет moonbridge (создаётся сам).
"""

import os

from dotenv import load_dotenv

load_dotenv()


class Config:
    # Путь к SQLite-файлу (тот же, что у moonbridge). Дефолт — data/moonbridge.db.
    DB_PATH = os.getenv("MOONBRIDGE_DB", "data/moonbridge.db")

    TELEGRAM_BOT_TOKEN = os.getenv("TELEGRAM_BOT_TOKEN")
    ADMIN_CHAT_IDS = [
        int(x) for x in os.getenv("ADMIN_CHAT_IDS", "").replace(" ", "").split(",") if x
    ]

    LOG_LEVEL = os.getenv("LOG_LEVEL", "INFO")

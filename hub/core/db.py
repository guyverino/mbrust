"""Доступ к SQLite — stdlib sqlite3, один общий коннект + RLock.

Тот же файл, что пишет moonbridge (WAL). sqlite3 из stdlib — никаких внешних
зависимостей. Плейсхолдеры `?`. Весь доступ сериализуется через RLock (бот +
фоновые задачи модулей ходят из разных корутин/потоков).
"""

import logging
import os
import sqlite3
import threading

from hub.config import Config

logger = logging.getLogger(__name__)


class Db:
    def __init__(self, path: str):
        d = os.path.dirname(path)
        if d:
            os.makedirs(d, exist_ok=True)
        self._lock = threading.RLock()
        self.conn = sqlite3.connect(path, check_same_thread=False)
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA busy_timeout=15000")  # ждать локи (18 писателей moonbridge + чекпоинты), не падать
        self.conn.execute("PRAGMA foreign_keys=ON")
        logger.info("SQLite connected: %s", path)

    @classmethod
    def from_env(cls) -> "Db":
        return cls(Config.DB_PATH)

    @property
    def lock(self) -> threading.RLock:
        return self._lock

    def execute(self, query: str, params=None) -> int:
        """Запрос без результата. Возвращает rowcount."""
        with self._lock:
            cur = self.conn.execute(query, params or [])
            self.conn.commit()
            return cur.rowcount

    def insert(self, query: str, params=None) -> int:
        """INSERT. Возвращает id вставленной строки (lastrowid)."""
        with self._lock:
            cur = self.conn.execute(query, params or [])
            self.conn.commit()
            return cur.lastrowid

    def fetch_one(self, query: str, params=None):
        with self._lock:
            return self.conn.execute(query, params or []).fetchone()

    def fetch_all(self, query: str, params=None) -> list:
        with self._lock:
            return self.conn.execute(query, params or []).fetchall()

    def executescript(self, script: str) -> None:
        with self._lock:
            self.conn.executescript(script)
            self.conn.commit()

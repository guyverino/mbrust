"""app_settings с TTL-кэшем (порт из mb_ai `src/core/settings.py`).

Значения читаются из БД не чаще раза в TTL; между обращениями — из кэша.
Позволяет менять параметры в рантайме (UPDATE app_settings) без рестарта.
Общий с moonbridge источник — таблица app_settings.
"""

import time


class Settings:
    def __init__(self, db, ttl_sec: int = 60):
        self._db = db
        self._ttl = ttl_sec
        self._cache: dict[str, str] = {}
        self._loaded_at = 0.0

    def _ensure_fresh(self):
        if time.time() - self._loaded_at < self._ttl and self._cache:
            return
        rows = self._db.fetch_all("SELECT key, value FROM app_settings")
        self._cache = {k: v for k, v in rows}
        self._loaded_at = time.time()

    def get(self, key: str, default=None):
        self._ensure_fresh()
        return self._cache.get(key, default)

    def get_int(self, key: str, default: int = 0) -> int:
        try:
            return int(self.get(key, default))
        except (TypeError, ValueError):
            return default

    def get_float(self, key: str, default: float = 0.0) -> float:
        try:
            return float(self.get(key, default))
        except (TypeError, ValueError):
            return default

    def get_bool(self, key: str, default: bool = False) -> bool:
        v = self.get(key)
        if v is None:
            return default
        return str(v).strip().lower() in ("true", "1", "yes", "on", "enabled")

    def set(self, key: str, value: str):
        self._db.execute("UPDATE app_settings SET value=? WHERE key=?", (str(value), key))
        self._loaded_at = 0.0  # форсим перезагрузку

"""Отправка команд в ядро — через очередь `commands` (шов с moonbridge).

Модуль не говорит по MoonProto (это Rust). Он кладёт «намерение» в таблицу
`commands` со status='pending'; moonbridge дренит очередь и отправляет через
MoonProto, проставляя sent/failed + result/error.

Список поддерживаемых `kind` и формат payload — в moonbridge/src/send.rs.
"""

import json


class Commands:
    def __init__(self, db):
        self._db = db

    def enqueue(self, server_id: int, kind: str, payload: dict | None = None) -> int | None:
        """Поставить команду в очередь. Возвращает id строки commands."""
        return self._db.insert(
            "INSERT INTO commands (server_id, kind, payload) VALUES (?, ?, ?)",
            (server_id, kind, json.dumps(payload or {})),
        )

    def status(self, command_id: int):
        """(status, result, error) команды или None."""
        return self._db.fetch_one(
            "SELECT status, result, error FROM commands WHERE id=?", (command_id,)
        )

"""Proposals — общий поток «предложить → одобрить → отправить».

Модуль формирует предложение (что сделать = список команд ядру) и вызывает
`hub.proposals.create(...)`. Дальше:
  * если у модуля включён авто-апрув (app_settings `<module>_auto_approve=true`)
    — команды ставятся в очередь сразу (status='auto');
  * иначе — в Telegram уходит карточка с ✅/❌; по ✅ команды уходят в очередь
    `commands` (moonbridge отправит их в ядро), по ❌ — reject.

Действие = список команд: [{server_id, kind, payload}], т.е. proposal опирается
на тот же шов, что и прямая отправка. Список поддерживаемых kind — в
moonbridge/src/send.rs.
"""

import json
import logging

from aiogram import Bot, F, Router
from aiogram.types import (
    CallbackQuery,
    InlineKeyboardButton,
    InlineKeyboardMarkup,
)

from hub.config import Config

logger = logging.getLogger(__name__)

SCHEMA = """
CREATE TABLE IF NOT EXISTS proposals (
    id          INTEGER PRIMARY KEY,
    module      TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    server_id   INTEGER,
    actions     TEXT    NOT NULL DEFAULT '[]',
    status      TEXT    NOT NULL DEFAULT 'pending',
    result      TEXT,
    error       TEXT,
    created_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    decided_at  TEXT
);
CREATE INDEX IF NOT EXISTS proposals_pending ON proposals (module, id) WHERE status='pending';
"""


class Proposals:
    def __init__(self, db, settings, commands, bot: Bot):
        self._db = db
        self._settings = settings
        self._commands = commands
        self._bot = bot

    def ensure_schema(self) -> None:
        self._db.executescript(SCHEMA)

    async def create(self, module: str, title: str, actions: list[dict],
                     server_id: int | None = None) -> int | None:
        """Создать предложение. actions = [{server_id, kind, payload}]."""
        auto = self._settings.get_bool(f"{module}_auto_approve", False)
        pid = self._db.insert(
            "INSERT INTO proposals (module, title, server_id, actions, status) "
            "VALUES (?, ?, ?, ?, ?)",
            (module, title, server_id, json.dumps(actions),
             "auto" if auto else "pending"),
        )
        if pid is None:
            return None

        if auto:
            await self._enqueue(pid, actions)
            logger.info("proposal #%s (%s) auto-approved", pid, module)
            return pid

        kb = InlineKeyboardMarkup(inline_keyboard=[[
            InlineKeyboardButton(text="✅ Одобрить", callback_data=f"prop:{pid}:approve"),
            InlineKeyboardButton(text="❌ Отклонить", callback_data=f"prop:{pid}:reject"),
        ]])
        for chat_id in Config.ADMIN_CHAT_IDS:
            try:
                await self._bot.send_message(
                    chat_id, f"🧩 <b>{module}</b>\n{title}",
                    reply_markup=kb, parse_mode="HTML",
                )
            except Exception as e:  # noqa: BLE001
                logger.warning("proposal card send to %s failed: %s", chat_id, e)
        return pid

    async def approve(self, pid: int) -> bool:
        row = self._db.fetch_one(
            "SELECT actions, status FROM proposals WHERE id=?", (pid,)
        )
        if not row or row[1] != "pending":
            return False
        await self._enqueue(pid, json.loads(row[0]))
        return True

    async def reject(self, pid: int) -> bool:
        self._db.execute(
            "UPDATE proposals SET status='rejected', decided_at=CURRENT_TIMESTAMP "
            "WHERE id=? AND status='pending'",
            (pid,),
        )
        return True

    async def _enqueue(self, pid: int, actions: list[dict]) -> None:
        """Положить команды предложения в очередь и отметить итог."""
        try:
            ids = [
                self._commands.enqueue(a["server_id"], a["kind"], a.get("payload"))
                for a in actions
            ]
            self._db.execute(
                "UPDATE proposals SET status='executed', result=?, "
                "decided_at=CURRENT_TIMESTAMP WHERE id=?",
                (json.dumps({"command_ids": ids}), pid),
            )
        except Exception as e:  # noqa: BLE001
            self._db.execute(
                "UPDATE proposals SET status='failed', error=?, "
                "decided_at=CURRENT_TIMESTAMP WHERE id=?",
                (str(e), pid),
            )
            logger.error("proposal #%s enqueue failed: %s", pid, e)


def build_router(proposals: Proposals) -> Router:
    """Роутер обработки кнопок ✅/❌ под карточками предложений."""
    router = Router(name="proposals")

    @router.callback_query(F.data.startswith("prop:"))
    async def on_decision(cb: CallbackQuery):
        try:
            _, pid_s, decision = cb.data.split(":")
            pid = int(pid_s)
        except ValueError:
            await cb.answer("bad callback")
            return

        base = cb.message.text if cb.message else ""
        if decision == "approve":
            ok = await proposals.approve(pid)
            suffix = "\n\n✅ одобрено, команды отправлены" if ok else "\n\n⚠️ уже обработано"
        else:
            await proposals.reject(pid)
            suffix = "\n\n❌ отклонено"
        try:
            await cb.message.edit_text(base + suffix)
        except Exception:  # noqa: BLE001
            pass
        await cb.answer()

    return router

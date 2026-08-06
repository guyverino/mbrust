"""Команды ботам — START/STOP/SellALL/BL по серверу или всем (порт handlers/settings.py, тексты 1:1).

Меню: 🎛 Команды ботам → выбор сервера (или ВСЕ) → START/STOP/SellALL/BL+/BL-.
BL+/BL- спрашивают монету (FSM). Команды уходят через commander-шим в очередь.
"""

from __future__ import annotations

from aiogram import F, Router
from aiogram.fsm.context import FSMContext
from aiogram.fsm.state import State, StatesGroup
from aiogram.types import CallbackQuery, Message

from hub.core.tg import kb, render

MENU_ENTRY = ("🎛 Команды ботам", "cmd:menu")


class BLCoin(StatesGroup):
    coin = State()


def _targets_screen(db):
    rows = [[(f"🖥 {name}", f"cmd:t:{sid}")] for sid, name in db.fetch_all("SELECT id, name FROM servers ORDER BY id")]
    rows.append([("🌐 ВСЕ СЕРВЕРА", "cmd:t:ALL")])
    return "Выберите целевой сервер для отправки команды:", kb(rows)


def _actions_screen(db, target: str):
    if target == "ALL":
        name = "ВСЕ СЕРВЕРА"
    else:
        r = db.fetch_one("SELECT name FROM servers WHERE id=?", (int(target),))
        name = r[0] if r else "?"
    rows = [
        [("🟢 START", f"cmd:a:{target}:Start")],
        [("🛑 STOP", f"cmd:a:{target}:Stop")],
        [("🔴 SellALL", f"cmd:a:{target}:SellALL")],
        [("➕ BL + <coin>", f"cmd:a:{target}:BL+"), ("➖ BL - <coin>", f"cmd:a:{target}:BL-")],
        [("🔙 Назад", "cmd:menu")],
    ]
    return f"Команды для: **{name}**\n\nВыберите команду:", kb(rows)


def build_router(hub) -> Router:
    router = Router(name="botcmd")
    db = hub.db

    def targets(target: str) -> list:
        if target == "ALL":
            return [s[0] for s in db.fetch_all("SELECT id FROM servers")]
        return [int(target)]

    @router.callback_query(F.data == "cmd:menu")
    async def cb_menu(cb: CallbackQuery, state: FSMContext):
        await state.clear()
        await render(cb, *_targets_screen(db), parse_mode=None)

    @router.callback_query(F.data.startswith("cmd:t:"))
    async def cb_target(cb: CallbackQuery):
        await render(cb, *_actions_screen(db, cb.data.split(":", 2)[2]), parse_mode="Markdown")

    @router.callback_query(F.data.startswith("cmd:a:"))
    async def cb_action(cb: CallbackQuery, state: FSMContext):
        _, _, target, action = cb.data.split(":", 3)
        if action in ("BL+", "BL-"):
            await state.update_data(target=target, action=action)
            await state.set_state(BLCoin.coin)
            act_text = "в Блэклист" if action == "BL+" else "из Блэклиста"
            await cb.message.answer(f"Напишите тикер монеты для добавления {act_text} (например BTC):")
            await cb.answer()
            return
        fn = {"Start": hub.commander.start, "Stop": hub.commander.stop, "SellALL": hub.commander.sell_all}.get(action)
        if fn:
            n = sum(1 for sid in targets(target) if fn(sid) is not None)
            await cb.message.answer(f"Команда `{action.upper()}` отправлена на {n} серверов.", parse_mode="Markdown")
        await cb.answer()

    @router.message(BLCoin.coin)
    async def bl_coin(msg: Message, state: FSMContext):
        data = await state.get_data()
        await state.clear()
        coin = (msg.text or "").strip().upper()
        add = data.get("action") == "BL+"
        n = sum(1 for sid in targets(data.get("target"))
                if hub.commander.set_global_blacklist(sid, coin, add=add) is not None)
        act_text = "добавлена в" if add else "удалена из"
        await msg.answer(f"Монета `{coin}` {act_text} ЧС на {n} серверах.", parse_mode="Markdown")

    return router

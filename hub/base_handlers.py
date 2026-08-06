"""Базовое меню hub (inline-кнопки, без сырых команд).

Структура:
  Главное меню ─┬─ 📊 Статус
                └─ ⚙️ Настройки ─── 🖥 Ядра ─┬─ ➕ Добавить (пошагово: имя → ключ)
                                             └─ <ядро> ─┬─ 🧩 Модули (тумблеры)
                                                        └─ 🗑 Удалить (с подтверждением)

Приватные модули могут добавлять свои пункты в Настройки через
hub-расширение позже; сейчас — сборка серверов.

ВНИМАНИЕ: moonbridge читает серверы/модули при старте. Изменения применяются
после его перезапуска (в сообщениях об этом предупреждается).
"""

import json
import os
import signal
import subprocess

from aiogram import F, Router
from aiogram.filters import Command
from aiogram.fsm.context import FSMContext
from aiogram.fsm.state import State, StatesGroup
from aiogram.types import (
    CallbackQuery,
    InlineKeyboardButton,
    InlineKeyboardMarkup,
    Message,
)

DEFAULT_MODULES = {
    "listener_strategies": True,
    "listener_orders": True,
    "listener_logs": True,
}

RESTART_HINT = "\n\n⚠️ применится после перезапуска приёма (кнопка 🔄 в Настройках)"


def restart_moonbridge() -> str:
    """Перезапустить сервис moonbridge, убив его процесс: systemd (Restart=always)
    поднимет заново через ~5с. Работает без sudo — hub и moonbridge от одного
    юзера, сигнал разрешён. Возвращает статус-строку для пользователя."""
    try:
        out = subprocess.run(
            ["systemctl", "show", "-p", "MainPID", "--value", "moonbridge"],
            capture_output=True, text=True, timeout=5,
        )
        pid = int((out.stdout or "0").strip() or 0)
    except Exception as e:  # noqa: BLE001
        return f"не удалось узнать PID: {e}"
    if pid <= 0:
        return "moonbridge не запущен как сервис (нет MainPID)"
    try:
        os.kill(pid, signal.SIGTERM)
    except PermissionError:
        return "нет прав убить процесс (разные юзеры?)"
    except ProcessLookupError:
        return "процесс уже не найден"
    return f"🔄 moonbridge (pid {pid}) перезапускается, поднимется через ~5с"


class AddServer(StatesGroup):
    name = State()
    key = State()


class EditSetting(StatesGroup):
    value = State()


from hub.core.tg import kb as _kb  # общий сборщик inline-клавиатуры
from hub.core.tg import render as _render


def build_base_router(hub, loaded: list) -> Router:
    router = Router(name="base")

    # ── экраны ──────────────────────────────────────────────────

    def main_screen():
        rows = [[("📊 Статус", "m:status")]]
        rows.append([("💰 Отчёт профит", "rep:menu")])
        rows.append([("🎛 Команды ботам", "cmd:menu")])
        # Кнопки активных модулей, объявивших menu_entry() (напр. 📋 Логи).
        for l in loaded:
            if not l.active:
                continue
            try:
                entry = l.module.menu_entry()
            except Exception:  # noqa: BLE001 — модуль не должен ронять меню
                entry = None
            if entry:
                label, cb = entry
                rows.append([(label, cb)])
        rows.append([("⚙️ Настройки", "m:settings")])
        return "🏠 <b>moonbridge hub</b>\nОткрытая платформа. Выбери раздел:", _kb(rows)

    def status_screen():
        servers = hub.db.fetch_one("SELECT count(*) FROM servers")[0]
        strategies = hub.db.fetch_one("SELECT count(*) FROM strategies")[0]
        orders = hub.db.fetch_one("SELECT count(*) FROM orders")[0]
        pending = hub.db.fetch_one("SELECT count(*) FROM commands WHERE status='pending'")[0]
        active = sum(1 for l in loaded if l.active)
        text = (
            "📊 <b>Статус</b>\n\n"
            f"🖥 серверов: <b>{servers}</b>\n"
            f"🧠 стратегий: <b>{strategies}</b>\n"
            f"📈 ордеров: <b>{orders}</b>\n"
            f"📨 команд в очереди: <b>{pending}</b>\n"
            f"🧩 модулей hub (плагинов): <b>{active}</b>"
        )
        return text, _kb([[("⬅️ Назад", "m:main")]])

    def settings_screen():
        return "⚙️ <b>Настройки</b>", _kb([
            [("🖥 Ядра", "m:servers")],
            [("🎚 Параметры", "m:params")],
            [("🔄 Перезапустить приём", "m:restart")],
            [("⬅️ Назад", "m:main")],
        ])

    def params_groups_screen():
        rows = hub.db.fetch_all("SELECT DISTINCT module FROM app_settings ORDER BY module")
        text = "🎚 <b>Параметры</b> (все в БД, меняются на лету)\nвыбери группу:"
        kb_rows = []
        for (module,) in rows:
            label = module if module else "общее"
            code = module if module else "__global"
            n = hub.db.fetch_one(
                "SELECT count(*) FROM app_settings WHERE module IS ?"
                if module is None else
                "SELECT count(*) FROM app_settings WHERE module = ?",
                (module,),
            )[0]
            kb_rows.append([(f"⚙️ {label} ({n})", f"m:pgrp:{code}")])
        kb_rows.append([("⬅️ Назад", "m:settings")])
        return text, _kb(kb_rows)

    def param_group_screen(code: str):
        if code == "__global":
            rows = hub.db.fetch_all(
                "SELECT key, value FROM app_settings WHERE module IS NULL ORDER BY key"
            )
            label = "общее"
        else:
            rows = hub.db.fetch_all(
                "SELECT key, value FROM app_settings WHERE module=? ORDER BY key", (code,)
            )
            label = code
        text = f"🎚 <b>Параметры · {label}</b>\nнажми, чтобы изменить:"
        kb_rows = [[(f"{k} = {v}", f"m:set:{k}")] for k, v in rows]
        kb_rows.append([("⬅️ Назад", "m:params")])
        return text, _kb(kb_rows)

    def servers_screen():
        rows = hub.db.fetch_all("SELECT id, name, ip, port FROM servers ORDER BY id")
        text = "🖥 <b>Ядра</b>\n"
        kb_rows = []
        if not rows:
            text += "\nпока нет ни одного ядра."
        for sid, name, ip, port in rows:
            ep = f" · {ip}:{port}" if ip else ""
            kb_rows.append([(f"{name}{ep}", f"m:srv:{sid}")])
        kb_rows.append([("➕ Добавить ядро", "m:addsrv")])
        kb_rows.append([("⬅️ Назад", "m:settings")])
        return text, _kb(kb_rows)

    def server_screen(sid: int):
        row = hub.db.fetch_one(
            "SELECT name, ip, port, modules FROM servers WHERE id=?", (sid,)
        )
        if not row:
            return "ядро не найдено", _kb([[("⬅️ Назад", "m:servers")]])
        name, ip, port, modules = row
        mdict = json.loads(modules) if modules else {}
        ep = f"{ip}:{port}" if ip else "— (возьмётся из ключа)"
        on = ", ".join(k for k, v in mdict.items() if v) or "нет"
        text = (
            f"🖥 <b>{name}</b>\n\n"
            f"адрес: {ep}\n"
            f"модули: {on}"
        )
        return text, _kb([
            [("🧩 Модули", f"m:mods:{sid}")],
            [("🗑 Удалить ядро", f"m:del:{sid}")],
            [("⬅️ Назад", "m:servers")],
        ])

    def modules_screen(sid: int):
        row = hub.db.fetch_one("SELECT name, modules FROM servers WHERE id=?", (sid,))
        if not row:
            return "ядро не найдено", _kb([[("⬅️ Назад", "m:servers")]])
        name, modules = row
        mdict = json.loads(modules) if modules else {}
        catalog = hub.db.fetch_all(
            "SELECT name, description FROM modules ORDER BY category, name"
        )
        text = f"🧩 <b>Модули · {name}</b>\nнажми, чтобы вкл/выкл:"
        kb_rows = []
        for mname, _desc in catalog:
            mark = "✅" if mdict.get(mname) else "⬜️"
            kb_rows.append([(f"{mark} {mname}", f"m:tog:{sid}:{mname}")])
        kb_rows.append([("⬅️ Назад", f"m:srv:{sid}")])
        return text, _kb(kb_rows)

    async def render(cb: CallbackQuery, screen):
        await _render(cb, *screen)  # screen = (text, markup)

    # ── вход в меню ─────────────────────────────────────────────

    @router.message(Command("start", "menu"))
    async def open_menu(msg: Message, state: FSMContext):
        await state.clear()
        text, kb = main_screen()
        await msg.answer(text, reply_markup=kb, parse_mode="HTML")

    # ── навигация ───────────────────────────────────────────────

    @router.callback_query(F.data == "m:main")
    async def cb_main(cb: CallbackQuery, state: FSMContext):
        await state.clear()
        await render(cb, main_screen())

    @router.callback_query(F.data == "m:status")
    async def cb_status(cb: CallbackQuery):
        await render(cb, status_screen())

    @router.callback_query(F.data == "m:settings")
    async def cb_settings(cb: CallbackQuery):
        await render(cb, settings_screen())

    @router.callback_query(F.data == "m:servers")
    async def cb_servers(cb: CallbackQuery):
        await render(cb, servers_screen())

    @router.callback_query(F.data == "m:params")
    async def cb_params(cb: CallbackQuery):
        await render(cb, params_groups_screen())

    @router.callback_query(F.data.startswith("m:pgrp:"))
    async def cb_pgrp(cb: CallbackQuery):
        await render(cb, param_group_screen(cb.data.split(":", 2)[2]))

    @router.callback_query(F.data.startswith("m:set:"))
    async def cb_setkey(cb: CallbackQuery, state: FSMContext):
        key = cb.data.split(":", 2)[2]
        cur = hub.db.fetch_one("SELECT value, description FROM app_settings WHERE key=?", (key,))
        if not cur:
            await cb.answer("нет такого параметра")
            return
        await state.set_state(EditSetting.value)
        await state.update_data(key=key)
        desc = f"\n<i>{cur[1]}</i>" if cur[1] else ""
        await cb.message.edit_text(
            f"🎚 <b>{key}</b>{desc}\n\nтекущее: <code>{cur[0]}</code>\n\nпришли новое значение:",
            parse_mode="HTML",
        )
        await cb.answer()

    @router.message(EditSetting.value)
    async def set_value(msg: Message, state: FSMContext):
        data = await state.get_data()
        key = data.get("key")
        val = (msg.text or "").strip()
        await state.clear()
        hub.db.execute(
            "UPDATE app_settings SET value=?, updated_at=CURRENT_TIMESTAMP WHERE key=?",
            (val, key),
        )
        mod = hub.db.fetch_one("SELECT module FROM app_settings WHERE key=?", (key,))
        code = mod[0] if mod and mod[0] else "__global"
        await msg.answer(
            f"✅ <b>{key}</b> = <code>{val}</code>\n"
            "применится в течение ~60с (TTL-кэш) — рестарт не нужен.",
            reply_markup=_kb([[("🎚 К группе", f"m:pgrp:{code}")]]),
            parse_mode="HTML",
        )

    @router.callback_query(F.data == "m:restart")
    async def cb_restart(cb: CallbackQuery):
        result = restart_moonbridge()
        await render(cb, (
            f"🔄 <b>Перезапуск приёма</b>\n\n{result}",
            _kb([[("⬅️ Назад", "m:settings")]]),
        ))
        await cb.answer("moonbridge перезапускается")

    @router.callback_query(F.data.startswith("m:srv:"))
    async def cb_server(cb: CallbackQuery):
        await render(cb, server_screen(int(cb.data.split(":")[2])))

    @router.callback_query(F.data.startswith("m:mods:"))
    async def cb_mods(cb: CallbackQuery):
        await render(cb, modules_screen(int(cb.data.split(":")[2])))

    @router.callback_query(F.data.startswith("m:tog:"))
    async def cb_toggle(cb: CallbackQuery):
        _, _, sid_s, module = cb.data.split(":", 3)
        sid = int(sid_s)
        row = hub.db.fetch_one("SELECT modules FROM servers WHERE id=?", (sid,))
        if row:
            mdict = json.loads(row[0]) if row[0] else {}
            mdict[module] = not mdict.get(module, False)
            hub.db.execute(
                "UPDATE servers SET modules=?, updated_at=CURRENT_TIMESTAMP WHERE id=?",
                (json.dumps(mdict), sid),
            )
        await render(cb, modules_screen(sid))
        await cb.answer("применится после перезапуска moonbridge")

    # ── удаление ────────────────────────────────────────────────

    @router.callback_query(F.data.startswith("m:del:"))
    async def cb_del(cb: CallbackQuery):
        sid = int(cb.data.split(":")[2])
        row = hub.db.fetch_one("SELECT name FROM servers WHERE id=?", (sid,))
        name = row[0] if row else "?"
        await render(cb, (
            f"🗑 Удалить ядро <b>{name}</b>?",
            _kb([
                [("✅ Да, удалить", f"m:delok:{sid}")],
                [("⬅️ Отмена", f"m:srv:{sid}")],
            ]),
        ))

    @router.callback_query(F.data.startswith("m:delok:"))
    async def cb_delok(cb: CallbackQuery):
        sid = int(cb.data.split(":")[2])
        hub.db.execute("DELETE FROM servers WHERE id=?", (sid,))
        await render(cb, servers_screen())
        await cb.answer("удалено · применится после перезапуска moonbridge")

    # ── добавление ядра (FSM: имя → ключ) ───────────────────────

    @router.callback_query(F.data == "m:addsrv")
    async def cb_addsrv(cb: CallbackQuery, state: FSMContext):
        await state.set_state(AddServer.name)
        await cb.message.edit_text(
            "➕ <b>Добавить ядро</b>\n\nПришли <b>имя</b> ядра одним сообщением:",
            parse_mode="HTML",
        )
        await cb.answer()

    @router.message(AddServer.name)
    async def add_name(msg: Message, state: FSMContext):
        await state.update_data(name=(msg.text or "").strip())
        await state.set_state(AddServer.key)
        await msg.answer("Теперь пришли <b>ключ</b> (экспорт из MoonBot):", parse_mode="HTML")

    @router.message(AddServer.key)
    async def add_key(msg: Message, state: FSMContext):
        data = await state.get_data()
        name = data.get("name", "").strip()
        key = (msg.text or "").strip()
        await state.clear()
        # Ключ — секрет: убираем сообщение с ним из истории чата.
        try:
            await msg.delete()
        except Exception:  # noqa: BLE001 — нет прав/старое сообщение
            pass
        if not name or not key:
            text, kb = main_screen()
            await msg.answer("пустое имя или ключ — отменил.")
            await msg.answer(text, reply_markup=kb, parse_mode="HTML")
            return
        hub.db.execute(
            "INSERT INTO servers (name, token, modules) VALUES (?, ?, ?) "
            "ON CONFLICT (name) DO UPDATE SET token=excluded.token",
            (name, key, json.dumps(DEFAULT_MODULES)),
        )
        await msg.answer(
            f"✅ ядро <b>{name}</b> сохранено. 🔒 ключ удалён из чата. "
            f"ip/port возьмутся из ключа." + RESTART_HINT,
            reply_markup=_kb([[("🖥 К ядрам", "m:servers")]]),
            parse_mode="HTML",
        )

    return router

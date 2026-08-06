"""Отчёт профит — прибыль по серверам за период (порт handlers/reports.py, тексты 1:1).

Меню: 💰 Отчёт профит → Сегодня / Неделя / Этот месяц → таблица по серверам из orders.
"""

from __future__ import annotations

from datetime import datetime, timedelta

from aiogram import F, Router
from aiogram.types import CallbackQuery

from hub.core.tg import kb, render

MENU_ENTRY = ("💰 Отчёт профит", "rep:menu")

_PERIODS = {
    "today": ("за Сегодня", lambda now: now.replace(hour=0, minute=0, second=0, microsecond=0)),
    "week": ("за Текущую Неделю",
             lambda now: (now - timedelta(days=now.weekday())).replace(hour=0, minute=0, second=0, microsecond=0)),
    "month": ("за Этот Месяц", lambda now: now.replace(day=1, hour=0, minute=0, second=0, microsecond=0)),
}


def _menu_screen():
    return "Выберите период для отчета:", kb([[
        ("Сегодня", "rep:p:today"), ("Неделя", "rep:p:week"), ("Этот месяц", "rep:p:month"),
    ]])


def _report_screen(db, period: str):
    if period not in _PERIODS:
        return "Неизвестный период", kb([[("⬅️ Назад", "rep:menu")]])
    title, start_fn = _PERIODS[period]
    now = datetime.now()
    start_ts, end_ts = int(start_fn(now).timestamp()), int(now.timestamp())
    rows = db.fetch_all(
        "SELECT COALESCE(s.name, 'Unknown (' || o.server_id || ')') AS server_name, "
        "COUNT(*) AS order_count, SUM(o.profitbtc) AS total_profit, SUM(o.spentbtc) AS total_volume "
        "FROM orders o LEFT JOIN servers s ON o.server_id = s.id "
        "WHERE o.buydate >= ? AND o.buydate <= ? GROUP BY s.name, o.server_id ORDER BY s.name ASC",
        (start_ts, end_ts),
    )
    if not rows:
        msg = f"📊 **Отчет {title}**\n\nНичего не найдено."
    else:
        header = f"{'Сервер':<12} | {'Орд':>5} | {'Проф':>7} | {'Объем':>7}"
        table = [header, "-" * len(header)]
        total_orders, total_profit = 0, 0.0
        for srv_name, cnt, prof, vol in rows:
            p, v = float(prof or 0), float(vol or 0)
            total_orders += cnt
            total_profit += p
            table.append(f"{srv_name:<12} | {cnt:>5} | {p:>7.1f} | {v / 1000:>6.1f}k")
        msg = (f"📊 **Отчет {title}**\n```\n" + "\n".join(table) + "\n```"
               f"\n🔥 **ИТОГО:** `{total_orders}` орд. | Пр: `{total_profit:.1f}`")
    return msg, kb([[("⬅️ Период", "rep:menu")]])


def build_router(hub) -> Router:
    router = Router(name="reports")

    @router.callback_query(F.data == "rep:menu")
    async def cb_menu(cb: CallbackQuery):
        await render(cb, *_menu_screen(), parse_mode=None)

    @router.callback_query(F.data.startswith("rep:p:"))
    async def cb_report(cb: CallbackQuery):
        await render(cb, *_report_screen(hub.db, cb.data.split(":")[2]), parse_mode="Markdown")

    return router

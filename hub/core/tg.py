"""Общие Telegram-хелперы: сборка inline-клавиатуры и рендер экрана.

Убирает дублирование между базовым меню (base_handlers) и модулями: раньше
`_kb` и логика `edit_text(...) + answer()` копировались в каждый роутер.
"""

from __future__ import annotations

from aiogram.types import CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup


def kb(rows: list[list[tuple[str, str]]]) -> InlineKeyboardMarkup:
    """rows = [[(text, callback_data), ...], ...] → InlineKeyboardMarkup."""
    return InlineKeyboardMarkup(
        inline_keyboard=[
            [InlineKeyboardButton(text=t, callback_data=c) for t, c in row] for row in rows
        ]
    )


async def render(
    cb: CallbackQuery, text: str, markup: InlineKeyboardMarkup, *, parse_mode: str = "HTML"
) -> None:
    """Заменить сообщение экраном (text+markup) и закрыть callback. Идемпотентно
    глотает «message is not modified» и т.п."""
    try:
        await cb.message.edit_text(text, reply_markup=markup, parse_mode=parse_mode)
    except Exception:  # noqa: BLE001
        pass
    await cb.answer()

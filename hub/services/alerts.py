"""Alerts — уведомления с дедупом и сводками (порт notifier из mb_ai).

Модули НЕ шлют в Telegram напрямую — только через hub.alerts:
  * alert(key, text, dedup_sec) — не чаще раза в интервал на ключ (анти-спам);
  * bucket(service, category, detail) + flush(service) — накопить и отдать
    одним сводным сообщением (не спамить по событию).

Отправку делает переданный notify-корутин (bot → админам).
"""

import time


class Alerts:
    def __init__(self, notify, settings):
        self._notify = notify
        self.settings = settings
        self._last_sent: dict[str, float] = {}
        # service → category → ordered-set(detail). dict-ключи дедуплицируют
        # повторы между циклами (health в dry-run кладёт один и тот же
        # деградировавший emu каждый цикл — иначе сводка = циклы×N).
        self._buckets: dict[str, dict[str, dict[str, None]]] = {}

    async def alert(self, key: str, text: str, dedup_sec: int = 3600, reply_markup=None) -> bool:
        """Отправить, если по этому ключу не слали в последние dedup_sec секунд.

        reply_markup — опциональная inline-клавиатура (напр. кнопки скрыть/игнор)."""
        now = time.time()
        if now - self._last_sent.get(key, 0.0) < dedup_sec:
            return False
        self._last_sent[key] = now
        await self._notify(text, reply_markup)
        return True

    def bucket(self, service: str, category: str, detail: str, key: str | None = None) -> None:
        """Накопить событие для сводки (не отправляется сразу). Дедуп по `key`
        (по умолчанию = detail): один и тот же объект за окно сводки = одна строка,
        даже если его detail дрейфует между циклами (напр. причина по стратегии:
        «нет ордеров» ↔ «убыток» при пересечении границы окна). Показываем detail
        первого появления."""
        bucket = self._buckets.setdefault(service, {}).setdefault(category, {})
        bucket.setdefault(detail if key is None else key, detail)

    async def flush(self, service: str) -> bool:
        """Отправить накопленную сводку сервиса и очистить бакет."""
        cats = self._buckets.pop(service, {})
        if not cats:
            return False
        lines = [f"📊 {service} — сводка"]
        for cat, items in cats.items():
            details = list(items.values())  # detail первого появления, дедуп по ключу
            lines.append(f"\n{cat} ({len(details)}):")
            for it in details[:20]:
                lines.append(f"  • {it}")
            if len(details) > 20:
                lines.append(f"  … и ещё {len(details) - 20}")
        await self._notify("\n".join(lines))
        return True

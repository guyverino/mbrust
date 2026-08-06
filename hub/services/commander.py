"""Commander — совместимый шов старого `MoonbotCommander` поверх очереди команд.

Старые модули (analyzer/generator/cloner/ai) звали `MoonbotCommander.<verb>()` по
ИМЕНИ стратегии через UDP mooncmd. Здесь те же методы, но:
  * стратегия резолвится ИМЯ → moonbot_id (таблица `strategies`, per server);
  * действие кладётся в очередь `commands` (kind+payload) → moonbridge шлёт
    типизированную MoonProto-команду (см. moonbridge/src/send.rs).

Возвращает id команды в очереди (или None, если действие невозможно —
например стратегия не найдена). Благодаря этому логика модулей переносится почти
без правок: меняется только импорт коммандера.
"""

import logging
import time

logger = logging.getLogger(__name__)


class Commander:
    def __init__(self, db, commands):
        self._db = db
        self._commands = commands

    def _strategy_id(self, server_id: int, name: str):
        """Вернуть moonbot_id как u64 (MoonProto ждёт u64). В БД он хранится
        ЗНАКОВЫМ int64 (Delphi/BIGINT), поэтому битово переинтерпретируем:
        отрицательное `x` → `x + 2^64` (маска 64 бит)."""
        r = self._db.fetch_one(
            "SELECT moonbot_id FROM strategies WHERE server_id=? AND name=?",
            (server_id, name),
        )
        if not r:
            logger.warning("commander: стратегия %r не найдена на сервере %s", name, server_id)
            return None
        return r[0] & 0xFFFFFFFFFFFFFFFF

    # ── глобально по ядру ────────────────────────────────────────
    def start(self, server_id: int):
        return self._commands.enqueue(server_id, "strategy_start")

    def stop(self, server_id: int):
        return self._commands.enqueue(server_id, "strategy_stop")

    def cancel_all(self, server_id: int):
        return self._commands.enqueue(server_id, "cancel_all_orders")

    def sell_all(self, server_id: int):
        return self._commands.enqueue(server_id, "sell_all")  # TODO: kind в send.rs

    # ── по стратегии (ИМЯ → id) ──────────────────────────────────
    def start_strategy(self, server_id: int, strategy: str):
        sid = self._strategy_id(server_id, strategy)
        if sid is None:
            return None
        return self._commands.enqueue(
            server_id, "strategy_set_checked", {"strategy_id": sid, "checked": True}
        )

    def stop_strategy(self, server_id: int, strategy: str, time_min: int = 0):
        sid = self._strategy_id(server_id, strategy)
        if sid is None:
            return None
        if time_min:
            logger.info(
                "commander: sgStop %s на %s мин — таймер пока не поддержан, ставлю бессрочный стоп",
                strategy, time_min,
            )
        return self._commands.enqueue(
            server_id, "strategy_set_checked", {"strategy_id": sid, "checked": False}
        )

    def set_param(self, server_id: int, strategy: str, field: str, value):
        sid = self._strategy_id(server_id, strategy)
        if sid is None:
            return None
        return self._commands.enqueue(
            server_id, "strategy_set_param", {"strategy_id": sid, "field": field, "value": value}
        )

    def set_params(self, server_id: int, strategy: str, params: dict):
        """Батч-правка нескольких параметров ОДНОЙ стратегии ОДНОЙ командой.
        Пустой dict → ничего не шлём."""
        if not params:
            return None
        sid = self._strategy_id(server_id, strategy)
        if sid is None:
            return None
        return self._commands.enqueue(
            server_id, "strategy_set_params", {"strategy_id": sid, "params": dict(params)}
        )

    def set_params_many(self, server_id: int, items):
        """Правка МНОГИХ стратегий за ОДНУ команду. items — итерируемое из
        (имя_стратегии, params_dict). Резолвит имена→id и шлёт `strategy_edit_batch`:
        moonbridge применит все правки к полному списку и синкнет его ОДНИМ
        sync_local_strategies. Это обязательно для массовых правок — иначе
        отдельные sync затирают друг друга (стратегия пропадает из снапшота).
        Возвращает id команды или None, если правок не набралось."""
        edits = []
        for name, params in items:
            if not params:
                continue
            sid = self._strategy_id(server_id, name)
            if sid is None:
                continue
            edits.append({"strategy_id": sid, "params": dict(params)})
        if not edits:
            return None
        return self._commands.enqueue(server_id, "strategy_edit_batch", {"edits": edits})

    def pause_strategy(self, server_id: int, strategy: str, minutes: int):
        """Пауза стратегии на N минут (порт mooncmd `sgStop <S> <min>`): выставляем
        `PreventWorkingUntil = now + N*60` (unix-сек). Active/checked не трогаем —
        по истечении паузы стратегия сама возобновится."""
        until = int(time.time()) + int(minutes) * 60
        return self.set_param(server_id, strategy, "PreventWorkingUntil", until)

    def set_strategy_blacklist(self, server_id: int, strategy: str, coin: str, add: bool = True):
        """ЧС монеты В СТРАТЕГИИ (порт mooncmd SetBL+/-). MoonProto не умеет
        инкремент — читаем текущий `CoinsBlackList` из последней версии, правим
        comma-список, и шлём целиком через set_param."""
        import json
        coin = coin.upper()
        r = self._db.fetch_one(
            "SELECT sv.raw_data FROM strategies s JOIN strategy_versions sv ON sv.strategy_id=s.id "
            "WHERE s.server_id=? AND s.name=? ORDER BY sv.version_date DESC LIMIT 1",
            (server_id, strategy),
        )
        bl = ""
        if r and r[0]:
            try:
                d = json.loads(r[0]) if isinstance(r[0], str) else r[0]
                bl = (d.get("CoinsBlackList") or "") if isinstance(d, dict) else ""
            except (TypeError, json.JSONDecodeError):
                bl = ""
        coins = [c.strip() for c in bl.split(",") if c.strip()]
        coins = [c for c in coins if c.upper() != coin]  # дедуп/удаление
        if add:
            coins.append(coin)
        return self.set_param(server_id, strategy, "CoinsBlackList", ",".join(coins))

    # ── монеты / плечо ───────────────────────────────────────────
    def sell_coin(self, server_id: int, coin: str):
        return self._commands.enqueue(server_id, "panic_sell_coin", {"coin": coin.upper()})

    def set_global_blacklist(self, server_id: int, coin: str, add: bool = True):
        kind = "coin_blacklist_add" if add else "coin_blacklist_remove"
        return self._commands.enqueue(server_id, kind, {"coin": coin.upper()})

    def set_leverage(self, server_id: int, market: str, leverage: int):
        return self._commands.enqueue(
            server_id, "set_leverage", {"market": market, "leverage": int(leverage)}
        )

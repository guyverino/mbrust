"""Реестр/загрузчик модулей.

Сканирует пакеты в `hub/modules/`, импортирует каждый, берёт `MODULE`. Для
успешно загруженных: апсертит строку в каталоге `modules`, применяет их
миграции, проверяет зависимости. «Не установлен» = пакета нет в hub/modules/
→ не сканируется → фича не активна. Ошибка импорта → модуль помечается locked
(не роняет hub).

Порядок в bot.py: discover() → apply_migrations()+setup() для не-locked →
include routers → запуск background tasks.
"""

from __future__ import annotations

import importlib
import json
import logging
import pkgutil
from dataclasses import dataclass, field

from hub.sdk import HubServices, Module

logger = logging.getLogger(__name__)


@dataclass
class Loaded:
    name: str
    module: Module | None = None
    locked: bool = False
    reason: str = ""
    deps_missing: list[str] = field(default_factory=list)

    @property
    def active(self) -> bool:
        return not self.locked and self.module is not None


def discover() -> list[Loaded]:
    """Импортировать все пакеты из hub/modules/, вернуть список Loaded."""
    import hub.modules as pkg

    out: list[Loaded] = []
    for _finder, name, _ispkg in pkgutil.iter_modules(pkg.__path__):
        if name.startswith("_"):
            continue
        try:
            mod = importlib.import_module(f"hub.modules.{name}")
            obj = getattr(mod, "MODULE")
            if not isinstance(obj, Module):
                raise TypeError("MODULE не является экземпляром hub.sdk.Module")
            out.append(Loaded(name=obj.name or name, module=obj))
            logger.info("module discovered: %s", obj.name or name)
        except Exception as e:  # noqa: BLE001
            out.append(Loaded(name=name, locked=True, reason=str(e)))
            logger.warning("module '%s' locked: %s", name, e)
    return out


def check_dependencies(loaded: list[Loaded]) -> None:
    """Пометить locked те модули, чьи depends_on не активны."""
    active_names = {l.name for l in loaded if l.active}
    for l in loaded:
        if not l.active:
            continue
        missing = [d for d in (l.module.depends_on or []) if d not in active_names]
        if missing:
            l.locked = True
            l.deps_missing = missing
            l.reason = f"missing deps: {', '.join(missing)}"
            logger.warning("module '%s' locked: %s", l.name, l.reason)


def register_catalog(db, l: Loaded) -> None:
    """Апсерт строки модуля в каталог `modules`."""
    m = l.module
    db.execute(
        "INSERT INTO modules (name, description, category, dependencies, default_enabled) "
        "VALUES (?, ?, ?, ?, 1) "
        "ON CONFLICT (name) DO UPDATE SET description=excluded.description, "
        "category=excluded.category, dependencies=excluded.dependencies",
        (m.name, m.description or m.name, m.category, json.dumps(list(m.depends_on or []))),
    )


def apply_migrations(db, l: Loaded) -> None:
    for sql in l.module.migrations():
        db.execute(sql)


async def activate(db, hub: HubServices, loaded: list[Loaded]) -> list[Loaded]:
    """Каталог + миграции + setup для активных модулей. Возвращает активные."""
    active = [l for l in loaded if l.active]
    for l in active:
        try:
            register_catalog(db, l)
            apply_migrations(db, l)
            await l.module.setup(hub)
            logger.info("module activated: %s", l.name)
        except Exception as e:  # noqa: BLE001
            l.locked = True
            l.reason = f"activate failed: {e}"
            logger.error("module '%s' activate failed: %s", l.name, e)
    return [l for l in loaded if l.active]

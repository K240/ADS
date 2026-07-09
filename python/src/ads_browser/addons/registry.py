"""Ordered registry of context-menu addons."""

from __future__ import annotations

from ads_browser.addons.base import ContextMenuAddon

_addons: list[ContextMenuAddon] = []


def register_addon(addon: ContextMenuAddon) -> None:
    """Append an addon (call from host / DCC bootstrap)."""
    _addons.append(addon)


def iter_addons() -> tuple[ContextMenuAddon, ...]:
    return tuple(_addons)


def clear_addons() -> None:
    """Tests / hosts that need a fresh registry."""
    _addons.clear()

"""Public API for ADS context-menu addons."""

from __future__ import annotations

from ads_browser.addons.base import AdsAssetContext, ContextMenuAddon, MenuItem
from ads_browser.addons.example import ExampleContextMenuAddon
from ads_browser.addons.registry import clear_addons, iter_addons, register_addon

# hou_addon is imported lazily by hosts; keep it optional so `import ads_browser.addons`
# never requires Houdini.

__all__ = [
    "AdsAssetContext",
    "ContextMenuAddon",
    "ExampleContextMenuAddon",
    "MenuItem",
    "clear_addons",
    "iter_addons",
    "register_addon",
]

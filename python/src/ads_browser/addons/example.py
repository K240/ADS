"""Sample addon: copy ADS URI / asset key (template for DCC hooks)."""

from __future__ import annotations

import sys
from collections.abc import Callable
from typing import Any

from ads_browser.addons.base import AdsAssetContext, ContextMenuAddon, MenuItem
from ads_browser.uri import build_asset_uri


def _default_copy(text: str) -> None:
    try:
        from PySide6.QtGui import QGuiApplication

        app = QGuiApplication.instance()
        if app is not None:
            clipboard = app.clipboard()
            if clipboard is not None:
                clipboard.setText(text)
                return
    except ImportError:
        pass
    print(text, file=sys.stderr)


class ExampleContextMenuAddon(ContextMenuAddon):
    """Register with ``register_addon(ExampleContextMenuAddon())``."""

    def __init__(self, *, copy_text: Callable[[str], None] | None = None) -> None:
        self._copy_text = copy_text or _default_copy

    @property
    def addon_id(self) -> str:
        return "example"

    def menu_items(self, ctx: AdsAssetContext) -> list[MenuItem]:
        items = [
            MenuItem("copy_uri", "Copy ADS URI"),
            MenuItem("copy_key", "Copy asset key"),
        ]
        if ctx.version is not None and str(ctx.version) != "":
            items.append(MenuItem("copy_uri_version", "Copy ADS URI (versioned)"))
        if ctx.manifest_entries:
            items.append(MenuItem("log_manifest", "Log manifest (stderr)"))
        return items

    def invoke(self, ctx: AdsAssetContext, action_id: str) -> None:
        if action_id == "copy_uri":
            self._copy_text(ctx.ads_uri)
        elif action_id == "copy_key":
            self._copy_text(ctx.asset_key)
        elif action_id == "copy_uri_version":
            uri = build_asset_uri(
                ctx.category,
                ctx.asset_code,
                ctx.department,
                ctx.version,
            )
            self._copy_text(uri)
        elif action_id == "log_manifest":
            print(
                f"[{self.addon_id}] {ctx.asset_key}@{ctx.version}: "
                f"{len(ctx.manifest_entries)} entries",
                file=sys.stderr,
            )
            for entry in ctx.manifest_entries:
                path = entry.get("relative_path", "")
                size = entry.get("size", "")
                print(f"  {path}\t{size}", file=sys.stderr)

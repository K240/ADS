"""Houdini context-menu addon: LOP / stage actions for selected ADS assets."""

from __future__ import annotations

import sys
from collections.abc import Callable

from ads_browser.addons.base import AdsAssetContext, ContextMenuAddon, MenuItem
from ads_browser.uri import build_asset_uri


def _hou_module():
    try:
        import hou  # type: ignore[import-not-found]
    except ImportError:
        return None
    return hou


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


class HoudiniContextMenuAddon(ContextMenuAddon):
    """Inject via ``create_ads_browser(..., addons=[HoudiniContextMenuAddon()])``."""

    def __init__(self, *, copy_text: Callable[[str], None] | None = None) -> None:
        self._copy_text = copy_text or _default_copy

    @property
    def addon_id(self) -> str:
        return "houdini"

    @staticmethod
    def _active_editor():
        hou = _hou_module()
        if hou is None:
            return None
        return hou.ui.paneTabOfType(hou.paneTabType.NetworkEditor)

    def menu_items(self, ctx: AdsAssetContext) -> list[MenuItem]:
        items: list[MenuItem] = []
        editor = self._active_editor()
        if editor is None:
            return items

        pwd = editor.pwd()
        if pwd is None:
            return items

        type_name = pwd.type().name()
        # LOP / stage networks: offer reference + copy URI
        if type_name in ("stage", "lopnet", "manager"):
            items.append(MenuItem("houdini.copy_uri", "Copy ADS URI"))
            items.append(
                MenuItem(
                    "houdini.create_reference",
                    "Create Reference (ads://)",
                )
            )
            if ctx.version is not None and str(ctx.version) != "":
                items.append(
                    MenuItem(
                        "houdini.create_reference_versioned",
                        f"Create Reference ?v={ctx.version}",
                    )
                )
        else:
            # Other networks: still allow copy for convenience when Houdini is up
            items.append(MenuItem("houdini.copy_uri", "Copy ADS URI"))

        return items

    def invoke(self, ctx: AdsAssetContext, action_id: str) -> None:
        if action_id == "houdini.copy_uri":
            self._copy_text(ctx.ads_uri)
            return

        if action_id == "houdini.create_reference":
            self._create_reference(ctx, versioned=False)
            return

        if action_id == "houdini.create_reference_versioned":
            self._create_reference(ctx, versioned=True)
            return

    def _create_reference(self, ctx: AdsAssetContext, *, versioned: bool) -> None:
        hou = _hou_module()
        if hou is None:
            raise RuntimeError("hou module is not available")

        editor = self._active_editor()
        if editor is None:
            raise RuntimeError("No active NetworkEditor")
        parent = editor.pwd()
        if parent is None:
            raise RuntimeError("NetworkEditor has no pwd()")

        uri = (
            build_asset_uri(ctx.category, ctx.asset_code, ctx.department, ctx.version)
            if versioned
            else ctx.ads_uri
        )
        # Prefer a LOP reference when inside a stage; otherwise fall back to
        # copying the URI so the artist can paste into a File / Sublayer parm.
        type_name = parent.type().name()
        if type_name in ("stage", "lopnet"):
            node_name = f"ads_{ctx.asset_code}_{ctx.department}"
            node_name = "".join(c if c.isalnum() or c == "_" else "_" for c in node_name)
            ref = parent.createNode("reference", node_name)
            # Common LOP reference parm names across Houdini versions.
            for parm_name in ("filepath1", "filepath", "file"):
                parm = ref.parm(parm_name)
                if parm is not None:
                    parm.set(uri)
                    break
            else:
                self._copy_text(uri)
                print(
                    f"[{self.addon_id}] created {ref.path()} but no filepath parm; "
                    f"URI copied: {uri}",
                    file=sys.stderr,
                )
                return
            ref.moveToGoodPosition()
            print(f"[{self.addon_id}] {ref.path()} → {uri}", file=sys.stderr)
        else:
            self._copy_text(uri)
            print(
                f"[{self.addon_id}] not in LOP stage ({type_name}); URI copied: {uri}",
                file=sys.stderr,
            )

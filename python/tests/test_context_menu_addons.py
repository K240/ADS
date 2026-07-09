"""Tests for ADS context-menu addons (Qt-free where possible)."""

from __future__ import annotations

import unittest

from ads_browser.addons.base import AdsAssetContext, MenuItem
from ads_browser.addons.example import ExampleContextMenuAddon
from ads_browser.addons.registry import clear_addons, iter_addons, register_addon
from ads_browser.uri import build_asset_uri


def _ctx(**overrides) -> AdsAssetContext:
    base = dict(
        profile="default",
        category="char",
        asset_code="hero",
        department="model",
        version="3",
        ads_uri=build_asset_uri("char", "hero", "model"),
        current=3,
        latest=3,
        thumbnail_sha256="",
        manifest_entries=(),
        wip_seqs=(),
    )
    base.update(overrides)
    return AdsAssetContext(**base)


class RegistryTests(unittest.TestCase):
    def tearDown(self) -> None:
        clear_addons()

    def test_register_and_iter(self) -> None:
        clear_addons()
        self.assertEqual(iter_addons(), ())
        addon = ExampleContextMenuAddon(copy_text=lambda _t: None)
        register_addon(addon)
        self.assertEqual(len(iter_addons()), 1)
        self.assertIs(iter_addons()[0], addon)


class ExampleAddonTests(unittest.TestCase):
    def test_menu_items_include_versioned_when_version_set(self) -> None:
        copied: list[str] = []
        addon = ExampleContextMenuAddon(copy_text=copied.append)
        items = addon.menu_items(_ctx(version="2"))
        ids = [i.action_id for i in items]
        self.assertIn("copy_uri", ids)
        self.assertIn("copy_key", ids)
        self.assertIn("copy_uri_version", ids)
        self.assertNotIn("log_manifest", ids)

    def test_menu_items_omit_versioned_without_version(self) -> None:
        addon = ExampleContextMenuAddon(copy_text=lambda _t: None)
        ids = [i.action_id for i in addon.menu_items(_ctx(version=None))]
        self.assertNotIn("copy_uri_version", ids)

    def test_menu_items_log_manifest_when_entries_present(self) -> None:
        addon = ExampleContextMenuAddon(copy_text=lambda _t: None)
        ctx = _ctx(
            manifest_entries=(
                {"relative_path": "hero.usd", "size": 10},
            )
        )
        ids = [i.action_id for i in addon.menu_items(ctx)]
        self.assertIn("log_manifest", ids)

    def test_invoke_copy_uri_and_key(self) -> None:
        copied: list[str] = []
        addon = ExampleContextMenuAddon(copy_text=copied.append)
        ctx = _ctx()
        addon.invoke(ctx, "copy_uri")
        addon.invoke(ctx, "copy_key")
        self.assertEqual(copied[0], ctx.ads_uri)
        self.assertEqual(copied[1], "char/hero/model")

    def test_invoke_copy_uri_version(self) -> None:
        copied: list[str] = []
        addon = ExampleContextMenuAddon(copy_text=copied.append)
        ctx = _ctx(version="7")
        addon.invoke(ctx, "copy_uri_version")
        self.assertEqual(copied[0], "ads://char/hero/model/hero.usd?v=7")


class ContextMenuBridgeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        try:
            from PySide6.QtWidgets import QApplication
        except ImportError as exc:  # pragma: no cover
            raise unittest.SkipTest("PySide6 not installed") from exc
        import sys

        cls._app = QApplication.instance() or QApplication(sys.argv)

    def tearDown(self) -> None:
        clear_addons()

    def test_prepare_menu_flattens_multiple_addons(self) -> None:
        from ads_browser.addons.base import ContextMenuAddon
        from ads_browser.context_menu import ContextMenuBridge, ContextMenuModel

        class ExtraAddon(ContextMenuAddon):
            @property
            def addon_id(self) -> str:
                return "extra"

            def menu_items(self, ctx: AdsAssetContext) -> list[MenuItem]:
                return [MenuItem("extra.ping", "Ping")]

            def invoke(self, ctx: AdsAssetContext, action_id: str) -> None:
                self.last = (ctx.asset_key, action_id)

        copied: list[str] = []
        example = ExampleContextMenuAddon(copy_text=copied.append)
        extra = ExtraAddon()
        model = ContextMenuModel()
        bridge = ContextMenuBridge(model, addons=[example, extra])
        bridge.prepareMenu("default", "char", "hero", "model", "3")

        self.assertGreaterEqual(model.rowCount(), 3)
        labels = [model.data(model.index(i, 0)) for i in range(model.rowCount())]
        self.assertIn("Copy ADS URI", labels)
        self.assertIn("Ping", labels)

        # Invoke the Ping row (last enabled item from extra).
        ping_row = labels.index("Ping")
        bridge.invoke(ping_row)
        self.assertEqual(extra.last, ("char/hero/model", "extra.ping"))

        # Invoke copy_uri
        uri_row = labels.index("Copy ADS URI")
        bridge.invoke(uri_row)
        self.assertTrue(copied)
        self.assertTrue(copied[-1].startswith("ads://char/hero/model/"))

    def test_host_injection_ignores_global_registry(self) -> None:
        from ads_browser.context_menu import ContextMenuBridge, ContextMenuModel

        register_addon(ExampleContextMenuAddon(copy_text=lambda _t: None))
        model = ContextMenuModel()
        bridge = ContextMenuBridge(model, addons=())
        bridge.prepareMenu("default", "char", "hero", "model", "")
        self.assertEqual(model.rowCount(), 0)


class HoudiniAddonImportTests(unittest.TestCase):
    def test_hou_addon_imports_without_hou(self) -> None:
        from ads_browser.addons.hou_addon import HoudiniContextMenuAddon

        addon = HoudiniContextMenuAddon(copy_text=lambda _t: None)
        self.assertEqual(addon.addon_id, "houdini")
        # Outside Houdini, menu is empty (no NetworkEditor).
        self.assertEqual(addon.menu_items(_ctx()), [])


if __name__ == "__main__":
    unittest.main()

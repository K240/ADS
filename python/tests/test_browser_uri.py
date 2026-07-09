"""Tests for ads_browser helpers (no Qt display required)."""

from __future__ import annotations

import unittest

from ads_browser.uri import build_asset_uri


class BuildAssetUriTests(unittest.TestCase):
    def test_without_version(self):
        self.assertEqual(
            build_asset_uri("char", "hero", "model"),
            "ads://char/hero/model/hero.usd",
        )

    def test_with_version(self):
        self.assertEqual(
            build_asset_uri("char", "hero", "model", 3),
            "ads://char/hero/model/hero.usd?v=3",
        )

    def test_wip_version_is_encoded(self):
        self.assertEqual(
            build_asset_uri("char", "hero", "model", "wip"),
            "ads://char/hero/model/hero.usd?v=wip",
        )


if __name__ == "__main__":
    unittest.main()

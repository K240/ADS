import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from ads.client import AdsCommandError
from ads.usd_deps import (
    build_pull_plan,
    collect_ads_dependencies,
    collect_usd_dependencies,
    execute_pull_plan,
    parse_ads_uri,
)


class FakeAds:
    def __init__(self):
        self.calls = []
        self.materialized = set()

    def resolve(self, asset_path, *, store, workspace=None, mode="local", remote_base_url=None):
        self.calls.append(("resolve", asset_path, store, workspace, mode))
        if "hero" in asset_path or asset_path in self.materialized:
            return "D:/workspace/.ads-cache/manifests/abc123/hero.usd"
        # Schema v8: execute warms the view cache via resolve, so the fake
        # succeeds on the second attempt for any path.
        self.materialized.add(asset_path)
        raise AdsCommandError(["ads", "resolve"], 1, "", "missing local objects")


class UsdDepsTests(unittest.TestCase):
    def test_collects_ads_dependencies_from_text_usd_and_local_sublayer(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            sub = Path(temp) / "sub.usda"
            root.write_text(
                """
#usda 1.0
(
    subLayers = [@sub.usda@]
)
def Xform "root" (
    references = @ads://char/hero/model/hero.usd@
)
{
}
""",
                encoding="utf-8",
            )
            sub.write_text(
                """
#usda 1.0
def Xform "sub" (
    references = @ads://prop/crate/model/crate.usd?v=v003@
)
{
}
""",
                encoding="utf-8",
            )

            all_dependencies = collect_usd_dependencies(root)
            ads_dependencies = collect_ads_dependencies(root)

        self.assertIn("sub.usda", all_dependencies)
        self.assertEqual(
            ads_dependencies,
            [
                "ads://char/hero/model/hero.usd",
                "ads://prop/crate/model/crate.usd?v=v003",
            ],
        )

    def test_udim_style_tokens_stay_part_of_the_uri(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text(
                """
#usda 1.0
def Material "skin"
{
    asset inputs:file = @ads://tex/skin/lookdev/skin.<UDIM>.tx@
    asset inputs:cache = "ads://fx/burn/sim/cache.<F4>.vdb"
}
""",
                encoding="utf-8",
            )

            ads_dependencies = collect_ads_dependencies(root)

        self.assertEqual(
            ads_dependencies,
            [
                "ads://tex/skin/lookdev/skin.<UDIM>.tx",
                "ads://fx/burn/sim/cache.<F4>.vdb",
            ],
        )

    def test_triple_at_escaped_asset_paths_are_collected(self):
        # USD escapes asset paths containing @ as @@@...@@@; a single @
        # inside the triple form is literal.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text(
                """
#usda 1.0
def Xform "root" (
    references = @@@D:/textures/tex@4k/hero.usd@@@
)
{
}
""",
                encoding="utf-8",
            )

            all_dependencies = collect_usd_dependencies(root)

        self.assertIn("D:/textures/tex@4k/hero.usd", all_dependencies)

    def test_mixed_file_with_udim_escaped_and_plain_references(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text(
                """
#usda 1.0
def Xform "root" (
    references = [
        @ads://char/hero/model/hero.usd@,
        @@@D:/textures/tex@4k/hero.usd@@@
    ]
)
{
    asset inputs:file = @ads://tex/skin/lookdev/skin.<UDIM>.tx@
}
""",
                encoding="utf-8",
            )

            all_dependencies = collect_usd_dependencies(root)
            ads_dependencies = collect_ads_dependencies(root)

        self.assertIn("D:/textures/tex@4k/hero.usd", all_dependencies)
        self.assertEqual(
            ads_dependencies,
            [
                "ads://char/hero/model/hero.usd",
                "ads://tex/skin/lookdev/skin.<UDIM>.tx",
            ],
        )

    def test_missing_pxr_warns_before_text_fallback(self):
        # The plain test environment has no pxr; downgrading to the text
        # scanner must be loud, not silent.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text("#usda 1.0\n", encoding="utf-8")

            with self.assertWarns(RuntimeWarning):
                collect_usd_dependencies(root)

    def test_pxr_path_uses_dependency_metadata_without_serializing(self):
        # Structural check with a stubbed pxr: ads:// dependencies come from
        # ComputeAllDependencies results and per-layer composition metadata,
        # never from ExportToString, and the text fallback stays off. The
        # real-pxr behavior is covered by the hython smoke test.
        class FakeLayer:
            def __init__(self, identifier):
                self.identifier = identifier
                self.realPath = identifier

            def GetCompositionAssetDependencies(self):
                return ["ads://char/hero/model/hero.usd", "./local.usd"]

            @property
            def externalReferences(self):
                return ["ads://tex/skin/lookdev/skin.<UDIM>.tx"]

            def ExportToString(self):
                raise AssertionError("pxr path must not serialize layers")

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text(
                """
#usda 1.0
def Xform "root" (
    references = @ads://only/in/text/never.usd@
)
{
}
""",
                encoding="utf-8",
            )

            layer = FakeLayer(str(root))
            fake_pxr = types.ModuleType("pxr")
            fake_pxr.UsdUtils = types.SimpleNamespace(
                ComputeAllDependencies=lambda path: (
                    [layer],
                    ["D:/textures/wood.jpg"],
                    ["ads://prop/crate/model/crate.usd?v=3"],
                )
            )
            with patch.dict(sys.modules, {"pxr": fake_pxr}):
                ads_dependencies = collect_ads_dependencies(root)

        self.assertEqual(
            ads_dependencies,
            [
                "ads://char/hero/model/hero.usd",
                "ads://tex/skin/lookdev/skin.<UDIM>.tx",
                "ads://prop/crate/model/crate.usd?v=3",
            ],
        )

    def test_parse_ads_uri_supports_category_and_version(self):
        simple = parse_ads_uri("ads://hero/model/hero.usd")
        self.assertIsNone(simple.category)
        self.assertEqual(simple.asset_code, "hero")
        self.assertEqual(simple.department, "model")

        categorized = parse_ads_uri("ads://env/city/street/model/v003/street.usd")
        self.assertEqual(categorized.category, "env/city")
        self.assertEqual(categorized.asset_code, "street")
        self.assertEqual(categorized.department, "model")
        self.assertEqual(categorized.version, "v003")

        query_version = parse_ads_uri("ads://char/hero/model/hero.usd?v=v002")
        self.assertEqual(query_version.category, "char")
        self.assertEqual(query_version.version, "v002")

        integer_version = parse_ads_uri("ads://char/hero/model/hero.usd?v=2")
        self.assertEqual(integer_version.version, "2")

    def test_build_pull_plan_marks_present_and_missing_dependencies(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text(
                """
#usda 1.0
def Xform "root" (
    references = [
        @ads://char/hero/model/hero.usd@,
        @ads://prop/crate/model/crate.usd?v=v003@
    ]
)
{
}
""",
                encoding="utf-8",
            )

            fake = FakeAds()
            plan = build_pull_plan(root, store="D:/store", workspace="D:/workspace", ads=fake)

        self.assertEqual(len(plan.dependencies), 2)
        self.assertEqual(plan.dependencies[0].action, "none")
        self.assertTrue(plan.dependencies[0].present)
        self.assertEqual(plan.dependencies[1].action, "materialize")
        self.assertFalse(plan.dependencies[1].present)
        self.assertEqual(plan.pull_urls, ["ads://prop/crate/model/crate.usd?v=v003"])

    def test_execute_pull_plan_materializes_via_resolve(self):
        # Schema v8: executing the plan warms the manifest view cache with a
        # plain local resolve; the canonical pin form is a bare integer.
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "root.usda"
            root.write_text(
                """
#usda 1.0
def Xform "root" (
    references = @ads://prop/crate/model/crate.usd?v=3@
)
{
}
""",
                encoding="utf-8",
            )

            fake = FakeAds()
            plan = build_pull_plan(root, store="D:/store", workspace="D:/workspace", ads=fake)
            self.assertEqual(plan.dependencies[0].action, "materialize")
            execute_pull_plan(plan, store="D:/store", workspace="D:/workspace", ads=fake)

        item = plan.dependencies[0]
        self.assertEqual(item.action, "none")
        self.assertTrue(item.present)
        self.assertIn(".ads-cache/manifests", item.local_path)
        resolve_calls = [call for call in fake.calls if call[0] == "resolve"]
        self.assertEqual(len(resolve_calls), 2)
        self.assertEqual(resolve_calls[1][1], "ads://prop/crate/model/crate.usd?v=3")


if __name__ == "__main__":
    unittest.main()

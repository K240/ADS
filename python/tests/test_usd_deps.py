import tempfile
import unittest
from pathlib import Path

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

    def resolve(self, asset_path, *, store, workspace=None, mode="local", remote_base_url=None):
        self.calls.append(("resolve", asset_path, store, workspace, mode))
        if "hero" in asset_path:
            return "D:/workspace/char/hero/model/v002/hero.usd"
        raise AdsCommandError(["ads", "resolve"], 1, "", "missing local file")

    def restore(self, **kwargs):
        self.calls.append(("restore", kwargs))
        return "restored"

    def pull(self, **kwargs):
        self.calls.append(("pull", kwargs))
        return "pulled"


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
        self.assertEqual(plan.dependencies[1].action, "restore")
        self.assertFalse(plan.dependencies[1].present)
        self.assertEqual(plan.pull_urls, ["ads://prop/crate/model/crate.usd?v=v003"])

    def test_execute_pull_plan_restores_explicit_version(self):
        # The canonical v8 pin form is a bare integer; v### stays accepted.
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
            execute_pull_plan(plan, store="D:/store", workspace="D:/workspace", ads=fake)

        restore_calls = [call for call in fake.calls if call[0] == "restore"]
        self.assertEqual(len(restore_calls), 1)
        self.assertEqual(restore_calls[0][1]["version"], "3")


if __name__ == "__main__":
    unittest.main()

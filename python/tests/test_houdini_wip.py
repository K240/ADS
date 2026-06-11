import unittest

from ads.houdini_wip import WipStaging


class FakeAds:
    def __init__(self):
        self.calls = []

    def wip_add(self, *, store, category, asset_code, department, source):
        self.calls.append(
            {
                "store": store,
                "category": category,
                "asset_code": asset_code,
                "department": department,
                "source": source,
            }
        )
        return "registered wip seq=1"


class WipStagingTests(unittest.TestCase):
    def _staging(self):
        return WipStaging(
            workspace_root=r"D:\workspace",
            category="char",
            asset_code="hero",
            department="model",
            run_id="run1",
        )

    def test_redirects_department_saves_into_staging_run(self):
        staging = self._staging()

        redirected = staging.redirect(r"D:\workspace\char\hero\model\hero.usd")
        self.assertEqual(
            redirected,
            "D:/workspace/.ads-staging/run1/char/hero/model/hero.usd",
        )

        nested = staging.redirect(r"D:\workspace\char\hero\model\geo\body.usd")
        self.assertEqual(
            nested,
            "D:/workspace/.ads-staging/run1/char/hero/model/geo/body.usd",
        )

    def test_paths_outside_the_department_root_pass_through(self):
        staging = self._staging()

        other_department = r"D:\workspace\char\hero\anim\clip.usd"
        self.assertEqual(staging.redirect(other_department), other_department)

        external = r"E:\renders\beauty.exr"
        self.assertEqual(staging.redirect(external), external)

    def test_from_environment_reads_wip_target(self):
        staging = WipStaging.from_environment(
            {
                "ADS_RESOLVER_WORKSPACE": r"D:\workspace",
                "ADS_WIP_CATEGORY": "env/city",
                "ADS_WIP_ASSET_CODE": "street",
                "ADS_WIP_DEPARTMENT": "layout",
                "ADS_WIP_RUN_ID": "run9",
            }
        )

        self.assertEqual(
            staging.redirect(r"D:\workspace\env\city\street\layout\street.usd"),
            "D:/workspace/.ads-staging/run9/env/city/street/layout/street.usd",
        )

    def test_commit_registers_staged_folder_and_cleans_up(self):
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as temp:
            staging = WipStaging(
                workspace_root=temp,
                category="char",
                asset_code="hero",
                department="model",
                run_id="run1",
            )
            staged_file = Path(staging.staging_root) / "hero.usd"
            staged_file.parent.mkdir(parents=True)
            staged_file.write_text("wip", encoding="utf-8")

            fake = FakeAds()
            output = staging.commit(store=r"D:\store", ads=fake)

            self.assertEqual(output, "registered wip seq=1")
            self.assertEqual(len(fake.calls), 1)
            self.assertEqual(fake.calls[0]["category"], "char")
            self.assertEqual(fake.calls[0]["source"], staging.staging_root)
            self.assertFalse(Path(staging.staging_root).exists())

    def test_commit_without_staged_writes_is_a_no_op(self):
        staging = self._staging()
        fake = FakeAds()
        self.assertIsNone(staging.commit(store=r"D:\store", ads=fake))
        self.assertEqual(fake.calls, [])


if __name__ == "__main__":
    unittest.main()

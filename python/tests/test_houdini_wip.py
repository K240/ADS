import importlib.util
import os
import sys
import tempfile
import time
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from ads import houdini_wip
from ads.houdini_wip import WipStaging, commit_staged


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
        return f"registered wip seq={len(self.calls)}"


class FailingAds:
    def wip_add(self, **kwargs):
        raise RuntimeError("simulated wip add failure")


def _stage_run(workspace, run_id, category="char", asset_code="hero", department="model"):
    leaf = Path(workspace).joinpath(".ads-staging", run_id, *category.split("/"), asset_code, department)
    leaf.mkdir(parents=True)
    (leaf / f"{asset_code}.usd").write_text("wip", encoding="utf-8")
    return leaf


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
            # The run root is pruned once no other target's writes remain.
            self.assertFalse((Path(temp) / ".ads-staging" / "run1").exists())

    def test_commit_cleanup_preserves_sibling_targets_in_the_same_run(self):
        # A run id can be shared by several targets in one session; committing
        # one target must not destroy another's staged-but-uncommitted bytes.
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            _stage_run(workspace, "shared", asset_code="hero")
            sibling = _stage_run(workspace, "shared", asset_code="villain")

            staging = WipStaging(
                workspace_root=workspace,
                category="char",
                asset_code="hero",
                department="model",
                run_id="shared",
            )
            fake = FakeAds()
            staging.commit(store=r"D:\store", ads=fake)

            self.assertFalse(Path(staging.staging_root).exists())
            self.assertTrue(sibling.exists())
            self.assertTrue((Path(workspace) / ".ads-staging" / "shared").exists())

    def test_commit_without_staged_writes_is_a_no_op(self):
        staging = self._staging()
        fake = FakeAds()
        self.assertIsNone(staging.commit(store=r"D:\store", ads=fake))
        self.assertEqual(fake.calls, [])


class CommitStagedTests(unittest.TestCase):
    def setUp(self):
        houdini_wip._process_run_id = None
        self.addCleanup(setattr, houdini_wip, "_process_run_id", None)

    def _env(self, workspace):
        return {
            "ADS_RESOLVER_WORKSPACE": workspace,
            "ADS_RESOLVER_STORE": r"D:\store",
            "ADS_WIP_CATEGORY": "char",
            "ADS_WIP_ASSET_CODE": "hero",
            "ADS_WIP_DEPARTMENT": "model",
        }

    def test_documented_houdini_flow_commits_the_processor_run(self):
        # Regression: the output processor and commit_staged() previously
        # generated two unrelated uuids, so the documented post-render hook
        # silently registered nothing.
        module = _load_wip_output_processor_module()
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            with patch.dict("os.environ", self._env(workspace)):
                os.environ.pop("ADS_WIP_RUN_ID", None)

                processor_cls = module.usdOutputProcessor()
                processor = processor_cls()
                self.assertIsNone(processor._staging)
                # Regression: the run id must not leak into the environment,
                # where child processes (farm tasks) would inherit it.
                self.assertNotIn("ADS_WIP_RUN_ID", os.environ)

                save_path = f"{workspace}/char/hero/model/hero.usd"
                redirected = processor.processSavePath(save_path, None, True)
                run_id = processor._staging.run_id
                self.assertEqual(
                    redirected,
                    f"{workspace}/.ads-staging/{run_id}/char/hero/model/hero.usd",
                )
                # husd builds a fresh processor per save; it must join the run.
                self.assertEqual(
                    processor_cls().processSavePath(save_path, None, True),
                    redirected,
                )

                staged = Path(redirected)
                staged.parent.mkdir(parents=True)
                staged.write_text("wip", encoding="utf-8")

                fake = FakeAds()
                output = commit_staged(ads=fake)

                self.assertEqual(output, "registered wip seq=1")
                self.assertEqual(len(fake.calls), 1)
                self.assertEqual(fake.calls[0]["store"], r"D:\store")
                self.assertEqual(
                    fake.calls[0]["source"],
                    f"{workspace}/.ads-staging/{run_id}/char/hero/model",
                )
                self.assertFalse((Path(workspace) / ".ads-staging" / run_id).exists())

                # The commit ends the run: the next render must stage into a
                # fresh id instead of merging into a committed one.
                self.assertIsNone(houdini_wip._process_run_id)
                self.assertNotEqual(
                    processor_cls().processSavePath(save_path, None, True),
                    redirected,
                )

    def test_output_processor_menu_construction_does_not_require_wip_env(self):
        module = _load_wip_output_processor_module()
        with patch.dict("os.environ", {}, clear=True):
            processor_cls = module.usdOutputProcessor()
            processor = processor_cls()
            self.assertIsNone(processor._staging)

    def test_orphan_runs_are_reported_but_never_committed(self):
        # Discovered runs may belong to a live concurrent session; committing
        # them would register half-written layers and delete staged bytes.
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            for run_id, age in (("newer", 100), ("older", 200)):
                _stage_run(workspace, run_id)
                run_root = Path(workspace) / ".ads-staging" / run_id
                stamp = time.time() - age
                os.utime(run_root, (stamp, stamp))

            fake = FakeAds()
            with self.assertWarns(UserWarning) as caught:
                output = commit_staged(env=self._env(workspace), ads=fake)

            self.assertIsNone(output)
            self.assertEqual(fake.calls, [])
            message = str(caught.warning)
            self.assertIn("older", message)
            self.assertIn("newer", message)
            self.assertIn("never committed automatically", message)
            self.assertTrue((Path(workspace) / ".ads-staging" / "older").exists())
            self.assertTrue((Path(workspace) / ".ads-staging" / "newer").exists())

    def test_commit_staged_warns_when_no_run_was_recorded(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            fake = FakeAds()
            with self.assertWarns(UserWarning) as caught:
                output = commit_staged(env=self._env(workspace), ads=fake)

            self.assertIsNone(output)
            self.assertEqual(fake.calls, [])
            message = str(caught.warning)
            self.assertIn("char/hero/model", message)
            self.assertIn("output processor", message)

    def test_commit_staged_warns_when_recorded_run_staged_nothing(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            houdini_wip.process_run_id()
            fake = FakeAds()
            with self.assertWarns(UserWarning) as caught:
                output = commit_staged(env=self._env(workspace), ads=fake)

            self.assertIsNone(output)
            self.assertEqual(fake.calls, [])
            self.assertIn("no staged WIP writes found", str(caught.warning))

    def test_explicit_run_id_commits_only_that_run(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            _stage_run(workspace, "run9")
            _stage_run(workspace, "stray")
            env = {**self._env(workspace), "ADS_WIP_RUN_ID": "run9"}

            fake = FakeAds()
            output = commit_staged(env=env, ads=fake)

            self.assertEqual(output, "registered wip seq=1")
            self.assertEqual(len(fake.calls), 1)
            self.assertEqual(
                fake.calls[0]["source"],
                f"{workspace}/.ads-staging/run9/char/hero/model",
            )
            self.assertFalse((Path(workspace) / ".ads-staging" / "run9").exists())
            self.assertTrue((Path(workspace) / ".ads-staging" / "stray").exists())

    def test_sibling_target_commit_keeps_run_id_for_failed_targets_retry(self):
        # Regression: ROP A (hero) fails its commit, ROP B (villain) joins the
        # same module run and commits successfully. Clearing the run id there
        # would break hero's documented retry and strand its staged bytes
        # until cache gc deletes them.
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            run_id = houdini_wip.process_run_id()
            _stage_run(workspace, run_id, asset_code="hero")
            _stage_run(workspace, run_id, asset_code="villain")

            hero_env = self._env(workspace)
            with self.assertRaises(RuntimeError):
                commit_staged(env=hero_env, ads=FailingAds())

            villain_env = {**self._env(workspace), "ADS_WIP_ASSET_CODE": "villain"}
            fake = FakeAds()
            with self.assertWarns(UserWarning) as caught:
                output = commit_staged(env=villain_env, ads=fake)
            self.assertEqual(output, "registered wip seq=1")
            # The successful commit surfaces the still-staged sibling instead
            # of silently stranding it.
            self.assertIn("remain uncommitted", str(caught.warning))
            self.assertEqual(houdini_wip._process_run_id, run_id)

            retry = commit_staged(env=hero_env, ads=fake)
            self.assertEqual(retry, "registered wip seq=2")
            self.assertEqual(fake.calls[1]["asset_code"], "hero")
            # Fully drained now: the run id resets for the next render.
            self.assertIsNone(houdini_wip._process_run_id)
            self.assertFalse((Path(workspace) / ".ads-staging" / run_id).exists())

    def test_failed_wip_add_keeps_the_run_for_retry(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = temp.replace("\\", "/")
            run_id = houdini_wip.process_run_id()
            _stage_run(workspace, run_id)

            with self.assertRaises(RuntimeError):
                commit_staged(env=self._env(workspace), ads=FailingAds())

            # The staged bytes and the recorded run id both survive, so the
            # next commit_staged() can retry the registration.
            self.assertEqual(houdini_wip._process_run_id, run_id)
            self.assertTrue((Path(workspace) / ".ads-staging" / run_id).exists())

            fake = FakeAds()
            output = commit_staged(env=self._env(workspace), ads=fake)
            self.assertEqual(output, "registered wip seq=1")


def _load_wip_output_processor_module():
    class _BaseOutputProcessor:
        def __init__(self):
            pass

    husd = types.ModuleType("husd")
    husd_outputprocessor = types.ModuleType("husd.outputprocessor")
    husd_outputprocessor.OutputProcessor = _BaseOutputProcessor
    husd.outputprocessor = husd_outputprocessor
    plugin_path = (
        Path(__file__).resolve().parents[2]
        / "houdini"
        / "husdplugins"
        / "outputprocessors"
        / "adswipstaging.py"
    )
    spec = importlib.util.spec_from_file_location("ads_houdini_wip_staging_test", plugin_path)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {"husd": husd, "husd.outputprocessor": husd_outputprocessor}):
        spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main()

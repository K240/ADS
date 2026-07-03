"""Houdini WIP staging helpers (schema v8).

Every write under the publish target's workspace folder is redirected to a
unique per-run staging folder, so a save never overwrites bytes another
process may have open — the original usdc lock problem cannot occur. After
the ROP finishes, :func:`commit_staged` registers the staged folder as a WIP
micro-version (``ads wip add``) and removes the staging copy; the bytes live
on in the content-addressed store.

The publish target is explicit, not inferred from paths (nested categories
make path inference ambiguous):

    ADS_RESOLVER_WORKSPACE  workspace root
    ADS_WIP_CATEGORY        target category, for example char or env/city
    ADS_WIP_DEPARTMENT      target department
    ADS_WIP_ASSET_CODE      target asset code
    ADS_WIP_RUN_ID          optional explicit run id pin; overrides the run
                            recorded in this interpreter (advanced — never
                            share one id across concurrent tasks)

Typical ROP wiring: select the "ADS WIP Staging" output processor and call
``ads.houdini_wip.commit_staged()`` from the post-render script.
"""

from __future__ import annotations

import os
import shutil
import uuid
import warnings
from typing import Mapping

from .client import AdsCli

STAGING_DIR = ".ads-staging"

# Run id shared by every ADS output processor in this interpreter. Module
# state rather than an environment variable: os.environ leaks into child
# processes (farm submitters snapshot it), which cross-linked unrelated
# renders into one staging run and let one commit delete another's staged
# bytes. husd constructs a fresh processor per save and one render saves the
# root layer and sublayers as separate files, so saves within a render must
# share a run to commit as a single micro-version; commit_staged() ends the
# run so the next render mints a fresh id.
_process_run_id: str | None = None


def process_run_id() -> str:
    """Returns (minting on first use) this interpreter's staging run id."""

    global _process_run_id
    if _process_run_id is None:
        _process_run_id = uuid.uuid4().hex
    return _process_run_id


def _clean_path(path: str) -> str:
    path = path.replace("\\", "/")
    if len(path) > 1:
        path = path.rstrip("/")
    return path


def _casefold_path(path: str) -> str:
    return path.casefold() if len(path) >= 2 and path[1] == ":" else path


def _relative_to_root(path: str, root: str) -> str | None:
    path = _clean_path(os.path.expandvars(os.path.expanduser(path)))
    root = _clean_path(root)
    path_cmp = _casefold_path(path)
    root_cmp = _casefold_path(root)
    if path_cmp == root_cmp:
        return ""
    prefix = root_cmp.rstrip("/") + "/"
    if not path_cmp.startswith(prefix):
        return None
    return path[len(root.rstrip("/")) + 1 :]


class WipStaging:
    """Redirects one department's workspace saves into a staging run."""

    def __init__(
        self,
        *,
        workspace_root: str | os.PathLike[str],
        category: str,
        asset_code: str,
        department: str,
        run_id: str | None = None,
    ) -> None:
        workspace = _clean_path(os.path.expandvars(os.path.expanduser(os.fspath(workspace_root))))
        if not workspace:
            raise ValueError("workspace_root is required for WIP staging")
        if not category or not asset_code or not department:
            raise ValueError("category, asset_code, and department are required for WIP staging")
        self.workspace_root = workspace
        self.category = category.strip("/")
        self.asset_code = asset_code
        self.department = department
        self.run_id = run_id or uuid.uuid4().hex

    @classmethod
    def from_environment(cls, env: Mapping[str, str] | None = None) -> "WipStaging":
        env = os.environ if env is None else env
        return cls(
            workspace_root=env.get("ADS_RESOLVER_WORKSPACE", ""),
            category=env.get("ADS_WIP_CATEGORY", ""),
            asset_code=env.get("ADS_WIP_ASSET_CODE", ""),
            department=env.get("ADS_WIP_DEPARTMENT", ""),
            run_id=env.get("ADS_WIP_RUN_ID") or None,
        )

    @property
    def department_root(self) -> str:
        return "/".join(
            [self.workspace_root, self.category, self.asset_code, self.department]
        )

    @property
    def staging_root(self) -> str:
        return "/".join(
            [
                self.workspace_root,
                STAGING_DIR,
                self.run_id,
                self.category,
                self.asset_code,
                self.department,
            ]
        )

    def redirect(self, save_path: str) -> str:
        """Maps a save path under the department root into the staging run.

        Paths outside the department root (other assets, absolute exports)
        pass through unchanged.
        """

        relative = _relative_to_root(str(save_path), self.department_root)
        if relative is None:
            return save_path
        if relative == "":
            return self.staging_root
        return f"{self.staging_root}/{relative}"

    def commit(
        self,
        *,
        store: str | os.PathLike[str],
        ads: AdsCli | None = None,
        cleanup: bool = True,
    ) -> str | None:
        """Registers the staged writes as a WIP micro-version.

        Returns the ``ads wip add`` output, or None when nothing was staged.
        Cleanup removes only this target's staged leaf plus emptied parents:
        a run id can be shared by other targets in the same session, and a
        whole-run rmtree would delete their staged bytes before they were
        committed.
        """

        if not os.path.isdir(self.staging_root):
            return None
        ads = ads or AdsCli()
        output = ads.wip_add(
            store=store,
            category=self.category,
            asset_code=self.asset_code,
            department=self.department,
            source=self.staging_root,
        )
        if cleanup:
            shutil.rmtree(self.staging_root, ignore_errors=True)
            self._prune_empty_staging_parents()
        return output

    def _prune_empty_staging_parents(self) -> None:
        base_prefix = _casefold_path("/".join([self.workspace_root, STAGING_DIR])) + "/"
        parent = os.path.dirname(self.staging_root)
        while _casefold_path(parent).startswith(base_prefix):
            try:
                os.rmdir(parent)
            except OSError:
                # Not empty (a sibling target is still staged) or already
                # gone — either way the walk upward is over.
                break
            parent = os.path.dirname(parent)


def _staged_run_ids(staging: WipStaging) -> list[str]:
    """Run ids under this workspace containing writes for this target, oldest first.

    Reporting only: discovered runs are never committed, because a found run
    may belong to a live concurrent session still writing into it.
    """

    staging_base = "/".join([staging.workspace_root, STAGING_DIR])
    try:
        candidates = os.listdir(staging_base)
    except OSError:
        return []
    found: list[tuple[float, str]] = []
    for run_id in candidates:
        run_root = f"{staging_base}/{run_id}"
        leaf = "/".join(
            [run_root, staging.category, staging.asset_code, staging.department]
        )
        if not os.path.isdir(leaf):
            continue
        try:
            found.append((os.path.getmtime(run_root), run_id))
        except OSError:
            continue
    found.sort()
    return [run_id for _, run_id in found]


def commit_staged(
    *,
    store: str | os.PathLike[str] | None = None,
    env: Mapping[str, str] | None = None,
    ads: AdsCli | None = None,
    cleanup: bool = True,
    run_id: str | None = None,
) -> str | None:
    """Commits the staging run recorded for this process.

    Intended as a Houdini ROP post-render hook:
    ``from ads.houdini_wip import commit_staged; commit_staged()``.

    The run to commit is, in order: the explicit ``run_id`` argument, the
    ADS_WIP_RUN_ID environment variable (advanced pinning), or the run id
    the output processor recorded in this interpreter. Other runs found
    under .ads-staging are never committed — they may belong to a live
    concurrent session, and registering a half-written run would corrupt
    the WIP head and destroy that session's staged bytes. Stale leftovers
    are removed by ``ads cache gc --staging-hours``.

    Finding nothing staged warns instead of raising so a post-render hook
    cannot break the render. Once the run is fully drained the interpreter's
    recorded run id is cleared so the next render starts a fresh run; while
    another target's leaf is still staged under the run (its commit failed
    or has not happened yet) the id is kept so that target's retry can find
    it. A failed ``wip add`` raises before any clear, letting the next
    commit retry.
    """

    global _process_run_id
    env = os.environ if env is None else env
    store = store or env.get("ADS_RESOLVER_STORE", "")
    if not store:
        raise ValueError("store is required: pass store= or set ADS_RESOLVER_STORE")

    env_run = env.get("ADS_WIP_RUN_ID") or None
    resolved = run_id or env_run or _process_run_id
    used_process_run = run_id is None and env_run is None and resolved is not None

    staging = WipStaging.from_environment(env)
    if resolved is not None:
        staging.run_id = resolved

    output: str | None = None
    if resolved is not None and os.path.isdir(staging.staging_root):
        output = staging.commit(store=store, ads=ads, cleanup=cleanup)
    run_root = "/".join([staging.workspace_root, STAGING_DIR, staging.run_id])
    if used_process_run and not os.path.isdir(run_root):
        # Clear only when the run is fully drained. Another target's leaf can
        # still be staged under this run (its commit failed, or its
        # post-render hook has not run yet); dropping the id then would break
        # its retry and strand the only copy of its bytes until cache gc
        # deletes them.
        _process_run_id = None
    if output is not None and os.path.isdir(run_root):
        warnings.warn(
            "commit_staged: other targets staged in run "
            f"{staging.run_id} remain uncommitted under {run_root}; their "
            "commit_staged() (with the matching ADS_WIP_* target) registers "
            "them, and 'ads cache gc' deletes them if never committed",
            stacklevel=2,
        )

    if output is None:
        target = "/".join([staging.category, staging.asset_code, staging.department])
        if resolved is None:
            message = (
                "commit_staged: no staging run was recorded in this process for "
                f"{target}; is the 'ADS WIP Staging' output processor enabled on "
                "the ROP (or set ADS_WIP_RUN_ID)?"
            )
        else:
            message = (
                "commit_staged: no staged WIP writes found for "
                f"{target}; looked at {staging.staging_root}"
            )
        orphans = [r for r in _staged_run_ids(staging) if r != staging.run_id]
        if orphans:
            message += (
                "; leftover staging runs exist ("
                + ", ".join(orphans[:5])
                + ") but are never committed automatically because they may "
                "belong to a live session; 'ads cache gc' removes stale ones"
            )
        warnings.warn(message, stacklevel=2)
    return output

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
    ADS_WIP_ASSET_CODE      target asset code
    ADS_WIP_DEPARTMENT      target department
    ADS_WIP_RUN_ID          optional explicit staging run id

Typical ROP wiring: select the "ADS WIP Staging" output processor and call
``ads.houdini_wip.commit_staged()`` from the post-render script.
"""

from __future__ import annotations

import os
import shutil
import uuid
from typing import Mapping

from .client import AdsCli

STAGING_DIR = ".ads-staging"


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
        Cleanup removes the whole staging run folder; the registered bytes
        already live in the content-addressed store.
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
            run_root = "/".join([self.workspace_root, STAGING_DIR, self.run_id])
            shutil.rmtree(run_root, ignore_errors=True)
        return output


def commit_staged(
    *,
    store: str | os.PathLike[str] | None = None,
    env: Mapping[str, str] | None = None,
    ads: AdsCli | None = None,
    cleanup: bool = True,
) -> str | None:
    """Commits the staging run configured by the environment.

    Intended as a Houdini ROP post-render hook:
    ``from ads.houdini_wip import commit_staged; commit_staged()``.
    """

    env = os.environ if env is None else env
    staging = WipStaging.from_environment(env)
    store = store or env.get("ADS_RESOLVER_STORE", "")
    if not store:
        raise ValueError("store is required: pass store= or set ADS_RESOLVER_STORE")
    return staging.commit(store=store, ads=ads, cleanup=cleanup)

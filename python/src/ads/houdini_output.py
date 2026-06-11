"""Helpers for Houdini USD ROP ADS output processing."""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path

_VERSION_RE = re.compile(r"^v[0-9]{3,}$")


@dataclass(frozen=True)
class AdsPathMapping:
    """A physical ADS version path mapped to an ads:// URI."""

    source_root: str
    relative_path: str
    category: str | None
    asset_code: str
    department: str
    version: str
    asset_relative_path: str
    uri: str


class AdsPathMapper:
    """Map Houdini USD ROP file paths to stable ADS URIs."""

    def __init__(
        self,
        *,
        workspace_root: str | os.PathLike[str] | None = None,
        public_root: str | os.PathLike[str] | None = None,
        include_version_query: bool = True,
    ) -> None:
        self.workspace_root = _normalize_root(workspace_root)
        self.public_root = _normalize_root(public_root)
        self.include_version_query = include_version_query

    @classmethod
    def from_environment(cls) -> "AdsPathMapper":
        include_version = os.environ.get("ADS_OUTPUT_VERSION_QUERY", "1").lower()
        return cls(
            workspace_root=os.environ.get("ADS_OUTPUT_WORKSPACE_ROOT")
            or os.environ.get("ADS_RESOLVER_WORKSPACE"),
            public_root=os.environ.get("ADS_OUTPUT_PUBLIC_ROOT"),
            include_version_query=include_version not in {"0", "false", "no"},
        )

    def to_ads_uri(self, asset_path: str) -> str:
        if asset_path.startswith("ads://"):
            return asset_path
        mapping = self.map_path(asset_path)
        return mapping.uri if mapping else asset_path

    def to_public_save_path(self, asset_path: str) -> str:
        if not self.workspace_root or not self.public_root:
            return asset_path
        relative_path = _relative_to_root(asset_path, self.workspace_root)
        if relative_path is None:
            return asset_path
        return _join_normalized(self.public_root, relative_path)

    def map_path(self, asset_path: str) -> AdsPathMapping | None:
        for root in self._roots():
            relative_path = _relative_to_root(asset_path, root)
            if relative_path is None:
                continue
            parsed = _parse_ads_layout(relative_path)
            if parsed is None:
                continue
            category, asset_code, department, version, asset_relative_path = parsed
            uri = _build_ads_uri(
                category=category,
                asset_code=asset_code,
                department=department,
                version=version,
                asset_relative_path=asset_relative_path,
                include_version_query=self.include_version_query,
            )
            return AdsPathMapping(
                source_root=root,
                relative_path=relative_path,
                category=category,
                asset_code=asset_code,
                department=department,
                version=version,
                asset_relative_path=asset_relative_path,
                uri=uri,
            )
        return None

    def _roots(self) -> list[str]:
        roots = []
        for root in (self.public_root, self.workspace_root):
            if root and root not in roots:
                roots.append(root)
        return roots


def _parse_ads_layout(
    relative_path: str,
) -> tuple[str | None, str, str, str, str] | None:
    parts = [part for part in relative_path.replace("\\", "/").split("/") if part]
    for index, part in enumerate(parts):
        if not _VERSION_RE.match(part):
            continue
        if index < 2 or index + 1 >= len(parts):
            return None
        category_parts = parts[: index - 2]
        category = "/".join(category_parts) if category_parts else None
        asset_code = parts[index - 2]
        department = parts[index - 1]
        version = part
        asset_relative_path = "/".join(parts[index + 1 :])
        return category, asset_code, department, version, asset_relative_path
    return None


def _build_ads_uri(
    *,
    category: str | None,
    asset_code: str,
    department: str,
    version: str,
    asset_relative_path: str,
    include_version_query: bool,
) -> str:
    parts = []
    if category:
        parts.extend(category.split("/"))
    parts.extend([asset_code, department, asset_relative_path])
    uri = "ads://" + "/".join(part.strip("/") for part in parts if part)
    if include_version_query:
        uri += f"?v={_version_query_value(version)}"
    return uri


def _version_query_value(version: str) -> str:
    """Emits the canonical integer form (schema v8) for `v###` folder names."""
    digits = version[1:] if version.startswith("v") else version
    if digits.isdigit():
        return str(int(digits))
    return version


def _normalize_root(value: str | os.PathLike[str] | None) -> str | None:
    if value is None:
        return None
    value = os.path.expandvars(os.path.expanduser(os.fspath(value)))
    if not value:
        return None
    return _clean_path(value)


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


def _clean_path(path: str) -> str:
    path = path.replace("\\", "/")
    if len(path) > 1:
        path = path.rstrip("/")
    return path


def _casefold_path(path: str) -> str:
    return path.casefold() if _looks_like_windows_path(path) else path


def _looks_like_windows_path(path: str) -> bool:
    return len(path) >= 2 and path[1] == ":"


def _join_normalized(root: str, relative_path: str) -> str:
    return str(Path(root.replace("/", os.sep)).joinpath(*relative_path.split("/")))

from __future__ import annotations

import argparse
import json
import re
import warnings
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable, Sequence
from urllib.parse import parse_qs, urlsplit

from .client import AdsCli, AdsCommandError


# Angle brackets are URI terminators except for complete <...> tokens, so
# placeholder segments like <UDIM> or <F4> stay part of the dependency.
ADS_URI_RE = re.compile(r"ads://(?:<[^<>@\s]+>|[^\s\"'<>@,)\]}])+")
# USD asset paths are @...@, or @@@...@@@ when the path itself contains @
# (single @ inside the triple form is literal).
ASSET_TOKEN_RE = re.compile(r"@@@([^\n\r]+?)@@@|@([^@\n\r]+)@")
VERSION_RE = re.compile(r"^v[0-9]+$")


@dataclass(frozen=True)
class AdsUriParts:
    asset_path: str
    category: str | None
    asset_code: str | None
    department: str | None
    version: str | None


@dataclass
class DependencyPlanItem:
    asset_path: str
    category: str | None
    asset_code: str | None
    department: str | None
    version: str
    local_path: str | None
    present: bool | None
    action: str
    error: str | None = None


@dataclass
class DependencyPlan:
    root: str
    all_dependencies: list[str]
    dependencies: list[DependencyPlanItem]

    @property
    def pull_urls(self) -> list[str]:
        return [item.asset_path for item in self.dependencies if item.action == "materialize"]

    def to_dict(self) -> dict[str, object]:
        return {
            "root": self.root,
            "all_dependencies": self.all_dependencies,
            "pull_urls": self.pull_urls,
            "dependencies": [asdict(item) for item in self.dependencies],
        }


def collect_usd_dependencies(root: str | Path) -> list[str]:
    """Collect unique USD dependency paths.

    OpenUSD Python is used when available. The text USD fallback runs only
    when pxr is unavailable or fails (with a warning); it is intentionally
    conservative and supports local .usd/.usda recursion only.
    """

    root_path = Path(root)
    paths = _collect_with_pxr(root_path)
    if paths is None:
        paths = _collect_from_text_usd(root_path, set())
    return _unique(paths)


def collect_ads_dependencies(root: str | Path) -> list[str]:
    """Collect unique ads:// dependencies from a USD file.

    If OpenUSD Python modules are available, this uses UsdUtils first. Otherwise
    it falls back to scanning text USD files and local text sublayers/references.
    """

    paths: list[str] = []
    for value in collect_usd_dependencies(root):
        if value.startswith("ads://"):
            paths.append(value)
        else:
            paths.extend(_extract_ads_uris(value))
    return _unique(paths)


def parse_ads_uri(asset_path: str) -> AdsUriParts:
    split = urlsplit(asset_path)
    if split.scheme != "ads":
        return AdsUriParts(asset_path, None, None, None, None)

    segments = [split.netloc, *[part for part in split.path.split("/") if part]]
    query = parse_qs(split.query)
    version = _first(query.get("v")) or _first(query.get("version"))

    version_index = next(
        (index for index, segment in enumerate(segments) if VERSION_RE.match(segment)),
        None,
    )
    if version_index is not None:
        version = version or segments[version_index]
        path_segments = segments[:version_index]
    else:
        path_segments = segments

    has_file_tail = bool(path_segments and _looks_like_file(path_segments[-1]))
    logical_segments = path_segments[:-1] if has_file_tail else path_segments
    if len(logical_segments) < 2:
        return AdsUriParts(asset_path, None, None, None, version)

    department = logical_segments[-1]
    asset_code = logical_segments[-2]
    category_segments = logical_segments[:-2]
    category = "/".join(category_segments) if category_segments else None
    return AdsUriParts(asset_path, category, asset_code, department, version)


def build_pull_plan(
    root: str | Path,
    *,
    store: str | Path | None = None,
    workspace: str | Path | None = None,
    ads: AdsCli | None = None,
) -> DependencyPlan:
    ads = ads or AdsCli()
    all_dependencies = collect_usd_dependencies(root)
    items: list[DependencyPlanItem] = []
    for asset_path in _unique(
        value
        for dependency in all_dependencies
        for value in (
            [dependency] if dependency.startswith("ads://") else _extract_ads_uris(dependency)
        )
    ):
        parts = parse_ads_uri(asset_path)
        version = parts.version or "current"
        local_path: str | None = None
        present: bool | None = None
        error: str | None = None

        if store is not None:
            try:
                local_path = ads.resolve(
                    asset_path,
                    store=store,
                    workspace=workspace,
                    mode="local",
                )
                present = True
            except AdsCommandError as exc:
                present = False
                error = str(exc)

        action = _plan_action(parts, present)
        items.append(
            DependencyPlanItem(
                asset_path=asset_path,
                category=parts.category,
                asset_code=parts.asset_code,
                department=parts.department,
                version=version,
                local_path=local_path,
                present=present,
                action=action,
                error=error,
            )
        )

    return DependencyPlan(str(root), all_dependencies, items)


def execute_pull_plan(
    plan: DependencyPlan,
    *,
    store: str | Path,
    workspace: str | Path | None = None,
    ads: AdsCli | None = None,
    force: bool = False,
) -> DependencyPlan:
    # Schema v8: resolving in local mode materializes the immutable manifest
    # view, so warming the cache is a plain resolve per dependency. The view
    # never conflicts with existing content, which makes `force` obsolete.
    del force
    ads = ads or AdsCli()
    for item in plan.dependencies:
        if item.action == "none":
            continue
        if not item.asset_code or not item.department:
            item.action = "error"
            item.error = "cannot infer asset_code and department from ADS URI"
            continue
        try:
            item.local_path = ads.resolve(
                item.asset_path,
                store=store,
                workspace=workspace,
                mode="local",
            )
            item.action = "none"
            item.present = True
            item.error = None
        except AdsCommandError as exc:
            item.action = "error"
            item.error = str(exc)
    return plan


def format_text(plan: DependencyPlan, *, include_all: bool = False) -> str:
    lines = [
        f"root: {plan.root}",
        f"usd dependencies: {len(plan.all_dependencies)}",
        f"ads dependencies: {len(plan.dependencies)}",
    ]
    if include_all and plan.all_dependencies:
        lines.append("")
        lines.append("all usd dependencies:")
        lines.extend(f"  {path}" for path in plan.all_dependencies)
        lines.append("")
    for item in plan.dependencies:
        location = item.local_path or "-"
        present = "unknown" if item.present is None else str(item.present).lower()
        lines.append(
            f"{item.action:7} {item.asset_path} "
            f"version={item.version} present={present} local={location}"
        )
    if plan.pull_urls:
        lines.append("")
        lines.append("pull required:")
        lines.extend(f"  {url}" for url in plan.pull_urls)
    return "\n".join(lines)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="List ADS URI dependencies and optional pull plan")
    parser.add_argument("root", help="Root USD file to inspect")
    parser.add_argument("--store", help="ADS store path. Enables local presence checks")
    parser.add_argument("--workspace", help="ADS workspace path")
    parser.add_argument("--ads-executable", default="ads", help="ads executable path")
    parser.add_argument("--execute", action="store_true", help="materialize missing dependencies into the manifest view cache")
    parser.add_argument("--json", action="store_true", help="print JSON")
    parser.add_argument("--all", action="store_true", help="include all USD dependencies in text output")
    args = parser.parse_args(argv)

    ads = AdsCli(args.ads_executable)
    plan = build_pull_plan(args.root, store=args.store, workspace=args.workspace, ads=ads)
    if args.execute:
        if not args.store:
            parser.error("--execute requires --store")
        execute_pull_plan(
            plan,
            store=args.store,
            workspace=args.workspace,
            ads=ads,
        )

    if args.json:
        print(json.dumps(plan.to_dict(), indent=2, ensure_ascii=False))
    else:
        print(format_text(plan, include_all=args.all))

    return 1 if any(item.action == "error" for item in plan.dependencies) else 0


def _collect_with_pxr(root: Path) -> list[str] | None:
    """Collect dependencies via OpenUSD, or ``None`` to use the text fallback."""

    try:
        from pxr import UsdUtils  # type: ignore
    except Exception as error:
        warnings.warn(
            f"OpenUSD (pxr) is unavailable, falling back to text USD scanning: {error}",
            RuntimeWarning,
        )
        return None

    try:
        layers, assets, unresolved = UsdUtils.ComputeAllDependencies(str(root))
    except Exception as error:
        warnings.warn(
            f"UsdUtils.ComputeAllDependencies failed for {root}, "
            f"falling back to text USD scanning: {error}",
            RuntimeWarning,
        )
        return None

    paths: list[str] = []
    for layer in layers:
        for attr in ("realPath", "resolvedPath", "identifier"):
            value = getattr(layer, attr, None)
            if value:
                paths.append(str(value))
                break
        # ads:// references usually land in unresolvedPaths, but composition
        # metadata is queried per layer as well so pinned/odd forms are not
        # missed. Deliberately no ExportToString here: serializing every
        # layer costs seconds and GBs on production scenes.
        try:
            for value in layer.GetCompositionAssetDependencies():
                paths.extend(_extract_ads_uris(str(value)))
        except Exception:
            pass
        try:
            for value in layer.externalReferences:
                paths.extend(_extract_ads_uris(str(value)))
        except Exception:
            pass
    for value in [*assets, *unresolved]:
        paths.extend(_extract_ads_uris(str(value)))
    return paths


def _collect_from_text_usd(path: Path, seen: set[Path]) -> list[str]:
    try:
        resolved = path.resolve()
    except OSError:
        return []
    if resolved in seen:
        return []
    seen.add(resolved)

    try:
        text = path.read_text(encoding="utf-8-sig")
    except UnicodeDecodeError:
        return []
    except OSError:
        return []

    paths = _extract_ads_uris(text)
    for match in ASSET_TOKEN_RE.finditer(text):
        token = (match.group(1) or match.group(2) or "").strip()
        if not token:
            continue
        if token.startswith("ads://"):
            paths.append(token)
            continue
        paths.append(token)
        child = (path.parent / token).resolve()
        if child.exists() and child.suffix.lower() in {".usd", ".usda"}:
            paths.extend(_collect_from_text_usd(child, seen))
    return paths


def _extract_ads_uris(text: str) -> list[str]:
    return [_strip_uri_tail(match.group(0)) for match in ADS_URI_RE.finditer(text)]


def _strip_uri_tail(value: str) -> str:
    return value.rstrip(".;:")


def _unique(values: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    result: list[str] = []
    for value in values:
        value = _normalize_dependency(value)
        if value and value not in seen:
            seen.add(value)
            result.append(value)
    return result


def _normalize_dependency(value: str) -> str:
    value = value.strip()
    if value.startswith("ads://"):
        return value
    return value.replace("\\", "/")


def _looks_like_file(segment: str) -> bool:
    return "." in segment and not VERSION_RE.match(segment)


def _first(values: list[str] | None) -> str | None:
    if not values:
        return None
    value = values[0].strip()
    return value or None


def _plan_action(parts: AdsUriParts, present: bool | None) -> str:
    if present is True:
        return "none"
    if not parts.asset_code or not parts.department:
        return "inspect"
    return "materialize"


if __name__ == "__main__":
    raise SystemExit(main())

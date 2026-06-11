from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Mapping
from urllib.parse import unquote_plus


JsonObject = dict[str, Any]


@dataclass(frozen=True)
class CatalogConfig:
    server: str
    profile: str
    token: str
    q: str | None = None
    category: str | None = None
    department: str | None = None
    resolve_mode: str = "auto"
    timeout: float = 30.0


@dataclass
class CatalogNode:
    item_id: str
    label: str
    type_name: str
    parent_id: str = ""
    children: list[str] = field(default_factory=list)
    asset: JsonObject | None = None


def parse_datasource_args(
    args: str | None,
    env: Mapping[str, str] | None = None,
) -> CatalogConfig:
    env = env if env is not None else os.environ
    values = _parse_key_values(args)
    token_env = values.get("token_env")

    server = (
        values.get("server")
        or env.get("ADS_CATALOG_SERVER")
        or env.get("ADS_RESOLVER_SERVER")
        or ""
    ).strip()
    profile = (
        values.get("profile")
        or env.get("ADS_CATALOG_PROFILE")
        or env.get("ADS_RESOLVER_PROFILE")
        or "main"
    ).strip()
    token = _env_value(env, token_env) if token_env else _first_env(
        env,
        "ADS_CATALOG_API_TOKEN",
        "ADS_RESOLVER_API_TOKEN",
        "ADS_WEB_TOKEN",
    )
    resolve_mode = (
        values.get("resolve_mode")
        or env.get("ADS_CATALOG_RESOLVE_MODE")
        or env.get("ADS_RESOLVER_MODE")
        or "auto"
    ).strip()

    timeout = 30.0
    if values.get("timeout"):
        timeout = float(values["timeout"])
        if timeout <= 0:
            raise ValueError("timeout must be greater than zero")

    return CatalogConfig(
        server=server,
        profile=profile,
        token=token,
        q=_optional(values.get("q")),
        category=_optional(values.get("category")),
        department=_optional(values.get("department")),
        resolve_mode=resolve_mode,
        timeout=timeout,
    )


class CatalogIndex:
    def __init__(self, assets: list[JsonObject]) -> None:
        self._nodes: dict[str, CatalogNode] = {}
        self._root_children: list[str] = []
        self._leaf_ids: list[str] = []
        for asset in sorted(assets, key=_asset_sort_key):
            self._add_asset(asset)
        self._sort_children()

    @classmethod
    def from_response(cls, response: Any) -> CatalogIndex:
        if isinstance(response, Mapping):
            assets = response.get("assets", [])
        else:
            assets = response
        if not isinstance(assets, list):
            assets = []
        return cls([dict(asset) for asset in assets if isinstance(asset, Mapping)])

    def item_ids(self) -> list[str]:
        return list(self._leaf_ids)

    def all_item_ids(self) -> list[str]:
        return list(self._nodes)

    def child_item_ids(self, item_id: str | None = "") -> list[str]:
        if item_id in (None, "", "/"):
            return list(self._root_children)
        node = self._nodes.get(str(item_id))
        return list(node.children) if node else []

    def parent_id(self, item_id: str) -> str:
        node = self._nodes.get(item_id)
        return node.parent_id if node else ""

    def type_name(self, item_id: str) -> str:
        node = self._nodes.get(item_id)
        return node.type_name if node else ""

    def label(self, item_id: str) -> str:
        node = self._nodes.get(item_id)
        return node.label if node else ""

    def asset(self, item_id: str) -> JsonObject | None:
        node = self._nodes.get(item_id)
        return node.asset if node else None

    def is_leaf(self, item_id: str) -> bool:
        return self.asset(item_id) is not None

    def file_path(self, item_id: str) -> str:
        asset = self.asset(item_id)
        return asset_uri(asset) if asset else ""

    def blind_data(self, item_id: str) -> bytes | None:
        asset = self.asset(item_id)
        if not asset:
            return None
        import json

        data = {
            "name": asset.get("asset_code", ""),
            "refprimpath": "",
            "variants": {},
            "ads_uri": asset_uri(asset),
            "category": asset.get("category", ""),
            "asset_code": asset.get("asset_code", ""),
            "department": asset.get("department", ""),
            "current": _version_string(asset.get("current")),
            "latest": _version_string(asset.get("latest")),
        }
        return json.dumps(data, separators=(",", ":")).encode("ascii")

    def metadata(self, item_id: str) -> JsonObject:
        asset = self.asset(item_id)
        if not asset:
            return {}
        return {
            "category": asset.get("category", ""),
            "asset_code": asset.get("asset_code", ""),
            "department": asset.get("department", ""),
            "current": _version_string(asset.get("current")),
            "latest": _version_string(asset.get("latest")),
            "explicit_current": bool(asset.get("explicit_current")),
            "version_count": int(asset.get("version_count") or 0),
            "ads_uri": asset_uri(asset),
        }

    def tags(self, item_id: str) -> list[str]:
        asset = self.asset(item_id)
        if not asset:
            return []
        category = str(asset.get("category", ""))
        tags = [part for part in category.split("/") if part]
        tags.extend(
            value
            for value in (
                str(asset.get("asset_code", "")),
                str(asset.get("department", "")),
            )
            if value
        )
        return tags

    def status(self, item_id: str) -> str:
        asset = self.asset(item_id)
        if not asset:
            return ""
        current = _version_string(asset.get("current"))
        latest = _version_string(asset.get("latest"))
        if current and latest and current != latest:
            return f"current {current} / latest {latest}"
        if current:
            return f"current {current}"
        if latest:
            return f"latest {latest}"
        return "no version"

    def modification_time(self, item_id: str) -> int:
        asset = self.asset(item_id)
        if not asset:
            return 0
        return _timestamp(asset.get("latest_created_at"))

    def _add_asset(self, asset: JsonObject) -> None:
        category = str(asset.get("category", "")).strip("/")
        asset_code = str(asset.get("asset_code", ""))
        department = str(asset.get("department", ""))
        if not asset_code or not department:
            return

        parent = ""
        category_parts = [part for part in category.split("/") if part]
        for index, part in enumerate(category_parts):
            path = "/".join(category_parts[: index + 1])
            node_id = f"ads:folder:category:{path}"
            parent = self._ensure_node(node_id, part, "folder", parent)

        leaf_id = f"ads:asset:{category}/{asset_code}/{department}"
        if leaf_id not in self._nodes:
            self._nodes[leaf_id] = CatalogNode(
                item_id=leaf_id,
                label=f"{asset_code} / {department}",
                type_name="asset",
                parent_id=parent,
                asset=asset,
            )
            self._link_child(parent, leaf_id)
            self._leaf_ids.append(leaf_id)

    def _ensure_node(
        self,
        item_id: str,
        label: str,
        type_name: str,
        parent_id: str,
    ) -> str:
        if item_id not in self._nodes:
            self._nodes[item_id] = CatalogNode(
                item_id=item_id,
                label=label,
                type_name=type_name,
                parent_id=parent_id,
            )
            self._link_child(parent_id, item_id)
        return item_id

    def _link_child(self, parent_id: str, child_id: str) -> None:
        if parent_id:
            children = self._nodes[parent_id].children
        else:
            children = self._root_children
        if child_id not in children:
            children.append(child_id)

    def _sort_children(self) -> None:
        def sort_key(item_id: str) -> tuple[str, str]:
            node = self._nodes[item_id]
            return (node.label.lower(), item_id)

        self._root_children.sort(key=sort_key)
        for node in self._nodes.values():
            node.children.sort(key=sort_key)
        self._leaf_ids.sort(key=sort_key)


def asset_uri(asset: Mapping[str, Any]) -> str:
    category = str(asset.get("category", "")).strip("/")
    asset_code = str(asset.get("asset_code", ""))
    department = str(asset.get("department", ""))
    parts = ["ads://"]
    if category:
        parts.append(category)
        parts.append("/")
    parts.append(asset_code)
    parts.append("/")
    parts.append(department)
    parts.append("/")
    parts.append(asset_code)
    parts.append(".usd")
    return "".join(parts)


def _parse_key_values(args: str | None) -> dict[str, str]:
    values: dict[str, str] = {}
    if not args:
        return values
    for chunk in re.split(r"[;&]", args):
        chunk = chunk.strip()
        if not chunk:
            continue
        key, separator, value = chunk.partition("=")
        if not separator:
            values[unquote_plus(key).strip()] = ""
        else:
            values[unquote_plus(key).strip()] = unquote_plus(value).strip()
    return values


def _optional(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip()
    return value or None


def _first_env(env: Mapping[str, str], *names: str) -> str:
    for name in names:
        value = env.get(name)
        if value:
            return value
    return ""


def _env_value(env: Mapping[str, str], name: str | None) -> str:
    if not name:
        return ""
    return env.get(name, "")


def _version_string(value: Any) -> str | None:
    if value is None:
        return None
    # Versions arrive as plain integers from the v8 API; v### is display sugar.
    if isinstance(value, int):
        return f"v{value:03d}"
    return str(value)


def _asset_sort_key(asset: JsonObject) -> tuple[str, str, str]:
    return (
        str(asset.get("category", "")),
        str(asset.get("asset_code", "")),
        str(asset.get("department", "")),
    )


def _timestamp(value: Any) -> int:
    if not value:
        return 0
    try:
        text = str(value).replace("Z", "+00:00")
        return int(datetime.fromisoformat(text).timestamp())
    except ValueError:
        return 0

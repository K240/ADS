"""ADS URI helpers shared by the browser (no Qt dependency)."""

from __future__ import annotations

from urllib.parse import quote

from ads import asset_uri


def build_asset_uri(
    category: str,
    asset_code: str,
    department: str,
    version: int | str | None = None,
) -> str:
    uri = asset_uri(
        {
            "category": category,
            "asset_code": asset_code,
            "department": department,
        }
    )
    if version is None or version == "":
        return uri
    return f"{uri}?v={quote(str(version), safe='')}"

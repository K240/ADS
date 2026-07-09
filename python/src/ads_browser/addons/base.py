"""Context-menu addon contracts (DCC hooks register subclasses)."""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True, slots=True)
class AdsAssetContext:
    """Snapshot passed to addons on right-click / invoke."""

    profile: str
    category: str
    asset_code: str
    department: str
    version: str | None
    ads_uri: str
    current: int | str | None = None
    latest: int | str | None = None
    thumbnail_sha256: str = ""
    manifest_entries: tuple[dict[str, Any], ...] = ()
    wip_seqs: tuple[int, ...] = ()

    @property
    def asset_key(self) -> str:
        return f"{self.category}/{self.asset_code}/{self.department}"


@dataclass(frozen=True, slots=True)
class MenuItem:
    """One row in the asset context menu."""

    action_id: str
    label: str
    enabled: bool = True


class ContextMenuAddon(ABC):
    """Implement in DCC integration packages; register with ``register_addon``."""

    @property
    @abstractmethod
    def addon_id(self) -> str:
        """Stable id, e.g. ``houdini`` or ``example``."""

    @abstractmethod
    def menu_items(self, ctx: AdsAssetContext) -> list[MenuItem]:
        """Return items for this asset (can be empty)."""

    @abstractmethod
    def invoke(self, ctx: AdsAssetContext, action_id: str) -> None:
        """Run the action chosen by the user."""

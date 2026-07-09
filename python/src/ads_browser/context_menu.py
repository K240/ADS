"""Context menu model + bridge: build menu rows from registered addons."""

from __future__ import annotations

from collections.abc import Sequence
from typing import Any

from PySide6.QtCore import QAbstractListModel, QModelIndex, QObject, Qt, Signal, Slot

from ads_browser.addons.base import AdsAssetContext, ContextMenuAddon
from ads_browser.addons.registry import iter_addons
from ads_browser.uri import build_asset_uri


class ContextMenuModel(QAbstractListModel):
    """Rows expose role ``label`` for QML Instantiator / Repeater delegates."""

    _LabelRole = Qt.ItemDataRole.UserRole + 1

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._labels: list[str] = []

    def rowCount(self, parent: QModelIndex = QModelIndex()) -> int:  # noqa: B008
        if parent.isValid():
            return 0
        return len(self._labels)

    def data(self, index: QModelIndex, role: int = Qt.ItemDataRole.DisplayRole):
        if not index.isValid() or index.row() >= len(self._labels):
            return None
        if role in (Qt.ItemDataRole.DisplayRole, self._LabelRole):
            return self._labels[index.row()]
        return None

    def roleNames(self):
        return {self._LabelRole: b"label"}

    def set_labels(self, labels: list[str]) -> None:
        self.beginResetModel()
        self._labels = list(labels)
        self.endResetModel()


class ContextMenuBridge(QObject):
    """prepareMenu fills ContextMenuModel; invoke runs addon action by row index."""

    statusChanged = Signal(str, str)  # text, kind
    menuReady = Signal()

    def __init__(
        self,
        menu_model: ContextMenuModel,
        *,
        addons: Sequence[ContextMenuAddon] | None = None,
        parent: QObject | None = None,
    ) -> None:
        super().__init__(parent)
        self._menu_model = menu_model
        self._addons: tuple[ContextMenuAddon, ...] | None = (
            tuple(addons) if addons is not None else None
        )
        self._rows: list[tuple[str, str]] = []  # (addon_id, action_id)
        self._last_ctx: AdsAssetContext | None = None
        # Optional enrichment from the inspector (avoid HTTP on every right-click).
        self._enrichment: dict[str, Any] = {}

    def _iter_addons(self) -> tuple[ContextMenuAddon, ...]:
        return self._addons if self._addons is not None else iter_addons()

    def set_enrichment(
        self,
        *,
        current: int | str | None = None,
        latest: int | str | None = None,
        manifest_entries: Sequence[dict[str, Any]] | None = None,
        wip_seqs: Sequence[int] | None = None,
        thumbnail_sha256: str | None = None,
    ) -> None:
        """Cache inspector state so prepareMenu stays synchronous and cheap."""
        if current is not None:
            self._enrichment["current"] = current
        if latest is not None:
            self._enrichment["latest"] = latest
        if manifest_entries is not None:
            self._enrichment["manifest_entries"] = tuple(manifest_entries)
        if wip_seqs is not None:
            self._enrichment["wip_seqs"] = tuple(int(s) for s in wip_seqs)
        if thumbnail_sha256 is not None:
            self._enrichment["thumbnail_sha256"] = thumbnail_sha256

    def clear_enrichment(self) -> None:
        self._enrichment.clear()

    def _build_context(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
        *,
        thumbnail_sha256: str = "",
        current: str = "",
        latest: str = "",
    ) -> AdsAssetContext:
        ver = version.strip() or None
        enrich = self._enrichment
        thumb = thumbnail_sha256 or str(enrich.get("thumbnail_sha256", ""))
        cur: int | str | None = enrich.get("current")
        lat: int | str | None = enrich.get("latest")
        if current != "":
            cur = current
        if latest != "":
            lat = latest
        entries = enrich.get("manifest_entries", ())
        wips = enrich.get("wip_seqs", ())
        return AdsAssetContext(
            profile=profile,
            category=category,
            asset_code=asset_code,
            department=department,
            version=ver,
            ads_uri=build_asset_uri(category, asset_code, department, ver),
            current=cur,
            latest=lat,
            thumbnail_sha256=thumb,
            manifest_entries=tuple(entries) if entries else (),
            wip_seqs=tuple(wips) if wips else (),
        )

    def _fill_menu(self, ctx: AdsAssetContext) -> None:
        labels: list[str] = []
        rows: list[tuple[str, str]] = []
        for addon in self._iter_addons():
            for item in addon.menu_items(ctx):
                if item.enabled:
                    labels.append(item.label)
                    rows.append((addon.addon_id, item.action_id))
        self._rows = rows
        self._last_ctx = ctx
        self._menu_model.set_labels(labels)
        self.menuReady.emit()

    @Slot(str, str, str, str, str)
    def prepareMenu(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> None:
        ctx = self._build_context(profile, category, asset_code, department, version)
        self._fill_menu(ctx)

    @Slot(str, str, str, str, str, str, str, str)
    def prepareMenuFull(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
        thumbnail_sha256: str,
        current: str,
        latest: str,
    ) -> None:
        ctx = self._build_context(
            profile,
            category,
            asset_code,
            department,
            version,
            thumbnail_sha256=thumbnail_sha256,
            current=current,
            latest=latest,
        )
        self._fill_menu(ctx)

    @Slot(str)
    def setManifestJson(self, raw: str) -> None:
        """Optional: push inspector manifest JSON before prepareMenu."""
        import json

        try:
            data = json.loads(raw) if raw else []
        except json.JSONDecodeError:
            return
        if isinstance(data, list):
            self.set_enrichment(manifest_entries=data)

    @Slot(str)
    def setWipSeqsJson(self, raw: str) -> None:
        import json

        try:
            data = json.loads(raw) if raw else []
        except json.JSONDecodeError:
            return
        if isinstance(data, list):
            self.set_enrichment(wip_seqs=[int(x) for x in data])

    @Slot(int)
    def invoke(self, row: int) -> None:
        if row < 0 or row >= len(self._rows):
            return
        ctx = self._last_ctx
        if ctx is None:
            return
        addon_id, action_id = self._rows[row]
        for addon in self._iter_addons():
            if addon.addon_id == addon_id:
                try:
                    addon.invoke(ctx, action_id)
                    self.statusChanged.emit(f"{addon_id}:{action_id}", "ok")
                except Exception as exc:  # noqa: BLE001
                    self.statusChanged.emit(str(exc), "err")
                break

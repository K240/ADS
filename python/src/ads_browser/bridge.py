"""Qt bridge between QML and AdsHttpClient (network work off the UI thread)."""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any

from PySide6.QtCore import Property, QObject, QRunnable, QThreadPool, QUrl, Signal, Slot

from ads import AdsHttpClient, AdsHttpError

from .uri import build_asset_uri

__all__ = ["AdsBridge", "build_asset_uri", "default_server", "default_token"]


def _version_query(version: int | str | None) -> str | None:
    if version is None or version == "":
        return None
    return str(version)


class _Worker(QRunnable):
    def __init__(self, fn: Any, on_ok: Any, on_err: Any) -> None:
        super().__init__()
        self._fn = fn
        self._on_ok = on_ok
        self._on_err = on_err
        self.setAutoDelete(True)

    def run(self) -> None:
        try:
            self._on_ok(self._fn())
        except Exception as exc:  # noqa: BLE001 — surface any failure to QML
            self._on_err(exc)


class AdsBridge(QObject):
    """Expose ADS HTTP operations to QML with JSON payloads."""

    statusChanged = Signal(str, str)  # text, kind (ok|err|info)
    busyChanged = Signal()
    profilesLoaded = Signal(str)  # JSON
    assetsLoaded = Signal(str)  # JSON
    detailLoaded = Signal(str)  # JSON
    manifestLoaded = Signal(str)  # JSON
    thumbnailReady = Signal(str, str)  # cacheKey, fileUrl
    actionDone = Signal(str, str)  # action, JSON
    # Marshal worker-thread completions onto the GUI thread.
    _finished = Signal(object, object)  # on_ok, result
    _failed = Signal(object)  # exception

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._pool = QThreadPool.globalInstance()
        self._client: AdsHttpClient | None = None
        self._busy = 0
        self._thumb_dir = Path(tempfile.mkdtemp(prefix="ads-browser-thumbs-"))
        self._thumb_cache: dict[str, str] = {}
        self._finished.connect(self._on_finished)
        self._failed.connect(self._on_failed)

    @Property(bool, notify=busyChanged)
    def busy(self) -> bool:
        return self._busy > 0

    def _bump_busy(self, delta: int) -> None:
        was = self._busy > 0
        self._busy = max(0, self._busy + delta)
        now = self._busy > 0
        if was != now:
            self.busyChanged.emit()

    @Slot(object, object)
    def _on_finished(self, on_ok: Any, result: Any) -> None:
        self._bump_busy(-1)
        on_ok(result)

    @Slot(object)
    def _on_failed(self, exc: object) -> None:
        self._bump_busy(-1)
        if isinstance(exc, AdsHttpError) and exc.status:
            msg = f"HTTP {exc.status}: {exc}"
        else:
            msg = str(exc)
        self.statusChanged.emit(msg, "err")

    def _run(self, fn: Any, on_ok: Any, *, status: str | None = None) -> None:
        if self._client is None:
            self.statusChanged.emit("Not connected — set server and token", "err")
            return
        if status:
            self.statusChanged.emit(status, "info")
        self._bump_busy(1)

        bridge = self

        def ok(result: Any) -> None:
            bridge._finished.emit(on_ok, result)

        def err(exc: Exception) -> None:
            bridge._failed.emit(exc)

        self._pool.start(_Worker(fn, ok, err))

    @Slot(str, str)
    def connectToServer(self, base_url: str, token: str) -> None:
        url = base_url.strip().rstrip("/")
        tok = token.strip()
        if not url or not tok:
            self.statusChanged.emit("Server URL and token are required", "err")
            return
        self._client = AdsHttpClient(url, token=tok, timeout=60.0)
        self.statusChanged.emit(f"Connected to {url}", "ok")
        self.refreshProfiles()

    @Slot()
    def refreshProfiles(self) -> None:
        client = self._client
        assert client is not None

        def work() -> str:
            return json.dumps(client.profiles())

        self._run(
            work,
            lambda raw: self.profilesLoaded.emit(raw),
            status="Loading profiles…",
        )

    @Slot(str, str)
    def refreshAssets(self, profile: str, query: str) -> None:
        client = self._client
        assert client is not None
        q = query.strip() or None

        def work() -> str:
            return json.dumps(client.assets(profile=profile, q=q))

        self._run(
            work,
            lambda raw: self.assetsLoaded.emit(raw),
            status="Scanning assets…",
        )

    @Slot(str, str, str, str)
    def loadDetail(self, profile: str, category: str, asset_code: str, department: str) -> None:
        client = self._client
        assert client is not None

        def work() -> str:
            versions = client.versions(
                profile=profile,
                category=category,
                asset_code=asset_code,
                department=department,
            )
            wips = client.wips(
                profile=profile,
                category=category,
                asset_code=asset_code,
                department=department,
            )
            payload = {
                "category": category,
                "asset_code": asset_code,
                "department": department,
                "versions": versions.get("versions", []),
                "current_status": versions.get("current_status"),
                "thumbnails": versions.get("thumbnails", []),
                "wips": wips.get("wips", []),
            }
            return json.dumps(payload)

        self._run(
            work,
            lambda raw: self.detailLoaded.emit(raw),
            status="Loading detail…",
        )

    @Slot(str, str, str, str, str)
    def loadManifest(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> None:
        client = self._client
        assert client is not None
        ver = _version_query(version)

        def work() -> str:
            info = client.version_info(
                profile=profile,
                category=category,
                asset_code=asset_code,
                department=department,
                version=ver,
            )
            return json.dumps(info)

        self._run(
            work,
            lambda raw: self.manifestLoaded.emit(raw),
            status="Loading manifest…",
        )

    @Slot(str, str, str)
    def fetchThumbnail(self, profile: str, sha256: str, mime_type: str) -> None:
        if not sha256:
            return
        cache_key = f"{profile}:{sha256}"
        cached = self._thumb_cache.get(cache_key)
        if cached and Path(cached).is_file():
            self.thumbnailReady.emit(cache_key, QUrl.fromLocalFile(cached).toString())
            return

        client = self._client
        assert client is not None
        ext = {
            "image/png": ".png",
            "image/jpeg": ".jpg",
            "image/webp": ".webp",
        }.get(mime_type or "", ".bin")

        def work() -> tuple[str, str]:
            data = client.object_bytes(sha256, profile=profile)
            path = self._thumb_dir / f"{sha256[:16]}{ext}"
            if not path.is_file():
                path.write_bytes(data)
            return cache_key, str(path)

        def ok(result: tuple[str, str]) -> None:
            key, path = result
            self._thumb_cache[key] = path
            self.thumbnailReady.emit(key, QUrl.fromLocalFile(path).toString())

        self._run(work, ok)

    @Slot(str, result=str)
    def assetUriFor(self, asset_json: str) -> str:
        try:
            asset = json.loads(asset_json)
        except json.JSONDecodeError:
            return ""
        return build_asset_uri(
            str(asset.get("category", "")),
            str(asset.get("asset_code", "")),
            str(asset.get("department", "")),
            asset.get("version"),
        )

    @Slot(str, str, str, str, result=str)
    def buildUri(
        self,
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> str:
        return build_asset_uri(category, asset_code, department, version or None)

    @Slot(str)
    def copyText(self, text: str) -> None:
        from PySide6.QtGui import QGuiApplication

        clipboard = QGuiApplication.clipboard()
        if clipboard is None:
            self.statusChanged.emit("Clipboard unavailable", "err")
            return
        clipboard.setText(text)
        self.statusChanged.emit("Copied to clipboard", "ok")

    @Slot(str, str, str, str, str)
    def setCurrent(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
    ) -> None:
        client = self._client
        assert client is not None

        def work() -> str:
            result = client.current_set(
                profile=profile,
                category=category,
                asset_code=asset_code,
                department=department,
                version=version,
            )
            return json.dumps(result)

        def ok(raw: str) -> None:
            self.actionDone.emit("setCurrent", raw)
            self.statusChanged.emit(f"Pinned current → {version}", "ok")

        self._run(work, ok, status="Pinning current…")

    @Slot(str, str, str, str)
    def resetCurrent(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
    ) -> None:
        client = self._client
        assert client is not None

        def work() -> str:
            return json.dumps(
                client.current_reset(
                    profile=profile,
                    category=category,
                    asset_code=asset_code,
                    department=department,
                )
            )

        def ok(raw: str) -> None:
            self.actionDone.emit("resetCurrent", raw)
            self.statusChanged.emit("Current reset to latest", "ok")

        self._run(work, ok, status="Resetting current…")

    @Slot(str, str, str, str, str, bool)
    def pull(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
        force: bool,
    ) -> None:
        client = self._client
        assert client is not None

        def work() -> str:
            return json.dumps(
                client.pull(
                    profile=profile,
                    category=category,
                    asset_code=asset_code,
                    department=department,
                    version=_version_query(version),
                    force=force,
                )
            )

        def ok(raw: str) -> None:
            self.actionDone.emit("pull", raw)
            self.statusChanged.emit("Pull complete", "ok")

        self._run(work, ok, status="Pulling to workspace…")

    @Slot(str, str, str, str, int)
    def promote(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        wip_seq: int,
    ) -> None:
        client = self._client
        assert client is not None

        def work() -> str:
            return json.dumps(
                client.promote(
                    profile=profile,
                    category=category,
                    asset_code=asset_code,
                    department=department,
                    wip_seq=wip_seq,
                )
            )

        def ok(raw: str) -> None:
            self.actionDone.emit("promote", raw)
            self.statusChanged.emit(f"Promoted WIP #{wip_seq}", "ok")

        self._run(work, ok, status=f"Promoting WIP #{wip_seq}…")

    @Slot(str, str, str, str, str, str)
    def uploadThumbnail(
        self,
        profile: str,
        category: str,
        asset_code: str,
        department: str,
        version: str,
        file_url: str,
    ) -> None:
        client = self._client
        assert client is not None
        path = Path(QUrl(file_url).toLocalFile())
        if not path.is_file():
            self.statusChanged.emit(f"File not found: {path}", "err")
            return

        def work() -> str:
            return json.dumps(
                client.upload_thumbnail(
                    path,
                    profile=profile,
                    category=category,
                    asset_code=asset_code,
                    department=department,
                    version=version,
                )
            )

        def ok(raw: str) -> None:
            self.actionDone.emit("uploadThumbnail", raw)
            self.statusChanged.emit("Thumbnail uploaded", "ok")

        self._run(work, ok, status="Uploading thumbnail…")


def default_server() -> str:
    return os.environ.get("ADS_WEB_URL", os.environ.get("ADS_BROWSER_URL", "")).rstrip("/")


def default_token() -> str:
    return os.environ.get("ADS_WEB_TOKEN", os.environ.get("ADS_BROWSER_TOKEN", ""))

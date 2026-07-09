"""Entry point for the ADS PySide6 / QML asset browser."""

from __future__ import annotations

import argparse
import os
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

from PySide6.QtCore import QUrl
from PySide6.QtGui import QGuiApplication
from PySide6.QtQml import QQmlApplicationEngine
from PySide6.QtQuickControls2 import QQuickStyle

from ads_browser.addons.base import ContextMenuAddon
from ads_browser.addons.example import ExampleContextMenuAddon
from ads_browser.addons.registry import iter_addons, register_addon
from ads_browser.bridge import AdsBridge, default_server, default_token
from ads_browser.context_menu import ContextMenuBridge, ContextMenuModel


def _qml_dir() -> Path:
    return Path(__file__).resolve().parent / "qml"


def _ensure_basic_style() -> None:
    # Windows defaults to the native style, which rejects contentItem/background
    # overrides. Force Basic before creating the application.
    os.environ.setdefault("QT_QUICK_CONTROLS_STYLE", "Basic")
    QQuickStyle.setStyle("Basic")


def _resolve_addons(
    addons: Sequence[ContextMenuAddon] | None,
) -> tuple[ContextMenuAddon, ...]:
    """Host injection wins; otherwise use the global registry (seed example)."""
    if addons is not None:
        return tuple(addons)
    if not any(getattr(a, "addon_id", None) == "example" for a in iter_addons()):
        register_addon(ExampleContextMenuAddon())
    return iter_addons()


@dataclass
class AdsBrowserSession:
    """Handles kept alive for the QML application lifetime."""

    app: QGuiApplication
    engine: QQmlApplicationEngine
    ads: AdsBridge
    context_menu: ContextMenuBridge
    context_menu_model: ContextMenuModel


def create_ads_browser(
    *,
    server: str = "",
    token: str = "",
    auto_connect: bool = True,
    addons: Sequence[ContextMenuAddon] | None = None,
    argv: list[str] | None = None,
    existing_app: QGuiApplication | None = None,
) -> AdsBrowserSession:
    """Build the QML browser and register context-menu addons.

    DCC hosts should pass ``addons=[HoudiniContextMenuAddon()]`` so the global
    registry is ignored (same pattern as Megascans).
    """
    _ensure_basic_style()

    if existing_app is None:
        app = QGuiApplication(argv if argv is not None else sys.argv)
        app.setApplicationName("ADS Asset Browser")
        app.setOrganizationName("ADS")
    else:
        app = existing_app

    resolved = _resolve_addons(addons)

    ads = AdsBridge()
    menu_model = ContextMenuModel()
    context_menu = ContextMenuBridge(menu_model, addons=resolved)

    def _forward_status(text: str, kind: str) -> None:
        ads.statusChanged.emit(text, kind)

    context_menu.statusChanged.connect(_forward_status)

    engine = QQmlApplicationEngine()
    # Keep Python references for the process lifetime; QML only holds pointers.
    app._ads_bridge = ads  # type: ignore[attr-defined]
    app._ads_engine = engine  # type: ignore[attr-defined]
    app._ads_context_menu = context_menu  # type: ignore[attr-defined]
    app._ads_context_menu_model = menu_model  # type: ignore[attr-defined]

    ctx = engine.rootContext()
    ctx.setContextProperty("ads", ads)
    ctx.setContextProperty("contextMenu", context_menu)
    ctx.setContextProperty("contextMenuModel", menu_model)
    ctx.setContextProperty("adsInitialServer", server)
    ctx.setContextProperty("adsInitialToken", token)
    ctx.setContextProperty(
        "adsAutoConnect",
        bool(auto_connect and server and token),
    )

    qml_path = _qml_dir() / "Main.qml"
    engine.load(QUrl.fromLocalFile(str(qml_path)))
    if not engine.rootObjects():
        raise RuntimeError(f"failed to load QML: {qml_path}")

    return AdsBrowserSession(
        app=app,
        engine=engine,
        ads=ads,
        context_menu=context_menu,
        context_menu_model=menu_model,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="ADS Asset Browser (PySide6/QML)")
    parser.add_argument(
        "--server",
        default=default_server() or "http://td-ln10:8787",
        help="ads serve base URL (env: ADS_WEB_URL)",
    )
    parser.add_argument(
        "--token",
        default=default_token(),
        help="Bearer token (env: ADS_WEB_TOKEN). Prefer env over argv in shared shells.",
    )
    parser.add_argument(
        "--auto-connect",
        action="store_true",
        default=True,
        help="Connect on launch when server and token are set (default: on)",
    )
    parser.add_argument(
        "--no-auto-connect",
        action="store_true",
        help="Show the unlock screen even when credentials are available",
    )
    args = parser.parse_args(argv)

    token = args.token or os.environ.get("ADS_WEB_TOKEN", "")
    session = create_ads_browser(
        server=args.server,
        token=token,
        auto_connect=bool(args.auto_connect and not args.no_auto_connect),
        addons=None,
        argv=sys.argv[:1] + (argv or sys.argv[1:]),
    )
    return session.app.exec()


if __name__ == "__main__":
    raise SystemExit(main())

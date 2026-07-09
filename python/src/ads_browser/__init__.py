"""PySide6 / QML asset browser for ADS (WebApp-equivalent MVP)."""

__all__ = ["create_ads_browser", "main"]


def main() -> int:
    from .app import main as _main

    return _main()


def create_ads_browser(**kwargs):
    from .app import create_ads_browser as _create

    return _create(**kwargs)

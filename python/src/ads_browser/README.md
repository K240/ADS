# ADS Asset Browser (PySide6 / QML)
#
# WebApp-equivalent MVP for DCC / desktop use. Talks to `ads serve` through
# AdsHttpClient — same API surface as the embedded browser in src/webapp.rs.
#
# ## Run
#
# Prefer env vars so the token does not appear in process listings:
#
# ```powershell
# $env:ADS_WEB_URL = "http://td-ln10:8787"
# $env:ADS_WEB_TOKEN = "<token>"
# cd python
# uv run --extra browser ads-browser
# ```
#
# Or pass flags (token may show in `ps` / Task Manager):
#
# ```powershell
# uv run --extra browser ads-browser --server http://td-ln10:8787 --token <token>
# ```
#
# ## Features (MVP)
#
# - Profile select, search, category / department rails
# - Asset grid with async thumbnails
# - Inspector: ADS URI copy, version pin/reset, pull, WIP promote
# - Manifest listing, thumbnail upload
# - Right-click context menu driven by addons (Megascans-style)
#
# ## Context-menu addons
#
# DCC-specific actions register as `ContextMenuAddon` subclasses. The core
# browser never imports `hou` / Maya / etc.
#
# Contract (`ads_browser.addons`):
#
# - `AdsAssetContext` — snapshot for the selected asset
# - `MenuItem(action_id, label, enabled=True)`
# - `ContextMenuAddon` — `addon_id`, `menu_items(ctx)`, `invoke(ctx, action_id)`
# - `register_addon` / `iter_addons` / `clear_addons` — process-global registry
#
# Hosts should prefer **injection** over the global registry (same as Megascans):
#
# ```python
# import os
# from ads_browser import create_ads_browser
# from ads_browser.addons.hou_addon import HoudiniContextMenuAddon
#
# session = create_ads_browser(
#     server=os.environ["ADS_WEB_URL"],
#     token=os.environ["ADS_WEB_TOKEN"],
#     addons=[HoudiniContextMenuAddon()],
# )
# # session.app.exec()  # or embed under hou.qt.mainWindow()
# ```
#
# Standalone `ads-browser` auto-registers `ExampleContextMenuAddon` (Copy ADS URI,
# Copy asset key, Copy versioned URI).
#
# Right-click flow: QML → `contextMenu.prepareMenuFull(...)` → flat menu from all
# addons → `contextMenu.invoke(row)` dispatches `(addon_id, action_id)`.
#
# ## Houdini note
#
# This package targets a standalone PySide6 runtime. Inside Houdini, use the
# host's bundled Qt/PySide instead of installing PySide6 into Houdini's Python.
# Pass `HoudiniContextMenuAddon` via `addons=` so LOP Reference actions appear
# only when a NetworkEditor is active.

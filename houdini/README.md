# ADS Houdini Integration

This directory contains Houdini-side integration files for ADS.

## Package Setup

Copy or adapt `houdini/packages/ads.json.example` into a Houdini package directory. The package should add:

```text
PYTHONPATH                -> <repo>/python/src
HOUDINI_PATH              -> <repo>/houdini
HOUDINI_HUSDPLUGINS_PATH  -> <repo>/houdini/husdplugins
```

`HOUDINI_PATH` is needed for menu XML files such as
`AssetGallerySourceMenu.xml`. `HOUDINI_HUSDPLUGINS_PATH` is needed for Houdini
to discover `husdplugins/datasources/ads.py`.

It can also set ADS environment variables used by the resolver and USD ROP output processor:

```powershell
$env:ADS_RESOLVER_STORE = "D:\store"
$env:ADS_RESOLVER_WORKSPACE = "D:\workspace"
```

## Asset Catalog DataSource

`husdplugins/datasources/ads.py` registers a read-only Houdini Asset Gallery
datasource named `ADS`. It lists ADS assets from `ads serve` and returns
`ads://` file paths, so existing ADS AssetResolver settings decide whether the
asset opens from a local workspace or from remote object storage.
`AssetGallerySourceMenu.xml` adds an `Open ADS Asset Catalog` source menu item
to Houdini's regular Asset Gallery. It uses `hou.ui.setSharedLayoutDataSource`
so the catalog appears in the normal Asset Catalog, not only in the material
gallery.

Set the catalog API connection before launching Houdini:

```powershell
$env:ADS_CATALOG_SERVER = "http://127.0.0.1:8789"
$env:ADS_CATALOG_PROFILE = "main"
$env:ADS_CATALOG_API_TOKEN = "<token>"
$env:ADS_CATALOG_RESOLVE_MODE = "auto"
```

You can create the datasource explicitly from Houdini Python:

```python
import hou

source = hou.AssetGalleryDataSource(
    "ADS",
    "server=http://127.0.0.1:8789;profile=main",
)
print(source.isValid())
print(source.itemIds())
```

Optional datasource args are `server`, `profile`, `q`, `category`,
`department`, `token_env`, `resolve_mode`, and `timeout`. The `category`
filter is prefix-based, so `category=show/char` also matches
`show/char/main`. Token values should come from an environment variable rather
than the args string. For example:

```python
source = hou.AssetGalleryDataSource(
    "ADS",
    "server=http://127.0.0.1:8789;profile=main;category=default;department=model;token_env=ADS_CATALOG_API_TOKEN",
)
```

Catalog leaf items are `category / asset_code / department` current assets.
Their `filePath()` is `ads://<category>/<asset_code>/<department>/<asset_code>.usd`
without a version query, so the ADS current pointer is used by default.

If the menu item or datasource is not visible, verify that Houdini was launched
with both paths:

```python
import hou

print(hou.findFiles("AssetGallerySourceMenu.xml"))
print(hou.houdiniPath("HOUDINI_HUSDPLUGINS_PATH"))
```

The ADS menu XML file must appear in the first output, and
`<repo>/houdini/husdplugins` must appear in the second.

## Remote Resolver Launch

For remote object-read testing, build the resolver and launch Houdini through:

```bat
houdini\launch_ads_remote_houdini.bat http://127.0.0.1:8789 main <token> D:\workspace
```

The first argument is the ADS Web/API server started by `ads serve`.
The second argument is the allowed server profile. The third argument is the
Bearer token. The workspace argument is optional for remote read, but useful
for other Houdini-side ADS tools.

In native remote mode the resolver queries `/api/resolve` directly and reads
object URLs directly. It does not spawn `ads.exe` or `curl.exe` during USD
resolution.

The launcher sets:

```text
ADS_RESOLVER_MODE=remote
ADS_RESOLVER_SERVER=<ads-server>
ADS_RESOLVER_PROFILE=<profile>
ADS_CATALOG_SERVER=<ads-server>
ADS_CATALOG_PROFILE=<profile>
PXR_PLUGINPATH_NAME=<repo>\resolver\build\houdini\resources
PYTHONPATH=<repo>\python\src
HOUDINI_PATH=<repo>\houdini;&
HOUDINI_HUSDPLUGINS_PATH=<repo>\houdini\husdplugins;&
```

Optional environment overrides:

```bat
set HOUDINI_ROOT=C:\Program Files\Side Effects Software\Houdini 21.0.700
set ADS_RESOLVER_DEBUG=1
set ADS_RESOLVER_LOG_FILE=%TEMP%\ads_resolver_houdini.log
```

If Houdini reports a resolver error, enable `ADS_RESOLVER_DEBUG=1` and inspect
the log file printed by the launcher.

## WIP Staging Output Processor (schema v8)

`husdplugins/outputprocessors/adswipstaging.py` registers an `ADS WIP Staging`
output processor. Every save under the configured department root is
redirected to a unique per-run staging folder, so a write never overwrites
bytes another process holds open — the usdc lock problem cannot occur.

Set the publish target explicitly before launching Houdini (path inference is
ambiguous with nested categories):

```powershell
$env:ADS_RESOLVER_STORE = "D:\store"
$env:ADS_RESOLVER_WORKSPACE = "D:\workspace"
$env:ADS_WIP_CATEGORY = "char"
$env:ADS_WIP_ASSET_CODE = "hero"
$env:ADS_WIP_DEPARTMENT = "model"
```

Example redirect:

```text
D:/workspace/char/hero/model/hero.usd
  -> D:/workspace/.ads-staging/<run-id>/char/hero/model/hero.usd
```

Register the staged result from the ROP post-render script (Python):

```python
from ads.houdini_wip import commit_staged

commit_staged()
```

`commit_staged()` runs `ads wip add` on the staged folder, advances the WIP
head, and removes the staging copy; the bytes live on in the content-addressed
store. Re-registering unchanged content returns the existing head instead of
growing the stream. The artist's own session can follow the stream through
`ads://...?v=wip`, while everyone else keeps resolving `current`. Publish the
head with:

```powershell
ads publish promote `
  --store D:\store `
  --category char `
  --asset-code hero `
  --department model
```

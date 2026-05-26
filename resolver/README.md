# ADS USD Resolver

This directory contains the C++ OpenUSD `ArResolver` plugin for ADS.

The resolver registers the `ads` URI scheme and resolves paths such as:

```text
ads://hero/model/hero.usd
ads://hero/model/hero.usd?v=v002
ads://char/hero/model/hero.usd
```

The resolver is read-only. It delegates URI lookup to the `ads resolve` CLI command, then opens the resolved asset.

- Local filesystem paths are opened through `ArFilesystemAsset`.
- `http://` and `https://` resolved paths are downloaded into an in-memory `ArAsset`.

## Environment

Set these environment variables before launching Houdini:

```powershell
$env:ADS_RESOLVER_EXECUTABLE = "D:\tools\ads.exe"
$env:ADS_RESOLVER_STORE = "D:\store"
$env:ADS_RESOLVER_WORKSPACE = "D:\workspace"
$env:ADS_RESOLVER_MODE = "local"
$env:PXR_PLUGINPATH_NAME = "D:\work\apps\ads\resolver\build\houdini\resources"
```

`ADS_RESOLVER_MODE` defaults to `local`. Use `ads pull` before opening a stage when the version folder is not present in the workspace.

For remote direct reads, set `ADS_RESOLVER_MODE=remote` or `auto` and configure a remote object base URL in the ADS store or through `ADS_RESOLVER_REMOTE_BASE_URL`.

Texture-like files are resolved differently from USD layers. For reserved texture extensions such as `.tx`, `.rat`, `.exr`, `.tif`, `.png`, and `.jpg`, `ads resolve --mode local|auto` reads the manifest from the store and returns a hash-derived local cache path:

```text
<workspace>/.ads-cache/sha256/<prefix>/<sha256>.<ext>
```

USD layers (`.usd`, `.usda`, `.usdc`, `.usdz`) still resolve to their logical version-folder path.

When the same logical texture filename is updated in a newer version, the resolver keeps the `ads://.../body_diffuse.1001.tx` URI stable and returns a different hash-derived cache file for the selected `current`, `latest`, or explicit `?v=v###` version.

Optional:

```powershell
$env:ADS_RESOLVER_DEBUG = "1"
$env:ADS_RESOLVER_REMOTE_BASE_URL = "https://assets.example.com/objects/sha256"
$env:ADS_RESOLVER_HTTP_EXECUTABLE = "curl"
$env:ADS_RESOLVER_HTTP_BEARER_TOKEN = "<token>"
$env:ADS_RESOLVER_HTTP_TIMEOUT_SECONDS = "30"
```

Remote direct read uses the configured HTTP executable, defaulting to `curl`, and buffers the full response into memory through `ArInMemoryAsset`. This avoids creating workspace version files, but it is not a streaming or range-read implementation.

## Build With Houdini

From the repository root:

```powershell
.\resolver\build_houdini.ps1 -HoudiniRoot "C:\Program Files\Side Effects Software\Houdini 21.0.700"
```

The script uses Houdini's `hcustom.exe -U` and writes the plugin to:

```text
resolver/build/houdini/
  adsResolver.dll
  resources/plugInfo.json
```

Then set:

```powershell
$env:PXR_PLUGINPATH_NAME = "D:\work\apps\ads\resolver\build\houdini\resources"
```

## Houdini Smoke Test

After setting the environment variables, run this in Houdini Python:

```python
from pxr import Ar

resolver = Ar.GetResolver()
print(resolver.Resolve("ads://hero/model/hero.usd"))
```

For a full USD stage test:

```python
from pxr import Usd

stage = Usd.Stage.Open("D:/path/to/shot.usd")
```

If `shot.usd` contains references or sublayers using `ads://...`, USD should call the ADS resolver.

## Notes

- This resolver is read-only. Create and edit explicit workspace version folders, then publish with `ads add`.
- `ADS_RESOLVER_STORE` is required.
- `ADS_RESOLVER_WORKSPACE` is recommended.
- `ADS_RESOLVER_MODE=remote` can open HTTP object URLs directly when `curl` or a compatible executable is available.
- Remote assets are currently downloaded as complete in-memory buffers.
- The resolver treats ADS paths as context-dependent because `current`, `latest`, store configuration, and workspace configuration can change the resulting path.

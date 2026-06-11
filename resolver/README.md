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
- Native remote mode can query `ads serve` directly and does not spawn `ads.exe` or `curl.exe`.

## Environment

Set these environment variables before launching Houdini:

```powershell
$env:ADS_RESOLVER_EXECUTABLE = "D:\tools\ads.exe"
$env:ADS_RESOLVER_STORE = "D:\store"
$env:ADS_RESOLVER_WORKSPACE = "D:\workspace"
$env:ADS_RESOLVER_MODE = "local"
$env:PXR_PLUGINPATH_NAME = "D:\work\apps\ads\resolver\build\houdini\resources"
```

`ADS_RESOLVER_MODE` defaults to `local`. Resolution materializes the immutable manifest view from the store on demand (schema v8), so no pull step is needed before opening a stage; the local store just needs the objects.

For native remote direct reads, point the resolver at an ADS Web/API server:

```powershell
$env:ADS_RESOLVER_SERVER = "http://127.0.0.1:8789"
$env:ADS_RESOLVER_PROFILE = "main"
$env:ADS_RESOLVER_API_TOKEN = "<token>"
$env:ADS_RESOLVER_MODE = "remote"
$env:PXR_PLUGINPATH_NAME = "D:\work\apps\ads\resolver\build\houdini\resources"
```

In this mode the resolver calls `/api/resolve` through native HTTP and opens returned object URLs through native HTTP. On Windows the backend is WinHTTP. The HTTP backend is isolated so macOS/Linux can use a native library backend such as libcurl without shelling out to the `curl` command.

Resolution shape follows one rule. Composing formats that can carry relative
sibling references (`.usd`, `.usda`, `.usdc`, `.usdz`, `.mtlx`) resolve into
the eagerly materialized manifest view:

```text
<workspace>/.ads-cache/manifests/<manifest_hash>/<relative_path>
```

Every other file is a leaf (textures, volumes, caches, ...) and resolves
lazily to its flat blob cache path — only the requested file is copied:

```text
<workspace>/.ads-cache/sha256/<prefix>/<sha256>.<ext>
```

When the same logical filename is updated in a newer version, the resolver keeps the `ads://.../body_diffuse.1001.tx` URI stable and returns a different hash-derived cache file for the selected `current`, `latest`, `wip`, or explicit `?v=2` version.

Optional:

```powershell
$env:ADS_RESOLVER_DEBUG = "1"
$env:ADS_RESOLVER_LOG_FILE = "$env:TEMP\ads_resolver_houdini.log"
$env:ADS_RESOLVER_REMOTE_BASE_URL = "https://assets.example.com/objects/sha256"
$env:ADS_RESOLVER_HTTP_BEARER_TOKEN = "<object-token>"
$env:ADS_RESOLVER_HTTP_TIMEOUT_SECONDS = "30"
$env:ADS_RESOLVER_CACHE_TTL_SECONDS = "30"
$env:ADS_RESOLVER_MAX_DOWNLOAD_MB = "2048"
```

Resolve caching follows the schema v8 policy: explicit version pins (`?v=12`
or `?v=v012`) are immutable and cached for the whole session, while
`current` / `latest` resolutions expire after `ADS_RESOLVER_CACHE_TTL_SECONDS`
(default 30, `0` disables caching for mutable selectors) so pointer switches
on the server become visible without restarting Houdini. `?v=wip` resolutions
are never cached: the WIP head moves on every registered write. WIP is
local-only and does not resolve in remote mode.

Remote direct read buffers the full response into memory through `ArInMemoryAsset`. This avoids creating workspace version files, but it is not a streaming or range-read implementation. Downloads are capped at `ADS_RESOLVER_MAX_DOWNLOAD_MB` (default 2048, `0` disables the cap) — both via the announced Content-Length and during chunked reads — so one runaway object cannot take the host process down.

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
- `ADS_RESOLVER_MODE=remote` can open HTTP object URLs directly. On Windows this uses WinHTTP and does not spawn `curl.exe`.
- Remote assets are currently downloaded as complete in-memory buffers.
- The resolver treats ADS paths as context-dependent because `current`, `latest`, store configuration, and workspace configuration can change the resulting path.

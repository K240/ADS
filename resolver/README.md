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
- `http://` and `https://` resolved object URLs are downloaded once into an on-disk content-addressed blob cache and then opened as regular files; if no cache directory is available the download falls back to an in-memory `ArAsset`.
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

The served store must carry a remote object base URL (`ads set-remote
--remote-base-url https://assets.example.com/objects/sha256`), otherwise
`/api/resolve?mode=remote` fails with `remote base URL is not configured`.
`ads serve` does not serve raw object bytes itself: the base URL must point
at a static endpoint (object storage, CDN, or any web server) exposing the
store's `objects/sha256/<prefix>/<sha256>` layout.

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

In remote mode there is no manifest view: leaves still resolve to their flat
blob cache path, while composing formats keep the `ads://` URI as the
resolved path and read their bytes from the blob cache (see "Remote object
blob cache" below).

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
$env:ADS_RESOLVER_CACHE_DIR = "D:\cache\ads-resolver"
$env:ADS_RESOLVER_CLI_TIMEOUT_SECONDS = "60"
```

`ADS_RESOLVER_CLI_TIMEOUT_SECONDS` (default 60) bounds the whole lifetime of one
`ads resolve` child process on Windows (spawn, stdout read, exit). A CLI child
that exceeds the deadline is terminated, warned about once, and treated as a
failed resolution — a hung `ads.exe` can no longer hang a USD composition
thread.

Resolve caching follows the schema v8 policy: explicit version pins (`?v=12`
or `?v=v012`) are immutable and cached for the whole session, while
`current` / `latest` resolutions expire after `ADS_RESOLVER_CACHE_TTL_SECONDS`
(default 30, `0` disables caching for mutable selectors) so pointer switches
on the server become visible without restarting Houdini. `?v=wip` resolutions
are never cached: the WIP head moves on every registered write. WIP is
local-only and does not resolve in remote mode.

### Remote object blob cache

Remote object URLs (paths containing `objects/sha256/<prefix>/<sha256>`) are
materialized into an on-disk content-addressed blob cache at resolve time.
The resolved path follows the composing/leaf split above: leaf files resolve
to the cached **file path** and open through the normal filesystem asset
path, while composing formats keep the `ads://` URI as the resolved path —
a flat cache path would send their relative sibling references to
nonexistent cache-file siblings — and open their bytes from the cache file
warmed by that same resolve. Both shapes report the cache file's real
modification timestamp, so `SdfLayer::Reload` no longer refetches unchanged
content. The cache directory is chosen in this order:

1. `ADS_RESOLVER_CACHE_DIR` (blobs land under `<dir>/sha256/...`)
2. `<ADS_RESOLVER_WORKSPACE>/.ads-cache/sha256` — the same flat blob cache the
   CLI uses, so `ads cache gc` manages both
3. `%LOCALAPPDATA%\ads\resolver-cache\sha256` on Windows
   (`$XDG_CACHE_HOME/ads/resolver-cache/sha256` or
   `~/.cache/ads/resolver-cache/sha256` elsewhere)

The layout matches the CLI blob cache: `sha256/<2-hex-prefix>/<hash>.<ext>`,
with the extension taken from the `ads://` URI leaf. Downloads are written to
a temporary file, verified against the sha256 embedded in the object URL, and
renamed into place; a hash mismatch is never cached. Because blobs are
immutable content addressed by hash, cached blobs are kept permanently for
every version selector (including `wip`) — the resolve cache policy above
governs only the URI-to-location mapping. Concurrent requests for the same
object are deduplicated in-process: one thread downloads, the rest wait and
take the cache hit.

When no cache directory is available (or the object URL carries no parseable
sha256, or verification fails), the resolver falls back to the previous
behavior: the full response is buffered into memory through `ArInMemoryAsset`
on every open, and such paths report no modification timestamp. Downloads are
capped at `ADS_RESOLVER_MAX_DOWNLOAD_MB` (default 2048, `0` disables the cap)
so one runaway object cannot take the host process down. On Windows (WinHTTP)
the cap is enforced both via the announced Content-Length and cumulatively
during chunked reads. On the `curl` fallback path the cap is passed as
`--max-filesize`, which curl only honors when the server announces a
Content-Length; chunked responses are not cumulatively enforced there.

Resolution and download failures (unreachable server, non-2xx status, exceeded download cap, missing store configuration, server-to-CLI fallback) emit a `TF_WARN` visible in the host application's console. Each distinct failure warns once per process to avoid flooding; set `ADS_RESOLVER_DEBUG` / `ADS_RESOLVER_LOG_FILE` for the full per-asset trace.

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
- `ADS_RESOLVER_WORKSPACE` is recommended (it also provides the default blob cache location).
- `ADS_RESOLVER_MODE=remote` can open HTTP object URLs directly. On Windows this uses WinHTTP and does not spawn `curl.exe`.
- Remote objects are cached on disk (content addressed by sha256); the whole-buffer in-memory download remains only as a fallback when no cache directory is usable.
- The `ads:` scheme is matched case-insensitively (RFC 3986); malformed forms without authority slashes such as `ads:foo` are rejected with a warning instead of being guessed at.
- The resolver treats ADS paths as context-dependent because `current`, `latest`, store configuration, and workspace configuration can change the resulting path.

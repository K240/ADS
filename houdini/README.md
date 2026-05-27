# ADS Houdini Integration

This directory contains Houdini-side integration files for ADS.

## Package Setup

Copy or adapt `houdini/packages/ads.json.example` into a Houdini package directory. The package should add:

```text
PYTHONPATH   -> <repo>/python/src
HOUDINI_PATH -> <repo>/houdini
```

It can also set ADS environment variables used by the resolver and USD ROP output processor:

```powershell
$env:ADS_RESOLVER_STORE = "D:\store"
$env:ADS_RESOLVER_WORKSPACE = "D:\workspace"
$env:ADS_OUTPUT_PUBLIC_ROOT = "D:\public"
```

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
PXR_PLUGINPATH_NAME=<repo>\resolver\build\houdini\resources
PYTHONPATH=<repo>\python\src
HOUDINI_PATH=<repo>\houdini;&
```

Optional environment overrides:

```bat
set HOUDINI_ROOT=C:\Program Files\Side Effects Software\Houdini 21.0.700
set ADS_RESOLVER_DEBUG=1
set ADS_RESOLVER_LOG_FILE=%TEMP%\ads_resolver_houdini.log
```

If Houdini reports a resolver error, enable `ADS_RESOLVER_DEBUG=1` and inspect
the log file printed by the launcher.

## USD ROP Output Processor

`husdplugins/outputprocessors/adspublish.py` registers an `ADS Managed Publish` output processor for Solaris USD ROPs.

The processor:

- Redirects save paths under `ADS_RESOLVER_WORKSPACE` to `ADS_OUTPUT_PUBLIC_ROOT` when both are set.
- Rewrites references under the workspace or public root to version-pinned `ads://` URIs.
- Leaves unmanaged external paths unchanged.

Example conversion:

```text
D:/workspace/char/hero/model/v003/geo/body.usd
  -> ads://char/hero/model/geo/body.usd?v=v003

D:/public/char/hero/texture/v002/maps/body.1001.tx
  -> ads://char/hero/texture/maps/body.1001.tx?v=v002
```

Use this processor on USD ROPs that publish ADS-managed layers. After saving, register the public version with `ads publish register`.

## Register Public Output

After a USD ROP save, validate and register the public version folder:

```powershell
ads publish validate `
  --store D:\store `
  --public-root D:\public `
  --category char `
  --asset-code hero `
  --department model `
  --version v003

ads publish register `
  --store D:\store `
  --public-root D:\public `
  --category char `
  --asset-code hero `
  --department model `
  --version v003
```

`validate` checks text USD files for unmanaged `@...@` references before the folder is registered.

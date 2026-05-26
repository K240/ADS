# ADS Python API

This package provides a small Python API for ADS.

Phase 1 is intentionally pure Python and uses only the standard library. It is designed to work in Houdini Python environments without building a native extension module.

## Local CLI API

`AdsCli` wraps the `ads` executable.

```python
from ads import AdsCli

ads = AdsCli(r"D:\tools\ads.exe")

ads.init(r"D:\store")
ads.asset_create(
    store=r"D:\store",
    workspace=r"D:\workspace",
    category="char",
    asset_code="hero",
)
ads.new_version(
    store=r"D:\store",
    workspace=r"D:\workspace",
    category="char",
    asset_code="hero",
    department="model",
)
```

Register a version folder:

```python
ads.add(
    store=r"D:\store",
    workspace=r"D:\workspace",
    category="char",
    asset_code="hero",
    department="model",
    version="v001",
)
```

Resolve an ADS URI:

```python
path = ads.resolve(
    "ads://hero/model/hero.usd",
    store=r"D:\store",
    workspace=r"D:\workspace",
)
```

Pull the current workspace version:

```python
ads.pull(
    store=r"D:\store",
    workspace=r"D:\workspace",
    category="char",
    asset_code="hero",
    department="model",
)
```

Register a public USD ROP output folder:

```python
ads.publish_validate(
    store=r"D:\store",
    public_root=r"D:\public",
    category="char",
    asset_code="hero",
    department="model",
    version="v003",
)
ads.publish_register(
    store=r"D:\store",
    public_root=r"D:\public",
    category="char",
    asset_code="hero",
    department="model",
    version="v003",
)
```

## Remote API

`AdsHttpClient` talks to `ads serve`.

```python
from ads import AdsHttpClient

client = AdsHttpClient("http://ads-server:8787", token="secret")

assets = client.assets(profile="main", category="char", department="model")
client.current_set(
    profile="main",
    category="char",
    asset_code="hero",
    department="model",
    version="v002",
)
client.pull(
    profile="main",
    category="char",
    asset_code="hero",
    department="model",
)
```

## USD Dependency Utility

`ads-deps` lists ADS URI dependencies from a root USD file. When `--store` is provided, it also checks whether each dependency is present in the workspace and reports which URLs need `pull` or `restore`.

```powershell
uv run ads-deps D:\shots\shot010\shot.usda `
  --store D:\store `
  --workspace D:\workspace
```

JSON output:

```powershell
uv run ads-deps D:\shots\shot010\shot.usda `
  --store D:\store `
  --workspace D:\workspace `
  --json
```

Show every USD dependency discovered by OpenUSD:

```powershell
uv run ads-deps D:\shots\shot010\shot.usda --all
```

Execute the plan:

```powershell
uv run ads-deps D:\shots\shot010\shot.usda `
  --store D:\store `
  --workspace D:\workspace `
  --execute
```

Inside Houdini, run it with `hython -m ads.usd_deps` and set `PYTHONPATH` to `python/src` so the utility can use OpenUSD's dependency APIs for binary `.usd/.usdc` files.

## Houdini USD ROP Output Processor

`ads.houdini_output.AdsPathMapper` is the pure Python mapping layer used by the Houdini `ADS Managed Publish` output processor.

```python
from ads.houdini_output import AdsPathMapper

mapper = AdsPathMapper(
    workspace_root=r"D:\workspace",
    public_root=r"D:\public",
)

mapper.to_ads_uri(r"D:\public\char\hero\model\v003\hero.usd")
# ads://char/hero/model/hero.usd?v=v003
```

The Houdini plugin lives under `houdini/husdplugins/outputprocessors/adspublish.py`. It rewrites ADS-managed references emitted by Solaris USD ROPs to version-pinned `ads://` URIs, and can redirect save paths from the workspace root to a public root.

## Development

Use `uv` for Python commands.

```powershell
uv run python -m unittest discover -s tests
```

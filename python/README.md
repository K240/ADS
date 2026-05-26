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

## Development

Use `uv` for Python commands.

```powershell
uv run python -m unittest discover -s tests
```

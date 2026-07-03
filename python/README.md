# ADS Python API

This package provides a small Python API for ADS.

Phase 1 is intentionally pure Python and uses only the standard library. It is designed to work in Houdini Python environments without building a native extension module.

## Local CLI API

`AdsCli` wraps the `ads` executable. Set `ADS_CLI_TIMEOUT_SECONDS` (or pass
`AdsCli(timeout=...)`) to bound every CLI call; when unset, commands may run
as long as they need (heavy `wip add` runs on large sources take a while).

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
```

Register WIP writes and promote them to a publish version (schema v8):

```python
ads.wip_add(
    store=r"D:\store",
    category="char",
    asset_code="hero",
    department="model",
    source=r"D:\workspace\.ads-staging\run1\char\hero\model",
)
ads.publish_promote(
    store=r"D:\store",
    category="char",
    asset_code="hero",
    department="model",
)
```

Register a version directly from any source folder (the version number is
auto-assigned when omitted):

```python
ads.add(
    store=r"D:\store",
    category="char",
    asset_code="hero",
    department="model",
    source=r"D:\delivery\hero_model_fix",
)
```

The legacy workspace form locates the conventional
`<category>/<asset-code>/<department>/v###` folder and therefore requires an
explicit version:

```python
ads.add(
    store=r"D:\store",
    workspace=r"D:\workspace",
    category="char",
    asset_code="hero",
    department="model",
    version=3,
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

Materialize a version into an explicit folder when real files are needed
outside the resolver (local resolution itself needs no pull step):

```python
ads.checkout(
    store=r"D:\store",
    category="char",
    asset_code="hero",
    department="model",
    dest=r"D:\temp\hero_model",
)
```

Validate the publish reference policy (run automatically by
`publish_promote`; cross-asset references must be `ads://`, intra-version
relative references are allowed):

```python
ads.publish_validate(
    store=r"D:\store",
    category="char",
    asset_code="hero",
    department="model",
    wip=True,
)
ads.publish_validate(source=r"D:\delivery\hero_model_fix")
```

Fetch a remote version into a local store. `auth_token` is handed to the CLI
through the `ADS_WEB_TOKEN` environment variable instead of argv, so it never
shows up in process listings:

```python
ads.fetch(
    server="http://ads-server:8787",
    auth_token="secret",
    profile="main",
    store=r"D:\local-cache",
    workspace=r"D:\workspace",
    category="char",
    asset_code="hero",
    department="model",
    version="v003",
    materialize=True,
)
```

Sync multiple remote assets into a local store:

```python
ads.sync(
    server="http://ads-server:8787",
    auth_token="secret",
    profile="main",
    store=r"D:\local-cache",
    category="char",
    department="model",
    all_versions=True,
)
```

Push a local version to a remote store:

```python
ads.push(
    server="http://ads-server:8787",
    auth_token="secret",
    profile="main",
    store=r"D:\local-cache",
    category="char",
    asset_code="hero",
    department="model",
    version="v003",
    set_current=True,
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

Remote store read helpers:

```python
info = client.version_info(
    profile="main",
    category="char",
    asset_code="hero",
    department="model",
    version="v003",
)

data = client.object_bytes(info["manifest"]["entries"][0]["sha256"], profile="main")
status = client.object_status(info["manifest"]["entries"][0]["sha256"], profile="main")
client.import_thumbnail_info(thumbnail_record, profile="main")
```

Register a WIP micro-version on a served store. This is the remote
counterpart of `ads wip add` for when `ads serve` holds the store's RocksDB
lock: upload every manifest object first (`upload_object`), then post the
manifest. `source_path` defaults server-side to
`category/asset_code/department`. The server requires the canonical manifest
form (entries strictly ascending by `relative_path`, no duplicates);
`wip_import` sorts the entries for you, duplicates are rejected with a 400.

```python
wips = client.wips(
    profile="main",
    category="char",
    asset_code="hero",
    department="model",
)

outcome = client.wip_import(
    {
        "entries": [
            {"relative_path": "hero.usd", "sha256": sha, "size": size, "mode": 33188},
        ]
    },
    profile="main",
    category="char",
    asset_code="hero",
    department="model",
)
# outcome: {"created": ..., "seq": ..., "manifest_hash": ..., "file_count": ..., "total_bytes": ...}
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

## Houdini WIP Staging

`ads.houdini_wip.WipStaging` is the pure Python layer used by the Houdini
`ADS WIP Staging` output processor
(`houdini/husdplugins/outputprocessors/adswipstaging.py`). It redirects saves
under the department work folder to a unique staging run, and
`commit_staged()` registers the result as a WIP micro-version from a ROP
post-render script. See `houdini/README.md` for the ROP wiring.

```python
from ads.houdini_wip import WipStaging

staging = WipStaging(
    workspace_root=r"D:\workspace",
    category="char",
    asset_code="hero",
    department="model",
)

staging.redirect(r"D:\workspace\char\hero\model\hero.usd")
# D:/workspace/.ads-staging/<run-id>/char/hero/model/hero.usd
```

## Development

Use `uv` for Python commands.

```powershell
uv run python -m unittest discover -s tests
```

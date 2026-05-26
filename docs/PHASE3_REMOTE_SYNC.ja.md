# Phase 3 Remote Store / Sync Notes

## MVP-A: Remote Read API

Phase 3 starts by making the central `ads serve` process usable as a remote store read endpoint.

New authenticated API endpoints:

```text
GET /api/version?profile=main&category=char&asset_code=hero&department=model&version=v003
GET /api/object?profile=main&sha256=<64-hex>
```

`/api/version` returns `VersionInfo`, including the `VersionRecord` and `Manifest`.  
`/api/object` returns raw object bytes from `objects/sha256/<prefix>/<hash>`.

These endpoints are intentionally read-only. They are the base for later `fetch` and `sync` clients:

```text
remote /api/version
  -> local metadata import
  -> collect missing manifest entry sha256 values
  -> remote /api/object
  -> local objects/sha256 cache
```

## Python Client

The Python HTTP client exposes:

```python
client.version_info(
    profile="main",
    category="char",
    asset_code="hero",
    department="model",
    version="v003",
)

client.object_bytes("<sha256>", profile="main")
client.download_object("<sha256>", r"D:\cache\object.bin", profile="main")
```

## Next Step

MVP-B adds a local fetch command that consumes these endpoints:

```powershell
ads fetch `
  --server http://ads-server:8787 `
  --auth-token $env:ADS_WEB_TOKEN `
  --profile main `
  --store D:\local-cache `
  --category char `
  --asset-code hero `
  --department model `
  --version v003
```

`ads fetch` initializes the local store if needed, downloads missing objects, verifies SHA-256 checksums, imports the remote `VersionInfo`, and can optionally materialize the selected workspace version:

```powershell
ads fetch `
  --server http://ads-server:8787 `
  --auth-token $env:ADS_WEB_TOKEN `
  --profile main `
  --store D:\local-cache `
  --workspace D:\workspace `
  --category char `
  --asset-code hero `
  --department model `
  --version v003 `
  --materialize
```

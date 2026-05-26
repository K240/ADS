# Phase 3 Remote Store / Sync Notes

## MVP-A: Remote Read API

Phase 3 は、中央の `ads serve` プロセスを remote store の read endpoint として使えるようにするところから始める。

追加した認証付き API endpoint:

```text
GET /api/version?profile=main&category=char&asset_code=hero&department=model&version=v003
GET /api/object?profile=main&sha256=<64-hex>
```

`/api/version` は `VersionRecord` と `Manifest` を含む `VersionInfo` を返す。
`/api/object` は `objects/sha256/<prefix>/<hash>` から raw object bytes を返す。

これらの endpoint は意図的に read-only とする。後続の `fetch` / `sync` client の土台になる。

```text
remote /api/version
  -> local metadata import
  -> collect missing manifest entry sha256 values
  -> remote /api/object
  -> local objects/sha256 cache
```

## Python Client

Python HTTP client は以下を提供する。

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

MVP-B では、これらの endpoint を使う local fetch command を追加した。

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

`ads fetch` は local store がなければ初期化し、不足 object を download し、SHA-256 checksum を検証したうえで remote `VersionInfo` を import する。必要に応じて、選択した version を workspace へ materialize できる。

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

## MVP-C: Filtered Sync

`ads sync` は、任意 filter に一致する asset の current、latest、または全 version を取得する。

```powershell
ads sync `
  --server http://ads-server:8787 `
  --auth-token $env:ADS_WEB_TOKEN `
  --profile main `
  --store D:\local-cache `
  --category char `
  --department model
```

既定では各 asset department の remote current version を同期する。`--latest` では latest version を同期し、`--all-versions` では一致する全 version を local store に mirror する。

```powershell
ads sync `
  --server http://ads-server:8787 `
  --auth-token $env:ADS_WEB_TOKEN `
  --profile main `
  --store D:\local-cache `
  --category char `
  --all-versions
```

`--materialize` は current/latest sync と組み合わせて使う。metadata と object を取得した後、選択された version を workspace に復元する。

# Phase 3 Completion

Status: complete for the remote store MVP.

Phase 3 の目的は、central `ads serve` を remote store として扱い、local store / workspace と基本的に往復できる状態にすることだった。現時点では以下を満たしている。

## 実装済み

- `ads serve` に authenticated remote read/write API を追加。
- `ads fetch` で remote version metadata / manifest / object を local store へ取得。
- `ads sync` で filter に一致する asset の current / latest / all versions を同期。
- `ads push` で local version、object、thumbnail metadata を remote store へ送信。
- object は SHA-256 content-addressed store として dedup し、転送時も checksum を検証。
- current pointer は fetch / sync / push の対象として扱う。
- thumbnail object / metadata は version と同じ remote transfer path に乗せる。
- Python API に remote read/write helper を追加。
- C++ ADS Resolver が `http://` / `https://` resolved path を `ArInMemoryAsset` として直接読める。

## Resolver Remote Read

Resolver は `ads resolve --mode remote|auto` が返した HTTP URL を `curl` 互換 command で取得し、USD には in-memory asset として渡す。

```powershell
$env:ADS_RESOLVER_MODE = "remote"
$env:ADS_RESOLVER_REMOTE_BASE_URL = "https://assets.example.com/objects/sha256"
$env:ADS_RESOLVER_HTTP_EXECUTABLE = "curl"
```

認証付き endpoint を読む場合:

```powershell
$env:ADS_RESOLVER_HTTP_BEARER_TOKEN = $env:ADS_WEB_TOKEN
```

## Phase 4 以降に送る項目

- conflict report / dry-run push plan
- object garbage collection / pruning
- remote direct read の range request / streaming / retry policy
- user / role / audit / approval workflow
- WebApp からの authoring 操作
- schema migration

Phase 3 は remote store MVP として完了し、以降は production hardening と governance の領域に移る。

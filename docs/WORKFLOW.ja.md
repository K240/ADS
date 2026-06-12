# ADS 作業フローマニュアル

対象: ADS schema version 8(WIP/Publishモデル)
作成日: 2026-06-11

設計の背景は `WHITEPAPER.ja.md`、仕様の詳細は `SPEC_WIP_PUBLISH.ja.md` を参照してください。本書は「日々どう操作するか」だけをまとめた実務マニュアルです。

## 1. 用語と考え方(1分で)

| 用語 | 意味 |
|---|---|
| store | 正規データ。RocksDBメタデータ + content-addressed object store。直接編集しない |
| workspace | 作業領域(scratch)。ADSはレイアウトを要求しない。消しても正規データは無傷 |
| workフォルダ | `<workspace>/<category>/<asset_code>/<department>/`。編集の起点であり、WIP stagingの監視対象 |
| WIP | 書き出し1回 = 1 micro-version。local限定・自動採番・GC対象。`?v=wip` でのみ見える |
| publish version | 整数番号を持つ公開version。`publish promote` でWIPから昇格(コピーゼロ) |
| current | 部門ごとの参照ポインタ。未設定ならlatestと同じ。production参照はこれに任せる |
| manifest view | `<workspace>/.ads-cache/manifests/<hash>/` の不変キャッシュ。Resolverの解決先 |

原則はひとつだけ覚えてください: **登録済みのバイト列はどこでも上書きされません**。書き出しは常に一意なパスへ行き、読み込みは不変キャッシュから行われます。usdcのロックを気にする必要はありません。

## 2. セットアップ(TD / 管理者)

### Store作成

```powershell
ads init D:\store --remote-base-url https://assets.example.com/objects/sha256
```

`--remote-base-url` はremote mode解決とthumbnail配信に使うobject URLの基底です(後から `ads set-remote` で変更可)。

### サーバ起動(WebApp + JSON API)

```powershell
ads serve `
  --bind 0.0.0.0:8787 `
  --auth-token <token> `
  --profile main=D:\store::D:\workspace
```

- `--profile name=<store>::<workspace>` は複数指定できます
- ブラウザで `http://<host>:8787` を開き、Bearer tokenを入力します
- **注意**: 読み取り系コマンド(`resolve` / `list` / `info` / `checkout` / `materialize` / `wip list` / `verify` / `publish validate` / `push` 等)はread-only openで動くため、**serve起動中でもそのまま使えます**(HoudiniのResolverもこちら)。書き込み系(`wip add` / `publish promote` / `add` / `current set` / `gc` 等)はRocksDBの書き込み排他のため、serveを止めるか別storeに対して実行します

### Houdini環境(packageまたは起動スクリプト)

```powershell
# Resolver(local mode)
$env:ADS_RESOLVER_EXECUTABLE = "D:\tools\ads.exe"
$env:ADS_RESOLVER_STORE     = "D:\store"
$env:ADS_RESOLVER_WORKSPACE = "D:\workspace"
$env:ADS_RESOLVER_MODE      = "local"
$env:PXR_PLUGINPATH_NAME    = "<repo>\resolver\build\houdini\resources"

# WIP書き出し対象(作業するアセットを明示)
$env:ADS_WIP_CATEGORY    = "char"
$env:ADS_WIP_ASSET_CODE  = "hero"
$env:ADS_WIP_DEPARTMENT  = "model"

# Asset Catalog(WebApp APIを参照)
$env:ADS_CATALOG_SERVER    = "http://ads-server:8787"
$env:ADS_CATALOG_PROFILE   = "main"
$env:ADS_CATALOG_API_TOKEN = "<token>"
```

remote mode(local store不要)の場合はResolver側を以下に置き換えます。

```powershell
$env:ADS_RESOLVER_SERVER    = "http://ads-server:8787"
$env:ADS_RESOLVER_PROFILE   = "main"
$env:ADS_RESOLVER_API_TOKEN = "<token>"
$env:ADS_RESOLVER_MODE      = "remote"
```

## 3. アーティストの日常ループ

```text
種まき → 編集 → 書き出し(=WIP自動登録) → ?v=wip で確認 → promote → current運用
```

### 3.1 作業を始める(種まき)

currentの内容をworkフォルダへ展開します。**v###フォルダは作られません** — 展開先はWIP stagingが監視している場所そのものです。

- WebApp: アセットを選択 → versionを選ぶ → **Pull to Workspace**
- CLI:

```powershell
ads materialize `
  --store D:\store `
  --workspace D:\workspace `
  --category char `
  --asset-code hero `
  --department model
```

`--version 2` / `--latest` で起点を選べます。workフォルダに編集中の内容が残っている場合は失敗します(`--force` で置き換え)。新規アセットなら `ads asset create` の後、空のworkフォルダから始めれば十分です。

### 3.2 編集して書き出す(WIPは自動で溜まる)

Houdini Solarisで通常どおり作業し、USD ROPの **Output Processor に「ADS WIP Staging」** を選びます。保存先はworkフォルダ配下を指定してください(例: `$ADS_RESOLVER_WORKSPACE/char/hero/model/hero.usd`)。

- 実際の書き込みは `.ads-staging/<run-id>/...` へ自動で振り替わるため、**前回の書き出しを誰が開いていても保存は失敗しません**
- ROPの **Post-Render Script**(Python)に次の1行を設定します:

```python
from ads.houdini_wip import commit_staged; commit_staged()
```

これで書き出し完了ごとにWIP micro-versionが登録され、stagingは消えます。同一内容の書き出しは新しいWIPを作りません(dedup)。

登録時のスキャンはstatベースのハッシュメモ(`<source>/.ads-cache/hash-index.json`)で高速化されており、(mtime, size)が変わっていないファイルは読み直しません(実測: 120ファイル/120MBで初回2.3秒→変更なし再登録0.05秒)。メモはstoreへの書き込みでは信用されない設計なので、stale化してもstoreは壊れません。無効化する場合は `ADS_HASH_CACHE=0`。

Houdiniを使わない場合や手動で登録したい場合:

```powershell
ads wip add `
  --store D:\store `
  --category char `
  --asset-code hero `
  --department model `
  --source <書き出したフォルダ>
```

ストリームの確認は `ads wip list --store ... --category ... --asset-code ... --department ...`。

### 3.3 自分のWIPを確認する

自分のシーン・テストレンダからは `?v=wip` でWIP headを参照できます。

```text
ads://char/hero/model/hero.usd?v=wip
```

- wipは**自分のマシンのlocal storeに閉じています**。他人には見えず、pushもされません
- Resolverはwip解決を一切キャッシュしないため、書き出すたびに最新が返ります(開いているstageへの反映は手動reload)
- 他の人は同じURIの `current` を見ているので、作業中のデータが他人のシーンに混ざることはありません
- WebAppのインスペクタにも **WIP Stream** セクションがあり、書き出し履歴(seq・ファイル数・サイズ・日時)とHEADを一覧できます。行クリックで `?v=wip` URIをコピーできます

### 3.4 公開する(promote)

満足したらWIP headに番号を付けて公開します。**ファイルコピーは発生せず、即時に完了します。**

```powershell
ads publish promote `
  --store D:\store `
  --category char `
  --asset-code hero `
  --department model
```

- 戻り値の `promoted v003` が公開番号です。途中のWIPに戻して公開したい場合は `--wip-seq <N>` を指定します
- 公開済みversionと同一内容なら新規作成せず既存番号を返します
- 昇格したversionには `promoted_from`(由来WIPのseq)が記録されます
- **promoteは参照検証ゲートをデフォルトで実行します**: 他アセット参照は `ads://`、同一version内は相対参照OK。絶対パス・`file://`・version外/存在しないファイルへの参照があると昇格は拒否されます(意図的に通すなら `--no-validate`)
- WebAppからも昇格できます: インスペクタの WIP Stream で対象行の **PROMOTE** を押すと同じ検証ゲートを通って公開されます(拒否時は理由がステータスに表示されます)

検証だけを単体で行うこともできます(昇格前のWIPチェックや、登録前のフォルダ検査):

```powershell
ads publish validate --store D:\store --category char --asset-code hero --department model --wip
ads publish validate --source D:\delivery\hero_model_fix
```

### 3.5 currentの運用

通常、参照側は `current` を見ています(未設定ならlatest)。公開しただけではcurrentは動きません(currentが未pinならlatest追従で自動的に新versionが見えます)。

- WebApp: versionを選んで **Pin Current** / **Reset**(latest追従に戻す)
- CLI:

```powershell
ads current set   --store D:\store --category char --asset-code hero --department model --version 2
ads current reset --store D:\store --category char --asset-code hero --department model
ads current get   --store D:\store --category char --asset-code hero --department model
```

production showで安定させたい場合はpin、開発中はreset(latest追従)が基本です。

## 4. アセットを参照する(消費側)

### 4.1 URI規約

USDの参照には物理パスではなく `ads://` を書きます。

```text
ads://char/hero/model/hero.usd          currentを参照(推奨デフォルト)
ads://char/hero/model/hero.usd?v=2      整数pin(正準形。v002形式も受理)
ads://char/hero/model/hero.usd?v=latest 常に最新(production publishでは非推奨)
ads://char/hero/model/hero.usd?v=wip    自分のWIP head(local限定)
ads://hero/model/hero.usd               category省略形(一意に解決できる場合)
```

- ファイル名を省略すると `<asset_code>.usd` が既定です
- WebAppのインスペクタからアセット単位URI、Manifest一覧の行クリックで**ファイル単位URI**をコピーできます

### 4.2 解決のしくみ(知っておくと便利)

- local/autoの解決先はworkspaceの**不変キャッシュ**です: 合成形式(usd/usda/usdc/usdz/mtlx)は `.ads-cache/manifests/<hash>/`(version全体を実体化、相対参照が生きる)、それ以外の葉ファイル(テクスチャ・ボリューム・キャッシュ等)は `.ads-cache/sha256/`(要求した1ファイルだけ遅延取得)。workフォルダは読みません
- 事前のpullは不要です(解決時にstoreから自動実体化)。多数の依存を事前に温めたい場合のみ `uv run ads-deps <root.usd> --store ... --workspace ... --execute`
- Resolverキャッシュ: pin(`?v=2`)はセッション中ずっと、current/latestは30秒(`ADS_RESOLVER_CACHE_TTL_SECONDS`)、wipはキャッシュなし
- currentの切替を**即時に**開いているstageへ反映したい場合は、DCC内で `import ads.usd_refresh; ads.usd_refresh.refresh()`(シェルフ/パイプラインメニュー登録を推奨)。Resolverキャッシュを破棄して `ArNotice::ResolverChanged` を送るため、TTLを待たずに再解決されます

### 4.3 実体フォルダが必要なとき

レンダーファームへの引き渡しや納品など、Resolver外でファイルが必要な場合だけ `checkout` を使います。

```powershell
ads checkout `
  --store D:\store `
  --category char --asset-code hero --department model `
  --version 2 `
  D:\delivery\hero_model_v002
```

出力先は任意です。`--version` 省略でcurrent、`--latest` も指定できます。

## 5. リモート運用

### Remote store only(推奨デフォルト)

作業PCにstoreを持たず、central serveに対してWebApp閲覧・remote mode Resolver・`/api/pull` でのworkフォルダ種まきを行います。追加の同期コマンドは不要です。

### Local + remote store

回線が細い環境では、local storeをcacheとして挟みます。

```powershell
# remoteから取得(metadata+objects)
ads sync  --server http://ads-server:8787 --auth-token <token> --profile main `
          --store D:\local-store --category char --asset-code hero --department model --latest

# 単一versionの取得(+workフォルダ種まき)
ads fetch --server ... --auth-token ... --profile main --store D:\local-store `
          --category char --asset-code hero --department model --latest `
          --workspace D:\workspace --materialize

# localでpromoteしたversionをremoteへ公開
ads push  --server ... --auth-token ... --profile main --store D:\local-store `
          --category char --asset-code hero --department model
```

**WIPはpush/sync/fetchの対象外です**(local専用)。共有したいものはpromoteしてからpushします。

## 6. メンテナンス(TD / 管理者)

### Garbage Collection(定期実行を推奨)

WIPは書き出しごとにobjectを生むため、GCが必須です。週次程度で:

```powershell
ads gc --store D:\store --dry-run     # まず削除対象を確認
ads gc --store D:\store               # retention 20 / 猶予24hで実行
```

- 保持されるもの: 全publish version、thumbnail、部門ごとのWIP直近20件(`--retention`)
- 作成から24時間以内(`--grace-hours`)のobjectは並行書き込み保護のため削除されません

中央サーバ運用では、serve稼働中のstoreに対してAPIから実行できます(サーバ内で他の書き込みと直列化されます):

```powershell
curl.exe -X POST "http://server:8787/api/gc" `
  -H "Authorization: Bearer <token>" -H "Content-Type: application/json" `
  -d '{"profile":"main","dry_run":true}'
```

`retention` / `grace_hours` / `dry_run` は省略可(既定はCLIと同じ 20 / 24 / false)。

### Workspaceキャッシュの掃除

`.ads-cache`(manifest view+blob)はWIP運用で書き出しごとに成長します。各作業マシンで定期的に掃除してください。

```powershell
ads cache gc --store D:\store --workspace D:\workspace [--staging-hours 24] [--dry-run]
```

- 保持されるもの: 各departmentの **latest・明示current・wip head** が参照するviewとblob
- それ以外は削除されます。**キャッシュはstoreから再構築可能**なので常に安全で、pinした旧version(`?v=N`)も次のresolveで自動的に再実体化されます
- `.ads-staging` に残った古いrun(クラッシュしたROPの残骸)も `--staging-hours` より古ければ削除されます
- storeはread-only openで読むだけなので、**serve起動中でも実行できます**

### 整合性検査・サムネイル

```powershell
ads verify --store D:\store
ads thumbnail set --store D:\store --category char --asset-code hero --department model --version 2 D:\thumbs\hero.png
```

`ads serve` 稼働中の store に対して書き込み系 CLI を実行する場合は、RocksDB を直接開かず Web API 経由にします。

```powershell
ads add --server http://server:8787 --auth-token <token> --profile default `
  --category char --asset-code hero --department model --source D:\publish\hero

ads thumbnail set --server http://server:8787 --auth-token <token> --profile default `
  --category char --asset-code hero --department model --version 1 D:\thumbs\hero.png
```

サムネイルはWebAppのインスペクタからもアップロードできます。バックアップはstoreの `db/` と `objects/` を両方対象にしてください。

## 7. トラブルシューティング

| 症状 | 原因と対処 |
|---|---|
| `work folder exists and is not empty` | workフォルダに編集中データがある。意図的に置き換えるならForce(WebAppはチェックボックス、CLIは `--force`) |
| `Failed to create lock file ... LOCK` | serveがstoreを開いている状態で直接storeを開く**書き込み系**コマンド。serve稼働中は `ads add --server ...` / `ads thumbnail set --server ...` などWeb API経由で実行(読み取り系はread-only openで共存可能) |
| `unsupported store schema version` | 旧schemaのstore。開発中storeは再作成が前提(`ads init` し直し) |
| `wip versions are local-only` | `?v=wip` をremote modeで解決しようとした。wipは自分のlocal storeでのみ有効 |
| `department has no wip versions` | そのdepartmentにWIPが未登録。書き出し(またはwip add)が先 |
| 書き出したのにviewerが古い | 開いているstageは自動では再解決されない。`import ads.usd_refresh; ads.usd_refresh.refresh()` で即時再解決(またはreload)。放置でもcurrent/latestの切替は最大30秒(Resolver TTL)で反映 |
| Resolverの動きを調べたい | `ADS_RESOLVER_DEBUG=1` と `ADS_RESOLVER_LOG_FILE=%TEMP%\ads_resolver.log` を設定 |
| `version must be the next version` | `add --version` の明示番号が飛んでいる。`--version` を省略すれば自動採番 |

## 8. コマンド早見表

| やりたいこと | コマンド |
|---|---|
| store作成 | `ads init <store> [--remote-base-url <url>]` |
| サーバ起動 | `ads serve --bind <addr> --auth-token <t> --profile <n>=<store>::<ws>` |
| アセット作成 | `ads asset create --store --workspace --category --asset-code` |
| workフォルダ種まき | `ads materialize --store --workspace --category --asset-code --department [--version N\|--latest] [--force]` |
| WIP登録 | `ads wip add --store --category --asset-code --department --source <dir>` |
| WIP一覧 | `ads wip list --store --category --asset-code --department` |
| 公開(昇格) | `ads publish promote --store --category --asset-code --department [--wip-seq N]` |
| 直接登録 | `ads add --store --category --asset-code --department --source <dir> [--version N]` / serve稼働中は `ads add --server <url> --auth-token <t> --profile <p> ...` |
| current操作 | `ads current set/get/reset/status ...` |
| 実体取り出し | `ads checkout --store --category --asset-code --department [--version N] <dest>` |
| URI解決 | `ads resolve --store --workspace --mode local\|remote\|auto <ads://...>` |
| 一覧/詳細 | `ads list` / `ads info` |
| GC(store) | `ads gc --store [--retention N] [--grace-hours H] [--dry-run]` |
| GC(中央サーバ) | `POST /api/gc` body `{"profile":"main","dry_run":true}` |
| GC(workspaceキャッシュ) | `ads cache gc --store --workspace [--staging-hours H] [--dry-run]` |
| Resolver即時更新(DCC内) | `import ads.usd_refresh; ads.usd_refresh.refresh()` |
| 整合性検査 | `ads verify --store` |
| リモート同期 | `ads fetch` / `ads sync` / `ads push` |
| 参照検証 | `ads publish validate --store ... [--version N\|--wip\|--wip-seq N]` または `--source <dir>` |

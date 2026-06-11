# ADS仕様改訂: WIP / Publishモデル

対象: ADS schema version 8
作成日: 2026-06-11
Status: Implemented(2026-06-11、全3段階実装済み)

実装ノート: 以下の緩和策・周辺項目は本体実装から意図的に切り離し、未実装のまま残しています。

- (path, mtime, size) → sha256 のハッシュ高速化サイドテーブル
- department別のWIP自動登録ポリシー(off設定)
- ArNoticeによるstage自動refresh(現状は手動reload)

実装後の改訂: 解決形状の分類を「texture拡張子/texture department」から
「合成形式(usd/usda/usdc/usdz/mtlx)→ manifest view / それ以外の葉 →
flat blob cache(遅延取得)」へ一般化。texture拡張子リストと
department名規約は撤廃し、葉ファイルのautoモードにもremoteフォールバックを
追加(本文のtexture記述は決定当時の表現)。

## 背景

ADSはusdcファイルが上書きできない問題を起点に設計され、その回避策として
物理versionフォルダ(`category/asset_code/department/v###`)を中核に据えてきた
(schema version 7、`WHITEPAPER.ja.md` 参照)。

その後の実装で以下が確定した。

- content-addressed object storeは構造的に上書きが発生しない
  (同一内容=同一hash、既存objectは再書き込みしない)。
- remote modeのResolverはversionフォルダを一切参照しない。
- textureはlocal/auto modeでも `.ads-cache/sha256/<hash>.<ext>` へ解決され、
  versionフォルダを参照しない。
- 登録前のWIPイテレーション(同一versionフォルダ内への反復書き出し)は
  versionフォルダでは保護されておらず、当初のロック問題がそのまま残っている。

つまり「上書きからの保護」の実体は既にCASへ移っており、versionフォルダは
local modeのUSD layer解決と、人間向けの物理パス提供だけを担う過渡期の
仕組みになっている。本仕様はこれを整理する。

## 決定事項

### D1. versionフォルダの廃止

workspaceの標準レイアウト(`v###` フォルダ)を契約から外す。
workspaceは純粋な作業領域(scratch)となり、ADSはそのレイアウトに関与しない。
versionの実体フォルダが必要な場合は `checkout` がオンデマンドで実体化する。

### D2. WIP / Publishの2層化

「version」を性質の異なる2層に分離する。

- **WIP層(micro-version)**: 書き出し1回 = 1 micro-version。
  自動登録、local storeのみ、GC対象、明示指定でのみ解決される。
- **Publish層(named version)**: 従来のversion。密な整数列、
  current/latest、remote push、将来のgovernance(approval等)はこの層に付く。

publishはWIP micro-versionへの番号付与(メタデータ昇格)であり、
ファイルコピーは発生しない。

### D3. version表現の整数化

`v###` 文字列を廃止し、正準表現を整数(u32)とする。
`v###` はUI表示層の整形としてのみ残してよい。

## 設計原則(改訂)

旧 "Version Folder First" を置き換え、以下を原則とする。

1. **Store Is Canonical**(継続) — 正規データはstore(metadata + objects)。
2. **Never Overwrite** — すべての書き出しは一意パスへ行う。
   登録済みバイト列の上書きはシステムのどこにも存在しない。
3. **Cache Is Immutable** — 読み込みは不変なキャッシュ実体
   (object blob / manifest view)から行う。
4. **Workspace Is Scratch** — workspaceは契約外。ADSはレイアウトを要求しない。
5. **WIP Stream + Promoted Publish** — 頻繁な書き出しはWIPストリームに吸収し、
   公開はpromotionで行う。

## データモデル

### VersionId(整数化)

- 正準表現: `u32`、1始まり。0は不正。
- 受理(パース): `12` / `v12` / `v012`(先頭ゼロ可)。寛容パースは恒久仕様とする。
  published USD内に焼き込まれた `?v=v003` 形式のpinned参照を将来も読めるようにするため。
- 出力(シリアライズ): 整数のみ。JSON APIはnumber型、URIは `?v=12`、
  CLIは `--version 12`。
- 表示: WebApp / Houdiniパネル等のUI層が `v012` に整形することは許可するが、
  プロトコル・ストレージには持ち込まない。

### WIP micro-version

- 単位: department(category + asset_code + department)。
- 識別: department内の単調増加シーケンス(u64)+ manifest hash。
  シーケンスは内部識別子であり、ユーザー向け番号ではない。
- 実体: publish versionと同一のmanifest機構を共有する。
  manifest / objectsの保存・dedupは従来通り。
- 保存先: local storeのみ。`push` / `sync` の対象外。
  これによりWIPにユーザー概念は不要(マシン = 作業者に閉じる)。
- `wip head`: department単位の単一ポインタ。最新micro-versionを指す。
  複数WIPストリームの並行はMVPではスコープ外(未決事項参照)。

### Publish version

- 密な整数列(1, 2, 3, ...)。promotion時に採番する。
  WIPは番号を消費しないため欠番は発生しない。
- `current` / `latest` の意味は従来通り(currentは明示ポインタ、
  未設定時はlatestと同等)。

### VersionRecord(変更点)

従来のフィールドに加え、以下を記録する。

- `promoted_from`: 昇格元WIPシーケンス(WIP経由の場合)
- `source_path` は任意パス可(標準versionフォルダ要求の撤廃)

### RocksDB key設計(v8)

versionをキーに含める箇所はすべて固定幅10桁ゼロ埋め十進
(WIPシーケンスは20桁)とし、辞書順 = 数値順を保証する。
これによりprefix seekによる範囲取得が可能になる
(従来の可変幅 `v###` はv1000以降で辞書順が壊れるため全件走査+再ソートが必要だった)。

```text
meta/schema_version                                          = 8
asset/<category>/<asset_code>
version/<category>/<asset_code>/<department>/<%010d>
wip/<category>/<asset_code>/<department>/<%020d>
wip_head/<category>/<asset_code>/<department>
latest/<category>/<asset_code>/<department>
current/<category>/<asset_code>/<department>
thumbnail/<category>/<asset_code>/<department>/<%010d>
manifest/<manifest_hash>
manifest_index/<category>/<asset_code>/<department>/<manifest_hash>
```

### Migration

なし。従来方針通り、開発中storeは再作成する(schema 7 → 8)。
published USD内のpinned URI(`?v=v003`)は寛容パースで吸収されるため
データ作り直しは不要。

## キャッシュ仕様

読み込みの実体はworkspace配下のキャッシュに置く。2層構成とする。

```text
<workspace>/.ads-cache/
  sha256/<prefix>/<hash>.<ext>            # object blob(従来のtexture cacheを継続)
  manifests/<manifest_hash>/<relpath>     # manifest view(フォルダ形状の実体化)
```

- **object blob**: content-addressed。従来のtexture cacheと同一機構。
- **manifest view**: あるmanifestのフォルダ構造を相対パスのまま実体化したもの。
  各ファイルはobject blobへのhardlink(失敗時はコピーにfallback)とし、
  version間で内容が同じファイルはディスクを消費しない。
  manifest hashが変わらない限り不変であり、上書きは発生しない。

USD layerの解決先はmanifest viewのパスとする。これにより `ads://` 化されていない
相対参照が残っているデータでも構成が壊れない。textureは従来通り
flat blob pathへ解決してよい(同一blobの別名hardlinkにすぎない)。

## 解決(Resolver)仕様

### versionセレクタ

```text
?v=<int>   明示version(pin)
current    currentポインタ(default、未設定時はlatest)
latest     最大publish version
wip        wip head(新規)
```

### キャッシュポリシー

解決結果のキャッシュ可否はセレクタの可変性で決まる。

- `?v=<int>`: manifestは不変 → 解決結果を恒久キャッシュしてよい。
- `current` / `latest`: 可変ポインタ → TTL付きキャッシュ(default 30秒)
  または無効化通知が必要。**恒久キャッシュは禁止**(現行C++ Resolverの
  無期限staticキャッシュはこの仕様で改修対象となる)。
- `wip`: 書き出しごとに動く → **キャッシュ禁止**。

### モード

- `local`: local store → manifest view実体化 → ローカルパスを返す。
  versionフォルダへの解決は廃止。
- `remote`: central API `/api/resolve` → object URL(従来通り)。
- `auto`: manifest view → local store → remoteの順。

`wip` セレクタはlocal storeにのみ存在するため、remote modeでは解決できない。

## 書き出し(WIP)仕様

### Output processorによるstaging振り替え

USD ROPの保存パスを一意なstagingパスへ透過的に振り替える。

```text
書き出し要求: <workspace>/char/hero/model/hero.usd
実際の書き込み: <workspace>/.ads-staging/<run-id>/hero.usd
```

1. ROP実行ごとに一意な `<run-id>` を生成し、全保存パスを振り替える。
2. 書き出し完了後、stagingからmanifestを構築しobjectsをlocal storeへ登録、
   micro-versionを作成して `wip head` を更新する。
3. stagingを削除する(実体はCASへ移っている)。

書き込み先が毎回一意であるため、viewer / husk / 別セッションが旧ファイルを
mmapで掴んでいても書き出しは失敗しない。**当初のusdcロック問題は
WIPイテレーションを含めて構造的に消滅する。**

### ハッシュコストの緩和

- `(絶対パス, mtime, size) → sha256` のサイドテーブルをlocal storeに持ち、
  未変更ファイルの再ハッシュを省略する。
- department単位のポリシーでWIP自動登録をoffにできる
  (GBクラスのfxキャッシュ等。offの場合は従来同様の直接パス書き出しとなり、
  publish時にのみ登録される)。

### 非USD ROP経路

SOPレベルのキャッシュ等、output processorを通らない書き出しは
本仕様の対象外とする。必要な場合は `ads wip add <path>` で明示登録できる。

## Publish仕様

```powershell
ads publish promote `
  --store D:\store `
  --category char `
  --asset-code hero `
  --department model
```

- wip head(または `--wip-seq` / `--manifest` で明示指定)のmanifestに
  次のpublish番号を採番し、version recordを作成する。
- objectsは登録済みのため**ファイルコピーは発生しない**。メタデータ書き込みのみで
  アトミックに完了する。
- `publish validate` は昇格前の検査として `publish promote` にデフォルトで配線される
  (`--no-validate` で回避可)。検証対象はstore内のmanifest(version / WIP head /
  WIP seq指定)または `--source` の任意フォルダ。参照ポリシーはv8で改訂:
  他アセット参照は `ads://`、**同一manifest内に解決できる相対参照は許可**
  (manifest viewが相対レイアウトを保持するため)。絶対パス・`file://`・
  manifest外/不在ファイルへの参照はエラー。public root機構と
  `publish register` は廃止(直接登録は `add --source`)。
- `fetch` / `sync` / `push` の対象はpublish層のみ。

## GC仕様(必須機能への昇格)

WIPが書き出しごとにobjectを生むため、GCはoptionalではなくなる。

- roots:
  - 全publish versionのmanifestが参照するobject
  - 各departmentのWIP直近N件(default: 20、設定可能)
  - 猶予期間内(default: 24時間)に作成されたobject(書き込み競合保護)
- mark(manifest走査)& sweep(objects走査)方式。
- `ads gc --store <path> [--dry-run]` を追加する。
- `ads verify` はGC後の整合検査として従来通り機能する。

## CLI / API変更一覧

| 区分 | コマンド/API | 内容 |
|------|-------------|------|
| 廃止 | `new-version` | versionフォルダ作成が不要になるため |
| 変更 | `add` | 任意パスからの登録を許可(標準versionフォルダ要求の撤廃)。WIP経由しない直接publishとして残す |
| 変更 | `pull` / `restore` | manifest viewへの実体化+`checkout` に整理。workspace標準フォルダという概念が消えるため、明示の出力先を取る `checkout` に寄せる |
| 変更 | `resolve` | セレクタに `wip` を追加。local解決先はmanifest view |
| 追加 | `publish promote` | WIP → publish昇格 |
| 追加 | `wip add` / `wip list` | 明示WIP登録、WIP一覧 |
| 追加 | `gc` | object garbage collection |
| 継続 | `checkout` | フォルダ実体化の唯一の手段。出力フォルダ名にUI整形(`v012`)を使ってよい |
| 継続 | `current` / `verify` / `serve` / `fetch` / `sync` / `push` | 意味は従来通り(対象はpublish層) |

API:

- JSONのversionフィールドはすべてnumber型に変更する。
- `/api/resolve` は `v=12 | current | latest | wip` を受理する
  (`v=v012` も寛容パースで受理)。
- `PUT /api/version` 等のpush系APIはpublish層のみを扱う(従来通り)。

## Houdini統合

- **ADS Managed Publish**: 撤去(versionフォルダ廃止によりv###パス解析が成立しないため)。
  公開経路はWIP staging + promoteに一本化。
- **WIP作業セッション**: Resolver環境変数にwipセレクタ指定を追加し、
  作業者自身のマシンでのみ `ads://...` がwip headへ解決されるようにする。
  他のマシン・production参照は従来通りcurrentを見る。
- **stage refresh**: 解決先が変わっても開いているstageは自動では再解決されない。
  MVPでは手動reloadを前提とし、ArNotice連携による自動refreshは将来項目とする。
- **asset catalog datasource**: publish層のみを表示する。WIPの表示は将来オプション。

## 未決事項(本仕様のスコープ外)

- 複数WIPストリームの並行(同一departmentを複数タスクで同時作業)。
  MVPは単一wip head。必要になった時点でstream識別子を導入する。
- WIPのリモート共有(レビュー用途)。WIPをpush可能にするとユーザー概念が
  必要になるため、Phase 4(governance)と合わせて検討する。
- stageの自動refresh(ArNotice / Houdini側イベント連携)。
- remote object のrange read / streaming(従来からの将来項目)。

## 実装順序

依存関係に基づき3段階に分ける。各段階は独立してリリース可能。

1. **読み込み側の切り離し**: VersionId整数化(キーv8化を含む)、
   manifest viewキャッシュ、resolverのlocal解決先変更、
   resolverキャッシュポリシー(pinned恒久 / current TTL / wipなし)。
   この時点でversionフォルダはload対象から外れる。
2. **書き込み側のWIP化**: output processorのstaging振り替え、
   WIP micro-version自動登録、wip head、`?v=wip` 解決、GC。
   この時点でロック問題が消滅する。
3. **publishの昇格化と整理**: `publish promote`、`new-version` 廃止、
   `pull`/`restore` の `checkout` への整理、WHITEPAPERの設計原則章の改訂。

## 結論

versionフォルダは「上書きできないusdc」への過渡的な回答であり、
その役割はcontent-addressed storeとmanifest機構に既に移っている。
本仕様はそれを正面から認め、workspaceをscratchに格下げし、
頻繁なローカル書き出しをWIPストリームとして一級市民化し、
publishをコピーゼロの昇格操作に置き換える。
versionは整数となり、`v###` はUIの整形にすぎなくなる。

これにより、当初のロック問題はWIPイテレーションを含めて構造的に解消され、
`new-version` のlatestコピー・`pull --force` の競合判定・preflight必須運用が
不要になる。代償として、GCとresolverキャッシュ無効化が必須機能となる。

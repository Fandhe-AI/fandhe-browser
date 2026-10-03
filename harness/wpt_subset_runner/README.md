# wpt_subset_runner

WPT（web-platform-tests）サブセット実行基盤の入力となる選定定義。TASK-101.1（Issue #273）で
導入。対応するビヘイビア ID は `PLUG-10`、マイルストーンは MS-8。ID から SSOT
（`docs/spec` の `04-behavior/`）を参照すること
（[spec-reference](../../.claude/rules/spec-reference.md)）。

`wpt-subset.json` は PoC-16 で選定した 257 件を移植した設定データで、後続のランナー
（#554・TASK-101.2.2）と集計・レポート（#276・#277）が読み込む。

## 由来

- 移植元: spec の `03-poc/browser-behavior-profile/proto/wpt-subset.json`
  （spec コミット `c7f2995d21d94339ff8ffbb3b66fae4953043c18`）。出自は JSON の `source` に記録している
- 選定方法: PoC-16 の plan に基づき、各ディレクトリからアルファベット順に先頭 N 件
  （`support/`・`-ref`・`-notref` は除外）
- `harness` 分類（`testharness` / `reftest` / `other`）は選定スクリプトの出力ではなく、後から付与されたもの
- PoC 時点の `executionStatus`（JS 未実装による実行不能の記述）は現状と異なるため移植していない。
  実行状態・合否・実行不能理由はこのファイルではなくレポート（#276・#277）が出力する
- reftest・other を実行対象外とする方針は人間判断（#279）の事項のため、定義は 257 件すべてを中立に保つ

## スキーマ契約

| フィールド | 型 | 必須 | 制約 |
| ---------- | -- | ---- | ---- |
| `schemaVersion` | 整数 | 必須 | 現在 `1` |
| `source` | オブジェクト | 必須 | 出自情報。`wptRevision` は文字列または `null` |
| `totalSelected` | 整数 | 必須 | `subset` の件数と一致 |
| `subset[].file` | 文字列 | 必須 | WPT ルートからの相対パス。`^[A-Za-z0-9._/-]+$`、`..` セグメント禁止、先頭 `/` 禁止、`<dir>/` で始まる、配列内で一意 |
| `subset[].dir` | 文字列 | 必須 | `perDirectorySummary[].dir` のいずれか |
| `subset[].mappedFeatures` | 文字列配列 | 必須 | web-features の feature ID |
| `subset[].harness` | 列挙 | 必須 | `testharness` / `reftest` / `other` |
| `perDirectorySummary[]` | オブジェクト配列 | 必須 | `dir`・`mappedFeatures`・`requested`・`available`・`picked`・`reason`。`picked` は `subset` 内のそのディレクトリの件数と一致 |
| `harnessBreakdown` | オブジェクト | 必須 | 3 種別の件数。`subset` の集計と一致 |

- 上限: エントリ数 10000 件以下・ファイルサイズ 1 MiB 以下。読み込み側は上限を確認してから確保する
- 消費側（#554・#277）は読み込み時にこの契約を再検証し、違反したら fail-closed（エラー）にする。
  `file` は WPT クローンのルートと結合する前に再検証し、正規化後もルート配下であることを確認する

## #554 への申し送り

- WPT のリビジョンが未記録（`source.wptRevision` は `null`）。ランナー実装時に WPT のコミットを
  固定して埋めること。取得元はハードコードした公式リポジトリに限定する
- 固定したリビジョンに該当ファイルが無い場合は、独自の状態（例: missing）として記録し、黙って選び直さない

## コミットしないもの

WPT のクローン・テスト内容は `.gitignore` で除外する（`wpt-work/`・`wpt/`）。

## ライセンス

WPT は BSD-3-Clause。本ディレクトリはファイルパスの一覧だけでテストコードを含まないため
帰属表示は不要とする（PoC-16 のデータライセンス整理を引き継ぐ。`NOTICE` は TASK-103・`OSS-7` が担当）。

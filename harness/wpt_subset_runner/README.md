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

## グローバル環境と結果の受け渡し

TASK-101.2.1（Issue #553・`PLUG-10`・MS-8）で追加した lib crate `wpt-subset-runner`。
`fandhe-browser-core` の `JsRuntime` だけに依存し（js crate へは直接依存しない）、testharness.js を
JS エンジン上で動かす最小の環境と、サブテスト結果を Rust 側で受け取る経路を提供する。

呼び出し順の契約（#554 が従う）:

1. `install_testharness_globals`: ネイティブ関数 `__fandheWptReportResult`・`__fandheWptReportCompletion` を
   注入し、`self` を `globalThis` として定義する（`document`・`window`・`setTimeout` は定義しない）
2. testharness.js を評価する
3. `attach_result_reporter`: `add_result_callback` / `add_completion_callback` へアダプタを登録する
4. テストファイルを評価する
5. `ResultCollector::take` で結果を取り出す（不正な通知が 1 件でもあれば `Err`）

ステータス対応（testharness.js の値）:

| 種別 | 値 |
| ---- | -- |
| サブテスト | PASS=0・FAIL=1・TIMEOUT=2・NOTRUN=3・PRECONDITION_FAILED=4 |
| ハーネス | OK=0・ERROR=1・TIMEOUT=2・PRECONDITION_FAILED=3 |

上限: サブテスト 10000 件・名前 8 KiB（超過は違反）・メッセージ 16 KiB（超過は文字境界で切り詰め、
`message_truncated` で明示）。

テストは `cargo test -p wpt-subset-runner`（エンジンなしの分岐）、`--features js-v8`、`--features js-boa` で
構成ごとに実行する。結合テストは WPT のコードではなく本リポで書いた偽 testharness
（`tests/fixtures/fake_testharness.js`）を使う。

## 制限（簡易実装。実装済みを装わない）

- `document`・`window` が無いため、DOM を要するテストは testharness.js の Shell 環境で失敗する
- `setTimeout`・イベントループが無く、`async_test`・`promise_test`・`step_timeout` 系は未対応
- microtask の実行はエンジン依存（V8 は microtask checkpoint を保証せず、boa の `eval` は job を実行しない）。
  completion の通知時期・有無は保証せず、確実な経路は同期 `test()` の result 通知だけ
- 子プロセスの再起動時は、注入関数は再登録されるが prelude・testharness.js・アダプタの状態は失われる
- 評価 1 回あたりの上限（スクリプト 1 MiB・実行 2 秒）は js crate の固定値に従う
- `JsValue` はスカラーのみのため、受け渡しは文字列・数値に限る

## #554 への申し送り

- WPT のリビジョンが未記録（`source.wptRevision` は `null`）。ランナー実装時に WPT のコミットを
  固定して埋めること。取得元はハードコードした公式リポジトリに限定する
- 固定したリビジョンに該当ファイルが無い場合は、独自の状態（例: missing）として記録し、黙って選び直さない
- ファイルごとに新しい `JsRuntime` を作る（エンジン再起動で状態が失われるため、ファイル単位の失敗として扱う）
- completion に依存せず、result 通知を正とする
- 本 crate の結合テストは偽 testharness を使う。固定リビジョンの実物の testharness.js での動作確認は #554 で行う

## コミットしないもの

WPT のクローン・テスト内容は `.gitignore` で除外する（`wpt-work/`・`wpt/`）。

## ライセンス

WPT は BSD-3-Clause。本ディレクトリはファイルパスの一覧だけでテストコードを含まないため
帰属表示は不要とする（PoC-16 のデータライセンス整理を引き継ぐ。`NOTICE` は TASK-103・`OSS-7` が担当）。

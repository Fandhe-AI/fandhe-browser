# wpt_subset_runner

WPT（web-platform-tests）サブセット実行基盤の入力となる選定定義。TASK-101.1（Issue #273）で
導入。対応するビヘイビア ID は `PLUG-10`、マイルストーンは MS-8。ID から SSOT
（`docs/spec` の `04-behavior/`）を参照すること
（[spec-reference](../../.claude/rules/spec-reference.md)）。

`wpt-subset.json` は PoC-16 で選定した 257 件を移植した設定データで、ランナー
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

## 取得とランナー（TASK-101.2.2・Issue #554）

`wpt-subset.json` の `source.wptRevision` に WPT のリビジョン（`74ca910926d76710943f2a8798817102f69d6e40`）を固定した。
リビジョンの正本は JSON で、スクリプトに二重定義しない。

```bash
bash harness/wpt_subset_runner/fetch-wpt.sh   # jq・git・ネットワークが必要。CI には組み込まない
```

- 取得元は公式 `web-platform-tests/wpt` にハードコード（引数・環境変数で変更不可）。`resources/`・`common/`・
  `css/support/` と選定ディレクトリだけを sparse checkout する（出力先は `wpt-work/`。`WPT_WORK_DIR` で変更可）
- 既存の `wpt-work/wpt` は、symlink でなく・`.git` が実体ディレクトリで・自身がトップレベルで・origin が公式 URL と
  完全一致する場合だけ再利用し、違えば exit 2（fail-closed）。fetch も公式 URL を明示して行う。
  検証は `bash harness/wpt_subset_runner/fetch-wpt-self-test.sh`（ネットワーク不要）で確認する
- `wpt-work/subset.tsv`（1 行 `<harness>\t<file>`）を書き出す。Rust 側は JSON を読まない（新規依存を避けるため）
- `file`・`dir` はパス規則（許可文字・`..` 禁止）で検証してから git へ渡す。スキーマ全体の検証は #278 の担当

ランナー（`wpt_subset_runner::runner`）の流れ: `parse_subset_tsv` → `run_entry` / `run_subset`。
ファイルごとに新しい `JsRuntime` を作り、HTML の `<script>` を文書順に評価する
（`testharness.js` の評価直後に `attach_result_reporter`。`testharnessreport.js` は読み込まない）。
`src` は `/` 始まりならルート基準、それ以外はテストファイル基準で字句的に解決し、canonicalize 後に
ルート配下であることを確認する。URL 形式の `src` は取得せず拒否する。

結果分類（`FileOutcome`。集計はしない）:

| 分類 | 意味 |
| ---- | ---- |
| `Skipped` | testharness 以外（reftest・other）。読まずにスキップ |
| `Missing` | 固定リビジョンにファイルが無い（黙って選び直さない） |
| `ReadFailed` / `TooLarge` / `HtmlParseFailed` | 読み込み・サイズ上限（1 MiB）・パースの失敗 |
| `HarnessNotReferenced` | testharness 種別なのに `/resources/testharness.js` を読み込まない |
| `HarnessLoadFailed` | testharness.js の評価またはアダプタ登録の失敗 |
| `SupportScriptMissing` / `ScriptRejected` | 外部スクリプトが無い / 参照規則違反 |
| `LimitExceeded` | 1 ファイルの総量上限超過（`RunLimits`。手順 64 件・合計 8 MiB・総時間 30 秒。件数と合計サイズは実行前に検証。根拠は `runner.rs` の定数コメント） |
| `ScriptFailed` | テスト側スクリプトの評価失敗（そのファイルで打ち切る） |
| `UnsupportedScript` | `type="module"` を含む（評価できないため実行しない。classic だけで Pass にしない） |
| `EngineUnavailable` | JS ランタイムを作れない・エンジン未指定（`None` は JS 無効。既定エンジンへは落とさない） |
| `CollectFailed` | JS 側から不正な通知があった |
| `Completed` | 実行完了。`verdict` は `Pass`（1 件以上で全 PASS かつ completion が `OK` で届いた）・`Fail`（PASS 以外がある・completion が `OK` 以外）・`Incomplete`（全 PASS だが completion が無く未完了テストを除外できない。Pass にしない）・`NoResults`（0 件は Pass にしない） |

completion が無いと未完了の async_test / promise_test を判別できないため `Pass` にはしない（`Incomplete`）。届いて `OK` 以外なら verdict は `Fail`。

実物の testharness.js での動作確認（リビジョン固定時点。V8 / boa。DOM・タイマーが無い環境のまま）:
testharness 152 件は `Completed/Fail` 53 / 46・`Completed/NoResults` 3 / 10・`ScriptFailed` 93・`Missing` 1・
`SupportScriptMissing` 2（V8 / boa の順。`Pass` は 0 件）。reftest・other の 105 件は `Skipped`。
実物の testharness.js は読み込めるが、`window`・`document` を要するテストは失敗する
（偽の DOM で通さない方針。合格率は #276 が扱う）。

テストは偽の WPT ツリー（一時ディレクトリ）で `tests/runner_subset.rs` が検証する。

## 実行不能項目の記録（TASK-101.5・#277・PLUG-10・MS-8）

`report` モジュールが、実行できない 2 群の理由と確度を JSON セクションとして書き出す
（`UnrunnableReport::to_json`）。#276 のレポートが `"unrunnable"` キーへ埋め込む前提で、
このセクションのキー名は固定する。

| 理由コード | 対象 | 確度（`basis`） | 件数 |
| ---------- | ---- | --------------- | ---- |
| `reftest-comparison-not-implemented` | reftest（本ハーネスが描画比較を未実装。同一ブラウザーでの比較自体は原理上可能） | `confirmed` | 88 |
| `unverified-likely-unrunnable` | other（内容未検証・実行できない可能性が高い） | `speculative` | 17 |

- 出力は `schemaVersion`・`total`・`byReason[]`（`reason`・`basis`・`harness`・`description`・`count`・`files`）。0 件の理由も必ず出す
- ファイルは昇順で、出力は決定的。件数上限は 10,000
- 記録するのは理由だけで、対象外にする方針の承認は人間担当の #279（TASK-101.h1）が決める
- JSON は手書きで出力し、`wpt-subset.json` はライブラリでは読まない（依存を追加しないため。テストだけが読む）

## コミットしないもの

WPT のクローン・テスト内容は `.gitignore` で除外する（`wpt-work/`・`wpt/`）。

## ライセンス

WPT は BSD-3-Clause。本ディレクトリはファイルパスの一覧だけでテストコードを含まないため
帰属表示は不要とする（PoC-16 のデータライセンス整理を引き継ぐ。`NOTICE` は TASK-103・`OSS-7` が担当）。

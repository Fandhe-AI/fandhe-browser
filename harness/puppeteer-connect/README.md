# puppeteer-connect

Puppeteer 接続試験の実行基盤（TASK-45.1・Issue #480・ビヘイビア `CDP-3`・MS-4）。
`puppeteer.connect({browserWSEndpoint})` 相当の接続フローを、実クライアントで試すための土台。

## 役割分担

| 範囲 | Issue |
| ---- | ----- |
| 実行基盤（サーバー起動・スクリプト実行・結果の構造化回収） | #480（本ディレクトリ） |
| 接続後の `newPage`・`goto`・セレクタ取得の段階別到達結果の回収（`stages` 契約・`stages.mjs`・オフライン自己テスト） | #481（TASK-45.2。本ディレクトリ） |
| 実 Puppeteer での実行・実測値の固定（`puppeteer-core` 導入承認後） | #481 の残り（承認待ち） |
| 結果レポート | #482（TASK-45.3） |

## 構成

| ファイル | 内容 |
| -------- | ---- |
| `connect.mjs` | `puppeteer-core` で段階実行し、結果を 1 行で報告するスクリプト |
| `stages.mjs` | 段階実行の純粋ロジック（クライアント注入可）。connect → newPage → goto → selector → disconnect |
| `contract-sample.mjs` | Rust 側パーサーとの契約テスト用に `stages.mjs` の出力を再現する（`tests/puppeteer_contract.rs` から実行。要 node） |
| `self-test.mjs` / `self-test.sh` | `stages.mjs` と `connect.mjs` のオフライン自己テスト（`make check-puppeteer-connect`。要 node） |
| `crates/fandhe-browser-cdp/tests/script_harness/` | 共有基盤。`runner.rs`（子プロセス実行・結果解析。契約テストも単独で取り込む）・`server.rs`（サーバー起動）・`preflight.rs`（導入確認） |
| `crates/fandhe-browser-cdp/tests/puppeteer_connect.rs` | 基盤の自己テスト（unix のみ。`cargo test --workspace` で常に実行。node 不要） |
| `crates/fandhe-browser-cdp/tests/puppeteer_contract.rs` | `stages.mjs` と Rust パーサーの契約テスト 1 件（3 OS。要 node。`#[ignore]` のため `self-test.sh` が `--ignored` で実行し、0 件実行は fail） |

## スクリプトとの契約

- WS エンドポイントは環境変数 `FANDHE_CDP_WS_ENDPOINT` で渡す
- stdout に `FANDHE_SCRIPT_RESULT` + 半角スペースで始まる 1 行の JSON を出す:
  `{"ok": bool, "step": string, "error": {"name": string, "message": string} | null}`
- 任意フィールド `stages`（TASK-45.2）: `[{"name", "status": "ok" | "failed" | "not_reached", "error"}]`。
  無ければ空配列（後方互換）。件数は 16、名前は 64 バイトが上限。`failed` は `error` 必須、
  `ok` / `not_reached` は `error` null、`failed` / `not_reached` の後ろに `ok` は置けず、
  トップレベル `ok: true` と矛盾する報告は `Malformed`（偽陽性防止）
- 段階は `connect` → `newPage` → `goto` → `selector` → `disconnect`。最初の失敗で止め、残りは `not_reached`（先行段階が失敗済みの切断は後始末のみで結果を上書きしない）。
  各段階は期限（既定 10 秒）付き。`goto` は `about:blank` 固定（ネットワーク取得なしで確定遷移として
  扱われ、SSRF 防御を緩めずに到達可否だけを測るため）
- 回収結果は `ScriptOutcome`（`Completed`〈`stages` を含む〉 / `NoResult` / `ExitedAbnormally` /
  `TimedOut` / `SpawnFailed` / `Malformed`）で表す
- 呼び出し側が `run_script` に渡す締め切り（既定値なし）で kill し、stdout / stderr は各 1MiB、結果行は 64KiB を上限とする

## 実行方法

実 Puppeteer を使う試験ターゲットは未追加（下記「導入状況」）。現時点で実行できるのは
基盤の自己テスト（`cargo test -p fandhe-browser-cdp --test puppeteer_connect`）と、
`make check-puppeteer-connect`（`stages.mjs` の自己テストと契約テスト `puppeteer_contract`）のみ。
`preflight_puppeteer` は `puppeteer-core` 未導入なら skip せず、理由付きで失敗を返す。

## 導入状況（承認待ち）

`puppeteer-core` の導入・バージョン固定はユーザー承認が必要で、現時点では未承認のため
`package.json`・`package-lock.json`・CI ジョブは追加していない。したがって実 Puppeteer での
実行は未検証で、基盤は偽スクリプトで検証している。実試験ターゲットは `cargo test --workspace` で実行されない
`test = false` 形式にせず、依存導入と CI 組み込みと同時に追加する。承認後に追加する内容:

- 実試験ターゲット（実測した段階別 status・失敗段階のエラー・`CdpState::received_methods()` を
  具体値で固定する。TASK-45.2 の受入基準の完全達成はこの実行で満たされる）と `make test-puppeteer-connect`。
  サーバーの実装済みメソッドは `Page.navigate`・`DOM.*` の一部のみのため、`connect` 段階で失敗する
  可能性が高い（未実測。到達 0 段階でも記録された結果として扱う）

- `package.json`（`puppeteer-core` を完全固定）と `package-lock.json`、`.gitignore` の `node_modules/`
- 導入済み版の照合（`preflight_puppeteer`）
- CI ジョブ（`actions/setup-node` を SHA 固定、`npm ci --ignore-scripts`）と `make ci` への組み込み

Windows は `Profile::open` が未対応（XOS-7〜XOS-10 待ち）のため実行対象外。

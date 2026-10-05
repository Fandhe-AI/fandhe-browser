# puppeteer-connect

Puppeteer 接続試験の実行基盤（TASK-45.1・Issue #480・ビヘイビア `CDP-3`・MS-4）。
`puppeteer.connect({browserWSEndpoint})` 相当の接続フローを、実クライアントで試すための土台。

## 役割分担

| 範囲 | Issue |
| ---- | ----- |
| 実行基盤（サーバー起動・スクリプト実行・結果の構造化回収） | #480（本ディレクトリ） |
| 接続後の `newPage`・`goto`・セレクタ取得の到達確認 | #481（TASK-45.2。`puppeteer_connect_live.rs` にステップを足す） |
| 結果レポート | #482（TASK-45.3） |

## 構成

| ファイル | 内容 |
| -------- | ---- |
| `connect.mjs` | `puppeteer-core` で接続し、結果を 1 行で報告するスクリプト |
| `crates/fandhe-browser-cdp/tests/script_harness/mod.rs` | サーバー起動・子プロセス実行・結果解析の共有基盤 |
| `crates/fandhe-browser-cdp/tests/puppeteer_connect.rs` | 基盤の自己テスト（Node 不要。`cargo test --workspace` で常に実行） |
| `crates/fandhe-browser-cdp/tests/puppeteer_connect_live.rs` | 実 Puppeteer を使う明示実行ターゲット（`test = false`） |

## スクリプトとの契約

- WS エンドポイントは環境変数 `FANDHE_CDP_WS_ENDPOINT` で渡す
- stdout に `FANDHE_SCRIPT_RESULT` + 半角スペースで始まる 1 行の JSON を出す:
  `{"ok": bool, "step": string, "error": {"name": string, "message": string} | null}`
- 回収結果は `ScriptOutcome`（`Completed` / `NoResult` / `TimedOut` / `Malformed`）で表す
- 締め切り（既定 60 秒）で kill し、stdout / stderr は各 1MiB、結果行は 64KiB を上限とする

## 実行方法

```bash
make test-puppeteer-connect
```

`puppeteer-core` が未導入（または `node` が無い）場合は skip せず、理由と導入手順
（`npm ci --ignore-scripts` を本ディレクトリで実行）を表示して失敗する。

## 導入状況（承認待ち）

`puppeteer-core` の導入・バージョン固定はユーザー承認が必要で、現時点では未承認のため
`package.json`・`package-lock.json`・CI ジョブは追加していない。したがって実 Puppeteer での
実行は未検証で、基盤は偽スクリプトで検証している。承認後に追加する内容:

- `package.json`（`puppeteer-core` を完全固定）と `package-lock.json`、`.gitignore` の `node_modules/`
- 導入済み版の照合（`preflight_puppeteer`）
- CI ジョブ（`actions/setup-node` を SHA 固定、`npm ci --ignore-scripts`）と `make ci` への組み込み

Windows は `Profile::open` が未対応（XOS-7〜XOS-10 待ち）のため実行対象外。

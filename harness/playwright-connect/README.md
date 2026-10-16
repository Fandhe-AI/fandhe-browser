# playwright-connect

Playwright 接続試験の実行基盤（TASK-44.1・Issue #476・ビヘイビア `CDP-2`・MS-4）。
`chromium.connectOverCDP` → `newContext` → `newPage` の一連フローを、実クライアントで
試すための土台。共有基盤 `crates/fandhe-browser-cdp/tests/script_harness/`（TASK-45.1）を再利用する。

## 役割分担

| 範囲 | Issue |
| ---- | ----- |
| 実行基盤（サーバー・fixture 起動、スクリプト実行、結果の構造化回収、未導入時の明示的失敗） | #476（本ディレクトリ） |
| `newPage()` 到達の確認（実 Playwright テスト `cdp2_real_playwright_reaches_new_page`） | #477（TASK-44.2） |
| `goto`・セレクタ取得 | #478（TASK-44.3） |
| 結果レポート | #479（TASK-44.4） |

先行の `harness/playwright-trace/`（TASK-43.1）は CDP トレース収集用で、本ディレクトリはテスト基盤用。

## 構成

| ファイル | 内容 |
| -------- | ---- |
| `connect.mjs` | `playwright-core` で段階実行し、結果を 1 行で報告するスクリプト |
| `stages.mjs` | 段階実行の純粋ロジック（クライアント注入可）。connect → newContext → newPage |
| `contract-sample.mjs` | Rust 側パーサーとの契約テスト用に `stages.mjs` の出力を再現する |
| `self-test.mjs` / `self-test.sh` | オフライン自己テスト（`make check-playwright-connect`。要 node。Playwright・ネットワーク不要） |
| `run.sh` | 実 Playwright 実行の入口（`make test-playwright-connect`。手動・要ネットワーク・CI 対象外） |
| `tests/script_harness/fixture.rs` | ローカル fixture HTTP サーバー（`127.0.0.1:0` のみ。到達パスを記録） |
| `tests/playwright_flow.rs` | 基盤の自己テスト（unix のみ。node 不要）と実 Playwright 実行テスト（`#[ignore]`） |
| `tests/playwright_contract.rs` | `stages.mjs` と Rust パーサーの契約テスト（`#[ignore]`。`self-test.sh` が実行し、0 件実行は fail） |

## スクリプトとの契約

- 環境変数（シェル文字列は経由しない）:
  - `FANDHE_CDP_WS_ENDPOINT`: サーバーの HTTP エンドポイント `http://127.0.0.1:<port>`
    （名前は共有基盤のまま。Playwright の `connectOverCDP` は HTTP を受け取る。ループバック IP リテラル以外は拒否）
  - `FANDHE_PLAYWRIGHT_MODULE_DIR`: `node_modules/playwright-core` を持つ導入先
  - `FANDHE_FIXTURE_URL`: fixture ページ URL（`goto` は TASK-44.3 で使用。現状未使用）
- 結果行・`stages` の形式は `harness/puppeteer-connect/README.md` と同じ。段階は `connect` / `newContext` / `newPage`
- 既知の回避策: `connectOverCDP(url, { noDefaults: true })`・`newContext({ acceptDownloads: false, viewport: null })`
  （`harness/playwright-trace/README.md`）。UA・フィンガープリントは変更しない（SEC-2）

## 実行方法

- 基盤の自己テスト: `cargo test -p fandhe-browser-cdp --test playwright_flow`（node 不要）
- スクリプト・契約の自己テスト: `make check-playwright-connect`（要 node）
- 実 Playwright: `make test-playwright-connect`（`playwright-core` を `mktemp -d` へ一時導入。
  `scripts/versions.sh` の `PLAYWRIGHT_VERSION` / `PLAYWRIGHT_INTEGRITY`、`verify-install.mjs` で検証）

## 未導入時の扱い（AC3）

`preflight_playwright` は導入先未指定・`playwright-core` 不在・版不一致・`node` 不在のいずれでも
skip せず、理由と導入手順（`PLAYWRIGHT_INSTALL_HINT`）つきの `HarnessError::ToolUnavailable` を返す。
実 Playwright 実行テストはこれを panic として表面化させる（成功を装わない）。

## 導入状況（承認待ち）

テスト基盤・CI での `playwright-core` 導入は **未承認**。リポジトリに `package.json`・lockfile・
`node_modules` は置かず、CI ジョブにも組み込んでいない。承認依頼の内容:

- パッケージ: `playwright-core`（`scripts/versions.sh` の exact 固定版・integrity。ブラウザ本体は取得しない）
- 導入方式: (a) 推奨: 本 `run.sh` の一時導入（リポに何も置かない）、(b) `package.json` + lockfile をコミットして `npm ci --ignore-scripts`
- CI 組み込み（`actions/setup-node` の SHA 固定・npm レジストリへのネットワークアクセス）は別途承認事項

## 既知の制約（TASK-44.3 の前提）

`start_server()` が使う `builtin_handlers()` の `Page.navigate` は既定の `FetchOptions`（内部アドレス拒否）で
構築され、注入経路が無い。このため `127.0.0.1` の fixture への `goto` は SSRF ガードで拒否される見込み。
44.3 でテスト用のループバック許可経路（cdp crate の変更。要セキュリティレビュー）が必要。

Windows は `Profile::open` が未対応（#295・#955）のため `playwright_flow.rs` は unix のみ。

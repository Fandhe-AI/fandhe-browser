# playwright-connect

Playwright 接続試験の実行基盤（TASK-44.1・Issue #476・ビヘイビア `CDP-2`・MS-4）。
`chromium.connectOverCDP` → `newContext` → `newPage` → `goto` → セレクタ取得の一連フローを、実クライアントで
試すための土台。共有基盤 `crates/fandhe-browser-cdp/tests/script_harness/`（TASK-45.1）を再利用する。

## 役割分担

| 範囲 | Issue |
| ---- | ----- |
| 実行基盤（サーバー・fixture 起動、スクリプト実行、結果の構造化回収、未導入時の明示的失敗） | #476（本ディレクトリ） |
| `newPage()` 到達の確認（実 Playwright テストは 44.3 で `cdp2_real_playwright_goto_and_selector` に置き換え） | #477（TASK-44.2） |
| `goto`・セレクタ取得（5 段階の実 Playwright テストとループバック fixture 用の `test-support` 入口） | #478（TASK-44.3） |
| 結果レポート | #479（TASK-44.4） |

先行の `harness/playwright-trace/`（TASK-43.1）は CDP トレース収集用で、本ディレクトリはテスト基盤用。

## 構成

| ファイル | 内容 |
| -------- | ---- |
| `connect.mjs` | `playwright-core` で段階実行し、結果を 1 行で報告するスクリプト |
| `stages.mjs` | 段階実行の純粋ロジック（クライアント注入可）。connect → newContext → newPage → goto → selector。期待値 `FIXTURE_EXPECT`（`#target` のテキスト・`data-role` 属性） |
| `contract-sample.mjs` | Rust 側パーサーとの契約テスト用に `stages.mjs` の出力を再現する |
| `self-test.mjs` / `self-test.sh` | オフライン自己テスト（`make check-playwright-connect`。要 node。Playwright・ネットワーク不要） |
| `run.sh` | 実 Playwright 実行の入口（`make test-playwright-connect`。手動・要ネットワーク・CI 対象外） |
| `tests/script_harness/fixture.rs` | ローカル fixture HTTP サーバー（`127.0.0.1:0` のみ。到達パスを記録） |
| `tests/playwright_flow.rs` | 基盤の自己テスト（node 不要）と実 Playwright 実行テスト。独自 `main`（`harness = false`。V8 ワーカー役を兼ねる）で、ケース名 `cdp2_real_playwright_goto_and_selector` を引数に渡したときだけ実ケースを実行する（TASK-44.9・#1220） |
| `tests/playwright_contract.rs` | `stages.mjs` と Rust パーサーの契約テスト（`#[ignore]`。`self-test.sh` が実行し、0 件実行は fail） |

## スクリプトとの契約

- 環境変数（シェル文字列は経由しない）:
  - `FANDHE_CDP_WS_ENDPOINT`: サーバーの HTTP エンドポイント `http://127.0.0.1:<port>`
    （名前は共有基盤のまま。Playwright の `connectOverCDP` は HTTP を受け取る。ループバック IP リテラル以外は拒否）
  - `FANDHE_PLAYWRIGHT_MODULE_DIR`: `node_modules/playwright-core` を持つ導入先
  - `FANDHE_FIXTURE_URL`: `goto` 先の fixture ページ URL（必須。ループバック IP リテラルの `http://` 以外は、Playwright 読み込み前に `goto` 段階の結果行つきで拒否）
- 結果行・`stages` の形式は `harness/puppeteer-connect/README.md` と同じ。段階は `connect` / `newContext` / `newPage` / `goto` / `selector`。
  セレクタ段階は `FIXTURE_EXPECT` と比べ、不一致は `SelectorMismatch`（期待値と実測値をメッセージに含む）、要素なしは `SelectorNotFound` で失敗する
- 既知の回避策: `connectOverCDP(url, { noDefaults: true })`・`newContext({ acceptDownloads: false, viewport: null })`
  （`harness/playwright-trace/README.md`）。UA・フィンガープリントは変更しない（SEC-2）

## 実行方法

- 基盤の自己テスト: `cargo test -p fandhe-browser-cdp --test playwright_flow`（node 不要）
- スクリプト・契約の自己テスト: `make check-playwright-connect`（要 node）
- 実 Playwright: `make test-playwright-connect`（`--features test-support` 付きで実行。`playwright-core` を `mktemp -d` へ一時導入。
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

## 既知の制約（TASK-44.3）

- ループバック fixture への `goto`: 本番の `endpoints()` は `Page.navigate` を内部アドレス拒否（SSRF ガード）で構築する。
  実テストは feature `test-support` の `test_support::endpoints_allowing_loopback` でサーバーを起動して許可する
  （既定ビルド・本番の `endpoints()` は拒否のまま）。
- 評価コンテキスト（TASK-44.9・#1220）: 実ケースは cli 本番と同じ `ScriptContextHandle::new(&JsConfig::default())` を
  テストサーバーへ渡す。`run.sh` は `--features test-support,js-v8` で実行し、評価器が無効なら（feature の付け忘れ）
  起動前に panic する。成功は完了マーカー行 `playwright_flow: real playwright case passed (1 case)` がちょうど 1 件のときだけ。
- セレクタ段階: 実行コンテキストは成立し `StageTimeout` は解消したが、再実測では core の universal selector（`*`）
  未対応で失敗する（追跡: #1237）。結果は `docs/design/playwright-compat.md` の「再実測（TASK-44.9）」を参照。

`Profile::open` は Windows でも `%LOCALAPPDATA%` 配下なら成功するため、`playwright_flow.rs` は 3 OS 共通（#955）。

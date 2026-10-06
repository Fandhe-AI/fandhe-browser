# CI・ローカル検証規約（リポ固有。XOS・REPAIR 系ビヘイビア）

## ローカルゲート（コミット・PR 前）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- テストは変更のたびに全件実行し、失敗・警告を 1 件でも残したまま進めない（fail-closed。REPAIR-5・REPAIR-6）
- feature gate を触った場合は既定ビルドと `--features rendering` の両方で検証する

## 3 OS CI（Linux・macOS・Windows 一級対応）

- 各 OS のネイティブランナーでビルド・テストする（クロスコンパイル前提にしない）
- matrix は ubuntu / macos / windows の 3 OS を必須とし、特定 OS のみの skip で CI を通さない
- OS 依存のファイルシステム挙動（パス・大文字小文字・ロック）のテストは 3 OS すべてで実行する

## 分離・構成の検証

- 既定ビルドに Servo が含まれないことを `cargo tree` で検証する（RENDER-1）。判定ロジックの正本は `scripts/check-render-isolation.sh`（`make check-render-isolation` から呼ぶ薄いラッパー）で、CI では `render-isolation` ジョブ（3 OS matrix）が同スクリプトを直接実行する（TASK-34.1・Issue #465）。`fandhe-browser-cli` の既定 feature も検査対象だが、cli の manifest が無い場合は NG（fail-closed。TASK-41.5・#174・#633）
- JS エンジン構成の依存グラフを `cargo tree` で検証する（TASK-32.4・`JS-1`・Issue #168）。軽量ビルド（`--no-default-features --features js-boa`）に `v8` が、エンジンなし（`--no-default-features`）に `v8`・`boa_engine` が含まれないことを `scripts/check-js-engine-isolation.sh`（`make check-js-engine-isolation`）で判定し、CI では `js-engine-isolation` ジョブ（3 OS matrix）が直接実行する。軽量ビルドの build・test は `rust-ci-js-boa` ジョブ、両エンジン同梱の共通テストは `rust-ci-default-features` ジョブ内で実行する
- ライセンス検査は `cargo deny check licenses` で行う（[licensing](./licensing.md)）
- 対象サイト群の動作率回帰チェック（`REPAIR-8`・`COMPAT-1`・`COMPAT-4`。TASK-9.2）:
  `make check-compat-regression`（CI では `compat-regression` ジョブ）が
  `harness/compat-regression/check-matrix.sh` で全体・`--categories` 指定の類型
  （static/spa/form。存在必須として明示列挙）に加え `--all-categories` で
  マトリクス内に実在する全ての類型（`cat` は `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`
  に一致する文字列に限る。lazy・table 等も含む）の動作率が閾値 70% 以上かを
  判定する。実マトリクス `harness/compat-practical/results/matrix.json`
  （TASK-71.3・Issue #312 が生成予定）が未導入の間は `--allow-missing` を渡し、
  ファイル不在時は `::warning::` を出して通過させる。#312 の完了後は
  `--allow-missing` を外し fail-closed（ファイル不在は exit 2）に戻す。
  スキーマ契約・終了コードは `harness/compat-regression/README.md` を参照
- 許可外ライセンス（GPL/AGPL/LGPL/MPL-2.0・ライセンス未記載）を持つ canary を `cargo deny` が reject することを `make check-deny-license-reject` で検証する（TASK-9.1・REPAIR-8）
- Puppeteer 接続試験ハーネスのオフライン自己テスト（`CDP-3`。TASK-45.2）は `make check-puppeteer-connect`（`make ci` に含む。要 node）。CI では `puppeteer-connect-selftest` ジョブ（3 OS）が `harness/puppeteer-connect/self-test.sh` を実行する。`stages.mjs` と Rust パーサーの契約テスト `cdp3_stages_mjs_result_line_satisfies_rust_contract`（`tests/puppeteer_contract.rs`。要 node）は `#[ignore]` で既定の `cargo test` から外れ、self-test.sh が `--ignored` 付きで実行する（0 件実行は fail-closed）
- feature 無効（既定）時のリリースバイナリサイズが `RENDER-2`（基準は `CORE-2` と同じ「Chromium 比 80% 以上削減」）の上限以下かを `make check-binary-size`（CI では `binary-size` ジョブ・3 OS）で検証する（TASK-34.2・TASK-34.3）。既定 package `fandhe-browser-cli` が workspace に無い場合は exit 2（fail-closed。#633。詳細は `harness/binary-size/README.md`）

## ワークフロー変更時の注意

- GitHub Actions のサードパーティ action はコミット SHA で固定する
- `permissions` は最小権限で明示する
- secrets を `pull_request` イベントのログへ出力しない
- CI 設定の変更は infra-builder が担当し、reviewer / security-auditor のレビューを経る

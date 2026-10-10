# CI・ローカル検証規約（リポ固有。XOS・REPAIR 系ビヘイビア）

## ローカルゲート（コミット・PR 前）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- テストは変更のたびに全件実行し、失敗・警告を 1 件でも残したまま進めない（fail-closed。REPAIR-5・REPAIR-6）
- feature gate を触った場合は既定ビルドと `--features rendering` の両方で検証する

## ジョブ構成と required checks

`ci.yml` は matrix・reusable workflow 呼び出しを使わない OS ごとの明示ジョブで構成する。job-level `if` でスキップされた matrix ジョブは展開されず check 名が変わり、required を満たせなくなるため。`ci.yml` が出す check-run はすべて required にする（自動マージは required でない check-run が 1 件でもあるとマージしない）。非 required の補助ジョブを追加しない。

| ジョブ | OS | 実行条件 | 内容 |
| ------ | -- | -------- | ---- |
| `changes` | ubuntu | 常時 | 変更ファイルから `rust`・`harness` を判定（`scripts/ci-changes.sh`） |
| `lint-docs` | ubuntu | 常時 | markdownlint・yamllint・editorconfig-checker・commitlint（PR のみ） |
| `rust-lint` | ubuntu | rust | `cargo fmt --all --check`・`cargo deny check`・`make check-deny-license-reject` |
| `rust-test-{ubuntu,macos,windows}` | 3 OS（PR は xos 時のみ macOS・Windows） | rust | clippy（`--all-features -D warnings`）・`cargo test --workspace --all-features` |
| `rust-features-{ubuntu,macos,windows}` | 3 OS（PR は xos 時のみ macOS・Windows） | rust | 既定 feature・エンジンなし・両エンジン同梱・`js-boa` 軽量ビルド・`js-v8` 単独の clippy / test |
| `harness-{ubuntu,macos,windows}` | 3 OS（PR は xos 時のみ macOS・Windows） | rust または harness | 分離検証・ハーネス自己テスト・サイズ検査（後述。`mcp-envelope` は unix のみ） |
| `windows-servo-prereqs` | windows | rust | Servo の Windows ビルド前提（SDK・MSVC v143・ATL・Python・uv）の検証 |

- required checks は上表のジョブ名 13 件（`{ubuntu,macos,windows}` は各 3 件で展開）に加え、`ai-review.yml` の `codex / review`・`codex / preflight`・`codex / post_feedback` と `Cursor Bugbot`。ジョブの追加・改名時は GitHub の ruleset の required checks を同時に更新する
- 旧ジョブ名との対応: `rust-ci`（fmt・deny は `rust-lint`、clippy・test は `rust-test-*`）、`rust-ci-default-features`・`rust-ci-js-boa`・`rust-ci-js-v8` は `rust-features-*`、`render-isolation`・`js-engine-isolation`・`compat-regression`・`puppeteer-connect-selftest`・`bench-record-selftest`・`mcp-envelope-smoke`・`binary-size` は `harness-*` のステップ（ステップ名の `[...]` 接頭辞が旧ジョブ名）、`deny-license-reject` は `rust-lint` のステップ

### docs-only の判定（fail-closed）

- `changes` ジョブは、push（main）・`workflow_dispatch` では常に `rust=true`・`harness=true` にする。`pull_request` では `git diff --name-only <base.sha>...<head.sha>` を `scripts/ci-changes.sh` に渡して判定する
- `rust=false` になるのは、変更ファイルがすべて docs 扱いの allowlist（`*.md`（任意階層）・`docs/**`（`docs/spec` submodule ポインタを含む）・`.claude/**`・`.agents/**`・`LICENSE-*`・`NOTICE`・`skills-lock.json`・`.markdownlint.jsonc`・`.yamllint`・`.editorconfig-checker.json`・`commitlint.config.mjs`・`lefthook.yml`）に一致するときだけ。allowlist 外のパスが 1 つでもあれば `rust=true`。ただし `docs/design/**` はテスト・ビルドが読む（`include_str!` の `host-api.schema.json`・`read_to_string` のレポート md 等）ため、`*.md` であっても `rust=true` 扱い。`docs/` 配下を読むコードを追加したら `scripts/ci-changes.sh` の判定と自己テストを見直す
- `.github/**`・`Cargo.*`・`crates/**`（配下の `*.md` を含む）・`scripts/**`・`Makefile`・`deny.toml`・`rust-toolchain.toml`・`profiles/**`・`benches/**`・`tests/**`・`Dockerfile`・`compose.yaml` は常に `rust=true`。`harness/**` の変更は `harness=true`（`rust` は他のファイル次第）
- `git diff` の失敗・SHA の欠落・判定スクリプトの出力不正は両方 `true`（全実行）に倒す。判定ロジックの変更後は `bash scripts/ci-changes.sh --self-test` を通す
- 下流ジョブが skipped になっても、`changes` 自身が required かつ失敗し得るため見逃さない。docs-only PR では `lint-docs` と `changes` だけが走る（macOS ジョブは 0 件）

## 3 OS CI（Linux・macOS・Windows 一級対応）

- 各 OS のネイティブランナーでビルド・テストする（クロスコンパイル前提にしない）
- ビルド・テスト（`rust-test-*`・`rust-features-*`）と OS 依存のファイルシステム挙動のテスト（パス・大文字小文字・ロック）は、**main への push と release 前に 3 OS すべてで実行する**（オーナー判断 2026-10-10。CI 待ち行列の解消のため）。main への push が続いた場合は concurrency の `cancel-in-progress` で古い run を中止し、最新のコミットだけを検証する
- PR では既定で ubuntu のみ検証し、macOS・Windows ジョブ（`*-macos`・`*-windows`・`windows-servo-prereqs`）は skipped にする。ただし `changes` が `xos=true` を出した PR は 3 OS で実行する。`xos=true` になるのは、OS 差異の影響を受けやすいパス（`crates/fandhe-browser-profile/`・`.github/`・`Cargo.toml` / `Cargo.lock`・`rust-toolchain.toml`・`scripts/`）を含む PR と、`*.rs` の差分に `cfg(target_os / target_family / windows / unix)` の追加・削除がある PR
- main の push で macOS・Windows だけが失敗した場合は、修正 PR で直す（その PR は `xos=true` になるよう対象を含める。含まれない場合も、修正対象の OS 分岐を触れば content 判定で 3 OS になる）
- `cargo tree` による分離検証（`harness-*` の `[render-isolation]`・`[js-engine-isolation]`）は target 依存のため 3 OS で実行する。`cargo fmt`・`cargo deny` は OS 非依存のため ubuntu のみ（`rust-lint`）
- cache（`actions/cache`・`scripts/ci-prune-target.sh` で workspace 成果物を除いてから保存）は ubuntu・macOS のみ。Windows は cache prune の既知問題で除外する（その分 Windows ジョブは所要時間が長いため timeout を長めに取る）

## 分離・構成の検証

- 既定ビルドに Servo が含まれないことを `cargo tree` で検証する（RENDER-1）。判定ロジックの正本は `scripts/check-render-isolation.sh`（`make check-render-isolation` から呼ぶ薄いラッパー）で、CI では `harness-*` ジョブの `[render-isolation]` ステップが同スクリプトを直接実行する（TASK-34.1・Issue #465）。`fandhe-browser-cli` の既定 feature も検査対象だが、cli の manifest が無い場合は NG（fail-closed。TASK-41.5・#174・#633）
- JS エンジン構成の依存グラフを `cargo tree` で検証する（TASK-32.4・`JS-1`・Issue #168）。軽量ビルド（`--no-default-features --features js-boa`）に `v8` が、エンジンなし（`--no-default-features`）に `v8`・`boa_engine` が含まれないことを `scripts/check-js-engine-isolation.sh`（`make check-js-engine-isolation`）で判定し、CI では `harness-*` ジョブの `[js-engine-isolation]` ステップが直接実行する。軽量ビルドの build・test は `rust-features-*` ジョブの `[js-boa]` ステップ、両エンジン同梱の共通テストは同ジョブの `[default]` ステップで実行する
- ライセンス検査は `rust-lint` ジョブの `cargo deny check advisories bans licenses sources` で行う（[licensing](./licensing.md)）
- 対象サイト群の動作率回帰チェック（`REPAIR-8`・`COMPAT-1`・`COMPAT-4`。TASK-9.2）:
  `make check-compat-regression`（CI では `harness-*` ジョブの `[compat-regression]` ステップ）が
  `harness/compat-regression/check-matrix.sh` で全体・`--categories` 指定の類型
  （static/spa/form。存在必須として明示列挙）に加え `--all-categories` で
  マトリクス内に実在する全ての類型（`cat` は `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`
  に一致する文字列に限る。lazy・table 等も含む）の動作率が閾値 70% 以上かを
  判定する。実マトリクス `harness/compat-practical/results/matrix.json`
  （TASK-71.3・Issue #312 が生成予定）が未導入の間は `--allow-missing` を渡し、
  ファイル不在時は `::warning::` を出して通過させる。#312 の完了後は
  `--allow-missing` を外し fail-closed（ファイル不在は exit 2）に戻す。
  スキーマ契約・終了コードは `harness/compat-regression/README.md` を参照
- 実マトリクスの生成スクリプト `harness/compat-practical/make_matrix.sh`（TASK-71.3・Issue #312。`core_results.jsonl` と PoC-9 の Chromium 参照データ `reference/chromium_results.json` を `id` で突合。CI では `--validate-only` で入力の整合のみ検査。詳細は `harness/compat-practical/README.md`）
- 実測ハーネス compat-practical の自己テスト（`MEAS-4`。TASK-71.1・TASK-71.2・TASK-71.3）は `make check-compat-practical`（`make ci` に含む。要 jq・node 22 以降）。CI では `harness-*` ジョブの `[compat-regression]` ステップ内で `harness/compat-practical/self-test.sh` を実行する。`run_core.sh` の自己テストは偽 CDP サーバー（`fake_cdp_server.mjs`）のみを使い、実バイナリ・ネットワークへは出ない
- 許可外ライセンス（GPL/AGPL/LGPL/MPL-2.0・ライセンス未記載）を持つ canary を `cargo deny` が reject することを `make check-deny-license-reject` で検証する（TASK-9.1・REPAIR-8）
- Puppeteer 接続試験ハーネスのオフライン自己テスト（`CDP-3`。TASK-45.2）は `make check-puppeteer-connect`（`make ci` に含む。要 node）。CI では `harness-*` ジョブの `[puppeteer-connect-selftest]` ステップが `harness/puppeteer-connect/self-test.sh` を実行する。`stages.mjs` と Rust パーサーの契約テスト `cdp3_stages_mjs_result_line_satisfies_rust_contract`（`tests/puppeteer_contract.rs`。要 node）は `#[ignore]` で既定の `cargo test` から外れ、self-test.sh が `--ignored` 付きで実行する（0 件実行は fail-closed）
- feature 無効（既定）時のリリースバイナリサイズが `RENDER-2`（基準は `CORE-2` と同じ「Chromium 比 80% 以上削減」）の上限以下かを `make check-binary-size`（CI では `harness-*` ジョブの `[binary-size]` ステップ・3 OS）で検証する（TASK-34.2・TASK-34.3）。既定 package `fandhe-browser-cli` が workspace に無い場合は exit 2（fail-closed。#633。詳細は `harness/binary-size/README.md`）

## ワークフロー変更時の注意

- GitHub Actions のサードパーティ action はコミット SHA で固定し、新しい action を安易に追加しない（cache は `actions/cache`、toolchain は `dtolnay/rust-toolchain` のみ）
- `permissions` は最小権限で明示する
- 出る check-run の名前を変える変更（ジョブ・`name:` の追加 / 改名）は ruleset の required checks との同時更新が必要。reusable workflow 呼び出し（`caller / callee` 形式の名前）や matrix は required にするジョブでは使わない
- secrets を `pull_request` イベントのログへ出力しない
- CI 設定の変更は infra-builder が担当し、reviewer / security-auditor のレビューを経る

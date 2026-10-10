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
| `rust-test-{ubuntu,macos,windows}` | 3 OS（macOS・Windows は PR では skipped） | rust | clippy（`--all-features -D warnings`）・`cargo test --workspace --all-features` |
| `rust-features-{ubuntu,macos,windows}` | 3 OS（macOS・Windows は PR では skipped） | rust | 既定 feature・エンジンなし・両エンジン同梱・`js-boa` 軽量ビルド・`js-v8` 単独の clippy / test |
| `harness-{ubuntu,macos,windows}` | 3 OS（macOS・Windows は PR では skipped） | rust または harness | 分離検証・ハーネス自己テスト・サイズ検査（後述。`mcp-envelope` は unix のみ） |
| `windows-servo-prereqs` | windows（PR は skipped） | rust | Servo の Windows ビルド前提（SDK・MSVC v143・ATL・Python・uv）の検証 |
| `cross-target-check` | ubuntu | rust（PR でも実行） | macOS・Windows 向けのクロス型検査（`aarch64-apple-darwin`・`x86_64-pc-windows-msvc`。後述の範囲に限る） |

- required checks は上表のジョブ名 14 件（`{ubuntu,macos,windows}` は各 3 件で展開）に加え、`ai-review.yml` の `codex / review`・`codex / preflight`・`codex / post_feedback` と `Cursor Bugbot`。ジョブの追加・改名時は GitHub の ruleset の required checks を同時に更新する
- macOS・Windows のジョブは `pull_request` では `if:` により skipped になる（skipped は required を満たす）。push（main）・`workflow_dispatch` では実行される
- 旧ジョブ名との対応: `rust-ci`（fmt・deny は `rust-lint`、clippy・test は `rust-test-*`）、`rust-ci-default-features`・`rust-ci-js-boa`・`rust-ci-js-v8` は `rust-features-*`、`render-isolation`・`js-engine-isolation`・`compat-regression`・`puppeteer-connect-selftest`・`bench-record-selftest`・`mcp-envelope-smoke`・`binary-size` は `harness-*` のステップ（ステップ名の `[...]` 接頭辞が旧ジョブ名）、`deny-license-reject` は `rust-lint` のステップ

### docs-only の判定（fail-closed）

- `changes` ジョブは、push（main）・`workflow_dispatch` では常に `rust=true`・`harness=true` にする。`pull_request` では `git diff --name-only <base.sha>...<head.sha>` を `scripts/ci-changes.sh` に渡して判定する
- `rust=false` になるのは、変更ファイルがすべて docs 扱いの allowlist（`*.md`（任意階層）・`docs/**`（`docs/spec` submodule ポインタを含む）・`.claude/**`・`.agents/**`・`LICENSE-*`・`NOTICE`・`skills-lock.json`・`.markdownlint.jsonc`・`.yamllint`・`.editorconfig-checker.json`・`commitlint.config.mjs`・`lefthook.yml`）に一致するときだけ。allowlist 外のパスが 1 つでもあれば `rust=true`。ただし `docs/design/**` はテスト・ビルドが読む（`include_str!` の `host-api.schema.json`・`read_to_string` のレポート md 等）ため、`*.md` であっても `rust=true` 扱い。`docs/` 配下を読むコードを追加したら `scripts/ci-changes.sh` の判定と自己テストを見直す
- `.github/**`・`Cargo.*`・`crates/**`（配下の `*.md` を含む）・`scripts/**`・`Makefile`・`deny.toml`・`rust-toolchain.toml`・`profiles/**`・`benches/**`・`tests/**`・`Dockerfile`・`compose.yaml` は常に `rust=true`。`harness/**` の変更は `harness=true`。Rust のテスト・ビルドから読まれないと確認済みのディレクトリ（`binary-size`・`render-screenshot`・`cold-start`・`multi-instance-memory`）だけを harness のみ扱いにし、それ以外（`compat_fixtures`・`playwright-trace` 等の `include_str!` 入力、workspace メンバーの `wpt_subset_runner`、未知の新規ディレクトリ）は `rust=true` も立てる（fail-closed）。harness ディレクトリを Rust から読むようにしたら、この allowlist から外す
- `git diff` の失敗・SHA の欠落・判定スクリプトの出力不正は両方 `true`（全実行）に倒す。判定ロジックの変更後は `bash scripts/ci-changes.sh --self-test` を通す
- 下流ジョブが skipped になっても、`changes` 自身が required かつ失敗し得るため見逃さない。docs-only PR では `lint-docs` と `changes` だけが走る

## 3 OS CI（Linux・macOS・Windows 一級対応）

3 OS のネイティブ実行の時機（オーナー判断 2026-10-10。macOS ランナーの同時実行枠待ちで全 PR の CI が詰まるため。参考: Fandhe-AI/fandhe-container の CI）:

- **pull_request**: ubuntu のネイティブでビルド・テスト・分離検証を行う。macOS・Windows のネイティブジョブ（`*-macos`・`*-windows`・`windows-servo-prereqs`）は skipped にし、代わりに `cross-target-check` が Linux 上で macOS・Windows 向けの型検査を行う（範囲は下記）
- **main への push**: 3 OS すべてのネイティブランナーで全ジョブを実行する（OS 依存のファイルシステム挙動のテスト〔パス・大文字小文字・ロック〕を含む）
- **release 前**: 3 OS のネイティブで全部回す。`release.yml` は発火条件が `workflow_dispatch` 限定のため、`gh workflow run ci.yml --ref <ref>` で `ci.yml` を実行する（`workflow_dispatch` では 3 OS で回る）
- 見落とすのは macOS・Windows 固有の **実行時の誤り**（テストの失敗・BSD / Git Bash のツール差・パス・大文字小文字・改行）で、PR では検出できず main への push 後に判明する。これはマージ前の待ち時間と引き換えにオーナーが判断した残存リスク。main の push で macOS・Windows だけが失敗した場合は、修正 PR で直す
- concurrency: group は `${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}`・`cancel-in-progress: true`。main への新しい push は実行中の古い main の CI（3 OS）を取り消す。取り消された commit の macOS・Windows の結果は残らず、後続 commit の main の 3 OS 実行で検出する（`workflow_dispatch` を main へ向けた場合も同じ group）
- ruleset の required には PR で生成される check-run をすべて登録する。macOS・Windows のジョブは PR で skipped になるため required に含めてよい（skipped は required を満たす）
- main・`workflow_dispatch` の macOS・Windows ジョブを、特定 OS のみの skip や `continue-on-error` で通さない。PR で省くのはイベント単位の方針であり、個別のテスト・ステップを特定 OS だけ飛ばしてよいという意味ではない
- クロス型検査（`cross-target-check`）は ubuntu 上で `aarch64-apple-darwin` と `x86_64-pc-windows-msvc` に対し `cargo check --workspace --all-targets --all-features --target <t>` と `cargo clippy ... -- -D warnings`（`--all-features` と既定 feature）を実行する。リンクもテスト実行もしない型検査で、ネイティブ実行の代わりにはならない。**範囲は限定される**（2026-10-10 にローカル実測）: 依存の `rustls` が使う `ring` は build.rs で C / アセンブラをクロスコンパイルし、macOS SDK / MSVC のヘッダ・`lib.exe` が要るため ubuntu 上では成立しない。そのため `ring` を引く `fandhe-browser-core` とそれに依存する `-ai`・`-cdp`・`-cli`、および `wpt-subset-runner` は `--exclude` で外し、`fandhe-browser-js`（V8・boa）・`-profile`・`-mcp`・`-render` だけを検査する（この 4 crate は `cargo tree` で `cc`・`ring`・`cmake` を持たないことを確認済み）。core・ai・cdp・cli の OS 固有分岐のコンパイル・lint の誤りは PR では検出されず、main の macOS・Windows ネイティブ実行で初めて判明する。`--exclude` 方式のため、新たに `ring` 等を引く crate が増えると失敗として表面化する。`cc` を使う依存を js・profile・mcp・render に追加する場合は、その crate もここで `--exclude` する。v8 の build.rs が prebuilt 静的ライブラリ（約 150MB / ターゲット）をダウンロードするため、target は cache しない。将来の拡張候補は Windows 向けの `x86_64-pc-windows-gnu` + mingw（macOS には効かない）で、導入にはユーザー判断が要る
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

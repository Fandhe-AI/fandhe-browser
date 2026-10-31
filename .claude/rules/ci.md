# CI・ローカル検証規約（リポ固有。XOS・REPAIR 系ビヘイビア）

## ローカルゲート（コミット・PR 前）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- テストは変更のたびに全件実行し、失敗・警告を 1 件でも残したまま進めない（fail-closed。REPAIR-5・REPAIR-6）
- feature gate を触った場合は該当 feature の有無の両方で検証する（`rendering` feature は #236 で廃止済み。Servo 隔離は `make check-render-isolation`）
- 上記ゲートは手動で実行する規約で、git hooks（lefthook）では強制しない。GitHub Actions で実行する検査は hooks で重複実行せず、hooks は CI に無い秘密情報検査と軽量な整形・コミットメッセージ検査に限る（`lefthook.yml`。pre-push は置かない。フック編集の方針・時間予算は [git-hooks](./git-hooks.md)）

## ジョブ構成と required checks

`ci.yml` は matrix・reusable workflow 呼び出しを使わない OS ごとの明示ジョブで構成する。job-level `if` でスキップされた matrix ジョブは展開されず check 名が変わり、required を満たせなくなるため。`ci.yml` が出す check-run はすべて required にする（自動マージは required でない check-run が 1 件でもあるとマージしない）。非 required の補助ジョブを追加しない。

| ジョブ | OS | 実行条件 | 内容 |
| ------ | -- | -------- | ---- |
| `changes` | ubuntu | 常時 | 変更ファイルから `rust`・`harness`・`servo`・`native` を判定（`scripts/ci-changes.sh`。`native` は `rust-test-{macos,windows}` を PR で走らせるかの軸。`servo` は `servo-host-{ubuntu,macos,windows}` と `rust-lint` のホスト deny ステップが消費する） |
| `lint-docs` | ubuntu | 常時 | markdownlint・yamllint・editorconfig-checker・commitlint（PR のみ） |
| `rust-lint` | ubuntu | rust または servo | `cargo fmt --all --check`・`cargo deny check`・`make check-deny-license-reject`。servo 時はさらにホストの `cargo deny`（`scripts/deny.sh servo-host`。ホスト用 `servo-host/deny.toml`） |
| `rust-test-{ubuntu,macos,windows}` | 3 OS（macOS・Windows は PR では、OS 依存の変更〔`native=true`〕を含むときだけ実行し、それ以外は skipped） | rust（macOS・Windows の PR 実行は rust かつ native） | clippy（`--all-features -D warnings`）・`cargo test --workspace --all-features`。ubuntu はさらに `real_host`（`test = false` で `--all-targets` から外れる）の明示 clippy（コンパイル確認のみ・ホスト不要。#998） |
| `rust-features-{ubuntu,macos}` | ubuntu・macOS（macOS は PR では skipped） | rust | 既定 feature・エンジンなし・両エンジン同梱・`js-boa` 軽量ビルド・`js-v8` 単独の clippy / test。さらに `[plugin-registry]` ステップで ai の登録機構 off（`--no-default-features`）の clippy / test（#822） |
| `rust-features-windows-{default,engines,boa}` | windows（PR は skipped） | rust | 上と同じ検査を feature 構成の組ごとに 3 ジョブへ並列分割（`rust-features-windows` 1 ジョブでは cache なしで約 32 分・cache ありでも 15〜17 分かかったため。オーナー判断 2026-10-12。`default`: 既定 feature・`js-v8` 単独・cli エンジンなし check、`engines`: core 単独・両エンジン同梱、`boa`: `js-boa` 軽量ビルド・`js-boa` 単独） |
| `harness-{ubuntu,macos,windows}` | 3 OS（macOS・Windows は PR では skipped） | rust または harness | 分離検証・ハーネス自己テスト・サイズ検査（後述。`mcp-envelope` は unix のみ） |
| `windows-servo-prereqs` | windows（PR は skipped） | rust | Servo の Windows ビルド前提（SDK・MSVC v143・ATL・Python・uv）の検証。同一の検証を `servo-host-windows` が内包する（削除は ruleset の更新を伴うため後続 issue。#999） |
| `servo-host-ubuntu` | ubuntu | servo（PR でも実行） | Servo ホスト（`servo-host/`。別 workspace）の `cargo fmt --check`・clippy（dev・`--all-targets`・`-D warnings`）・test（契約テストを含む）・render の実ホスト結合テスト `tests/real_host.rs`（`[render-real-host]`。ホストの dev バイナリを `FANDHE_BROWSER_RENDER_HOST` に渡して `cargo test -p fandhe-browser-render --test real_host` を明示実行。`test = false` のため既定の `cargo test` に入らず、明示 clippy を持つ。未設定は panic で fail-closed、実行件数 0 も失敗。本体 workspace のコールドビルドが加わる。TASK-38.8b・#998）・分離検証（`scripts/check-render-isolation.sh`。C〜E がホスト側）。main・`workflow_dispatch` のみ release ビルドとサイズ記録（`RUNNER_TEMP` に出力し cache しない。閾値ゲートは SSOT に無いため置かない）。cache は ubuntu のみ・種別 `servohost` |
| `servo-host-macos` | macOS（PR は skipped） | servo（main・`workflow_dispatch` のみ） | Servo ホストの `cargo build --locked`（dev）・`--selftest`・`cargo test`。Xcode 同梱の objdump を `xcrun` で探して `LLVM_OBJDUMP` に設定。末尾に `[servo-host-release]`（release ビルドとサイズ記録。`RUNNER_TEMP` に出力・実行はしない・cache しない・閾値ゲートなし。#1279・親 #293・`XOS-4`）。cache なし。`timeout-minutes` は 55（release を含むジョブ全体の `workflow_dispatch` 実測 35.3 分〔release 23.9 分。run 40396545036〕× 約 1.5。#1279） |
| `servo-host-windows` | windows（PR は skipped） | servo（main・`workflow_dispatch` のみ） | 上と同じ検査に加え、前提検証（SDK・MSVC v143 の選択〔`vcvarsall -vcvars_ver`〕・ATL・libclang・Python・uv・MozTools 4.0〔zip を sha256 固定で取得し `MOZTOOLS_PATH` を設定〕）を内包。短い `--target-dir`・長パス有効化。ルート `.cargo/config.toml` の `+crt-static`（本体向け。#917）は動的 CRT 前提の mozjs_sys とリンクできないため、ジョブの `CARGO_ENCODED_RUSTFLAGS=-Ctarget-feature=-crt-static` で打ち消す（`--manifest-path` で呼ぶため `servo-host/.cargo/config.toml` は読まれない）。Windows は `no-wgl`（ANGLE）で動くため、ビルド後に mozangle の `OUT_DIR` から `libEGL.dll`・`libGLESv2.dll` を exe とテストバイナリの隣（`debug`・`debug\deps`）へ置く（該当 `OUT_DIR` が 1 件でなければ fail。`--selftest`・テストの前）。末尾に macOS と同じ `[servo-host-release]`（出力先は `D:\sr`。C: の空きが約 33.5GB で dev〔`C:\st`〕と 2 本のフルビルドが収まらないおそれがあるため。`-crt-static` はジョブの env が release にも効く。ANGLE の DLL は実行しないため配置しない）。cache なし。`timeout-minutes` は 90（release を含むジョブ全体の `workflow_dispatch` 実測 57.6 分〔ANGLE のビルドを含む dev 21.1 分・release 27.9 分。run 40396545036〕× 約 1.5。#1279） |
| `cross-target-check` | ubuntu | rust（PR でも実行） | macOS・Windows 向けのクロス型検査（`aarch64-apple-darwin`・`x86_64-pc-windows-msvc`。後述の範囲に限る） |

- required checks は上表のジョブ名 19 件（`{...}` は展開して数える）に加え、`ai-review.yml` の `codex / review`・`codex / preflight`・`codex / post_feedback` と `Cursor Bugbot`。ジョブの追加・改名時は GitHub の ruleset の required checks を同時に更新する
- macOS・Windows のジョブは `pull_request` では `if:` により skipped になる（skipped は required を満たす）。例外は `rust-test-macos`・`rust-test-windows` で、`native=true`（後述）のときは PR でも実行する。push（main）・`workflow_dispatch` では実行される
- 旧ジョブ名との対応: `rust-ci`（fmt・deny は `rust-lint`、clippy・test は `rust-test-*`）、`rust-ci-default-features`・`rust-ci-js-boa`・`rust-ci-js-v8` は `rust-features-*`（Windows は `rust-features-windows-*`）、`render-isolation`・`js-engine-isolation`・`compat-regression`・`puppeteer-connect-selftest`・`bench-record-selftest`・`mcp-envelope-smoke`・`binary-size` は `harness-*` のステップ（ステップ名の `[...]` 接頭辞が旧ジョブ名）、`deny-license-reject` は `rust-lint` のステップ。`servo-host-ubuntu` は新規ジョブ（#995）、`servo-host-macos`・`servo-host-windows` は新規ジョブ（#999）。いずれも ruleset への登録はオーナーが行い、登録されるまで自動マージが止まるのは想定内

### docs-only の判定（fail-closed）

- `changes` ジョブは、push（main）・`workflow_dispatch` では常に `rust=true`・`harness=true`・`servo=true` にする。`pull_request` では `git diff --name-only <base.sha>...<head.sha>` を `scripts/ci-changes.sh` に渡して判定する
- `rust=false` になるのは、変更ファイルがすべて docs 扱いの allowlist（`*.md`（任意階層）・`docs/**`（`docs/spec` submodule ポインタを含む）・`.claude/**`・`.agents/**`・`LICENSE-*`・`NOTICE`・`skills-lock.json`・`.markdownlint.jsonc`・`.yamllint`・`.editorconfig-checker.json`・`commitlint.config.mjs`・`lefthook.yml`）に一致するときだけ。allowlist 外のパスが 1 つでもあれば `rust=true`。ただし `docs/design/**` はテスト・ビルドが読む（`include_str!` の `host-api.schema.json`・`read_to_string` のレポート md 等）ため、`*.md` であっても `rust=true` 扱い。`docs/` 配下を読むコードを追加したら `scripts/ci-changes.sh` の判定と自己テストを見直す
- `.github/**`・`Cargo.*`・`crates/**`（配下の `*.md` を含む）・`scripts/**`・`Makefile`・`deny.toml`・`rust-toolchain.toml`・`profiles/**`・`benches/**`・`tests/**`・`Dockerfile`・`compose.yaml` は常に `rust=true`。`harness/**` の変更は `harness=true`。Rust のテスト・ビルドから読まれないと確認済みのディレクトリ（`binary-size`・`render-screenshot`・`cold-start`・`multi-instance-memory`・`container-memory`・`cli-conformance`・`endpoint-swap`・`egress-isolation`）だけを harness のみ扱いにし、それ以外（`compat_fixtures`・`playwright-trace` 等の `include_str!` 入力、workspace メンバーの `wpt_subset_runner`、未知の新規ディレクトリ）は `rust=true` も立てる（fail-closed）。harness ディレクトリを Rust から読むようにしたら、この allowlist から外す
- `native=true` は独立軸（`rust-test-macos`・`rust-test-windows` を PR で走らせるかの判断。OS 依存の変更が main への push 後にしか red にならない再発の防止）。次のいずれかで立つ。(1) 変更ファイルに `crates/fandhe-browser-profile/` または `.cargo/` 配下がある（`.cargo/config.toml` の target 別 rustflags〔Windows の `+crt-static` 等。#917〕はリンク・テストの失敗が Windows でしか出ないため）、(2) `*.rs` の差分（`git diff -U0 <base>...<head> -- '*.rs'`。`changes` ジョブが `CI_CHANGES_RS_DIFF_FILE` でスクリプトへ渡す）の追加・削除行に、単語境界つきの `windows`・`unix` トークン、または `target_os`・`target_family` がある。`cfg(` の有無は問わない（複数行の `#[cfg(any(` / `windows,` / `unix,` で `cfg(` と OS 名が別行になっても拾うため。過検出は安全側）がある。push（main）・`workflow_dispatch` では native の値によらず従来どおり全実行。`*.rs` の差分取得の失敗・diff ファイルが読めない・出力不正・入力が空は `native=true`
- `servo=true` は独立軸（TASK-38・`RENDER-1`・設計 `servo-host-design.md` §6.1）。`servo-host/**`・`crates/fandhe-browser-render/**`（契約 fixture を含む）・`docs/design/render-host-protocol*`・`scripts/**`・`.github/**`・`rust-toolchain.toml`・`.cargo/**`（`servo-host/` にも効くため）で立ち、入力が空のときも true。`servo-host/**` だけの変更は `rust=false`・`servo=true`（ただし `servo-host/deny.toml` は `make check-deny-license-reject`〔`rust-lint`〕が読むため `rust=true` にする。#991。別 workspace でルート `Cargo.toml` が `exclude` しており、本体のビルド・テストは読まない。契約 fixture は `crates/` 側に置く）
- `servo=true` のとき `servo-host-ubuntu` と `rust-lint` が走る（main・`workflow_dispatch` ではさらに `servo-host-macos`・`servo-host-windows`。PR では skipped）。docs-only（`rust=false`・`servo=false`）の PR は従来どおり `lint-docs` と `changes` だけが走る
- `git diff` の失敗・SHA の欠落・判定スクリプトの出力不正は `rust`・`harness`・`servo`・`native` のすべて `true`（全実行）に倒す。判定ロジックの変更後は `bash scripts/ci-changes.sh --self-test` を通す
- 下流ジョブが skipped になっても、`changes` 自身が required かつ失敗し得るため見逃さない。docs-only PR では `lint-docs` と `changes` だけが走る

## 3 OS CI（Linux・macOS・Windows 一級対応）

3 OS のネイティブ実行の時機（オーナー判断 2026-10-10。macOS ランナーの同時実行枠待ちで全 PR の CI が詰まるため。参考: Fandhe-AI/fandhe-container の CI）:

- **pull_request**: ubuntu のネイティブでビルド・テスト・分離検証を行う。macOS・Windows のネイティブジョブ（`*-macos`・`*-windows`・`windows-servo-prereqs`）は skipped にする（例外: OS 依存の変更〔`native=true`: profile crate 配下、または `*.rs` 差分の `cfg(windows`・`cfg(unix`・`target_os` 等〕を含む PR では `rust-test-macos`・`rust-test-windows` だけ実行する。2026-10-17 のユーザー承認。他の macOS・Windows ジョブは枠待ちで CI が詰まるのを避けるため対象外）。また、代わりに `cross-target-check` が Linux 上で macOS・Windows 向けの型検査を行う（範囲は下記）
- **main への push**: 3 OS すべてのネイティブランナーで全ジョブを実行する（OS 依存のファイルシステム挙動のテスト〔パス・大文字小文字・ロック〕を含む）
- **release 前**: 3 OS のネイティブで全部回す。`release.yml` は発火条件が `workflow_dispatch` 限定のため、`gh workflow run ci.yml --ref <ref>` で `ci.yml` を実行する（`workflow_dispatch` では 3 OS で回る）
- 見落とすのは macOS・Windows 固有の **実行時の誤り**（テストの失敗・BSD / Git Bash のツール差・パス・大文字小文字・改行）で、PR では検出できず main への push 後に判明する。これはマージ前の待ち時間と引き換えにオーナーが判断した残存リスク。main の push で macOS・Windows だけが失敗した場合は、修正 PR で直す
- concurrency: group は `${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}`・`cancel-in-progress: true`。main への新しい push は実行中の古い main の CI（3 OS）を取り消す。取り消された commit の macOS・Windows の結果は残らず、後続 commit の main の 3 OS 実行で検出する（`workflow_dispatch` を main へ向けた場合も同じ group）
- ruleset の required には PR で生成される check-run をすべて登録する。macOS・Windows のジョブは PR で skipped になるため required に含めてよい（skipped は required を満たす）
- main・`workflow_dispatch` の macOS・Windows ジョブを、特定 OS のみの skip や `continue-on-error` で通さない。PR で省くのはイベント単位の方針であり、個別のテスト・ステップを特定 OS だけ飛ばしてよいという意味ではない
- クロス型検査（`cross-target-check`）は ubuntu 上で `aarch64-apple-darwin` と `x86_64-pc-windows-msvc` に対し `cargo check --workspace --all-targets --all-features --target <t>` と `cargo clippy ... -- -D warnings`（`--all-features` と既定 feature）を実行する。リンクもテスト実行もしない型検査で、ネイティブ実行の代わりにはならない。**範囲は限定される**（2026-10-10 にローカル実測）: 依存の `rustls` が使う `ring` は build.rs で C / アセンブラをクロスコンパイルし、macOS SDK / MSVC のヘッダ・`lib.exe` が要るため ubuntu 上では成立しない。そのため `ring` を引く `fandhe-browser-core` とそれに依存する `-ai`・`-cdp`・`-cli`・`-render`（HostRenderer が core に依存。TASK-38・#235）、および `wpt-subset-runner` は `--exclude` で外し、`fandhe-browser-js`（V8・boa）・`-profile`・`-mcp` だけを検査する（この 3 crate は `cargo tree` で `cc`・`ring`・`cmake` を持たないことを確認済み）。core・ai・cdp・cli・render の OS 固有分岐のコンパイル・lint の誤りは PR では検出されず、main の macOS・Windows ネイティブ実行で初めて判明する。`--exclude` 方式のため、新たに `ring` 等を引く crate が増えると失敗として表面化する。`cc` を使う依存を js・profile・mcp に追加する場合は、その crate もここで `--exclude` する。v8 の build.rs が prebuilt 静的ライブラリ（約 150MB / ターゲット）をダウンロードするため、target は cache しない。将来の拡張候補は Windows 向けの `x86_64-pc-windows-gnu` + mingw（macOS には効かない）で、導入にはユーザー判断が要る
- `cargo tree` による分離検証（`harness-*` の `[render-isolation]`・`[js-engine-isolation]`）は target 依存のため 3 OS で実行する。`cargo fmt`・`cargo deny` は OS 非依存のため ubuntu のみ（`rust-lint`）
- cache（`actions/cache/restore`・`actions/cache/save`。保存は main のみで、`scripts/ci-prune-target.sh` で workspace 成果物を除いてから保存。key 設計・上限・予算は [github-actions](./github-actions.md)「キャッシュ方針」）は 3 OS で使う。Windows は prune が「想定外の crate / target 名」で fail したため除外していたが、Windows の jq の CRLF 出力が原因と推定し、`scripts/ci-prune-target.sh` で CR を落とす形に直して cache を入れた（修正後に Windows で prune が通ることを `workflow_dispatch` で確認した。原因は jq の CRLF 出力と推定で、CRLF 出力自体は直接観測していない。Windows ジョブの timeout は cache 導入後も変えない）。`scripts/ci-prune-target.sh --self-test`（CRLF の jq と Windows の成果物名を Linux 上で模擬。`make check-ci-changes` に含む）で回帰を防ぐ（`--workspace-root` 指定で別 workspace の成果物だけを prune し、cwd 側の `target` が不変なことも検証する）。`servo-host-macos`・`servo-host-windows` は cache なし（設計 §6.3。1 世代を 10GB 以内に保つため）。Servo ホストの cache（`servo-host/target`・種別 `servohost`・key は `servo-host/Cargo.lock` を hash）は ubuntu のみで、保存前に `scripts/ci-prune-target.sh --workspace-root servo-host` を呼ぶ（設計 `servo-host-design.md` §6.3）

## 分離・構成の検証

- 既定ビルドに Servo が含まれないことを `cargo tree` で検証する（RENDER-1）。判定ロジックの正本は `scripts/check-render-isolation.sh`（`make check-render-isolation` から呼ぶ薄いラッパー）で、CI では `harness-*` ジョブの `[render-isolation]` ステップが同スクリプトを直接実行する（TASK-34.1・Issue #465）。`fandhe-browser-cli` の既定 feature も検査対象だが、cli の manifest が無い場合は NG（fail-closed。TASK-41.5・#174・#633）
- Servo ホスト（`servo-host/`。別 workspace・別 `Cargo.lock`）が本体 workspace から分離されていることを `scripts/check-servo-host-isolation.sh` で検証する（TASK-38・`RENDER-1`・#985）。`cargo metadata --no-deps` の members に `servo-host` が現れないことと、ルート `Cargo.lock` に `servo`・`mozjs` 系パッケージが無いことを判定する。`make check-workspace-manifest` から呼ばれ、CI では `servo-host-ubuntu` の `[render-isolation]` ステップが `scripts/check-render-isolation.sh`（検査 C として本スクリプトとその自己テストを呼ぶ）経由で実行する（`harness-*` にあった重複ステップは #995 で削除）
- JS エンジン構成の依存グラフを `cargo tree` で検証する（TASK-32.4・`JS-1`・Issue #168）。軽量ビルド（`--no-default-features --features js-boa,plugin-registry`）に `v8` が、エンジンなし（`--no-default-features`）に `v8`・`boa_engine` が含まれないことを `scripts/check-js-engine-isolation.sh`（`make check-js-engine-isolation`）で判定し、CI では `harness-*` ジョブの `[js-engine-isolation]` ステップが直接実行する。軽量ビルドの build・test は `rust-features-*` ジョブの `[js-boa]` ステップ、両エンジン同梱の共通テストは同ジョブの `[default]` ステップで実行する
- musl 配布バイナリ（既定・軽量）の動的リンクなし検査（TASK-93.3・`PLUG-1`・`CTR-4`・Issue #362）: 判定の正本は `scripts/check-static-link.sh`（`make check-static-link`）。CI では `harness-ubuntu` の `[static-link]` ステップ（musl-tools の導入・自己テスト・検査）が実行する。Linux の ELF 成果物の検査のため ubuntu のみ（macOS・Windows は次項以降。`[mcp-envelope-smoke]` と同様に OS 固有の検査で、3 OS 共通の検査を特定 OS だけ飛ばすものではない）。`make ci` には含めない。ビルド先は `target/` の外（`RUNNER_TEMP`）で cache 対象外。`*-linux-gnu` のネイティブ配布物は対象外（#363）
- macOS 配布バイナリ（既定・軽量）の動的リンク検査（TASK-93.4・`PLUG-1`・Issue #364）: 判定の正本は `scripts/check-dynamic-link-macos.sh`（`make check-dynamic-link-macos`。許可リストは `scripts/macos-dylib-allowlist.txt`）。#363 のオーナー判断（2026-10-12）で当面は記録のみ（許可リスト外は warning で失敗させない。検査不能はモードに関係なく NG）。許可リスト確定後に `MACOS_DYLIB_MODE` の既定（`scripts/harness.sh`）を `enforce` へ切り替える。CI では `harness-macos` の `[dynamic-link-macos]` ステップ（PR では skipped。main・`workflow_dispatch` のみ）が実行し、`harness-ubuntu` は合成データの自己テストだけを PR でも回す。`make ci` には含めない。ビルド先は `RUNNER_TEMP` で cache 対象外
- Windows 配布バイナリ（既定・軽量）の動的リンク検査（TASK-93.5・`PLUG-1`・Issue #365）: 判定の正本は `scripts/check-dynamic-link-windows.sh`（`make check-dynamic-link-windows`。許可リストは `scripts/windows-dll-allowlist.txt`。`dumpbin -dependents` で通常・delay load の DLL を照合）。#363 のオーナー判断で当面は記録のみ（検査不能はモードに関係なく NG。CRT 系 DLL が出たら `+crt-static`〔#917〕の確認用 warning）。許可リスト確定後に `WINDOWS_DLL_MODE` の既定（`scripts/harness.sh`）を `enforce` へ切り替える。CI では `harness-windows` の `[dynamic-link-windows]` ステップ（PR では skipped。main・`workflow_dispatch` のみ）が実行し、`harness-ubuntu` は合成データの自己テストだけを PR でも回す。`make ci` には含めない。ビルド先は `RUNNER_TEMP` で cache 対象外
- ライセンス検査は `rust-lint` ジョブの `cargo deny check advisories bans licenses sources` で行う（[licensing](./licensing.md)）。Servo ホストは同ジョブの `scripts/deny.sh servo-host` ステップ（servo 時。`servo-host/deny.toml`。OSS-2・OSS-3）
- 対象サイト群の動作率回帰チェック（`REPAIR-8`・`COMPAT-1`・`COMPAT-4`。TASK-9.2）:
  `make check-compat-regression`（CI では `harness-*` ジョブの `[compat-regression]` ステップ）が
  `harness/compat-regression/check-matrix.sh --threshold 70 --categories static,spa,form` で
  全体と static/spa/form（存在必須として明示列挙）の動作率が閾値 70% 以上かを判定する。
  lazy・table は実測が 70% 未満のため判定対象に含めていない（閾値は緩めていない。含める基準変更は人間判断が必要なため別 Issue で扱う）。
  実マトリクス `harness/compat-practical/results/matrix.json` は実測（`run_core.sh`）から
  `make_matrix.sh` で生成してコミット済み（TASK-71.3・#920）。`--allow-missing` は渡さず、
  ファイル不在は exit 2（fail-closed）。手編集せず、再計測して再生成する
  スキーマ契約・終了コードは `harness/compat-regression/README.md` を参照
- コミット済み実マトリクスの生成スクリプト `harness/compat-practical/make_matrix.sh`（TASK-71.3・Issue #312・#920。`core_results.jsonl` と PoC-9 の Chromium 参照データ `reference/chromium_results.json` を `id` で突合。CI では `--validate-only` で入力の整合のみ検査。詳細は `harness/compat-practical/README.md`）
- 実測ハーネス compat-practical の自己テスト（`MEAS-4`。TASK-71.1・TASK-71.2・TASK-71.3）は `make check-compat-practical`（`make ci` に含む。要 jq・node 22 以降）。CI では `harness-*` ジョブの `[compat-regression]` ステップ内で `harness/compat-practical/self-test.sh` を実行する。`run_core.sh` の自己テストは偽 CDP サーバー（`fake_cdp_server.mjs`）のみを使い、実バイナリ・ネットワークへは出ない
- 許可外ライセンス（GPL/AGPL/LGPL/MPL-2.0・ライセンス未記載）を持つ canary を `cargo deny` が reject することを `make check-deny-license-reject` で検証する（TASK-9.1・REPAIR-8）
- コンテナ同時起動の集約メモリ測定スクリプトのオフライン自己テスト（`CTR-3`・`CTR-7`・`MEAS-6`。TASK-67・#929）は `make check-container-memory`（`make ci` に含む。要 jq・Linux のみ）。CI では `harness-ubuntu` の `[container-memory-selftest]` ステップが `harness/container-memory/self-test.sh` を実行する。偽 docker シムと偽の cgroup / proc / meminfo を使い、実 docker・実測は行わない。測定対象がホストの cgroup v2・`/proc` で Linux 固有のため ubuntu のみ（`[static-link]` と同じ。3 OS 共通の検査を特定 OS だけ飛ばすものではない）。実測（`make measure-container-memory`）は専用マシンで人間が実行し、`make ci` と CI には含めない
- Puppeteer 接続試験ハーネスのオフライン自己テスト（`CDP-3`。TASK-45.2）は `make check-puppeteer-connect`（`make ci` に含む。要 node）。CI では `harness-*` ジョブの `[puppeteer-connect-selftest]` ステップが `harness/puppeteer-connect/self-test.sh` を実行する。`stages.mjs` と Rust パーサーの契約テスト `cdp3_stages_mjs_result_line_satisfies_rust_contract`（`tests/puppeteer_contract.rs`。要 node）は `#[ignore]` で既定の `cargo test` から外れ、self-test.sh が `--ignored` 付きで実行する（0 件実行は fail-closed）
- feature 無効（既定）時のリリースバイナリサイズが `RENDER-2`（基準は `CORE-2` と同じ「Chromium 比 80% 以上削減」）の上限以下かを `make check-binary-size`（CI では `harness-*` ジョブの `[binary-size]` ステップ・3 OS）で検証する（TASK-34.2・TASK-34.3）。既定 package `fandhe-browser-cli` が workspace に無い場合は exit 2（fail-closed。#633。詳細は `harness/binary-size/README.md`）

## 手動計測 workflow（`xos-measure.yml`。required に含めない）

- `on: workflow_dispatch` のみ。`ci.yml` とは別 workflow で、PR・push では動かない。人間担当の計測 TASK-49（#292。CLI コンフォーマンステスト `harness/cli-conformance/`）と TASK-59（#294。PoC-1 代表タスク `harness/compat-practical/repeat_core.sh`）を 3 OS の GitHub ホステッドランナーで実行する（オーナー承認 2026-10-29: 実機の代替）。結果は Step Summary とアーティファクトに出すだけで、合否・OS 間の差の判定は人間が行う
- ここの check-run は required に登録しない（PR で pending のままになりマージできなくなる）。ジョブ名・構成の変更で ruleset の更新は要らない。`ci.yml` には手を入れない（`ci.yml` のジョブ追加・改名は ruleset の更新を伴う）。起動例は `gh workflow run xos-measure.yml -f suite=cli-conformance`

## ワークフロー変更時の注意

- ジョブ追加・変更時は [github-actions](./github-actions.md)（runner 方針・発火条件・追加チェックリスト・計測方法）に従う

- GitHub Actions のサードパーティ action はコミット SHA で固定し、新しい action を安易に追加しない（cache は `actions/cache`〔`/restore`・`/save` を含む〕、toolchain は `dtolnay/rust-toolchain` のみ）
- `permissions` は最小権限で明示する
- 出る check-run の名前を変える変更（ジョブ・`name:` の追加 / 改名）は ruleset の required checks との同時更新が必要。reusable workflow 呼び出し（`caller / callee` 形式の名前）や matrix は required にするジョブでは使わない
- secrets を `pull_request` イベントのログへ出力しない
- CI 設定の変更は infra-builder が担当し、reviewer / security-auditor のレビューを経る

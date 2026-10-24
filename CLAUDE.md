# CLAUDE.md

## Overview

Rust 製の軽量・ミニマムなブラウザの実装リポジトリ。AI エージェントによるブラウザ操作・クラウド上のスクレイピングを、Chromium のサイズ・バージョン組み合わせ問題・トークン過剰消費なしに実現することを目指す。

- **本リポは public**。仕様・ビヘイビア定義の SSOT は private リポ [fandhe-browser-spec](https://github.com/Fandhe-AI/fandhe-browser-spec)（`docs/spec` submodule の `04-behavior/`）。spec の内容は本リポに載せてよい（機密なし）が、ビヘイビア ID を併記して SSOT へ辿れるようにする（[spec-reference](.claude/rules/spec-reference.md)）。spec 本文に残る旧称 `rust-browser` は `fandhe-browser` に読み替える
- 実装方針の要点は README「実装方針（要点）」を参照（二層アーキテクチャ・V8 既定 / boa 切替・AI 最適化 API・CDP 互換・プラグイン境界・プロファイル分離・3 OS 一級対応・AI 自己補修）
- 依存は最小・`=x.y.z` 完全固定・ユーザー承認制（[dependency-policy](.claude/rules/dependency-policy.md)）。ライセンスは MIT OR Apache-2.0、Servo（MPL-2.0）は本体の workspace に入れず、別 workspace・別プロセスの Servo ホストに隔離する（[licensing](.claude/rules/licensing.md)）
- **実装の着手はユーザーの明示指示を経てから**行う。タスク定義は spec の `05-tasks.md`（TASK-n）、マイルストーンは `06-roadmap.md`（MS-n）
- 進捗・ステータスは本ファイルに逐次記録しない（Issue で管理する）

## Repository Structure

「（予定）」は spec のタスク定義に基づく計画上の配置で、まだ存在しない。

```text
fandhe-browser/
├── CLAUDE.md                      # Claude 運用方針（本ファイル）
├── AGENTS.md                      # ビルド・テスト・回帰確認コマンド / AI PR レビュー観点集（REPAIR-7）
├── README.md                      # 概要・実装方針（要点）・開発環境構築
├── LICENSE-MIT / LICENSE-APACHE   # デュアルライセンス
├── NOTICE                         # サードパーティ帰属表示・MPL 適用範囲（Servo ホストの配布物）。全文は LICENSE-THIRD-PARTY
├── rust-toolchain.toml            # stable + rustfmt/clippy（単一真実源）
├── .cargo/config.toml             # Windows（msvc）限定の `+crt-static`（#917・PLUG-1）。cwd から遡って効くため servo-host/ にも及ぶ（`servo-host/.cargo/config.toml` で打ち消す）
├── .editorconfig                  # インデント・改行・文字コード規約
├── skills-lock.json               # 導入スキルのロックファイル
├── Makefile                       # 開発タスク集約の薄い入口（処理の定義は scripts/。`make help`。編集は makefile.md）
├── lefthook.yml                   # git hooks（整形・秘密情報検査・commit-msg。clippy・test は CI に任せ pre-push は置かない）
├── commitlint.config.mjs          # commitlint 設定（type を 9 種に限定）
├── Dockerfile / compose.yaml      # 開発コンテナ（環境非依存の `make docker-ci`）
├── docker/                        # 配布用 Dockerfile（Dockerfile.release: 既定コンテナ（V8）・軽量コンテナ（`--target lite`・boa のみ）・musl 静的・scratch。x86_64 必須・aarch64 任意〔`--platform linux/arm64`〕。TASK-64.1・64.2・64.3・CTR-1・CTR-4）
├── .markdownlint.jsonc / .yamllint / .editorconfig-checker.json  # lint-docs 設定
├── Cargo.toml                     #（予定）workspace 定義
├── about.toml                     # cargo-about 設定（`make license-inventory`。全依存の解決後ライセンス一覧・OSS-4・TASK-78.2。deny.toml と同期。Servo ホスト側は servo-host/about.toml）
├── deny.toml                      # cargo-deny 設定（ライセンス・advisories 検査。本体用で MPL-2.0 は未許可。Servo ホスト側は servo-host/deny.toml）
├── crates/                        # 各 crate（mcp は stdio サーバー基盤のみ）
│   ├── fandhe-browser-core/       #   fetch・HTML パース・DOM・query・CSSOM・config・可観測性
│   ├── fandhe-browser-js/         #   JS エンジン抽象（V8 既定 / boa）
│   ├── fandhe-browser-ai/         #   AI 最適化 API（アクセシビリティツリー・簡約 DOM）・プラグイン API
│   ├── fandhe-browser-cdp/        #   CDP 互換サーバー
│   ├── fandhe-browser-render/     #   レンダリング層（Servo ホストへの IPC クライアント: `HostRenderer`・子プロセス管理・プロトコル。Servo 自体は `servo-host/`。licensing.md）
│   ├── fandhe-browser-profile/    #   プロファイル分離
│   ├── fandhe-browser-cli/        #   CLI（最小雛形: core・profile・cdp・ai を組み立ててサーバー起動。サブコマンドは TASK-47）
│   └── fandhe-browser-mcp/        #   MCP 参照プラグイン（別バイナリ。stdio サーバー基盤・起動時のホスト自己申告と navigate・snapshot ツールを実装）
├── servo-host/                    # Servo ホスト（別 workspace・別 Cargo.lock・本体 members の外でルート exclude。バイナリ fandhe-browser-servo-host。TASK-38・RENDER-1）
│   ├── Cargo.toml / Cargo.lock    #   自前の [workspace]（単一 package）。本体と独立
│   ├── .cargo/config.toml         #   Windows（msvc）の `-crt-static`。ルートの `+crt-static` を cwd が servo-host/ のときだけ打ち消す（mozjs_sys は動的 CRT 前提）。CI は `--manifest-path` で呼ぶため `servo-host-windows` の `CARGO_ENCODED_RUSTFLAGS` が担う
│   ├── deny.toml                  #   MPL-2.0 等の例外許可はここだけ
│   ├── about.toml                 #   cargo-about 設定（ホスト用。MPL-2.0 許可）
│   ├── src/                       #   main・server・engine・page・screenshot・layout
│   └── tests/                     #   結合テスト
├── tests/ / benches/              #（予定）結合テスト・ベンチ
├── scripts/                       # Makefile の実体（lib.sh・versions.sh・cargo-gate.sh・harness.sh・lint-docs.sh・setup.sh・deny.sh・about.sh・help.sh 等）と依存グラフ検査等のスクリプト。check-render-isolation.sh（TASK-34.1・RENDER-1）・check-servo-host-isolation.sh（TASK-38・RENDER-1・servo-host が本体 members に無いこととルート Cargo.lock に Servo 系が無いことの検査）・check-js-engine-isolation.sh（TASK-32.4・JS-1）・check-static-link.sh（TASK-93.3・PLUG-1・musl 配布バイナリの動的リンクなし検査）・check-dynamic-link-macos.sh（TASK-93.4・PLUG-1・macOS 配布バイナリの動的リンクを許可リストと照合。当面は記録のみ）・check-dynamic-link-windows.sh（TASK-93.5・PLUG-1・Windows 配布バイナリの動的リンクを許可リストと照合。当面は記録のみ）・ci-changes.sh（CI の docs-only 判定）・ci-prune-target.sh（CI cache 保存前の prune）
├── profiles/                      # CSSOM gating 用ブラウザ挙動プロファイルデータ（chrome.json・safari.json・README.md。TASK-100.1・PLUG-8）
├── harness/                       # 互換性テストハーネス。compat-regression/（TASK-9.2・対象サイト群の動作率回帰チェック）・playwright-trace/（TASK-43.1・Playwright newPage() 到達までの CDP トレース収集）・compat-practical/（TASK-71.1・22 タスク定義・アクセス確認・実行対象バイナリ解決・TASK-71.2・22 タスク実行 run_core.sh・TASK-71.3・Chromium 実測との突合 make_matrix.sh・TASK-72.1・類型別動作率集計 aggregate_by_type.sh）・playwright-connect/（TASK-44.1・Playwright 接続試験の実行基盤）・puppeteer-connect/（TASK-45.1・Puppeteer 接続試験の実行基盤・TASK-45.2・段階別到達結果の回収）・binary-size/（TASK-34.2・既定 feature リリースバイナリのサイズ検査・TASK-31.1・JS エンジン構成別サイズ計測）・container-size/（TASK-65.1・コンテナイメージサイズ計測）・container-smoke/（TASK-66.1・CTR-2・軽量 / 既定コンテナでの代表タスク確認。`query --file` を使用。旧バイナリでは blocked）・container-js-smoke/（TASK-66.3・CTR-2・既定コンテナ内の JS スモーク。ワーカープロトコル経由）・container-memory/（TASK-67・コンテナ同時起動の集約メモリ測定）実装済み
├── docs/
│   ├── design/                    # 設計ドキュメント（public。servo-host-design.md・render-host-protocol.schema.json 等）
│   ├── setup/                     # 環境別セットアップ手順書（windows.md）
│   └── spec/                      # fandhe-browser-spec submodule（private・要アクセス権）
├── .github/workflows/             # ai-review・update-external・ci（push/pull_request で稼働・PR は ubuntu＋`cross-target-check`＋OS 依存の変更〔profile crate・cfg 差分〕のときだけ macOS・Windows の `rust-test`、main push で 3 OS・docs-only 判定は `scripts/ci-changes.sh`）/ release（発火条件は workflow_dispatch 限定のまま）
├── .agents/skills/                # npx skills add の導入実体
└── .claude/
    ├── agents/                    # カテゴリ別 subagent 定義
    ├── rules/                     # 運用ルール
    ├── skills/                    # 導入スキル（.agents/skills への symlink）
    ├── workflows/                 # implement-issue-tree.js（相対 symlink）
    └── settings.json              # SessionStart / PostToolUse hooks
```

## 委譲方針（必読）

main セッションはオーケストレーションに徹し、調査・実装・レビューは subagent へ委譲してコンテキスト消費を抑える。詳細は [delegation](.claude/rules/delegation.md)（調査）・[delegation-impl](.claude/rules/delegation-impl.md)（実装）を参照。

### パスベース切り替え表

| 対象 | 調査 | 作成・編集 |
| ---- | ---- | ---------- |
| `crates/fandhe-browser-core/` | explorer | core-builder |
| `crates/fandhe-browser-js/` | explorer | js-engine-builder |
| `crates/fandhe-browser-ai/`・`crates/fandhe-browser-mcp/` | explorer | ai-api-builder |
| `crates/fandhe-browser-cdp/` | explorer | cdp-builder |
| `crates/fandhe-browser-render/`・`servo-host/`（`deny.toml` を除く） | explorer | render-builder |
| `crates/fandhe-browser-profile/`・OS 差異吸収 | explorer | profile-platform-builder |
| `crates/fandhe-browser-cli/`・`Cargo.toml`・CI・`deny.toml`（`servo-host/deny.toml` を含む）・`NOTICE`・`Dockerfile`・`benches/`・`harness/` | explorer | infra-builder |
| `docs/spec/`（private） | explorer | 変更しない（spec リポ側で管理） |
| 外部仕様（WHATWG・CDP・V8・boa・Servo・MCP） | reference-researcher | — |
| テスト・lint | test-runner / linter | — |
| ドキュメント・`AGENTS.md`・`.claude/`（agents・rules・settings.json） | explorer | docs-writer |

### model 配分表

| 用途 | model |
| ---- | ----- |
| 複雑な横断判断・アーキテクチャ設計（crate 境界・依存方向・エンジン抽象） | opus または fable（fable は特に大規模設計・横断判断の最上位 tier） |
| 調査・生成・実装・レビュー | sonnet |
| 機械的集計・lint・ドキュメント更新 | haiku |

## Sub-agents

| カテゴリ | subagent_type | model | 役割 |
| -------- | ------------- | ----- | ---- |
| research | explorer | sonnet | コードベース・spec 横断調査 |
| research | reference-researcher | sonnet | 外部仕様・依存候補クレートの調査 |
| implement | core-builder | sonnet | core crate（fetch・パース・DOM・CSSOM・config・可観測性） |
| implement | js-engine-builder | sonnet | js crate（エンジン抽象・V8 FFI・boa） |
| implement | ai-api-builder | sonnet | ai crate（AI 最適化 API・プラグイン API）・mcp crate |
| implement | cdp-builder | sonnet | cdp crate（CDP 互換サーバー・Playwright/Puppeteer 互換） |
| implement | render-builder | sonnet | render crate（Servo ホストへの IPC クライアント）・`servo-host/`（Servo 組込・MPL 隔離） |
| implement | profile-platform-builder | sonnet | profile crate・OS 差異吸収 |
| implement | infra-builder | sonnet | cli crate・workspace・3 OS CI・deny/NOTICE・Dockerfile・benches・harness |
| testing | test-runner | sonnet | cargo test / clippy 実行と失敗解析 |
| quality | reviewer | sonnet | 設計原則・AI 自己補修性・規約準拠のレビュー |
| quality | security-auditor | sonnet | 偽装機能・プロファイル境界・外部入力・unsafe・OWASP 監査 |
| quality | linter | haiku | rustfmt / clippy / cargo deny 等の機械的確認 |
| docs | docs-writer | haiku | README・CLAUDE.md・AGENTS.md・docs/design・.claude/ 更新 |

## Rules

| ファイル | 内容 |
| -------- | ---- |
| [delegation.md](.claude/rules/delegation.md) | 調査フェーズの委譲原則・パスベース切り替え |
| [delegation-impl.md](.claude/rules/delegation-impl.md) | 実装フェーズの委譲マッピング・標準フロー・着手条件（明示指示・人間担当タスク） |
| [coding-rust.md](.claude/rules/coding-rust.md) | Rust 規約（crate 境界・外部入力・unsafe/FFI・クロスプラットフォーム・テスト） |
| [security.md](.claude/rules/security.md) | 秘密情報・偽装機能禁止・プロファイル / プラグイン境界・OWASP Top 10 |
| [japanese-style.md](.claude/rules/japanese-style.md) | 日本語出力スタイル |
| [conventional-commits.md](.claude/rules/conventional-commits.md) | Conventional Commits 詳細規約（type/scope 一覧） |
| [code-comment-style.md](.claude/rules/code-comment-style.md) | コメント規約（役割・呼び出し文脈・スタブの将来仕様） |
| [out-of-scope-tracking.md](.claude/rules/out-of-scope-tracking.md) | スコープ外事項の Issue 追跡フロー |
| [spec-reference.md](.claude/rules/spec-reference.md) | **リポ固有**: spec（SSOT）の参照・ID 併記・編集禁止 |
| [dependency-policy.md](.claude/rules/dependency-policy.md) | **リポ固有**: 依存最小・`=x.y.z` 固定・ユーザー承認制 |
| [licensing.md](.claude/rules/licensing.md) | **リポ固有**: デュアルライセンス・Servo MPL 隔離・GPL 系禁止・NOTICE |
| [ci.md](.claude/rules/ci.md) | **リポ固有**: ローカルゲート・3 OS CI・feature 分離検証 |
| [makefile.md](.claude/rules/makefile.md) | **リポ固有**: Makefile は薄い入口・処理は scripts/ に置く・ターゲット追加手順・スクリプト規約 |
| [github-actions.md](.claude/rules/github-actions.md) | **リポ固有**: runner 方針（public → GitHub-hosted）・必須チェック・発火条件・ジョブ追加チェックリスト・キャッシュ方針（上限 10GB・key 設計・保存条件）・所要時間 |
| [git-hooks.md](.claude/rules/git-hooks.md) | **リポ固有**: lefthook のフック編集方針（時間予算・CI と重複する検査を置かない・明示的な例外） |

## Current Skills

`npx skills add`（Fandhe-AI/agent-cli-skills・Fandhe-AI/agent-reference-skills）で導入済み。ロックは `skills-lock.json`。

- **ワークフロー系**: create-commit / create-pr / create-issue / create-issue-tree / create-plan / implement-issue / implement-issue-tree / implement-review / implement-review-pr / update-issue-tree / update-docs / comment-code
- **メンテ系**: init-claude / update-claude / contribute-skill / setup-repo-guards
- **リファレンス系**: make / rust / github-docs / commitlint / lefthook / editorconfig / anthropic-claude-code / anthropic-claude-code-extend / anthropic-api-tools-mcp / anthropic-agent-sdk / openai-agents / playwright / fandhe-backend / windows-interop-modernize

## Conventions

- **ローカル検証**: `cargo fmt --all --check`・`cargo clippy --workspace --all-targets -- -D warnings`・`cargo test --workspace` を通してからコミットする（[ci](.claude/rules/ci.md)）。ビルド・テストは `docs/spec` 抜きで成立させる
- **日本語**: やりとり・報告・コミット説明文・コード内コメントは日本語（プログラム出力文字列は英語）
- **Conventional Commits**: `--no-verify` 禁止
- **セキュリティレビュー**: PR 作成前に OWASP Top 10＋偽装機能・プロファイル境界を確認
- **ユーザー承認フロー**: 実装の着手 / 依存の追加・更新 / `unsafe` の新規追加 / ライセンス判断 / Issue 起票 / 既存ファイル上書き / implement-issue の実装開始（計画承認後）は必ずユーザー承認を経る
- **spec 参照**: `docs/spec` の内容を引用・要約する際は TASK-n・ビヘイビア ID（`<PREFIX>-<N>`）・MS-n を併記する。`docs/spec` は本リポから編集しない
- **implement-issue-tree**: `.claude/workflows/implement-issue-tree.js`（相対 symlink）を named workflow として利用できる

## hooks（settings.json）

- **SessionStart**: 日本語・委譲・Conventional Commits・`--no-verify` 禁止・spec 参照（ID 併記）・依存承認制・実装着手条件のリマインダーを表示
- **PostToolUse**（Edit|Write）: `*.rs` 編集後に rustfmt で自動整形。edition はルート `Cargo.toml` から取得し（未作成時は 2024）、jq / rustfmt 未導入時は何もしない。整形失敗で作業を止めない

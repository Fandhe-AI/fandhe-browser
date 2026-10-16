# fandhe-browser の開発タスクランナー。
#
# Make は scripts/*.sh（と 1 行で済む docker compose 呼び出し）を呼ぶだけの薄い入口であり、
# 処理の定義（条件分岐・ツール導入・複数行シェル）は scripts/ 側にのみ置く。各レシピは 1 行の
# 呼び出しに限定し、ここでチェック内容を二重定義しない。Make を使わない場合の同等コマンドは
# scripts/help.sh を参照。変更時は .claude/rules/makefile.md に従う。
#
# `make setup` 一発で開発環境（サブモジュール・rustup・lefthook）を構築し、
# `make ci` でローカル検証（.claude/rules/ci.md のローカルゲート）を一括実行する。
# 実装が未整備の段階（Cargo.toml・member crate・deny.toml が無い等）では cargo 系ターゲットは
# scripts/cargo-gate.sh が skip し、揃った時点で自動的に有効化される（冪等セルフヒール）。
# Docker で環境非依存に開発・検証する場合は docker-* ターゲットを使う（compose.yaml 参照）。
# ツール版・上限値の正本は scripts/versions.sh（環境変数または `make <target> VAR=value` で上書き可）。
#
# GNU Make 3.81（macOS 標準）で動作する。3.82+ 専用機能（.ONESHELL・.RECIPEPREFIX 等）は使わない。
#
# 使い方: `make help`（既定ターゲット）でターゲット一覧を表示する。
# 一覧は下記の `## ` コメントから生成されるため、`## ` の無いターゲットは表示されない。
# ターゲットを追加・改名・削除したら scripts/help.sh も同じ変更で更新すること。
# 中身の無いターゲット（`@true` 等）は呼び出し元が終了コードを信用してしまうため置かない。
# Fandhe-AI/rust-ai-library の Makefile と同一方針。

.DEFAULT_GOAL := help

.PHONY: help setup rustup submodule hooks \
	lint-md lint-yaml lint-editorconfig lint-commits lint-docs \
	check-workspace-manifest fmt fmt-check lint test test-harness lint-rendering test-rendering \
	check-render-isolation check-js-engine-isolation check-publish-private deny deny-servo-host license-inventory license-inventory-servo-host check-deny-license-reject ci \
	check-compat-practical check-compat-regression bench-record check-ci-changes check-bench-record \
	check-binary-size check-static-link measure-mcp-envelope check-mcp-envelope measure-plugin-overhead check-plugin-overhead measure-js-binary-size measure-container-size measure-cssom-profile-cost \
	docker-build docker-shell docker-ci trace-playwright check-playwright-trace check-puppeteer-connect check-playwright-connect test-playwright-connect

help: ## ターゲット一覧を表示する（Make 非依存の一覧: scripts/help.sh）
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(firstword $(MAKEFILE_LIST)) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-28s\033[0m %s\n", $$1, $$2}'

# --------------------------------------------------
# 環境構築
# --------------------------------------------------

setup: ## 開発環境を一括構築する（サブモジュール → rustup → lefthook の順を保証）
	@scripts/setup.sh

rustup: ## rustup（cargo）の導入を確認する（未導入なら公式手順を案内して停止）
	@scripts/rustup-check.sh

submodule: ## docs/spec サブモジュールを初期化・更新する（private・アクセス権が無ければ警告のみ）
	@scripts/submodule.sh

hooks: ## lefthook の git hooks を導入する（未導入なら lefthook 本体も導入）
	@scripts/install-hooks.sh

# --------------------------------------------------
# ドキュメント／設定ファイル系 lint（CI の lint-docs ジョブと同等の内容）
# --------------------------------------------------

lint-md: ## markdownlint（.markdownlint.jsonc / .markdownlintignore 参照）
	@scripts/lint-docs.sh md

lint-yaml: ## yamllint（.yamllint 参照）
	@scripts/lint-docs.sh yaml

lint-editorconfig: ## editorconfig-checker（.editorconfig + .editorconfig-checker.json 参照）
	@scripts/lint-docs.sh editorconfig

lint-commits: ## commitlint（origin/main からの分岐点以降のコミットを検証）
	@scripts/lint-docs.sh commits

lint-docs: lint-md lint-yaml lint-editorconfig lint-commits ## ドキュメント／設定ファイル系 lint を一括実行する

# --------------------------------------------------
# 品質チェック（Rust。Cargo.toml・member crate 追加後に有効化）
# --------------------------------------------------

check-workspace-manifest: ## cargo verify-project で workspace manifest の妥当性を検証する
	@scripts/cargo-gate.sh check-workspace-manifest

fmt: ## cargo fmt --all で整形する
	@scripts/cargo-gate.sh fmt

fmt-check: ## cargo fmt --check（整形差分の検出）
	@scripts/cargo-gate.sh fmt-check

lint: ## cargo clippy -D warnings（既定 feature。lint ゲート）
	@scripts/cargo-gate.sh lint

test: ## cargo test（既定 feature。workspace 全体）
	@scripts/cargo-gate.sh test

test-harness: ## harness/render-screenshot のユニットテスト・結合テストを実行する（Python 標準ライブラリのみ）
	@scripts/harness.sh test-harness

lint-rendering: ## cargo clippy -D warnings（--features rendering。Servo 込みの検証）
	@scripts/cargo-gate.sh lint-rendering

test-rendering: ## cargo test（--features rendering。Servo 込みの検証）
	@scripts/cargo-gate.sh test-rendering

check-render-isolation: ## 既定ビルド（cli 既定 feature 含む）の依存グラフに Servo 系クレートが含まれないことを検証する
	@scripts/cargo-gate.sh check-render-isolation

check-js-engine-isolation: ## 軽量ビルド・エンジンなし構成の依存グラフに v8（エンジンなしは boa_engine も）が含まれないことを検証する
	@scripts/cargo-gate.sh check-js-engine-isolation

check-publish-private: ## workspace 内の全 member crate が publish = false であることを検証する
	@scripts/cargo-gate.sh check-publish-private

deny: ## cargo deny check advisories bans licenses sources（本体と servo-host の両方。cargo-deny 未導入なら自動導入）
	@scripts/deny.sh

deny-servo-host: ## cargo deny check（servo-host のみ。servo-host/deny.toml を適用）
	@scripts/deny.sh servo-host

license-inventory: ## cargo about で全依存ツリーの解決後ライセンス一覧を生成し GPL 系 0 件を確認する（本体と servo-host。_/license-inventory/ に出力）
	@scripts/about.sh

license-inventory-servo-host: ## cargo about（servo-host のみ。servo-host/about.toml を適用）
	@scripts/about.sh servo-host

check-deny-license-reject: ## cargo deny が許可外ライセンスを reject することを canary で検証する（受入基準2の証跡）
	@scripts/check-deny-license-reject.sh

# `rendering` feature（Servo）を含めた検証（ci.md「既定ビルドと --features rendering の両方で検証」）も
# make ci に含める。lint-rendering / test-rendering は feature 未定義の段階では skip して 0 終了するため、
# workspace 作成前・render crate 追加前の CI を壊さない。docker-ci は make ci を呼ぶため自動的に含む。
ci: lint-docs check-workspace-manifest fmt-check lint lint-rendering check-render-isolation check-js-engine-isolation check-publish-private test test-rendering deny check-deny-license-reject check-compat-regression check-compat-practical check-puppeteer-connect check-playwright-connect check-bench-record check-mcp-envelope check-ci-changes ## ローカルゲート（.claude/rules/ci.md）と同等のチェックを一括実行する

# --------------------------------------------------
# 実測ハーネス・ベンチ・計測（各 README: harness/*/README.md・benches/*/README.md）
# --------------------------------------------------

check-compat-practical: ## 実測ハーネス（22 タスク定義・アクセス確認・run_core 実行）の自己テストとスキーマ検証（MEAS-4）
	@scripts/harness.sh check-compat-practical

check-compat-regression: ## 対象サイト群の動作率マトリクスに対する回帰チェック（COMPAT-1・COMPAT-4）
	@scripts/harness.sh check-compat-regression

bench-record: ## competitor_lightpanda ベンチを実行し結果を時系列で記録する
	@scripts/harness.sh bench-record

check-ci-changes: ## CI の docs-only 判定・cache prune スクリプトの自己テスト
	@scripts/harness.sh check-ci-changes

check-bench-record: ## competitor_lightpanda 記録スクリプトの自己テスト
	@scripts/harness.sh check-bench-record

check-binary-size: ## feature 無効（既定）のリリースバイナリサイズが CORE-2 水準の上限以下か検査する（RENDER-2）
	@scripts/harness.sh check-binary-size

check-static-link: ## musl 配布バイナリ（既定・軽量）に動的リンクが無いことを検査する（PLUG-1・CTR-4・TASK-93.3。Linux のみ・要 musl ターゲットと musl-tools）
	@scripts/harness.sh check-static-link

measure-mcp-envelope: ## 方式 B 本体と MCP エンベロープのトークン・レイテンシを測定する（PLUG-5・TASK-97.1。unix のみ）
	@scripts/harness.sh measure-mcp-envelope

check-mcp-envelope: ## MCP エンベロープ測定ハーネスの e2e smoke を実バイナリで実行する（PLUG-5・TASK-97.1。unix のみ）
	@scripts/harness.sh check-mcp-envelope

measure-plugin-overhead: ## mcp プラグインのアイドル RSS 増分・cold start・呼び出しレイテンシを測定する（PLUG-3・TASK-95.1・95.2・95.3。unix のみ。PLUGIN_OVERHEAD_ARGS で引数指定）
	@scripts/harness.sh measure-plugin-overhead

check-plugin-overhead: ## プラグイン性能計測（RSS 増分・cold start・呼び出しレイテンシ）の e2e smoke を実バイナリで実行する（PLUG-3・TASK-95.1・95.2・95.3。unix のみ）
	@scripts/harness.sh check-plugin-overhead

measure-js-binary-size: ## JS エンジン構成別（既定 V8 / 軽量 boa / エンジンなし）のリリースバイナリサイズを計測する（TASK-31.1）
	@scripts/harness.sh measure-js-binary-size

measure-container-size: ## 既定・軽量コンテナと比較対象（Playwright 公式）イメージのサイズを計測する（TASK-65.1・CTR-1・CTR-5。要 docker・jq）
	@scripts/harness.sh measure-container-size

measure-cssom-profile-cost: ## CSSOM プロファイル読み込みのバイナリ増分・アイドル RSS 増分を計測する（TASK-100.6・PLUG-8）
	@scripts/harness.sh measure-cssom-profile-cost

trace-playwright: ## Playwright newPage() 到達までの CDP トレースを再取得する（手動・要 node/npm/ネットワーク。CI 対象外）
	@scripts/harness.sh trace-playwright

check-playwright-trace: ## harness/playwright-trace のオフライン自己テストを実行する（Playwright・ネットワーク不要。要 node）
	@scripts/harness.sh check-playwright-trace

check-puppeteer-connect: ## harness/puppeteer-connect のオフライン自己テストと Rust 契約テスト（puppeteer_contract）を実行する（要 node。TASK-45.2）
	@scripts/harness.sh check-puppeteer-connect

check-playwright-connect: ## harness/playwright-connect のオフライン自己テストと Rust 契約テスト（playwright_contract）を実行する（要 node。TASK-44.1）
	@scripts/harness.sh check-playwright-connect

test-playwright-connect: ## 実 Playwright で connectOverCDP→newContext→newPage→goto→セレクタ取得を検証する（手動・要 node/npm/ネットワーク。CI 対象外。TASK-44.1〜44.3）
	@scripts/harness.sh test-playwright-connect

# --------------------------------------------------
# Docker（環境非依存の開発・検証。詳細は compose.yaml / Dockerfile 参照）
# --------------------------------------------------

docker-build: ## 開発コンテナイメージをビルドする
	docker compose build

docker-shell: ## 開発コンテナのシェルに入る
	docker compose run --rm dev

docker-ci: ## コンテナ内で make ci を実行する（環境非依存の検証）
	docker compose run --rm dev make ci

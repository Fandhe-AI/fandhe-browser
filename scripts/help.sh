#!/usr/bin/env bash
# Make を介さない操作一覧。`make help` は Makefile の `## ` コメントから機械的に生成されるのに対し、
# 本ファイルは手動で保守する一覧であり、両者を自動で同期する仕組みは無い。
# Makefile のターゲットを追加・改名・削除したら同じ変更で本ファイルも更新すること。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

reject_args "$(basename "$0")" "$@"

cat <<'LIST'
このリポジトリの操作と、Make を使わずに直接実行する方法:

環境構築
  setup                      scripts/setup.sh                              サブモジュール → rustup 確認 → lefthook の順に構築
  rustup                     scripts/rustup-check.sh                       rustup の導入確認（自動導入はしない）
  submodule                  scripts/submodule.sh                          docs/spec サブモジュールの初期化・更新
  hooks                      scripts/install-hooks.sh                      lefthook の git hooks 導入

ドキュメント／設定ファイル lint
  lint-md                    scripts/lint-docs.sh md                       markdownlint
  lint-yaml                  scripts/lint-docs.sh yaml                     yamllint
  lint-editorconfig          scripts/lint-docs.sh editorconfig             editorconfig-checker
  lint-commits               scripts/lint-docs.sh commits                  commitlint（origin/main 分岐点以降）
  lint-docs                  （上記 4 つを順に実行）                        ドキュメント／設定ファイル系 lint 一括

Rust 品質チェック（Cargo.toml・member crate が無い間は skip）
  check-workspace-manifest   scripts/cargo-gate.sh check-workspace-manifest  cargo verify-project
  fmt / fmt-check            scripts/cargo-gate.sh fmt | fmt-check         整形 / 整形差分の検出
  lint / test                scripts/cargo-gate.sh lint | test             clippy -D warnings / cargo test（既定 feature）
  lint-rendering / test-rendering  scripts/cargo-gate.sh lint-rendering | test-rendering  --features rendering（Servo 込み）
  check-render-isolation     scripts/cargo-gate.sh check-render-isolation  Servo 系クレート分離検証
  check-js-engine-isolation  scripts/cargo-gate.sh check-js-engine-isolation  JS エンジン分離検証
  check-publish-private      scripts/cargo-gate.sh check-publish-private   全 member が publish = false
  deny                       scripts/deny.sh                               cargo deny check
  check-deny-license-reject  scripts/check-deny-license-reject.sh         許可外ライセンス reject の canary 検証
  ci                         （上記ゲート + ハーネス自己テストを順に実行）   ローカルゲート一括

実測ハーネス・ベンチ・計測（scripts/harness.sh <名前>）
  test-harness  check-compat-practical  check-compat-regression  bench-record  check-ci-changes
  check-bench-record  check-binary-size  measure-mcp-envelope  check-mcp-envelope
  measure-js-binary-size  measure-cssom-profile-cost  trace-playwright  check-playwright-trace
  check-puppeteer-connect

Docker（1 行呼び出しのため scripts/ には包まない）
  docker-build               docker compose build
  docker-shell               docker compose run --rm dev
  docker-ci                  docker compose run --rm dev make ci

`make <target>` は上記と同じコマンドを呼ぶだけの薄い入口であり、処理の定義は scripts/ 側にのみ存在する。
ツール版・上限値は scripts/versions.sh（環境変数または `make <target> VAR=value` で上書き可）。
LIST

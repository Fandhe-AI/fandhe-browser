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
Operations in this repository and how to run them without Make:

Setup
  setup                      scripts/setup.sh                              submodule, rustup check, then lefthook (in order)
  rustup                     scripts/rustup-check.sh                       check rustup is installed (never installs it)
  submodule                  scripts/submodule.sh                          init/update the docs/spec submodule
  hooks                      scripts/install-hooks.sh                      install lefthook git hooks

Docs / config lint
  lint-md                    scripts/lint-docs.sh md                       markdownlint
  lint-yaml                  scripts/lint-docs.sh yaml                     yamllint
  lint-editorconfig          scripts/lint-docs.sh editorconfig             editorconfig-checker
  lint-commits               scripts/lint-docs.sh commits                  commitlint (since the origin/main merge base)
  lint-docs                  (runs the four above in order)                all docs/config lint

Rust checks (skipped while Cargo.toml or member crates are absent)
  check-workspace-manifest   scripts/cargo-gate.sh check-workspace-manifest  cargo verify-project + servo-host isolation
  fmt / fmt-check            scripts/cargo-gate.sh fmt | fmt-check         format / detect formatting diffs
  lint / test                scripts/cargo-gate.sh lint | test             clippy -D warnings / cargo test (default features)
  lint-rendering / test-rendering  scripts/cargo-gate.sh lint-rendering | test-rendering  --features rendering (with Servo)
  check-render-isolation     scripts/cargo-gate.sh check-render-isolation  Servo isolation check
  check-js-engine-isolation  scripts/cargo-gate.sh check-js-engine-isolation  JS engine isolation check
  check-publish-private      scripts/cargo-gate.sh check-publish-private   every member crate has publish = false
  deny                       scripts/deny.sh                               cargo deny check (main + servo-host)
  deny-servo-host            scripts/deny.sh servo-host                    cargo deny check (servo-host only)
  license-inventory          scripts/about.sh                              cargo about license list (main + servo-host), GPL-family must be 0
  license-inventory-servo-host  scripts/about.sh servo-host                cargo about license list (servo-host only)
  check-deny-license-reject  scripts/check-deny-license-reject.sh         canary check that disallowed licenses are rejected
  ci                         (runs the gates above plus harness self-tests) full local gate

Harnesses, benches, measurements (scripts/harness.sh <name>)
  test-harness  check-compat-practical  check-compat-regression  bench-record  check-ci-changes
  check-bench-record  check-binary-size  check-static-link  measure-mcp-envelope  check-mcp-envelope
  measure-plugin-overhead  check-plugin-overhead
  measure-js-binary-size  measure-container-size  check-container-smoke  measure-cssom-profile-cost  trace-playwright  check-playwright-trace
  check-puppeteer-connect  check-playwright-connect  test-playwright-connect

Docker (one-line calls, not wrapped in scripts/)
  docker-build               docker compose build
  docker-shell               docker compose run --rm dev
  docker-ci                  docker compose run --rm dev make ci

Each `make <target>` is a thin entry point that runs the command above; the logic lives only in scripts/.
Tool versions and limits live in scripts/versions.sh (override via env or `make <target> VAR=value`).
LIST

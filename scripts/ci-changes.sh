#!/usr/bin/env bash
#
# PR の変更ファイル一覧から、CI の重いジョブ群を走らせる必要があるかを判定する正本
# スクリプト（.claude/rules/ci.md「docs-only 判定」・XOS-1・REPAIR-5・REPAIR-6）。
# 呼び出し元は .github/workflows/ci.yml の changes ジョブ。push（main）・
# workflow_dispatch・merge_group の全実行扱いと git diff の失敗時 fail-closed は
# ワークフロー側で扱い、本スクリプトは「ファイル一覧 -> rust= / harness=」の純粋な
# 判定だけを担う（ローカルで --self-test できるようにするため）。
#
# 使い方:
#   printf '%s\n' docs/a.md crates/x/src/lib.rs | scripts/ci-changes.sh
#   scripts/ci-changes.sh --self-test
#
# 入力: 標準入力に 1 行 1 パス（リポジトリルート相対）。
# 出力: 標準出力に `rust=true|false`・`harness=true|false`・`xos=true|false` の 3 行
#       （GITHUB_OUTPUT へそのまま追記できる形式）。
#
# xos（PR でも macOS・Windows ジョブを走らせるか）: PR は既定で ubuntu のみ検証し、3 OS は
#   main への push と release 前に行う（.claude/rules/ci.md「3 OS の扱い」）。OS 差異の
#   影響を受けやすいパス（profile crate・.github・Cargo.toml / Cargo.lock・
#   rust-toolchain.toml・scripts）を含む PR だけ xos=true にする。`cfg(target_os)` 等の
#   差分内容による判定はワークフロー側で行い、ここでの結果と OR する。
#
# 判定（fail-closed。どれにも当てはまらないパスは rust=true）:
#   1. ビルド・検査に影響しうるパス（crates・Cargo.*・.github・scripts・Makefile・
#      deny.toml・rust-toolchain.toml・profiles・benches・tests・Dockerfile・
#      compose.yaml）-> rust=true
#   2. harness/ 配下 -> harness=true。Rust から読まれないと確認済みのディレクトリ（binary-size・
#      render-screenshot・cold-start・multi-instance-memory）以外は rust=true も立てる
#   3. docs 扱いの allowlist（*.md・docs/・.claude/・.agents/・LICENSE-*（LICENSE-THIRD-PARTY*
#      を含む）・NOTICE・skills-lock.json・lint 設定ファイル類）-> どちらも変えない
#   4. 上記以外 -> rust=true
# 入力が 1 行も無い場合は判定材料が無いため両方 true にする。
# bash の case パターンの `*` は `/` もまたぐため、`*.md` は任意階層、`docs/*` は
# docs/spec（submodule ポインタ）を含む docs/ 配下全体に一致する。
set -euo pipefail

# 1 行 1 パスの入力を判定して `rust=` / `harness=` を出力する。
classify() {
  local rust=false harness=false xos=false seen=false path
  while IFS= read -r path || [ -n "$path" ]; do
    # 末尾の CR を除去（Windows 由来の改行混入対策）。空行は無視する
    path="${path%$'\r'}"
    [ -n "$path" ] || continue
    seen=true
    case "$path" in
      # OS 名を含むファイル（*windows*・*unix*・*macos*・platform/）と .gitattributes（Windows の
      # CRLF 変換を左右する）も OS 差異の影響を受けやすいため 3 OS で検証する
      crates/fandhe-browser-profile/* | .github/* | Cargo.toml | Cargo.lock | */Cargo.toml | \
        rust-toolchain.toml | scripts/* | .gitattributes | *windows* | *unix* | *macos* | \
        */platform/*)
        xos=true
        ;;
    esac
    case "$path" in
      # docs/design/ はテスト・ビルドが include_str! / read_to_string で読む（host-api.schema.json・
      # mcp-token-reduction-report.md 等）ため、*.md・docs/* より前で rust 扱いにする
      crates/* | Cargo.* | .github/* | scripts/* | Makefile | deny.toml | \
        rust-toolchain.toml | profiles/* | benches/* | tests/* | Dockerfile | compose.yaml | \
        docs/design/*)
        rust=true
        ;;
      # Rust のテスト・ビルドから読まれないと確認済みの harness ディレクトリだけを harness のみ扱い
      # にする。それ以外の harness/ 配下（compat_fixtures・playwright-trace 等の include_str! 入力、
      # workspace メンバーの wpt_subset_runner、未知の新規ディレクトリ）は rust も true（fail-closed）
      harness/binary-size/* | harness/render-screenshot/* | harness/cold-start/* | \
        harness/multi-instance-memory/*)
        harness=true
        ;;
      harness/*)
        rust=true
        harness=true
        ;;
      *.md | docs/* | .claude/* | .agents/* | LICENSE-* | NOTICE | \
        skills-lock.json | .markdownlint.jsonc | .yamllint | .editorconfig-checker.json | \
        commitlint.config.mjs | lefthook.yml)
        ;;
      *)
        rust=true
        ;;
    esac
  done
  if [ "$seen" = false ]; then
    rust=true
    harness=true
    xos=true
  fi
  printf 'rust=%s\nharness=%s\nxos=%s\n' "$rust" "$harness" "$xos"
}

# 自己テスト。期待値は「rust=<値> harness=<値>」の 1 行表記で比較する。
self_test() {
  local fail=0
  expect() { # $1=ケース名 $2=期待値 $3=入力（改行区切り）
    local got
    got="$(printf '%s' "$3" | classify | tr '\n' ' ' | sed 's/ $//')"
    if [ "$got" = "$2" ]; then
      echo "OK(self-test): $1 -> $got"
    else
      echo "NG(self-test): $1 expected '$2' but got '$got'" >&2
      fail=1
    fi
  }
  expect "docs-only" "rust=false harness=false xos=false" $'README.md\ndocs/setup/a.md\ndocs/spec'
  expect "docs/design json is rust" "rust=true harness=false xos=false" $'docs/design/host-api.schema.json'
  expect "docs/design md is rust" "rust=true harness=false xos=false" $'docs/design/x.md'
  expect "docs-only (all allowlist kinds)" "rust=false harness=false xos=false" \
    $'CLAUDE.md\n.claude/rules/ci.md\n.agents/skills/a/SKILL.md\nLICENSE-MIT\nNOTICE\nLICENSE-THIRD-PARTY.md\nskills-lock.json\n.markdownlint.jsonc\n.yamllint\n.editorconfig-checker.json\ncommitlint.config.mjs\nlefthook.yml'
  expect "md at any depth" "rust=false harness=false xos=false" $'a/b/c/d.md'
  expect "harness-only (known dirs)" "rust=false harness=true xos=false" \
    $'harness/binary-size/README.md\nharness/cold-start/measure.sh\nharness/multi-instance-memory/measure.sh\nharness/render-screenshot/RUNBOOK.md'
  expect "harness + docs" "rust=false harness=true xos=false" $'docs/a.md\nharness/cold-start/measure.sh'
  expect "harness fixture read by Rust" "rust=true harness=true xos=false" $'harness/compat_fixtures/a.html'
  expect "harness trace read by Rust" "rust=true harness=true xos=false" $'harness/playwright-trace/results/newpage-trace.jsonl'
  expect "harness workspace member" "rust=true harness=true xos=false" $'harness/wpt_subset_runner/src/main.rs'
  expect "unknown harness dir is rust (fail-closed)" "rust=true harness=true xos=false" $'harness/new-thing/run.sh'
  expect "crates change" "rust=true harness=false xos=false" $'crates/fandhe-browser-core/src/lib.rs'
  expect "md under crates is rust" "rust=true harness=false xos=false" $'crates/fandhe-browser-core/README.md'
  expect ".github change" "rust=true harness=false xos=true" $'.github/workflows/ci.yml'
  expect "mixed (docs + crates + harness)" "rust=true harness=true xos=false" $'docs/a.md\ncrates/a/src/lib.rs\nharness/cold-start/run.sh'
  expect "Cargo.lock" "rust=true harness=false xos=true" $'Cargo.lock'
  expect "scripts" "rust=true harness=false xos=true" $'scripts/ci-changes.sh'
  expect "Makefile" "rust=true harness=false xos=false" $'Makefile'
  expect "deny.toml" "rust=true harness=false xos=false" $'deny.toml'
  expect "rust-toolchain.toml" "rust=true harness=false xos=true" $'rust-toolchain.toml'
  expect "profiles" "rust=true harness=false xos=false" $'profiles/chrome.json'
  expect "benches" "rust=true harness=false xos=false" $'benches/a/self-test.sh'
  expect "tests" "rust=true harness=false xos=false" $'tests/a.rs'
  expect "Dockerfile / compose.yaml" "rust=true harness=false xos=false" $'Dockerfile\ncompose.yaml'
  expect "unknown path is rust (fail-closed)" "rust=true harness=false xos=false" $'unknown.txt'
  expect "not in allowlist (.editorconfig)" "rust=true harness=false xos=false" $'.editorconfig'
  expect ".gitattributes is xos" "rust=true harness=false xos=true" $'.gitattributes'
  expect "empty input runs all (fail-closed)" "rust=true harness=true xos=true" ''
  expect "blank lines only run all" "rust=true harness=true xos=true" $'\n\n'
  expect "CRLF input" "rust=false harness=false xos=false" $'README.md\r\ndocs/a.md\r\n'
  expect "no trailing newline" "rust=true harness=false xos=false" $'docs/a.md\ncrates/a.rs'
  expect "profile crate is xos" "rust=true harness=false xos=true" $'crates/fandhe-browser-profile/src/lib.rs'
  expect "crate Cargo.toml is xos" "rust=true harness=false xos=true" $'crates/fandhe-browser-ai/Cargo.toml'
  expect "OS-named file is xos" "rust=true harness=false xos=true" $'crates/fandhe-browser-js/src/windows_job.rs'
  expect "platform dir is xos" "rust=true harness=false xos=true" $'crates/fandhe-browser-core/src/platform/mod.rs'
  expect "ordinary crate change is ubuntu only" "rust=true harness=false xos=false" $'crates/fandhe-browser-ai/src/lib.rs'
  if [ "$fail" -ne 0 ]; then
    echo "NG: ci-changes.sh self-test failed" >&2
    return 1
  fi
  echo "OK: ci-changes.sh self-test passed"
}

main() {
  case "${1:-}" in
    --self-test)
      self_test
      ;;
    "")
      classify
      ;;
    *)
      echo "usage: $0 [--self-test]  (file list on stdin)" >&2
      exit 2
      ;;
  esac
}

main "$@"

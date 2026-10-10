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
# 出力: 標準出力に `rust=true|false` と `harness=true|false` の 2 行（GITHUB_OUTPUT へ
#       そのまま追記できる形式）。
#
# 判定（fail-closed。どれにも当てはまらないパスは rust=true）:
#   1. ビルド・検査に影響しうるパス（crates・Cargo.*・.github・scripts・Makefile・
#      deny.toml・rust-toolchain.toml・profiles・benches・tests・Dockerfile・
#      compose.yaml）-> rust=true
#   2. harness/ 配下 -> harness=true（rust は他のファイル次第）
#   3. docs 扱いの allowlist（*.md・docs/・.claude/・.agents/・LICENSE-*（LICENSE-THIRD-PARTY*
#      を含む）・NOTICE・skills-lock.json・lint 設定ファイル類）-> どちらも変えない
#   4. 上記以外 -> rust=true
# 入力が 1 行も無い場合は判定材料が無いため両方 true にする。
# bash の case パターンの `*` は `/` もまたぐため、`*.md` は任意階層、`docs/*` は
# docs/spec（submodule ポインタ）を含む docs/ 配下全体に一致する。
set -euo pipefail

# 1 行 1 パスの入力を判定して `rust=` / `harness=` を出力する。
classify() {
  local rust=false harness=false seen=false path
  while IFS= read -r path || [ -n "$path" ]; do
    # 末尾の CR を除去（Windows 由来の改行混入対策）。空行は無視する
    path="${path%$'\r'}"
    [ -n "$path" ] || continue
    seen=true
    case "$path" in
      crates/* | Cargo.* | .github/* | scripts/* | Makefile | deny.toml | \
        rust-toolchain.toml | profiles/* | benches/* | tests/* | Dockerfile | compose.yaml)
        rust=true
        ;;
      harness/*)
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
  fi
  printf 'rust=%s\nharness=%s\n' "$rust" "$harness"
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
  expect "docs-only" "rust=false harness=false" $'README.md\ndocs/design/a.md\ndocs/spec'
  expect "docs-only(allowlist 全種)" "rust=false harness=false" \
    $'CLAUDE.md\n.claude/rules/ci.md\n.agents/skills/a/SKILL.md\nLICENSE-MIT\nNOTICE\nLICENSE-THIRD-PARTY.md\nskills-lock.json\n.markdownlint.jsonc\n.yamllint\n.editorconfig-checker.json\ncommitlint.config.mjs\nlefthook.yml'
  expect "md 任意階層" "rust=false harness=false" $'a/b/c/d.md'
  expect "harness-only" "rust=false harness=true" $'harness/binary-size/README.md\nharness/x/run.sh'
  expect "harness + docs" "rust=false harness=true" $'docs/a.md\nharness/x/run.sh'
  expect "crates 変更" "rust=true harness=false" $'crates/fandhe-browser-core/src/lib.rs'
  expect "crates 配下の md も rust" "rust=true harness=false" $'crates/fandhe-browser-core/README.md'
  expect ".github 変更" "rust=true harness=false" $'.github/workflows/ci.yml'
  expect "混在(docs + crates + harness)" "rust=true harness=true" $'docs/a.md\ncrates/a/src/lib.rs\nharness/x/run.sh'
  expect "Cargo.lock" "rust=true harness=false" $'Cargo.lock'
  expect "scripts" "rust=true harness=false" $'scripts/ci-changes.sh'
  expect "Makefile" "rust=true harness=false" $'Makefile'
  expect "deny.toml" "rust=true harness=false" $'deny.toml'
  expect "rust-toolchain.toml" "rust=true harness=false" $'rust-toolchain.toml'
  expect "profiles" "rust=true harness=false" $'profiles/chrome.json'
  expect "benches" "rust=true harness=false" $'benches/a/self-test.sh'
  expect "tests" "rust=true harness=false" $'tests/a.rs'
  expect "Dockerfile / compose.yaml" "rust=true harness=false" $'Dockerfile\ncompose.yaml'
  expect "未知のパスは rust（fail-closed）" "rust=true harness=false" $'unknown.txt'
  expect "docs に見えるが allowlist 外（.editorconfig）" "rust=true harness=false" $'.editorconfig'
  expect "空入力は両方 true（fail-closed）" "rust=true harness=true" ''
  expect "空行のみは両方 true" "rust=true harness=true" $'\n\n'
  expect "CRLF 混入でも allowlist 判定" "rust=false harness=false" $'README.md\r\ndocs/a.md\r\n'
  expect "末尾改行なし" "rust=true harness=false" $'docs/a.md\ncrates/a.rs'
  if [ "$fail" -ne 0 ]; then
    echo "NG: ci-changes.sh の自己テストに失敗しました" >&2
    return 1
  fi
  echo "OK: ci-changes.sh の自己テストが全件成功しました"
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

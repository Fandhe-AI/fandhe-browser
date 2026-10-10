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
  local rust=false harness=false xos=false seen=false path
  while IFS= read -r path || [ -n "$path" ]; do
    # 末尾の CR を除去（Windows 由来の改行混入対策）。空行は無視する
    path="${path%$'\r'}"
    [ -n "$path" ] || continue
    seen=true
    case "$path" in
      crates/fandhe-browser-profile/* | .github/* | Cargo.toml | Cargo.lock | */Cargo.toml | \
        rust-toolchain.toml | scripts/*)
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
  expect "docs-only" "rust=false harness=false xos=false" $'README.md\ndocs/setup/windows.md\ndocs/spec'
  expect "docs/design の json は rust" "rust=true harness=false xos=false" $'docs/design/host-api.schema.json'
  expect "docs/design の md も rust" "rust=true harness=false xos=false" $'docs/design/x.md'
  expect "docs-only(allowlist 全種)" "rust=false harness=false xos=false" \
    $'CLAUDE.md\n.claude/rules/ci.md\n.agents/skills/a/SKILL.md\nLICENSE-MIT\nNOTICE\nLICENSE-THIRD-PARTY.md\nskills-lock.json\n.markdownlint.jsonc\n.yamllint\n.editorconfig-checker.json\ncommitlint.config.mjs\nlefthook.yml'
  expect "md 任意階層" "rust=false harness=false xos=false" $'a/b/c/d.md'
  expect "harness-only" "rust=false harness=true xos=false" $'harness/binary-size/README.md\nharness/x/run.sh'
  expect "harness + docs" "rust=false harness=true xos=false" $'docs/a.md\nharness/x/run.sh'
  expect "crates 変更" "rust=true harness=false xos=false" $'crates/fandhe-browser-core/src/lib.rs'
  expect "crates 配下の md も rust" "rust=true harness=false xos=false" $'crates/fandhe-browser-core/README.md'
  expect ".github 変更" "rust=true harness=false xos=true" $'.github/workflows/ci.yml'
  expect "混在(docs + crates + harness)" "rust=true harness=true xos=false" $'docs/a.md\ncrates/a/src/lib.rs\nharness/x/run.sh'
  expect "Cargo.lock" "rust=true harness=false xos=true" $'Cargo.lock'
  expect "scripts" "rust=true harness=false xos=true" $'scripts/ci-changes.sh'
  expect "Makefile" "rust=true harness=false xos=false" $'Makefile'
  expect "deny.toml" "rust=true harness=false xos=false" $'deny.toml'
  expect "rust-toolchain.toml" "rust=true harness=false xos=true" $'rust-toolchain.toml'
  expect "profiles" "rust=true harness=false xos=false" $'profiles/chrome.json'
  expect "benches" "rust=true harness=false xos=false" $'benches/a/self-test.sh'
  expect "tests" "rust=true harness=false xos=false" $'tests/a.rs'
  expect "Dockerfile / compose.yaml" "rust=true harness=false xos=false" $'Dockerfile\ncompose.yaml'
  expect "未知のパスは rust（fail-closed）" "rust=true harness=false xos=false" $'unknown.txt'
  expect "docs に見えるが allowlist 外（.editorconfig）" "rust=true harness=false xos=false" $'.editorconfig'
  expect "空入力は両方 true（fail-closed）" "rust=true harness=true xos=true" ''
  expect "空行のみは両方 true" "rust=true harness=true xos=true" $'\n\n'
  expect "CRLF 混入でも allowlist 判定" "rust=false harness=false xos=false" $'README.md\r\ndocs/a.md\r\n'
  expect "末尾改行なし" "rust=true harness=false xos=false" $'docs/a.md\ncrates/a.rs'
  expect "profile crate は xos" "rust=true harness=false xos=true" $'crates/fandhe-browser-profile/src/lib.rs'
  expect "crate の Cargo.toml は xos" "rust=true harness=false xos=true" $'crates/fandhe-browser-ai/Cargo.toml'
  expect "通常の crate 変更は ubuntu のみ" "rust=true harness=false xos=false" $'crates/fandhe-browser-ai/src/lib.rs'
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

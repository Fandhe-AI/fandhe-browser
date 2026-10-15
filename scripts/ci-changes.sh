#!/usr/bin/env bash
#
# PR の変更ファイル一覧から、CI の重いジョブ群を走らせる必要があるかを判定する正本
# スクリプト（.claude/rules/ci.md「docs-only 判定」・XOS-1・REPAIR-5・REPAIR-6）。
# 呼び出し元は .github/workflows/ci.yml の changes ジョブ。push（main）・
# workflow_dispatch・merge_group の全実行扱いと git diff の失敗時 fail-closed は
# ワークフロー側で扱い、本スクリプトは「ファイル一覧 -> rust= / harness= / servo=」の純粋な
# 判定だけを担う（ローカルで --self-test できるようにするため）。
#
# 使い方:
#   printf '%s\n' docs/a.md crates/x/src/lib.rs | scripts/ci-changes.sh
#   scripts/ci-changes.sh --self-test
#
# 入力: 標準入力に 1 行 1 パス（リポジトリルート相対）。
# 出力: 標準出力に `rust=true|false`・`harness=true|false`・`servo=true|false` の 3 行
#       （GITHUB_OUTPUT へそのまま追記できる形式）。
#       macOS・Windows ジョブを pull_request で走らせるかは本スクリプトの判定ではなく、
#       ワークフロー側がイベント種別で決める（.claude/rules/ci.md「3 OS CI」）。
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
#   5. servo 判定（rust・harness とは独立の軸。TASK-38・RENDER-1・設計 servo-host-design.md §6.1）:
#      servo-host/*・crates/fandhe-browser-render/*（契約 fixture を含む）・
#      docs/design/render-host-protocol*・scripts/*・.github/*・rust-toolchain.toml
#      -> servo=true。Servo ホストの重いビルドを走らせるかの判断材料で、消費する下流ジョブは別 Issue（#995）
#      servo-host/* だけの変更は rust=false にする。servo-host/ は別 workspace・別 Cargo.lock で、
#      ルート Cargo.toml が exclude しており、本体のビルド・テストは読まない（分離は
#      scripts/check-servo-host-isolation.sh が検証。契約 fixture は crates/ 側に置く）。
#      docs/design/render-host-protocol* は docs/design/* の rust 判定と両立する（rust=true かつ servo=true）。
# 入力が 1 行も無い場合は判定材料が無いため rust・harness・servo をすべて true にする。
# bash の case パターンの `*` は `/` もまたぐため、`*.md` は任意階層、`docs/*` は
# docs/spec（submodule ポインタ）を含む docs/ 配下全体に一致する。
set -euo pipefail

# 1 行 1 パスの入力を判定して `rust=` / `harness=` / `servo=` を出力する。
classify() {
  local rust=false harness=false servo=false seen=false path
  while IFS= read -r path || [ -n "$path" ]; do
    # 末尾の CR を除去（Windows 由来の改行混入対策）。空行は無視する
    path="${path%$'\r'}"
    [ -n "$path" ] || continue
    seen=true
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
      # servo-host/ は別 workspace で本体のビルド・テストは読まないため rust・harness は変えない
      # （servo 軸は下の 2 つ目の case で立てる）。未知のパスは従来どおり rust=true の *) に落ちるが、
      # servo-host/ だけはここで先に受けて rust=false を保つ
      # 例外: servo-host/deny.toml は make check-deny-license-reject（rust-lint）が読むため rust=true にする
      # （rust=false だと deny.toml だけの変更で canary が走らない fail-open になる。#991）
      servo-host/deny.toml)
        rust=true
        ;;
      servo-host/*)
        ;;
      *.md | docs/* | .claude/* | .agents/* | LICENSE-* | NOTICE | \
        skills-lock.json | .markdownlint.jsonc | .yamllint | .editorconfig-checker.json | \
        commitlint.config.mjs | lefthook.yml)
        ;;
      *)
        rust=true
        ;;
    esac
    # servo 軸（独立判定）。servo-host/ はホスト本体、render crate は IPC 契約・fixture の置き場、
    # render-host-protocol* はプロトコル仕様・スキーマ、scripts/.github は CI と判定スクリプト自身、
    # rust-toolchain.toml はホストも使う toolchain の単一真実源（設計 §1.3）
    case "$path" in
      servo-host/* | crates/fandhe-browser-render/* | docs/design/render-host-protocol* | \
        scripts/* | .github/* | rust-toolchain.toml)
        servo=true
        ;;
      *) ;;
    esac
  done
  if [ "$seen" = false ]; then
    rust=true
    harness=true
    servo=true
  fi
  printf 'rust=%s\nharness=%s\nservo=%s\n' "$rust" "$harness" "$servo"
}

# 自己テスト。期待値は「rust=<値> harness=<値> servo=<値>」の 1 行表記で比較する。
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
  expect "docs-only" "rust=false harness=false servo=false" $'README.md\ndocs/setup/a.md\ndocs/spec'
  expect "docs/design json is rust" "rust=true harness=false servo=false" $'docs/design/host-api.schema.json'
  expect "docs/design md is rust" "rust=true harness=false servo=false" $'docs/design/x.md'
  expect "docs-only (all allowlist kinds)" "rust=false harness=false servo=false" \
    $'CLAUDE.md\n.claude/rules/ci.md\n.agents/skills/a/SKILL.md\nLICENSE-MIT\nNOTICE\nLICENSE-THIRD-PARTY.md\nskills-lock.json\n.markdownlint.jsonc\n.yamllint\n.editorconfig-checker.json\ncommitlint.config.mjs\nlefthook.yml'
  expect "md at any depth" "rust=false harness=false servo=false" $'a/b/c/d.md'
  expect "harness-only (known dirs)" "rust=false harness=true servo=false" \
    $'harness/binary-size/README.md\nharness/cold-start/measure.sh\nharness/multi-instance-memory/measure.sh\nharness/render-screenshot/RUNBOOK.md'
  expect "harness + docs" "rust=false harness=true servo=false" $'docs/a.md\nharness/cold-start/measure.sh'
  expect "harness fixture read by Rust" "rust=true harness=true servo=false" $'harness/compat_fixtures/a.html'
  expect "harness trace read by Rust" "rust=true harness=true servo=false" $'harness/playwright-trace/results/newpage-trace.jsonl'
  expect "harness workspace member" "rust=true harness=true servo=false" $'harness/wpt_subset_runner/src/main.rs'
  expect "unknown harness dir is rust (fail-closed)" "rust=true harness=true servo=false" $'harness/new-thing/run.sh'
  expect "crates change" "rust=true harness=false servo=false" $'crates/fandhe-browser-core/src/lib.rs'
  expect "md under crates is rust" "rust=true harness=false servo=false" $'crates/fandhe-browser-core/README.md'
  expect "mixed (docs + crates + harness)" "rust=true harness=true servo=false" $'docs/a.md\ncrates/a/src/lib.rs\nharness/cold-start/run.sh'
  expect "Makefile" "rust=true harness=false servo=false" $'Makefile'
  expect "deny.toml" "rust=true harness=false servo=false" $'deny.toml'
  expect "profiles" "rust=true harness=false servo=false" $'profiles/chrome.json'
  expect "benches" "rust=true harness=false servo=false" $'benches/a/self-test.sh'
  expect "tests" "rust=true harness=false servo=false" $'tests/a.rs'
  expect "Dockerfile / compose.yaml" "rust=true harness=false servo=false" $'Dockerfile\ncompose.yaml'
  expect "unknown path is rust (fail-closed)" "rust=true harness=false servo=false" $'unknown.txt'
  expect "not in allowlist (.editorconfig)" "rust=true harness=false servo=false" $'.editorconfig'
  expect "CRLF input" "rust=false harness=false servo=false" $'README.md\r\ndocs/a.md\r\n'
  expect "no trailing newline" "rust=true harness=false servo=false" $'docs/a.md\ncrates/a.rs'
  expect ".github change" "rust=true harness=false servo=true" $'.github/workflows/ci.yml'
  expect "Cargo.lock" "rust=true harness=false servo=false" $'Cargo.lock'
  expect "servo-host only (rust=false; host is separate workspace)" "rust=false harness=false servo=true" $'servo-host/Cargo.toml\nservo-host/src/main.rs\nservo-host/Cargo.lock'
  expect "servo-host deny.toml is rust (canary reads it)" "rust=true harness=false servo=true" $'servo-host/deny.toml'
  expect "servo-host main.rs only" "rust=false harness=false servo=true" $'servo-host/src/main.rs'
  expect "servo-host README only" "rust=false harness=false servo=true" $'servo-host/README.md'
  expect "servo-host + docs" "rust=false harness=false servo=true" $'servo-host/src/main.rs\ndocs/a.md'
  expect "servo-host + core crate" "rust=true harness=false servo=true" $'servo-host/src/main.rs\ncrates/fandhe-browser-core/src/lib.rs'
  expect "servo-host + harness" "rust=false harness=true servo=true" $'servo-host/src/main.rs\nharness/cold-start/measure.sh'
  expect "render crate" "rust=true harness=false servo=true" $'crates/fandhe-browser-render/src/lib.rs'
  expect "render protocol fixture" "rust=true harness=false servo=true" $'crates/fandhe-browser-render/tests/fixtures/protocol/hello-ok.json'
  expect "render-host-protocol schema" "rust=true harness=false servo=true" $'docs/design/render-host-protocol.schema.json'
  expect "render-host-protocol md" "rust=true harness=false servo=true" $'docs/design/render-host-protocol.md'
  expect "servo-host-design md is not servo" "rust=true harness=false servo=false" $'docs/design/servo-host-design.md'
  expect "scripts" "rust=true harness=false servo=true" $'scripts/ci-changes.sh'
  expect "rust-toolchain.toml" "rust=true harness=false servo=true" $'rust-toolchain.toml'
  expect "empty input runs all (fail-closed)" "rust=true harness=true servo=true" ''
  expect "blank lines only run all" "rust=true harness=true servo=true" $'\n\n'
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

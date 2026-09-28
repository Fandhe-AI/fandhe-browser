#!/usr/bin/env bash
#
# check-binary-size.sh の自己テスト（TASK-34.2・RENDER-2・Issue #466）。合成
# ファイル（head -c による生成。macOS 標準には truncate が無いため使わない）に
# 対して判定モードの終了コードを検証し、あわせて package モードの metadata
# 確認だけを行う経路（ビルドしない経路）も検証する。呼び出し元は Makefile の
# check-binary-size ターゲットで、実バイナリの計測に先立って毎回実行し、
# 「上限超過で fail する」ことをログに証跡として残す。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$SCRIPT_DIR/check-binary-size.sh"

FAILURES=0
CASES=0
LAST_OUTPUT=""

# 合成ファイルは mktemp -d で作った一時ディレクトリに置き、trap で必ず削除する
# （一時ファイル配置規則: リポジトリ内へ常設しない）。
TMPDIR_SELF="$(mktemp -d "${TMPDIR:-/tmp}/binary-size-self-test.XXXXXX")"
cleanup() {
  rm -rf "$TMPDIR_SELF"
}
trap cleanup EXIT

# $1=case name, $2=expected exit code, remaining=checker args
expect_exit() {
  local name="$1" expected="$2"
  shift 2
  CASES=$((CASES + 1))
  local out status
  set +e
  out=$(bash "$CHECKER" "$@" 2>&1)
  status=$?
  set -e
  if [ "$status" -ne "$expected" ]; then
    echo "FAIL [$name]: expected exit $expected, got $status" >&2
    echo "  output: $out" >&2
    FAILURES=$((FAILURES + 1))
    LAST_OUTPUT=""
    return
  fi
  echo "ok [$name]: exit=$status"
  LAST_OUTPUT="$out"
}

# $1=haystack $2=needle $3=case name
expect_contains() {
  local haystack="$1" needle="$2" name="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    echo "FAIL [$name]: expected output to contain '$needle'" >&2
    echo "  output: $haystack" >&2
    FAILURES=$((FAILURES + 1))
  fi
}

# --- 判定モード（--file）。合成ファイルは head -c /dev/zero で作る（truncate は
# macOS 標準に無いため使わない。3.5 節）。---

F100="$TMPDIR_SELF/f100"
head -c 100 /dev/zero >"$F100"
F201="$TMPDIR_SELF/f201"
head -c 201 /dev/zero >"$F201"

expect_exit "100 bytes under limit 200 passes" 0 --file "$F100" --limit 200 --host x86_64-unknown-linux-gnu
expect_contains "$LAST_OUTPUT" "bytes=100" "100-byte case reports bytes=100"
expect_contains "$LAST_OUTPUT" "result=pass" "100-byte case reports result=pass"

expect_exit "100 bytes equal to limit 100 passes (boundary)" 0 --file "$F100" --limit 100 --host x86_64-unknown-linux-gnu
expect_contains "$LAST_OUTPUT" "result=pass" "boundary case reports result=pass"

expect_exit "201 bytes over limit 200 fails" 1 --file "$F201" --limit 200 --host x86_64-unknown-linux-gnu
expect_contains "$LAST_OUTPUT" "bytes=201" "201-byte case reports bytes=201"
expect_contains "$LAST_OUTPUT" "result=fail" "201-byte case reports result=fail"

expect_exit "missing file" 2 --file "$TMPDIR_SELF/does-not-exist" --limit 200

expect_exit "limit non-numeric" 2 --file "$F100" --limit abc
expect_exit "limit empty" 2 --file "$F100" --limit ""
expect_exit "limit zero" 2 --file "$F100" --limit 0
expect_exit "limit negative" 2 --file "$F100" --limit -1
expect_exit "limit 16 digits" 2 --file "$F100" --limit 1234567890123456

expect_exit "host with newline rejected" 2 --file "$F100" --limit 200 --host $'bad\nhost'
expect_exit "host with double-colon rejected" 2 --file "$F100" --limit 200 --host "bad::host"

expect_exit "unknown option" 2 --file "$F100" --limit 200 --bogus
expect_exit "missing --file and --package" 2 --limit 200
expect_exit "file and package mutually exclusive" 2 --file "$F100" --package foo --limit 200

# --- package モード（ビルドしない経路のみ。cargo と Cargo.toml が必要）。---

if command -v cargo >/dev/null 2>&1 && [ -f "$SCRIPT_DIR/../../Cargo.toml" ]; then
  # 既定 package（fandhe-browser-cli。TASK-41.5・Issue #174 が追加予定でまだ
  # workspace に存在しない）の不在に限り skip（exit 0）とする。既定値以外を
  # 明示指定して見つからない場合は package 名の誤記・設定のずれを「未導入の
  # ためスキップ」として握りつぶさないよう exit 2 にする（codex レビュー
  # 指摘, PR #563）。
  expect_exit "default package (fandhe-browser-cli) absence skips" 0 --package fandhe-browser-cli --limit 200
  expect_contains "$LAST_OUTPUT" "skip:" "default package absence reports skip:"

  expect_exit "non-default nonexistent package is a usage error" 2 --package this-package-does-not-exist --limit 200
  expect_contains "$LAST_OUTPUT" "not found in workspace" "non-default nonexistent package reports error"

  # fandhe-browser-core は lib のみで bin target を持たないため、metadata
  # 確認の時点で bin target 不在エラー（exit 2）になり、ビルドは走らない
  # （TASK-34.2 計画 3.1「bin target が無ければ fail」）。存在しない場合は
  # workspace 構成が変わっているとみなしケースをスキップする（fail-open ではなく
  # 「前提が崩れた」ことを明示するため警告を出す）。
  CORE_MANIFEST="$SCRIPT_DIR/../../crates/fandhe-browser-core/Cargo.toml"
  if [ -f "$CORE_MANIFEST" ]; then
    expect_exit "lib-only package has no bin target" 2 --package fandhe-browser-core --limit 200
    expect_contains "$LAST_OUTPUT" "no bin target" "lib-only package error mentions bin target"
  else
    echo "warning: crates/fandhe-browser-core/Cargo.toml not found; skipping lib-only-package case" >&2
  fi
else
  echo "warning: cargo or Cargo.toml not found; skipping package-mode cases" >&2
fi

echo "----"
echo "binary-size self-test: ${CASES} cases, $((CASES - FAILURES)) passed, ${FAILURES} failed"
if [ "$FAILURES" -ne 0 ]; then
  exit 1
fi
exit 0

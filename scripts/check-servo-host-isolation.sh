#!/usr/bin/env bash
#
# Servo ホスト（servo-host/）が本体 workspace から物理的・機械的に分離されていることを
# 検証する正本スクリプト（TASK-38・RENDER-1・Issue #985。設計は
# docs/design/servo-host-design.md §1.1〜§1.3）。呼び出し元は scripts/cargo-gate.sh の
# check-workspace-manifest（make check-workspace-manifest）と、ci.yml の harness-* ジョブの
# [render-isolation] 接頭辞のステップ。
#
# 検査:
#   A. `cargo metadata --no-deps` の workspace members に servo-host /
#      fandhe-browser-servo-host が含まれない（ルート Cargo.toml の exclude が効いている）
#   B. ルート Cargo.lock に Servo 系パッケージが無い。パッケージ名が
#      `servo` / `servo-*` / `servo_*` / `mozjs` / `mozjs-*` / `mozjs_*` に一致するものを禁止する
#      （例: servo_arc・servo-url・mozjs_sys）。名前の完全一致（接頭辞 + 区切り）に限るため、
#      無関係なクレート（例: `observo`）は誤検出しない。リネーム経由の混入は検出できない
#      ため、最終防御線は deny.toml（MPL-2.0 未許可）と check-render-isolation.sh。
#
# 取得・解析の失敗はすべて fail-closed（非ゼロ終了）。`--self-test` は合成データで判定関数だけを
# 確認する（cargo を呼ばない）。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 検出パターンの正本（固定値）。Cargo.lock の `name = "..."` 行の値に適用する。
LOCK_NAME_PATTERN='^(servo|mozjs)([-_].*)?$'
MEMBER_NAME_PATTERN='^(servo-host|fandhe-browser-servo-host)$'

# detect_members <package 名を 1 行ずつ並べた文字列>: Servo ホストが members にいれば 1。
detect_members() {
  if grep -Eq -- "$MEMBER_NAME_PATTERN" <<<"$1"; then
    echo "NG: workspace members に Servo ホストが含まれています（exclude = [\"servo-host\"] を確認）" >&2
    grep -E -- "$MEMBER_NAME_PATTERN" <<<"$1" >&2
    return 1
  fi
  echo "OK: workspace members に Servo ホストは含まれていません"
}

# detect_lock <Cargo.lock の全文>: Servo / mozjs 系パッケージがあれば 1。
detect_lock() {
  local names hits
  # `name = "x"` 行から値だけを取り出す。一致なしの grep 終了 1 は正常（空）として扱う。
  names="$(grep -E '^name = "' <<<"$1" | sed -E 's/^name = "([^"]*)".*$/\1/' || true)"
  hits="$(grep -E -- "$LOCK_NAME_PATTERN" <<<"$names" || true)"
  if [ -n "$hits" ]; then
    echo "NG: ルート Cargo.lock に Servo / mozjs 系パッケージが含まれています" >&2
    printf '%s\n' "$hits" >&2
    return 1
  fi
  echo "OK: ルート Cargo.lock に Servo / mozjs 系パッケージは含まれていません"
}

self_test() {
  local failures=0
  expect_fail() { # <説明> <関数> <入力>
    if "$2" "$3" >/dev/null 2>&1; then
      echo "NG(self-test): $1 を検出できませんでした" >&2
      failures=$((failures + 1))
    else
      echo "OK(self-test): $1 を検出しました"
    fi
  }
  expect_pass() {
    if ! "$2" "$3" >/dev/null 2>&1; then
      echo "NG(self-test): $1 を誤検出しました" >&2
      failures=$((failures + 1))
    else
      echo "OK(self-test): $1 は誤検出しませんでした"
    fi
  }
  expect_fail "members への fandhe-browser-servo-host 混入" detect_members $'fandhe-browser-core\nfandhe-browser-servo-host'
  expect_fail "members への servo-host 混入" detect_members $'servo-host'
  expect_pass "members の通常構成" detect_members $'fandhe-browser-core\nfandhe-browser-render\nwpt-subset-runner'
  expect_fail "Cargo.lock の servo" detect_lock $'[[package]]\nname = "servo"\nversion = "0.6.0"\n'
  expect_fail "Cargo.lock の servo_arc" detect_lock $'[[package]]\nname = "tokio"\n\n[[package]]\nname = "servo_arc"\nversion = "0.4.0"\n'
  expect_fail "Cargo.lock の servo-url" detect_lock $'name = "servo-url"\n'
  expect_fail "Cargo.lock の mozjs" detect_lock $'name = "mozjs"\n'
  expect_fail "Cargo.lock の mozjs_sys" detect_lock $'name = "mozjs_sys"\n'
  expect_pass "Cargo.lock の無関係クレート" detect_lock $'name = "tokio"\nname = "observo"\nname = "fandhe-browser-render"\nname = "reservoir"\n'
  expect_pass "空の Cargo.lock" detect_lock $'version = 4\n'
  if [ "$failures" -ne 0 ]; then
    echo "NG: self-test に ${failures} 件の失敗があります" >&2
    return 1
  fi
  echo "OK: self-test はすべて成功しました"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

command -v jq >/dev/null 2>&1 || { echo "error: jq is required by $(basename "$0")" >&2; exit 2; }
[ -f Cargo.lock ] || { echo "NG: ルート Cargo.lock が見つかりません" >&2; exit 1; }

meta="$(cargo metadata --no-deps --format-version 1)" || {
  echo "NG: cargo metadata に失敗しました" >&2
  exit 1
}
members="$(jq -r '. as $m | $m.packages[] | select(.id as $id | $m.workspace_members | index($id)) | .name' <<<"$meta" | tr -d '\r')" || {
  echo "NG: cargo metadata の解析に失敗しました" >&2
  exit 1
}
[ -n "$members" ] || { echo "NG: workspace members を取得できませんでした" >&2; exit 1; }
# manifest のパスでも検査する（package 名を変えても servo-host/ 配下なら検出する）。
paths="$(jq -r '. as $m | $m.packages[] | select(.id as $id | $m.workspace_members | index($id)) | .manifest_path' <<<"$meta" | tr -d '\r')"
# Windows の jq は CRLF で出力することがあるため CR を落とす（scripts/ci-prune-target.sh と同じ理由）。

STATUS=0
detect_members "$members" || STATUS=1
if grep -Eq '(^|[/\\])servo-host[/\\]' <<<"$paths"; then
  echo "NG: servo-host/ 配下の manifest が workspace members に含まれています" >&2
  STATUS=1
fi
detect_lock "$(cat Cargo.lock)" || STATUS=1
exit "$STATUS"

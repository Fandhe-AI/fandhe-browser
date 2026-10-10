#!/usr/bin/env bash
#
# harness/cold-start の自己テスト（TASK-104・Issue #397、ビヘイビア PERF-7）。
# /bin/true 等で measure.sh の統計・増分算出・タイマー方式・失敗時の終了コードを確認する。
# fandhe-browser 本体は使わない（実測はオーナー）。CI には組み込まない。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEASURE="$SCRIPT_DIR/measure.sh"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

bash -n "$MEASURE" || fail "syntax"
if command -v shellcheck >/dev/null 2>&1; then shellcheck "$MEASURE" "$0" || fail "shellcheck"; fi

TRUE_BIN="$(type -P true)"; FALSE_BIN="$(type -P false)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fandhe-cold-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
SLOW="$TMP/slow"
printf '#!/bin/sh\nsleep 0.02\n' >"$SLOW"; chmod +x "$SLOW"

num() { grep -o "\"$2\":-\?[0-9.]*" <<<"$1" | head -1 | cut -d: -f2; }
in_range() { awk -v v="$1" -v lo="$2" -v hi="$3" 'BEGIN{exit !(v >= lo && v <= hi)}'; }

# 入力検証
for args in "-n 0" "-n 1001" "-n x" "-n 08" "-n 010" "--warmup 101" "--warmup 03" "--threshold-ms x" "--threshold-ms 03" "--runs" "--bin-a" "--out"; do
  st=0; # shellcheck disable=SC2086
  "$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$TRUE_BIN" $args >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "'$args' must exit 2 (got $st)"
done
st=0; "$MEASURE" --bin-a /nonexistent --bin-b "$TRUE_BIN" >/dev/null 2>&1 || st=$?
[ "$st" -eq 2 ] || fail "missing binary must exit 2 (got $st)"
ok "input validation (leading zeros / missing option values exit 2)"
st=0; "$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$TRUE_BIN" --runs >/dev/null 2>&1 || st=$?
[ "$st" -eq 2 ] || fail "trailing --runs must exit 2 (got $st)"

# 同一バイナリ同士: 増分はほぼ 0（閾値 3ms 内）
OUT="$("$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$TRUE_BIN" -n 20 --warmup 2)" || fail "true vs true"
inc="$(num "$OUT" median_increase_ms)"
in_range "$inc" -3 3 || fail "true vs true median increase out of range: $inc"
[ "$(num "$OUT" runs)" = "20" ] || fail "runs != 20"
ok "true vs true: median_increase_ms=$inc (timer=$(grep -o '"timer":"[a-z-]*"' <<<"$OUT" | cut -d'"' -f4))"

# 20ms の差を検出できる（B が 20ms 遅い）
OUT="$("$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$SLOW" -n 10 --warmup 1)" || fail "true vs slow"
inc="$(num "$OUT" median_increase_ms)"
in_range "$inc" 15 100 || fail "slow detection: expected 15..100ms, got $inc"
grep -q '"reference_threshold_met":false' <<<"$OUT" || fail "threshold flag must be false"
ok "true vs 20ms sleep: median_increase_ms=$inc (reference_threshold_met=false)"

# タイマー方式を強制して同じ検出ができる
for t in date-ns perl; do
  case "$t" in
    date-ns) [[ "$(date +%s%N 2>/dev/null)" =~ ^[0-9]{19}$ ]] || { echo "note: GNU date %N unavailable; $t timer NOT verified"; continue; } ;;
    perl) command -v perl >/dev/null 2>&1 || { echo "note: perl unavailable; $t timer NOT verified"; continue; } ;;
  esac
  OUT="$(COLD_START_TIMER="$t" "$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$SLOW" -n 5 --warmup 1)" || fail "$t timer run"
  inc="$(num "$OUT" median_increase_ms)"
  in_range "$inc" 15 100 || fail "$t timer: expected 15..100ms, got $inc"
  ok "$t timer: median_increase_ms=$inc"
done

# 対象コマンドの非 0 終了は失敗（成功を装わない）
st=0; "$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$FALSE_BIN" -n 2 --warmup 0 >/dev/null 2>&1 || st=$?
[ "$st" -ne 0 ] || fail "non-zero command must fail"
ok "non-zero exit of target is a failure (status $st)"

# -- 以降の引数が対象へ渡る
OUT="$("$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$TRUE_BIN" -n 2 --warmup 0 -- --version)" || fail "args passthrough"
grep -q '"args":\["--version"\]' <<<"$OUT" || fail "args not recorded"
ok "arguments after -- are recorded"
if command -v jq >/dev/null 2>&1; then jq -e '.task == "TASK-104" and .a.runs == 2' <<<"$OUT" >/dev/null || fail "JSON shape"; fi

# 引数の JSON エスケープ（引用符・バックスラッシュ・改行・タブ・制御文字を元のまま記録）
ARG=$'a"b\\c\nd\te\001'
OUT="$("$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$TRUE_BIN" -n 1 --warmup 0 -- "$ARG")" || fail "escape run"
grep -qF '"args":["a\"b\\c\nd\te\u0001"]' <<<"$OUT" || fail "args escape mismatch: $OUT"
if command -v jq >/dev/null 2>&1; then
  [ "$(jq -r '.args[0]' <<<"$OUT")" = "$ARG" ] || fail "args must round-trip through JSON"
fi
ok "arguments are JSON-escaped"

# シグナル終了は失敗（3 種のタイマーすべて）
KILLED="$TMP/killed"
printf '#!/bin/sh\nkill -TERM $$\n' >"$KILLED"; chmod +x "$KILLED"
for t in epochrealtime date-ns perl; do
  st=0; COLD_START_TIMER="$t" "$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$KILLED" -n 1 --warmup 0 >/dev/null 2>&1 || st=$?
  [ "$st" -eq 1 ] || fail "$t: signal termination must exit 1 (got $st)"
done
ok "signal termination is a failure"

# カンマ小数ロケールでも JSON が壊れない（ロケールが無い環境では注記のみ）
for loc in de_DE.UTF-8 de_DE.utf8 fr_FR.UTF-8; do
  if LC_ALL="$loc" locale -a >/dev/null 2>&1 && locale -a 2>/dev/null | grep -qix "$loc"; then
    OUT="$(LC_ALL="$loc" "$MEASURE" --bin-a "$TRUE_BIN" --bin-b "$SLOW" -n 3 --warmup 0)" || fail "locale $loc run"
    grep -q ',[0-9]' <<<"${OUT//,\"/}" && fail "comma decimal in output under $loc"
    [ "$(num "$OUT" median_ms | head -1)" != "" ] || fail "median_ms missing under $loc"
    ok "locale $loc"; break
  fi
done

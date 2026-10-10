#!/usr/bin/env bash
#
# harness/multi-instance-memory の自己テスト（TASK-81・Issue #319、ビヘイビア PERF-4・MEAS-6）。
# 小さな N で measure.sh の起動・プロセスツリー走査・PSS 合算・JSON 出力・後始末を確認する。
# 実測値の妥当性や 50 インスタンスの結果は検証しない（それはオーナーの実測）。CI には組み込まない。
# Linux 専用。
#
# 終了コード:
#   0  すべて検証できた（Chromium 比較経路を含む）
#   1  失敗
#   3  Chromium が見つからず、Chromium 側の経路だけ未検証（他は合格。skip ではなく明示的に区別する）
#   4  Linux 以外
# 環境変数: FANDHE_BIN（実バイナリ。未設定なら実バイナリ部分は未検証と表示）、CHROMIUM_BIN。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEASURE="$SCRIPT_DIR/measure.sh"
[ "$(uname -s)" = "Linux" ] || { echo "error: Linux only" >&2; exit 4; }

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

bash -n "$MEASURE" || fail "syntax error in measure.sh"
if command -v node >/dev/null 2>&1; then node --check "$SCRIPT_DIR/navigate.mjs" || fail "navigate.mjs syntax"; fi
if command -v shellcheck >/dev/null 2>&1; then shellcheck "$MEASURE" "$0" || fail "shellcheck"; fi

# 入力検証: 範囲外・非数は起動前に終了コード 2
for bad in 0 201 abc ""; do
  st=0
  "$MEASURE" -n "$bad" --fandhe-bin /bin/true --skip-chromium >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "N='$bad' must exit 2 (got $st)"
done
ok "instance count validation (0, 201, abc, empty -> exit 2)"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/fandhe-mim-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# 偽バイナリ: 親 sh + 子 sleep 2 本（プロセスツリー走査を検証）。ポートは使わない。
FAKE="$TMP/fake-fandhe"
cat >"$FAKE" <<'FAKE_EOF'
#!/bin/sh
sleep 300 &
sleep 300 &
wait
FAKE_EOF
chmod +x "$FAKE"

json_num() { # $1=JSON $2=キー（最初の出現）
  grep -o "\"$2\":[0-9.]*" <<<"$1" | head -1 | cut -d: -f2
}
OUT="$("$MEASURE" -n 2 --fandhe-bin "$FAKE" --skip-chromium --conditions idle --settle 1 \
  --net-mode host --no-ready-check --allow-shared-port 2>/dev/null)" || fail "fake run failed"
procs="$(json_num "$OUT" processes)"; pss="$(json_num "$OUT" pss_kib)"
[ "$procs" = "6" ] || fail "expected 6 processes (2 x (sh + 2 sleep)), got '$procs'"
[ -n "$pss" ] && [ "$pss" -gt 0 ] || fail "pss_kib must be positive (got '$pss')"
if command -v jq >/dev/null 2>&1; then
  jq -e '.task == "TASK-81" and .instances == 2 and (.results | length) == 1' <<<"$OUT" >/dev/null \
    || fail "JSON shape"
fi
ok "fake binary x2: processes=$procs pss_kib=$pss (process tree walked, JSON emitted)"

# 後始末: 偽バイナリが 1 つも残っていない
if pgrep -f "$FAKE" >/dev/null 2>&1; then fail "leftover processes after run"; fi
ok "no leftover processes"

# 実バイナリ（N=1 host。固定ポート 9333 のため。N>=2 は netns が必要で、環境次第のため任意）
if [ -n "${FANDHE_BIN:-}" ] && [ -x "$FANDHE_BIN" ]; then
  OUT="$("$MEASURE" -n 1 --fandhe-bin "$FANDHE_BIN" --skip-chromium --conditions idle --settle 1 \
    --net-mode host 2>/dev/null)" || fail "real fandhe-browser run failed"
  pss="$(json_num "$OUT" pss_kib)"
  [ -n "$pss" ] && [ "$pss" -gt 0 ] || fail "real binary pss must be positive"
  ok "real fandhe-browser x1 (host): pss_kib=$pss"
  if unshare -Urn true 2>/dev/null || [ "$(id -u)" -eq 0 ]; then
    OUT="$("$MEASURE" -n 2 --fandhe-bin "$FANDHE_BIN" --skip-chromium --conditions idle --settle 1 \
      --net-mode netns 2>/dev/null)" || fail "real fandhe-browser x2 (netns) failed"
    ok "real fandhe-browser x2 (netns): pss_kib=$(json_num "$OUT" pss_kib)"
  else
    echo "note: netns unavailable here (needs root or unprivileged userns); x2 netns run NOT verified"
  fi
else
  echo "note: FANDHE_BIN not set; real fandhe-browser run NOT verified"
fi

# Chromium: 無い場合は skip 扱いで 0 にせず、終了コード 3 で区別する
CHROMIUM="${CHROMIUM_BIN:-}"
if [ -z "$CHROMIUM" ]; then
  for c in chromium chromium-browser google-chrome google-chrome-stable; do
    if command -v "$c" >/dev/null 2>&1; then CHROMIUM="$(command -v "$c")"; break; fi
  done
fi
if [ -z "$CHROMIUM" ]; then
  echo "NOT VERIFIED: Chromium not found (set CHROMIUM_BIN). Chromium comparison path was not exercised." >&2
  exit 3
fi
OUT="$("$MEASURE" -n 2 --fandhe-bin "$FAKE" --chromium-bin "$CHROMIUM" --chromium-extra-args "--no-sandbox" \
  --conditions idle --settle 3 --net-mode host --no-ready-check --allow-shared-port 2>/dev/null)" || fail "chromium run failed"
grep -q '"reduction_pct"' <<<"$OUT" || fail "comparison missing"
ok "chromium x2 + comparison emitted"

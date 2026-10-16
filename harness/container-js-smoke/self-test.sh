#!/usr/bin/env bash
#
# driver.mjs・run.sh の自己テスト（TASK-66.3・CTR-2）。
# docker・実コンテナは使わず、偽ワーカー（fake_worker.mjs）を同じ経路で起動して
# 成功と各失敗分類（blocked・protocol-error・crash・timeout・mismatch 等）の exit code・summary を具体値で検証する。
# 呼び出し元は scripts/harness.sh の check-container-js-smoke（実行の前に必ず通す）。要 node（>=18）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
command -v node >/dev/null 2>&1 || { echo "error: node is required but was not found on PATH" >&2; exit 2; }

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok: $1"; }
ng() { FAIL=$((FAIL + 1)); echo "NG: $1" >&2; }
expect_eq() {
  if [ "$2" = "$3" ]; then ok "$1"; else ng "$1 (expected=[$2] actual=[$3])"; fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# $1=モード $2..=driver の追加引数。stdout を $TMP/out、終了コードを RC に設定する。
run_mode() {
  local mode="$1"
  shift
  RC=0
  CONTAINER_JS_SMOKE_FAKE_WORKER_FOR_TEST="${HERE}/fake_worker.mjs" CONTAINER_JS_SMOKE_FAKE_MODE_FOR_TEST="$mode" \
    node "${HERE}/driver.mjs" "$@" >"${TMP}/out" 2>"${TMP}/err" || RC=$?
}
summary_of() { grep '^container-js-smoke-summary:' "${TMP}/out" || true; }
status_of() { grep "^container-js-smoke: case=$1 " "${TMP}/out" | sed 's/.*status=//' || true; }

# --- 正常系 ---
run_mode ok --case-timeout 10
expect_eq "ok: exit" 0 "$RC"
expect_eq "ok: summary" "container-js-smoke-summary: role=default total=17 executed=17 success=17 result=pass reason=none" "$(summary_of)"
expect_eq "ok: eval-arith" ok "$(status_of eval-arith)"
expect_eq "ok: dom-count" ok "$(status_of dom-count)"

# --- blocked ---
run_mode version --case-timeout 10
expect_eq "version: exit" 2 "$RC"
expect_eq "version: summary" "container-js-smoke-summary: role=default total=17 executed=0 success=0 result=blocked reason=hello-version-mismatch" "$(summary_of)"
run_mode boa --case-timeout 10
expect_eq "boa: exit" 2 "$RC"
expect_eq "boa: summary" "container-js-smoke-summary: role=default total=17 executed=0 success=0 result=blocked reason=hello-engine-mismatch" "$(summary_of)"

# --- fail（分類ごと） ---
run_mode badtag --case-timeout 10
expect_eq "badtag: exit" 1 "$RC"
expect_eq "badtag: first case" protocol-error "$(status_of eval-arith)"
expect_eq "badtag: later case is not-run" not-run "$(status_of dom-missing)"
expect_eq "badtag: summary" "container-js-smoke-summary: role=default total=17 executed=1 success=0 result=fail reason=protocol-error" "$(summary_of)"
run_mode huge --case-timeout 10
expect_eq "huge: exit" 1 "$RC"
expect_eq "huge: first case" protocol-error "$(status_of eval-arith)"
run_mode badutf8 --case-timeout 10
expect_eq "badutf8: exit" 1 "$RC"
expect_eq "badutf8: eval-string" protocol-error "$(status_of eval-string)"
run_mode eof --case-timeout 10
expect_eq "eof: exit" 1 "$RC"
expect_eq "eof: first case" crash "$(status_of eval-arith)"
expect_eq "eof: summary" "container-js-smoke-summary: role=default total=17 executed=1 success=0 result=fail reason=crash" "$(summary_of)"
run_mode hang --case-timeout 1
expect_eq "hang: exit" 1 "$RC"
expect_eq "hang: first case" timeout "$(status_of eval-arith)"
run_mode nohello --case-timeout 1
expect_eq "nohello: exit" 1 "$RC"
expect_eq "nohello: summary" "container-js-smoke-summary: role=default total=17 executed=0 success=0 result=fail reason=handshake-timeout" "$(summary_of)"
# ハンドシェイク失敗でワーカーを孤児として残さない（終了を待ってから戻る）。
NOHELLO_PID_FILE="${TMP}/nohello.pid"
CONTAINER_JS_SMOKE_FAKE_PID_FILE_FOR_TEST="$NOHELLO_PID_FILE" run_mode nohello --case-timeout 1
if [ -s "$NOHELLO_PID_FILE" ] && ! kill -0 "$(cat "$NOHELLO_PID_FILE")" 2>/dev/null; then
  ok "nohello: worker is not left running"
else
  ng "nohello: worker is not left running"
fi
run_mode rpcflood --case-timeout 1
expect_eq "rpcflood: exit" 1 "$RC"
expect_eq "rpcflood: first case" timeout "$(status_of eval-arith)"
run_mode mismatch --case-timeout 10
expect_eq "mismatch: exit" 1 "$RC"
expect_eq "mismatch: eval-arith" mismatch "$(status_of eval-arith)"
expect_eq "mismatch: summary" "container-js-smoke-summary: role=default total=17 executed=17 success=16 result=fail reason=case-failures" "$(summary_of)"
run_mode dirtyexit --case-timeout 10
expect_eq "dirtyexit: exit" 1 "$RC"
expect_eq "dirtyexit: summary" "container-js-smoke-summary: role=default total=17 executed=17 success=17 result=fail reason=shutdown-unclean" "$(summary_of)"

# --- 引数検証（docker・worker に到達する前に exit 2） ---
run_args() {
  RC=0
  node "${HERE}/driver.mjs" "$@" >"${TMP}/out" 2>"${TMP}/err" || RC=$?
}
run_args --image 'Bad Image'
expect_eq "driver: invalid image" 2 "$RC"
run_args --case-timeout 121
expect_eq "driver: timeout out of range" 2 "$RC"
run_args --platform linux/riscv64
expect_eq "driver: invalid platform" 2 "$RC"
for bad in "--image Bad;rm" "--case-timeout 0" "--case-timeout abc" "--platform linux/s390x" "--bogus x"; do
  RC=0
  # shellcheck disable=SC2086
  bash "${HERE}/run.sh" $bad >"${TMP}/out" 2>"${TMP}/err" || RC=$?
  expect_eq "run.sh: rejects [${bad}]" 2 "$RC"
done

echo "container-js-smoke self-test: pass=${PASS} fail=${FAIL}"
[ "$FAIL" -eq 0 ] || exit 1

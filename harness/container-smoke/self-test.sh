#!/usr/bin/env bash
#
# run.sh の自己テスト（TASK-66.1・CTR-2）。
# 実 daemon・ネットワークは使わず、偽 docker シム（PATH 先頭へ置く）で全体経路を検証する。
# シムは呼び出しを calls.log へ記録し、コンテナの挙動を FAKE_MODE で切り替える。
# 呼び出し元は scripts/harness.sh の check-container-smoke（実行の前に必ず通す）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./run.sh
source "${HERE}/run.sh"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "ok: $1"; }
ng() { FAIL=$((FAIL + 1)); echo "NG: $1" >&2; }

expect_eq() {
  if [ "$2" = "$3" ]; then ok "$1"; else ng "$1 (expected=[$2] actual=[$3])"; fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "${TMP}/bin" "${TMP}/state"

# --- 偽 docker シム ---
cat > "${TMP}/bin/docker" <<'SHIM'
#!/usr/bin/env bash
# 偽 docker。FAKE_DIR・FAKE_MODE・FAKE_TASKS・FAKE_TAR・FAKE_NOIMAGE を環境変数で受ける。
echo "$*" >> "${FAKE_DIR}/calls.log"
case "$1" in
  version) echo "27.0.0" ;;
  build) exit 0 ;;
  image) [ -z "${FAKE_NOIMAGE:-}" ] || exit 1
         case "$*" in *--format*) echo "sha256:$(printf '0%.0s' $(seq 64)) linux/amd64" ;; esac ;;
  create) printf 'a%.0s' $(seq 64); echo ;;
  export) cat "${FAKE_TAR}" ;;
  rm) ;;
  run)
    n=$(($(cat "${FAKE_DIR}/n" 2>/dev/null || echo 0) + 1)); echo "$n" > "${FAKE_DIR}/n"
    cid=$(printf '%064x' "$n")
    fx=""
    for a in "$@"; do case "$a" in /fixtures/*) fx="${a#/fixtures/}" ;; esac; done
    id=$(jq -r --arg f "$fx" '.[] | select(.fixture == $f) | .id' "${FAKE_TASKS}")
    kind=$(jq -r --arg f "$fx" '.[] | select(.fixture == $f) | .kind' "${FAKE_TASKS}")
    out=$(jq -c --arg f "$fx" '.[] | select(.fixture == $f) | .expected' "${FAKE_TASKS}")
    [ "$kind" = "texts" ] || out=$(printf '%s' "$out" | jq -r '.[0]')
    d="${FAKE_DIR}/state/${cid}"; mkdir -p "$d"
    echo 0 > "$d/exit"; echo false > "$d/running"; : > "$d/err"; printf '%s\n' "$out" > "$d/out"
    case "${FAKE_MODE}" in
      mismatch) [ "$id" != "C05" ] || echo wrong > "$d/out" ;;
      nonzero) [ "$id" != "C04" ] || echo 3 > "$d/exit" ;;
      server) echo true > "$d/running"; : > "$d/out"
              echo "DevTools listening on ws://127.0.0.1:9333/devtools/browser/x" > "$d/err" ;;
      hang) echo true > "$d/running"; : > "$d/out" ;;
    esac
    echo "$cid" ;;
  inspect)
    cid="${@: -1}"; d="${FAKE_DIR}/state/${cid}"
    case "$*" in *State.Running*) cat "$d/running" ;; *ExitCode*) cat "$d/exit" ;; esac ;;
  logs)
    cid="${@: -1}"; d="${FAKE_DIR}/state/${cid}"
    cat "$d/out"; cat "$d/err" >&2 ;;
  *) echo "fake docker: unsupported: $*" >&2; exit 99 ;;
esac
SHIM
chmod +x "${TMP}/bin/docker"

# --- export 用の tar ---
mkdir -p "${TMP}/rootfs/data" "${TMP}/rootfs-lib/lib"
: > "${TMP}/rootfs/fandhe-browser"
: > "${TMP}/rootfs-lib/fandhe-browser"
: > "${TMP}/rootfs-lib/lib/libc.so.6"
tar -C "${TMP}/rootfs" -cf "${TMP}/clean.tar" .
tar -C "${TMP}/rootfs-lib" -cf "${TMP}/lib.tar" .

# $1=名前 $2=期待 exit $3=FAKE_MODE $4=tar、残り=run.sh 引数。stdout を LAST_OUT へ保存する。
LAST_OUT=""
scenario() {
  local name="$1" want="$2" mode="$3" tarf="$4" got=0
  shift 4
  rm -rf "${TMP}/state" "${TMP}/calls.log" "${TMP}/n"; mkdir -p "${TMP}/state"
  LAST_OUT=$(PATH="${TMP}/bin:${PATH}" FAKE_DIR="$TMP" FAKE_MODE="$mode" FAKE_TASKS="${HERE}/tasks.json" \
    FAKE_TAR="${TMP}/${tarf}" FAKE_NOIMAGE="${FAKE_NOIMAGE:-}" bash "${HERE}/run.sh" "$@" 2>/dev/null) || got=$?
  expect_eq "$name (exit)" "$want" "$got"
}
count_lines() { printf '%s\n' "$LAST_OUT" | grep -c "$1" || true; }

# --- 純関数 ---
expect_eq "sanitize replaces control chars" "a b c" "$(printf 'a\nb\tc' | sanitize_sample)"
long300="$(head -c 500 /dev/zero | tr '\0' 'x' | sanitize_sample)"
expect_eq "sanitize truncates to 300" "300" "${#long300}"
expect_eq "count_lib_entries" "2" "$(printf './\n./fandhe-browser\n./lib/\n./lib/a\n./x.so.1\n' | count_lib_entries)"
build_task_argv attr "nav a" href /fixtures/static/05-nav-links.html
expect_eq "argv attr" "--file /fixtures/static/05-nav-links.html attr nav a href" "${TASK_ARGV[*]}"
build_task_argv texts "p" "" /fixtures/x.html
expect_eq "argv texts" "--file /fixtures/x.html texts p" "${TASK_ARGV[*]}"
if compare_result texts '["a","b"]' '["a","b"]'; then ok "compare texts"; else ng "compare texts"; fi
if compare_result texts '[ "a" , "b" ]' '["a","b"]'; then ok "compare texts normalizes"; else ng "compare texts normalizes"; fi
if compare_result texts '["a"]' '["a","b"]'; then ng "compare texts mismatch"; else ok "compare texts mismatch"; fi
if compare_result text $'x\n' '["x"]'; then ok "compare text strips newline"; else ng "compare text strips newline"; fi
if compare_result attr 'y' '["x"]'; then ng "compare attr mismatch"; else ok "compare attr mismatch"; fi

# --- 全体経路 ---
scenario "all 12 pass" 0 pass clean.tar --role lite --skip-build --task-timeout 2
expect_eq "pass: 12 ok lines" "12" "$(count_lines 'status=ok')"
expect_eq "pass: summary" "1" "$(count_lines 'result=pass reason=none')"
expect_eq "pass: shared_libs=0 in env line" "1" "$(count_lines 'shared_libs=0')"
CALLS="${TMP}/calls.log"
runs=$(grep -c '^run ' "$CALLS" || true)
expect_eq "calls: 13 runs (preflight + 12)" "13" "$runs"
expect_eq "calls: every run has --network none" "$runs" "$(grep '^run ' "$CALLS" | grep -c -- '--network none' || true)"
expect_eq "calls: every run has :ro mount" "$runs" "$(grep '^run ' "$CALLS" | grep -c -- ':/fixtures:ro' || true)"
expect_eq "calls: every run has --cap-drop ALL" "$runs" "$(grep '^run ' "$CALLS" | grep -c -- '--cap-drop ALL' || true)"
expect_eq "calls: every run has no-new-privileges" "$runs" "$(grep '^run ' "$CALLS" | grep -c -- 'no-new-privileges' || true)"
expect_eq "calls: no --privileged" "0" "$(grep -c -- '--privileged' "$CALLS" || true)"
expect_eq "calls: no --init" "0" "$(grep -c -- '--init' "$CALLS" || true)"
expect_eq "calls: no rmi/prune" "0" "$(grep -Ec '^(rmi|image rm|system prune|image prune)' "$CALLS" || true)"
expect_eq "calls: every container removed" "$runs" "$(grep -c '^rm -f [0-9a-f]\{64\}' "$CALLS" | awk -v c="$(grep -c '^create' "$CALLS")" '{print $1 - c}')"

scenario "one mismatch" 1 mismatch clean.tar --role lite --skip-build --task-timeout 2
expect_eq "mismatch: C05 line" "1" "$(count_lines 'id=C05 .*status=mismatch')"
expect_eq "mismatch: 11 ok" "11" "$(count_lines 'status=ok')"

scenario "non-zero exit" 1 nonzero clean.tar --role default --skip-build --task-timeout 2
expect_eq "nonzero: C04 line" "1" "$(count_lines 'id=C04 .*status=nonzero-exit exit=3')"

scenario "server image is blocked" 2 server clean.tar --role lite --skip-build --task-timeout 1
expect_eq "blocked: summary" "1" "$(count_lines 'result=blocked reason=one-shot-subcommand-unavailable')"
expect_eq "blocked: no ok lines" "0" "$(count_lines 'status=ok')"
expect_eq "blocked: container removed" "1" "$(grep -c '^rm -f [0-9a-f]\{64\}' "${TMP}/calls.log" | awk -v c="$(grep -c '^create' "${TMP}/calls.log")" '{print $1 - c}')"

scenario "hang without banner" 1 hang clean.tar --role lite --skip-build --task-timeout 1
expect_eq "hang: timeout summary" "1" "$(count_lines 'result=fail reason=timeout')"

scenario "shared libs present" 1 pass lib.tar --role lite --skip-build --task-timeout 2
expect_eq "libs: reason" "1" "$(count_lines 'reason=shared-libs-present')"

# --- 使用エラー（すべて exit 2） ---
FAKE_NOIMAGE=1 scenario "skip-build with missing image" 2 pass clean.tar --role lite --skip-build
scenario "bad role" 2 pass clean.tar --role evil --skip-build
scenario "bad image" 2 pass clean.tar --role lite --image 'a;b' --skip-build
scenario "bad platform" 2 pass clean.tar --role lite --platform linux/ppc64 --skip-build
scenario "bad timeout" 2 pass clean.tar --role lite --task-timeout 0 --skip-build
scenario "unknown arg" 2 pass clean.tar --role lite --bogus

# --- tasks.json の不正 ---
bad_tasks() { # $1=名前 $2=jq フィルタ
  jq -c "$2" "${HERE}/tasks.json" > "${TMP}/bad.json"
  scenario "tasks: $1" 2 pass clean.tar --role lite --skip-build --tasks "${TMP}/bad.json"
}
bad_tasks "11 entries" 'del(.[0])'
bad_tasks "duplicate id" '.[1].id = "C01"'
bad_tasks "unknown kind" '.[0].kind = "html"'
bad_tasks "path traversal fixture" '.[0].fixture = "static/../x.html"'
bad_tasks "attr missing" 'del(.[2].attr)'
bad_tasks "empty expected" '.[0].expected = []'

echo "passed=${PASS} failed=${FAIL}"
[ "$FAIL" -eq 0 ]

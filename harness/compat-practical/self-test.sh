#!/usr/bin/env bash
#
# access_check.sh・lib.sh の自己テスト（TASK-71.1・MEAS-4）。オフライン専用:
# curl は PATH 先頭のスタブへ差し替え、実ネットワークへは出ない。呼び出し元は
# .github/workflows/ci.yml の compat-regression ジョブと Makefile の
# check-compat-practical ターゲット。一時ファイルはすべて mktemp の絶対パスに置き、
# trap で削除する（リポジトリ内にはファイルを作らない）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$SCRIPT_DIR/access_check.sh"
TASKS="$SCRIPT_DIR/tasks.json"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"

FAILURES=0
CASES=0
LAST_OUTPUT=""
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# $1=name $2=expected exit $3...=command
expect_exit() {
  local name="$1" expected="$2" out status
  shift 2
  CASES=$((CASES + 1))
  set +e
  out=$("$@" 2>&1)
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

# $1=actual $2=expected $3=name
expect_eq() {
  CASES=$((CASES + 1))
  if [ "$1" != "$2" ]; then
    echo "FAIL [$3]: expected '$2', got '$1'" >&2
    FAILURES=$((FAILURES + 1))
  else
    echo "ok [$3]: $1"
  fi
}

# $1=haystack $2=needle $3=name
expect_contains() {
  CASES=$((CASES + 1))
  if [[ "$1" != *"$2"* ]]; then
    echo "FAIL [$3]: expected output to contain '$2'" >&2
    echo "  output: $1" >&2
    FAILURES=$((FAILURES + 1))
  else
    echo "ok [$3]"
  fi
}

# --- 本物の tasks.json ---
expect_exit "real tasks validate" 0 bash "$CHECK" --validate-only
expect_contains "$LAST_OUTPUT" "ok: 22 tasks validated" "real tasks count message"
expect_eq "$(jq length "$TASKS")" "22" "task count"
expect_eq "$(jq '[.[].id] | unique | length' "$TASKS")" "22" "unique ids"
expect_eq "$(jq -c '[.[] | select(.id == "b5" or .id == "d2" or .id == "d3") | .id]' "$TASKS")" '["b5","d2","d3"]' "js-required ids present"
expect_eq "$(jq -c '[.[].kind] | unique' "$TASKS")" '["form","text","texts"]' "kinds"
expect_eq "$(jq -c '[.[].cat] | unique' "$TASKS")" '["form","lazy","spa","static","table"]' "cats"
expect_eq "$(jq '[.[] | select(.url | startswith("https://") | not)] | length' "$TASKS")" "0" "all https"

# --- 不正な tasks.json（合成）---
write_tasks() { printf '%s\n' "$2" >"$WORK/$1.json"; }
VALID='{"id":"x1","cat":"static","url":"https://example.com/","selector":"h1","kind":"text"}'
write_tasks ok "[$VALID]"
expect_exit "valid synthetic" 0 bash "$CHECK" --validate-only --tasks "$WORK/ok.json"
write_tasks dup "[$VALID,$VALID]"
expect_exit "duplicate id" 2 bash "$CHECK" --validate-only --tasks "$WORK/dup.json"
expect_contains "$LAST_OUTPUT" "duplicate id: x1" "duplicate id message"
write_tasks http '[{"id":"x1","cat":"static","url":"http://example.com/","selector":"h1","kind":"text"}]'
expect_exit "http url" 2 bash "$CHECK" --validate-only --tasks "$WORK/http.json"
write_tasks file '[{"id":"x1","cat":"static","url":"file:///etc/passwd","selector":"h1","kind":"text"}]'
expect_exit "file url" 2 bash "$CHECK" --validate-only --tasks "$WORK/file.json"
write_tasks kind '[{"id":"x1","cat":"static","url":"https://example.com/","selector":"h1","kind":"click"}]'
expect_exit "unknown kind" 2 bash "$CHECK" --validate-only --tasks "$WORK/kind.json"
write_tasks cat '[{"id":"x1","cat":"a\nb","url":"https://example.com/","selector":"h1","kind":"text"}]'
expect_exit "newline in cat" 2 bash "$CHECK" --validate-only --tasks "$WORK/cat.json"
write_tasks empty '[]'
expect_exit "empty array" 2 bash "$CHECK" --validate-only --tasks "$WORK/empty.json"
write_tasks bad '{not json'
expect_exit "invalid json" 2 bash "$CHECK" --validate-only --tasks "$WORK/bad.json"
expect_exit "timeout zero" 2 bash "$CHECK" --validate-only --timeout 0
expect_exit "timeout over" 2 bash "$CHECK" --validate-only --timeout 61

# --- ネットワーク経路（curl スタブ）---
STUB_DIR="$WORK/stub"
mkdir -p "$STUB_DIR"
cat >"$STUB_DIR/curl" <<'STUB'
#!/usr/bin/env bash
# -w '%{http_code} %{time_total}' の出力を STUB_CODE と固定の 0.250 秒、終了コードを STUB_RC で固定する
printf '%s 0.250' "${STUB_CODE:-200}"
exit "${STUB_RC:-0}"
STUB
chmod +x "$STUB_DIR/curl"
write_tasks net "[$VALID,{\"id\":\"x2\",\"cat\":\"spa\",\"url\":\"https://example.org/\",\"selector\":\"h1\",\"kind\":\"text\"}]"

run_net() { PATH="$STUB_DIR:$PATH" STUB_CODE="$1" STUB_RC="$2" bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/out.jsonl"; }
result_field() { jq -r --arg id "$1" "select(.type==\"result\" and .id==\$id) | .$2" "$WORK/out.jsonl"; }

expect_exit "stub 200" 0 run_net 200 0
expect_contains "$LAST_OUTPUT" "reachable=2/2" "stub 200 summary"
expect_eq "$(jq -r 'select(.type=="meta") | .timeout_sec' "$WORK/out.jsonl")" "8" "meta timeout"
expect_eq "$(jq -r 'select(.type=="meta") | .bin' "$WORK/out.jsonl")" "null" "meta bin null"
expect_eq "$(jq -r 'select(.type=="meta") | .user_agent' "$WORK/out.jsonl")" "fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)" "meta user agent"
expect_eq "$(jq -s 'length' "$WORK/out.jsonl")" "3" "jsonl line count"
expect_eq "$(result_field x1 http_status)" "200" "200 http_status"
expect_eq "$(result_field x1 reachable)" "true" "200 reachable"
expect_eq "$(result_field x1 curl_exit)" "0" "200 curl_exit"
expect_eq "$(result_field x1 elapsed_ms)" "250" "200 elapsed_ms (ms 精度)"

expect_exit "stub timeout" 0 run_net 000 28
expect_contains "$LAST_OUTPUT" "reachable=0/2" "stub timeout summary"
expect_eq "$(result_field x1 http_status)" "0" "timeout http_status"
expect_eq "$(result_field x1 reachable)" "false" "timeout reachable"
expect_eq "$(result_field x1 curl_exit)" "28" "timeout curl_exit"

expect_exit "stub 403" 0 run_net 403 0
expect_eq "$(result_field x2 http_status)" "403" "403 http_status"
expect_eq "$(result_field x2 reachable)" "false" "403 reachable"
expect_eq "$(ls -A "$WORK" | grep -c '^\.access_check\.' || true)" "0" "no leftover temp file"

# --- resolve_bin ---
expect_exit "bin missing" 2 resolve_bin "$WORK/nope"
mkdir -p "$WORK/adir"
expect_exit "bin is directory" 2 resolve_bin "$WORK/adir"
printf '#!/bin/sh\n' >"$WORK/notexec"
chmod -x "$WORK/notexec"
expect_exit "bin not executable" 2 resolve_bin "$WORK/notexec"
printf '#!/bin/sh\n' >"$WORK/fakebin"
chmod +x "$WORK/fakebin"
expect_exit "bin ok via arg" 0 resolve_bin "$WORK/fakebin"
expect_eq "$LAST_OUTPUT" "$WORK/fakebin" "bin ok path"
expect_exit "bin ok via env" 0 env FANDHE_BROWSER_BIN="$WORK/fakebin" bash -c ". '$SCRIPT_DIR/lib.sh'; resolve_bin"
expect_eq "$LAST_OUTPUT" "$WORK/fakebin" "bin env path"
expect_exit "access_check --bin missing" 2 bash "$CHECK" --bin "$WORK/nope" --tasks "$WORK/net.json" --out "$WORK/out2.jsonl"
expect_exit "access_check --bin ok" 0 env PATH="$STUB_DIR:$PATH" bash "$CHECK" --bin "$WORK/fakebin" --tasks "$WORK/net.json" --out "$WORK/out3.jsonl"
expect_eq "$(jq -r 'select(.type=="meta") | .bin' "$WORK/out3.jsonl")" "$WORK/fakebin" "meta bin recorded"

echo "cases=$CASES failures=$FAILURES"
[ "$FAILURES" -eq 0 ]

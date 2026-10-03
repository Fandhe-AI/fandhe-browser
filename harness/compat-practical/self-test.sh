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
write_tasks idnl '[{"id":"x1\n","cat":"static","url":"https://example.com/","selector":"h1","kind":"text"}]'
expect_exit "trailing newline in id" 2 bash "$CHECK" --validate-only --tasks "$WORK/idnl.json"
write_tasks catnl '[{"id":"x1","cat":"static\n","url":"https://example.com/","selector":"h1","kind":"text"}]'
expect_exit "trailing newline in cat" 2 bash "$CHECK" --validate-only --tasks "$WORK/catnl.json"
write_tasks urlnl '[{"id":"x1","cat":"static","url":"https://example.com/\n","selector":"h1","kind":"text"}]'
expect_exit "trailing newline in url" 2 bash "$CHECK" --validate-only --tasks "$WORK/urlnl.json"
write_tasks empty '[]'
expect_exit "empty array" 2 bash "$CHECK" --validate-only --tasks "$WORK/empty.json"
write_tasks bad '{not json'
: >"$WORK/zero.json"
expect_exit "empty file" 2 bash "$CHECK" --validate-only --tasks "$WORK/zero.json"
expect_contains "$LAST_OUTPUT" "exactly one JSON document" "empty file message"
printf '%s\n%s\n' "[$VALID]" "[$VALID]" >"$WORK/multi.json"
expect_exit "multiple documents" 2 bash "$CHECK" --validate-only --tasks "$WORK/multi.json"
expect_contains "$LAST_OUTPUT" "exactly one JSON document" "multiple documents message"
expect_exit "invalid json" 2 bash "$CHECK" --validate-only --tasks "$WORK/bad.json"
expect_exit "timeout zero" 2 bash "$CHECK" --validate-only --timeout 0
expect_exit "timeout over" 2 bash "$CHECK" --validate-only --timeout 61
expect_exit "total timeout zero" 2 bash "$CHECK" --validate-only --total-timeout 0
expect_exit "total timeout over" 2 bash "$CHECK" --validate-only --total-timeout 3601

# --- ネットワーク経路（curl スタブ）---
STUB_DIR="$WORK/stub"
mkdir -p "$STUB_DIR"
cat >"$STUB_DIR/curl" <<'STUB'
#!/usr/bin/env bash
# -w '%{http_code} %{time_total} %{remote_ip} %{redirect_url}' の出力を固定する。
# STUB_CODE=コード（固定 0.250 秒）STUB_IP=接続先 IP STUB_REDIRECT=redirect_url STUB_RC=終了コード。
# STUB_LOG があれば呼び出しごとの引数を追記する。
[ -z "${STUB_LOG:-}" ] || echo "$*" >>"$STUB_LOG"
[ -z "${STUB_SLEEP:-}" ] || sleep "$STUB_SLEEP"
printf '%s 0.250 %s %s' "${STUB_CODE:-200}" "${STUB_IP:-93.184.216.34}" "${STUB_REDIRECT:-}"
exit "${STUB_RC:-0}"
STUB
chmod +x "$STUB_DIR/curl"
# getent スタブ: DNS へは出ず STUB_RESOLVE_IP（既定は公開アドレス）を返す
cat >"$STUB_DIR/getent" <<'STUB'
#!/usr/bin/env bash
[ -z "${STUB_RESOLVE_NONE:-}" ] || exit 0
[ -z "${STUB_RESOLVE_PID:-}" ] || echo $$ >"$STUB_RESOLVE_PID"
[ -z "${STUB_RESOLVE_SLEEP:-}" ] || exec sleep "$STUB_RESOLVE_SLEEP"
[ -z "${STUB_RESOLVE_DELAY:-}" ] || sleep "$STUB_RESOLVE_DELAY"
printf '%s      STREAM %s\n' "${STUB_RESOLVE_IP:-93.184.216.34}" "${2:-}"
STUB
chmod +x "$STUB_DIR/getent"
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

# --- 全体期限（--total-timeout）: 超過後のタスクは取得せず blocked で記録を確定する ---
: >"$WORK/curl-dl.log"
expect_exit "total deadline" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$WORK/curl-dl.log" STUB_SLEEP=3 bash "$CHECK" --tasks "$WORK/net.json" --total-timeout 2 --out "$WORK/dl.jsonl"
expect_eq "$(wc -l <"$WORK/curl-dl.log" | tr -d ' ')" "1" "total deadline: curl not invoked after expiry"
expect_eq "$(jq -r 'select(.id=="x2") | .blocked' "$WORK/dl.jsonl")" "total time limit exceeded" "total deadline blocked reason"
expect_eq "$(jq -r 'select(.id=="x2") | .curl_exit' "$WORK/dl.jsonl")" "28" "total deadline curl_exit"
expect_eq "$(jq -s 'length' "$WORK/dl.jsonl")" "3" "total deadline: all records written"

# --- SSRF 対策（url_check / 解決後アドレス / リダイレクト）---
expect_exit "url_check public host" 0 url_check "https://example.com/"
expect_contains "$(url_check "https://localhost/" || true)" "internal host name" "url_check localhost"
expect_contains "$(url_check "https://127.0.0.1/" || true)" "non-public" "url_check loopback"
expect_contains "$(url_check "https://169.254.169.254/latest" || true)" "non-public" "url_check metadata"
expect_contains "$(url_check "https://10.1.2.3/" || true)" "non-public" "url_check private"
expect_contains "$(url_check "https://2130706433/" || true)" "non-public" "url_check decimal ip"
expect_contains "$(url_check "https://0177.0.0.1/" || true)" "non-public" "url_check octal ip"
expect_contains "$(url_check "https://[::1]/" || true)" "IPv6 literal" "url_check ipv6"
expect_contains "$(url_check "https://user@example.com/" || true)" "userinfo" "url_check userinfo"
expect_contains "$(url_check "https://example.com:8443/" || true)" "port is not 443" "url_check port"
# 検証したホスト名と curl の接続先ホスト名が一致しない URL は拒否する（P0: DNS 固定の迂回防止。SEC 系）
for u in "https://example.com./" "https://example.com.:443/" "https://EXAMPLE.COM./x" "https://example..com/" \
  "https://exa%6dple.com/" "https://exa%2Emple.com/" "https://例え.jp/" \
  "https://-example.com/" "https://example_x.com/" "https://1.2.3.4./"; do
  expect_contains "$(url_check "$u" || true)" "canonical ASCII DNS name" "url_check rejects non-canonical host $u"
done
expect_exit "url_check rejects empty port" 1 url_check "https://example.com:/"
expect_exit "url_check rejects backslash userinfo" 1 url_check "https://example.com\\@evil.com/"
expect_exit "url_check uppercase host ok" 0 url_check "https://EXAMPLE.com:443/a?b#c"
expect_eq "$(url_host "https://EXAMPLE.com:443/a")" "example.com" "url_host lowercases"
expect_eq "$(url_host "https://example.com./")" "example.com." "url_host keeps trailing dot"
expect_exit "is_public_ip 8.8.8.8" 0 is_public_ip 8.8.8.8
expect_exit "is_public_ip 172.16.0.1" 1 is_public_ip 172.16.0.1
expect_exit "is_public_ip 172.32.0.1" 0 is_public_ip 172.32.0.1
expect_exit "is_public_ip 100.64.0.1" 1 is_public_ip 100.64.0.1
expect_exit "is_public_ip ::1" 1 is_public_ip ::1
expect_exit "is_public_ip fe80::1" 1 is_public_ip fe80::1
expect_exit "is_public_ip fd00::1" 1 is_public_ip fd00::1
expect_exit "is_public_ip ::ffff:127.0.0.1" 1 is_public_ip ::ffff:127.0.0.1
expect_exit "is_public_ip 2606:4700::1" 0 is_public_ip 2606:4700::1

write_tasks ssrf '[{"id":"s1","cat":"static","url":"https://127.0.0.1/","selector":"h1","kind":"text"},{"id":"s2","cat":"static","url":"https://localhost/x","selector":"h1","kind":"text"}]'
LOG="$WORK/curl.log"
: >"$LOG"
expect_exit "ssrf literal tasks" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" bash "$CHECK" --tasks "$WORK/ssrf.json" --out "$WORK/ssrf.jsonl"
expect_contains "$LAST_OUTPUT" "reachable=0/2" "ssrf literal summary"
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "0" "ssrf literal: curl not invoked"
expect_eq "$(jq -r 'select(.id=="s1") | .blocked' "$WORK/ssrf.jsonl")" "non-public or non-canonical IP literal" "ssrf literal blocked reason"

: >"$LOG"
expect_exit "ssrf resolves private" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_RESOLVE_IP=10.0.0.5 bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/ssrf2.jsonl"
expect_contains "$LAST_OUTPUT" "reachable=0/2" "ssrf resolve summary"
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "0" "ssrf resolve: curl not invoked"
expect_eq "$(jq -r 'select(.id=="x1") | .blocked' "$WORK/ssrf2.jsonl")" "host resolves to a non-public address" "ssrf resolve blocked reason"

: >"$LOG"
expect_exit "ssrf redirect to loopback" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_CODE=302 STUB_REDIRECT=https://127.0.0.1/admin bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/ssrf3.jsonl"
expect_contains "$LAST_OUTPUT" "reachable=0/2" "ssrf redirect summary"
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "2" "ssrf redirect: only first hop fetched per task"
expect_eq "$(jq -r 'select(.id=="x1") | .blocked' "$WORK/ssrf3.jsonl")" "non-public or non-canonical IP literal" "ssrf redirect blocked reason"

: >"$LOG"
expect_exit "ssrf redirect to http" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_CODE=301 STUB_REDIRECT=http://example.org/ bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/ssrf4.jsonl"
expect_eq "$(jq -r 'select(.id=="x1") | .blocked' "$WORK/ssrf4.jsonl")" "scheme is not https" "ssrf redirect http blocked"

: >"$LOG"
expect_exit "redirect loop" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_CODE=302 STUB_REDIRECT=https://example.net/ bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/loop.jsonl"
expect_eq "$(jq -r 'select(.id=="x1") | .curl_exit' "$WORK/loop.jsonl")" "47" "redirect loop curl_exit"
expect_eq "$(jq -r 'select(.id=="x1") | .reachable' "$WORK/loop.jsonl")" "false" "redirect loop unreachable"
expect_eq "$(grep -c . "$LOG")" "12" "redirect loop: 6 hops per task"

: >"$LOG"
expect_exit "connected to private ip" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_IP=192.168.0.1 bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/rip.jsonl"
expect_eq "$(jq -r 'select(.id=="x1") | .blocked' "$WORK/rip.jsonl")" "connected to a non-public address" "remote_ip post-check"

: >"$LOG"
expect_exit "dns pin" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/pin.jsonl"
expect_contains "$(cat "$LOG")" "--resolve example.com:443:93.184.216.34" "dns pinned to validated ip"

: >"$LOG"
expect_exit "dns pin ipv6" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_RESOLVE_IP=2606:4700::1 STUB_IP=2606:4700::1 bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/pin6.jsonl"
expect_contains "$(cat "$LOG")" "--resolve example.com:443:[2606:4700::1]" "ipv6 pinned to validated ip"

: >"$LOG"
expect_exit "unresolvable host rejected" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_RESOLVE_NONE=1 bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/nores.jsonl"
expect_contains "$LAST_OUTPUT" "reachable=0/2" "unresolvable summary"
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "0" "unresolvable: curl not invoked"
expect_eq "$(jq -r 'select(.id=="x1") | .blocked' "$WORK/nores.jsonl")" "host could not be resolved for address pinning" "unresolvable blocked reason"

# 末尾ドット付きホストは curl を呼ばず取得前に拒否する（--resolve の固定キーと接続先ホストのずれを作らない）
write_tasks dot '[{"id":"d1","cat":"static","url":"https://example.com./","selector":"h1","kind":"text"}]'
: >"$LOG"
expect_exit "trailing dot host rejected" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" bash "$CHECK" --tasks "$WORK/dot.json" --out "$WORK/dot.jsonl"
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "0" "trailing dot: curl not invoked"
expect_eq "$(jq -r 'select(.id=="d1") | .blocked' "$WORK/dot.jsonl")" "host name is not a canonical ASCII DNS name" "trailing dot blocked reason"
# リダイレクト先の末尾ドットも同様に拒否する
: >"$LOG"
expect_exit "redirect to trailing dot" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_CODE=302 STUB_REDIRECT=https://example.net./ bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/dot2.jsonl"
expect_eq "$(jq -r 'select(.id=="x1") | .blocked' "$WORK/dot2.jsonl")" "host name is not a canonical ASCII DNS name" "redirect trailing dot blocked"
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "2" "redirect trailing dot: only first hop fetched per task"

# DNS 解決にも期限をかける（P1）: 解決が詰まっても --total-timeout で打ち切り、以降の取得を止めて記録する
: >"$LOG"
dns_start=$SECONDS
expect_exit "slow dns total deadline" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_RESOLVE_SLEEP=20 bash "$CHECK" --tasks "$WORK/net.json" --total-timeout 1 --out "$WORK/slowdns.jsonl"
[ $((SECONDS - dns_start)) -lt 10 ] || { echo "FAIL [slow dns bounded]: took $((SECONDS - dns_start))s" >&2; FAILURES=$((FAILURES + 1)); }
CASES=$((CASES + 1))
expect_eq "$(wc -l <"$LOG" | tr -d ' ')" "0" "slow dns: curl not invoked"
expect_eq "$(jq -r 'select(.id=="x2") | .blocked' "$WORK/slowdns.jsonl")" "total time limit exceeded" "slow dns total deadline blocked reason"
expect_eq "$(jq -s 'length' "$WORK/slowdns.jsonl")" "3" "slow dns: all records written"
expect_eq "$(env PATH="$STUB_DIR:$PATH" STUB_RESOLVE_SLEEP=20 STUB_RESOLVE_PID="$WORK/res.pid" bash -c ". '$SCRIPT_DIR/lib.sh'; resolve_host_ips example.com 1")" "" "resolve_host_ips honors limit"
# 期限超過時に解決コマンド自体が残らない（P1: 子プロセスの回収）
sleep 0.5
expect_exit "slow dns resolver process killed" 1 kill -0 "$(cat "$WORK/res.pid")"

# DNS 解決で時間を使った後は残り時間を再計算する（P1）。解決スタブが 2 秒かかる場合、全体期限 3 秒では
# 解決後の curl に元の --max-time 3 を渡さない（残り 1 秒以下へ切り詰めるか、期限切れなら curl を呼ばない）
write_tasks one '[{"id":"o1","cat":"static","url":"https://example.com/","selector":"h1","kind":"text"}]'
: >"$LOG"
expect_exit "dns consumes total deadline" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" STUB_RESOLVE_DELAY=2 bash "$CHECK" --tasks "$WORK/one.json" --total-timeout 3 --out "$WORK/dnsused.jsonl"
expect_eq "$(grep -c -- '--max-time 3' "$LOG" || true)" "0" "dns consumed deadline: max-time is recomputed after resolve"

# getent / dscacheutil が無い環境（Windows の Bash 環境）では powershell で解決する。
# Linux / macOS では getent / dscacheutil が先に選ばれるため PATH をスタブだけに絞る。Windows（Git Bash）では
# 絞った PATH だと MSYS のシンボリックリンク・DLL 解決が不安定なため、スタブを PATH 先頭へ足すだけにする
PS_DIR="$WORK/ps"
mkdir -p "$PS_DIR"
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) PS_PATH="$PS_DIR:$PATH" ;;
  *)
    for t in awk sort tr; do ln -s "$(type -P "$t")" "$PS_DIR/$t"; done
    PS_PATH="$PS_DIR"
    ;;
esac
cat >"$PS_DIR/powershell.exe" <<'STUB'
#!/bin/sh
# CRLF 付きで STUB_PS_IPS（空白区切り）を返し、環境変数経由のホスト名を STUB_PS_LOG へ記録する
# STUB_PS_FAIL があれば DNS 失敗（GetHostAddresses の例外）を模して何も出さず非 0 で終わる
[ -z "${STUB_PS_LOG:-}" ] || echo "host=$FANDHE_RESOLVE_HOST" >>"$STUB_PS_LOG"
[ -z "${STUB_PS_FAIL:-}" ] || exit 1
for ip in $STUB_PS_IPS; do printf '%s\r\n' "$ip"; done
STUB
chmod +x "$PS_DIR/powershell.exe"
PS_LOG="$WORK/ps.log"
: >"$PS_LOG"
expect_eq "$(env PATH="$PS_PATH" STUB_PS_LOG="$PS_LOG" STUB_PS_IPS="93.184.216.34 2606:4700::1" "$BASH" -c ". '$SCRIPT_DIR/lib.sh'; resolve_host_ips example.com" | tr '\n' ' ')" \
  "2606:4700::1 93.184.216.34 " "powershell resolver output (CR 除去・ソート)"
expect_contains "$(cat "$PS_LOG")" "host=example.com" "powershell receives host via env"
: >"$PS_LOG"
expect_eq "$(env PATH="$PS_PATH" STUB_PS_LOG="$PS_LOG" STUB_PS_IPS="93.184.216.34" "$BASH" -c ". '$SCRIPT_DIR/lib.sh'; resolve_host_ips 'a;b\$(x).com'")" "" "powershell resolver rejects unsafe host"
expect_eq "$(wc -c <"$PS_LOG" | tr -d ' ')" "0" "powershell not invoked for unsafe host"
# DNS 失敗（powershell が非 0 終了）でも set -eo pipefail 下で resolve_host_ips は 0 を返し何も出さない
expect_exit "powershell dns failure returns 0" 0 env PATH="$PS_PATH" STUB_PS_FAIL=1 "$BASH" -c "set -eo pipefail; . '$SCRIPT_DIR/lib.sh'; r=\$(resolve_host_ips nx.example.com); [ -z \"\$r\" ]"

: >"$LOG"
expect_exit "proxy disabled" 0 env PATH="$STUB_DIR:$PATH" STUB_LOG="$LOG" bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/noproxy.jsonl"
expect_contains "$(cat "$LOG")" "-q -sS --noproxy *" "curl ignores curlrc and proxies"

# --- Windows の jq が出す CRLF を除去する（CR が URL・件数・出力へ混入しない）---
CRLF_DIR="$WORK/crlf"
mkdir -p "$CRLF_DIR"
cat >"$CRLF_DIR/jq" <<'STUB'
#!/usr/bin/env bash
"$REAL_JQ" "$@" | sed 's/$/\r/'
exit "${PIPESTATUS[0]}"
STUB
chmod +x "$CRLF_DIR/jq"
expect_exit "crlf jq" 0 env REAL_JQ="$(type -P jq)" PATH="$CRLF_DIR:$STUB_DIR:$PATH" bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/crlf.jsonl"
expect_contains "$LAST_OUTPUT" "reachable=2/2" "crlf jq summary"
expect_eq "$(grep -c "$(printf '\r')" "$WORK/crlf.jsonl" || true)" "0" "crlf jq: no CR in jsonl"
expect_eq "$(jq -r 'select(.id=="x1") | .url' "$WORK/crlf.jsonl")" "https://example.com/" "crlf jq: url intact"

# --- resolve_bin ---
expect_exit "bin missing" 2 resolve_bin "$WORK/nope"
mkdir -p "$WORK/adir"
expect_exit "bin is directory" 2 resolve_bin "$WORK/adir"
printf 'plain text, not a script\n' >"$WORK/notexec"
chmod -x "$WORK/notexec"
expect_exit "bin not executable" 2 resolve_bin "$WORK/notexec"
printf '#!/bin/sh\n' >"$WORK/fakebin"
chmod +x "$WORK/fakebin"
expect_exit "bin ok via arg" 0 resolve_bin "$WORK/fakebin"
expect_eq "$LAST_OUTPUT" "$WORK/fakebin" "bin ok path"
expect_exit "bin ok via env" 0 env FANDHE_BROWSER_BIN="$WORK/fakebin" bash -c ". '$SCRIPT_DIR/lib.sh'; resolve_bin"
expect_eq "$LAST_OUTPUT" "$WORK/fakebin" "bin env path"
expect_exit "access_check env bin missing" 2 env FANDHE_BROWSER_BIN="$WORK/nope" bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/out4.jsonl"
expect_exit "access_check env bin ok" 0 env PATH="$STUB_DIR:$PATH" FANDHE_BROWSER_BIN="$WORK/fakebin" bash "$CHECK" --tasks "$WORK/net.json" --out "$WORK/out5.jsonl"
expect_eq "$(jq -r 'select(.type=="meta") | .bin' "$WORK/out5.jsonl")" "$WORK/fakebin" "meta bin recorded via env"
expect_exit "access_check --bin missing" 2 bash "$CHECK" --bin "$WORK/nope" --tasks "$WORK/net.json" --out "$WORK/out2.jsonl"
expect_exit "access_check --bin ok" 0 env PATH="$STUB_DIR:$PATH" bash "$CHECK" --bin "$WORK/fakebin" --tasks "$WORK/net.json" --out "$WORK/out3.jsonl"
expect_eq "$(jq -r 'select(.type=="meta") | .bin' "$WORK/out3.jsonl")" "$WORK/fakebin" "meta bin recorded"

echo "cases=$CASES failures=$FAILURES"
[ "$FAILURES" -eq 0 ]

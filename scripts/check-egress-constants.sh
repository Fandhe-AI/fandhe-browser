#!/usr/bin/env bash
#
# egress プロキシ（本体 workspace）と servo-host の中継（別 workspace）で複製している
# 上限値が一致していることを検査する（RENDER-3・SEC-2・TASK-38・#1450）。
# 別 workspace のため import できず定数を複製しており、片側だけ緩める変更を CI で検出する。
# 呼び出し元: scripts/check-render-isolation.sh の検査 F（CI の harness-* と servo-host-ubuntu の
# [render-isolation] ステップ）と、Makefile の check-egress-constants。
#
# 比較する組（本体 = 左、ホスト relay.rs = 右）:
#   egress_proxy.rs   MAX_CONCURRENT_CONNECTIONS <-> MAX_CONNECTIONS
#   egress_proxy.rs   IDLE_TIMEOUT               <-> IDLE_TIMEOUT
#   egress_proxy.rs   CONNECT_TIMEOUT            <-> CONNECT_TIMEOUT
#   egress_proxy.rs   MAX_RESOLVER_THREADS       <-> MAX_DIAL_THREADS
#   server.rs         REJECT_WRITE_TIMEOUT       <-> REJECT_WRITE_TIMEOUT
# TUNNEL_TICK <-> TICK は対象外: relay.rs は複製と明記しておらず、アイドル判定の刻みは
# 独立に変えてよい値のため。
#
# 値は 1 行の `const NAME: T = <式>;` から取り、整数リテラル・Duration::from_secs(N)・
# Duration::from_millis(N) だけを解釈する（秒は ms に換算）。定義の欠落・重複・解釈不能は NG
# （fail-closed）。BSD/Git Bash でも動くよう grep -E・sed -E・bash 算術だけを使う。
#
# 使い方: 引数なし = 実ファイルで判定 / --self-test = 合成ファイルで判定関数を確認 / 他 = exit 2
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROXY_RS="crates/fandhe-browser-render/src/egress_proxy.rs"
SERVER_RS="crates/fandhe-browser-render/src/egress_proxy/server.rs"
RELAY_RS="servo-host/src/relay.rs"

# const_value <file> <NAME>: 正規化した値を標準出力へ。"int:N" か "ms:N"。失敗は非 0。
const_value() {
  local file="$1" name="$2" lines count expr n
  if [ ! -f "$file" ]; then
    echo "NG: file not found: ${file}" >&2
    return 1
  fi
  lines="$(tr -d '\r' <"$file" | grep -E "^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?const[[:space:]]+${name}:" || true)"
  if [ -z "$lines" ]; then
    count=0
  else
    count="$(printf '%s\n' "$lines" | wc -l | tr -d ' ')"
  fi
  if [ "$count" -ne 1 ]; then
    echo "NG: expected exactly 1 definition of ${name} in ${file}, found ${count}" >&2
    return 1
  fi
  expr="$(printf '%s\n' "$lines" | sed -E 's/^[^=]*=[[:space:]]*//; s/[[:space:]]*;.*$//')"
  if [[ "$expr" =~ ^[0-9][0-9_]*$ ]]; then
    echo "int:${expr//_/}"
  elif [[ "$expr" =~ ^Duration::from_secs\(([0-9][0-9_]*)\)$ ]]; then
    n="${BASH_REMATCH[1]//_/}"
    echo "ms:$((n * 1000))"
  elif [[ "$expr" =~ ^Duration::from_millis\(([0-9][0-9_]*)\)$ ]]; then
    echo "ms:${BASH_REMATCH[1]//_/}"
  else
    echo "NG: cannot interpret ${name} in ${file}: ${expr}" >&2
    return 1
  fi
}

# compare_pair <file_a> <name_a> <file_b> <name_b>
compare_pair() {
  local a b
  a="$(const_value "$1" "$2")" || return 1
  b="$(const_value "$3" "$4")" || return 1
  if [ "$a" != "$b" ]; then
    echo "NG: $2 (${a}) in $1 != $4 (${b}) in $3" >&2
    return 1
  fi
  echo "OK: $2 == $4 (${a})"
}

# compare <egress_proxy.rs> <server.rs> <relay.rs>
compare() {
  local proxy="$1" server="$2" relay="$3" st=0
  compare_pair "$proxy" MAX_CONCURRENT_CONNECTIONS "$relay" MAX_CONNECTIONS || st=1
  compare_pair "$proxy" IDLE_TIMEOUT "$relay" IDLE_TIMEOUT || st=1
  compare_pair "$proxy" CONNECT_TIMEOUT "$relay" CONNECT_TIMEOUT || st=1
  compare_pair "$proxy" MAX_RESOLVER_THREADS "$relay" MAX_DIAL_THREADS || st=1
  compare_pair "$server" REJECT_WRITE_TIMEOUT "$relay" REJECT_WRITE_TIMEOUT || st=1
  return "$st"
}

# write_set <dir> [conns] [idle] [connect] [threads] [reject]: 既定は一致する値の合成ファイル一式。
write_set() {
  local d="$1"
  mkdir -p "$d"
  {
    echo "pub const MAX_CONCURRENT_CONNECTIONS: usize = ${2:-128};"
    echo "pub const IDLE_TIMEOUT: Duration = ${3:-Duration::from_secs(300)};"
    echo "pub const CONNECT_TIMEOUT: Duration = ${4:-Duration::from_secs(10)};"
    echo "pub const MAX_RESOLVER_THREADS: usize = ${5:-32};"
  } >"$d/proxy.rs"
  echo "const REJECT_WRITE_TIMEOUT: Duration = ${6:-Duration::from_millis(100)};" >"$d/server.rs"
  {
    echo "const MAX_CONNECTIONS: usize = 128;"
    echo "const IDLE_TIMEOUT: Duration = Duration::from_secs(300);"
    echo "const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);"
    echo "const MAX_DIAL_THREADS: usize = 32;"
    echo "const REJECT_WRITE_TIMEOUT: Duration = Duration::from_millis(100);"
  } >"$d/relay.rs"
}

SELF_TEST_FAILURES=0

# expect <説明> <ok|ng> <dir>
expect() {
  local rc=0
  compare "$3/proxy.rs" "$3/server.rs" "$3/relay.rs" >/dev/null 2>&1 || rc=$?
  if { [ "$2" = ok ] && [ "$rc" -eq 0 ]; } || { [ "$2" = ng ] && [ "$rc" -ne 0 ]; }; then
    echo "OK(self-test): $1"
  else
    echo "NG(self-test): $1" >&2
    SELF_TEST_FAILURES=$((SELF_TEST_FAILURES + 1))
  fi
}

self_test() {
  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT
  write_set "$tmp/same"
  expect "matching values pass" ok "$tmp/same"
  write_set "$tmp/diff" 64
  expect "connection limit mismatch is NG" ng "$tmp/diff"
  write_set "$tmp/diff2" "" "" "" "" "Duration::from_millis(200)"
  expect "reject timeout mismatch is NG" ng "$tmp/diff2"
  write_set "$tmp/missing"
  sed -i.bak '/MAX_RESOLVER_THREADS/d' "$tmp/missing/proxy.rs"
  expect "missing definition is NG" ng "$tmp/missing"
  write_set "$tmp/dup"
  echo 'const IDLE_TIMEOUT: Duration = Duration::from_secs(300);' >>"$tmp/dup/relay.rs"
  expect "duplicate definition is NG" ng "$tmp/dup"
  write_set "$tmp/expr" 8
  sed -i.bak 's/= 8;/= 8 * 16;/' "$tmp/expr/proxy.rs"
  expect "uninterpretable expression is NG" ng "$tmp/expr"
  write_set "$tmp/units" "" "Duration::from_millis(300_000)" "Duration::from_millis(10000)"
  expect "from_secs and from_millis of the same duration pass" ok "$tmp/units"
  write_set "$tmp/vis"
  sed -i.bak 's/^pub const IDLE_TIMEOUT/pub(crate) const IDLE_TIMEOUT/' "$tmp/vis/proxy.rs"
  sed -i.bak 's/^const CONNECT_TIMEOUT/pub const CONNECT_TIMEOUT/' "$tmp/vis/relay.rs"
  expect "mixed pub and private consts pass" ok "$tmp/vis"
  write_set "$tmp/crlf"
  sed -i.bak 's/$/\r/' "$tmp/crlf/relay.rs"
  expect "CRLF files pass" ok "$tmp/crlf"
  if [ "$SELF_TEST_FAILURES" -ne 0 ]; then
    echo "NG: self-test has ${SELF_TEST_FAILURES} failure(s)" >&2
    return 1
  fi
  echo "OK: all self-tests passed"
}

case "${1:-}" in
  --self-test) self_test ;;
  "")
    cd "$REPO_ROOT"
    compare "$PROXY_RS" "$SERVER_RS" "$RELAY_RS"
    echo "OK: egress constants are in sync"
    ;;
  *)
    echo "usage: $(basename "$0") [--self-test]" >&2
    exit 2
    ;;
esac

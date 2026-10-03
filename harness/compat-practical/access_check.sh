#!/usr/bin/env bash
#
# 22 サイトへの到達可否を curl で記録する（TASK-71.1・MEAS-4。関連 COMPAT-4・JS-2）。
# tasks.json（PoC-9 から移植した 22 タスク定義）を検証し、各 URL へ HTTPS で到達できるかを
# JSONL に書く。結果は後続の run_core.sh（TASK-71.2・#311）と matrix.json 生成
# （TASK-71.3・#312）が jq で読む。本スクリプトは fandhe-browser を起動しない。
#
# 使い方: access_check.sh [--tasks P] [--out P] [--timeout SEC] [--bin P] [--validate-only]
# 終了コード: 0=記録完了（到達不能サイトがあっても 0。到達可否は計測結果でありゲートではない）
#            1=記録の書き込み失敗  2=入力・使用エラー（スキーマ違反・引数不正・jq/curl 無し・バイナリ検証失敗）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"

TASKS="$SCRIPT_DIR/tasks.json"
OUT="$SCRIPT_DIR/results/access_check.jsonl"
TIMEOUT=8
BIN_ARG=""
VALIDATE_ONLY=0
# UA は偽装しない正直な識別子（security.md: UA 偽装・anti-bot 回避の禁止）
UA="fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)"
ID_RE='^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'
MAX_ENTRIES=1000
MAX_BYTES=1048576

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --tasks) [ $# -ge 2 ] || die "--tasks requires a value"; TASKS="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out requires a value"; OUT="$2"; shift 2 ;;
    --timeout) [ $# -ge 2 ] || die "--timeout requires a value"; TIMEOUT="$2"; shift 2 ;;
    --bin) [ $# -ge 2 ] || die "--bin requires a value"; BIN_ARG="$2"; shift 2 ;;
    --validate-only) VALIDATE_ONLY=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

command -v jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"
if ! [[ "$TIMEOUT" =~ ^[1-9][0-9]?$ ]] || [ "$TIMEOUT" -gt 60 ]; then
  die "--timeout must be an integer in 1..60"
fi
[ -f "$TASKS" ] || die "tasks file not found: $TASKS"
size=$(wc -c <"$TASKS" | tr -d ' ')
[ "$size" -le "$MAX_BYTES" ] || die "tasks file too large: $size bytes"

# スキーマ検証。違反内容を 1 行ずつ jq が列挙する（不正 JSON は jq が非 0 で終わる）。
# id・cat は表示用に切り詰め、CI ログでワークフローコマンドとして解釈されない文字だけに制限済み。
set +e
violations=$(jq -r --arg re "$ID_RE" --argjson max "$MAX_ENTRIES" '
  def nm: (.id | tostring | .[0:80]);
  if type != "array" then "top-level must be an array"
  elif length < 1 or length > $max then "entry count must be in 1.." + ($max | tostring)
  elif any(.[]; type != "object") then "every entry must be an object"
  else
    ( (map(.id | tostring) | group_by(.) | map(select(length > 1) | "duplicate id: " + (.[0] | .[0:80]))[]),
      ( .[] | . as $t |
        ( if (.id | type) == "string" and (.id | test($re)) then empty else "invalid id" end ),
        ( if (.cat | type) == "string" and (.cat | test($re)) then empty else "invalid cat" end ),
        ( if (.kind | type) == "string" and (["text", "texts", "form"] | index($t.kind)) != null then empty else "invalid kind" end ),
        ( if (.url | type) == "string" and (.url | test("^https://[^[:space:][:cntrl:]]+$")) then empty else "invalid url" end ),
        ( if (.selector | type) == "string" and (.selector | length) > 0 then empty else "invalid selector" end )
      )
    )
  end' "$TASKS" 2>/dev/null)
jq_status=$?
set -e
[ "$jq_status" -eq 0 ] || die "tasks file is not valid JSON: $TASKS"
if [ -n "$violations" ]; then
  echo "error: tasks schema violations:" >&2
  printf '  %s\n' "$violations" >&2
  exit 2
fi

COUNT=$(jq length "$TASKS")
if [ "$VALIDATE_ONLY" -eq 1 ]; then
  echo "ok: $COUNT tasks validated"
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"

BIN_RESOLVED=""
if [ -n "$BIN_ARG" ]; then
  BIN_RESOLVED="$(resolve_bin "$BIN_ARG")" || exit 2
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR" || { echo "error: cannot create $OUT_DIR" >&2; exit 1; }
TMP="$(mktemp "$OUT_DIR/.access_check.XXXXXX")" || { echo "error: cannot create temp file" >&2; exit 1; }
trap 'rm -f "$TMP"' EXIT

jq -cn --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg ua "$UA" --argjson to "$TIMEOUT" \
  --arg sha "$(sha256_of "$TASKS")" --arg bin "$BIN_RESOLVED" \
  '{type:"meta",measured_at:$at,user_agent:$ua,timeout_sec:$to,tasks_sha256:$sha,bin:(if $bin=="" then null else $bin end)}' >"$TMP"

REACHABLE=0
while IFS=$'\t' read -r id cat url; do
  start=$(date +%s)
  set +e
  code=$(curl -sS -o /dev/null -w '%{http_code}' -L --max-redirs 5 --max-time "$TIMEOUT" \
    --proto '=https' --proto-redir '=https' -A "$UA" -- "$url" 2>/dev/null)
  rc=$?
  set -e
  elapsed=$((($(date +%s) - start) * 1000))
  [[ "$code" =~ ^[0-9]{3}$ ]] || code=0
  [ "$rc" -eq 0 ] || code=0
  reach=false
  if [ "$rc" -eq 0 ] && [ "$code" -ge 200 ] && [ "$code" -lt 400 ]; then
    reach=true
    REACHABLE=$((REACHABLE + 1))
  fi
  jq -cn --arg id "$id" --arg cat "$cat" --arg url "$url" --argjson st "$code" --argjson rc "$rc" \
    --argjson ms "$elapsed" --argjson reach "$reach" \
    '{type:"result",id:$id,cat:$cat,url:$url,http_status:$st,reachable:$reach,curl_exit:$rc,elapsed_ms:$ms}' >>"$TMP"
  echo "[$id] status=$code reachable=$reach"
done < <(jq -r '.[] | [.id, .cat, .url] | @tsv' "$TASKS")

mv -f "$TMP" "$OUT" || { echo "error: cannot write $OUT" >&2; exit 1; }
trap - EXIT
echo "reachable=$REACHABLE/$COUNT"

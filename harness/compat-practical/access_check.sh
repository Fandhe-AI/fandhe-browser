#!/usr/bin/env bash
#
# 22 サイトへの到達可否を curl で記録する（TASK-71.1・MEAS-4。関連 COMPAT-4・JS-2）。
# tasks.json（PoC-9 から移植した 22 タスク定義）を検証し、各 URL へ HTTPS で到達できるかを
# JSONL に書く。結果は後続の run_core.sh（TASK-71.2・#311）と matrix.json 生成
# （TASK-71.3・#312）が jq で読む。本スクリプトは fandhe-browser を起動しない。
#
# 使い方: access_check.sh [--tasks P] [--out P] [--timeout SEC] [--total-timeout SEC] [--bin P] [--validate-only]
# 終了コード: 0=記録完了（到達不能サイトがあっても 0。到達可否は計測結果でありゲートではない）
#            1=記録の書き込み失敗  2=入力・使用エラー（スキーマ違反・引数不正・jq/curl 無し・バイナリ検証失敗）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"

TASKS="$SCRIPT_DIR/tasks.json"
OUT="$SCRIPT_DIR/results/access_check.jsonl"
TIMEOUT=8
# 全タスク合計の実行時間上限（秒）。--timeout は curl 1 回ごとの上限で、最大 1000 タスク × 最大 6 取得
# （リダイレクト追従込み）では実行時間が際限なく伸びるため、全体の期限を別に設ける（P0: リソース上限）
TOTAL_TIMEOUT=600
BIN_ARG=""
VALIDATE_ONLY=0
# UA は偽装しない正直な識別子（security.md: UA 偽装・anti-bot 回避の禁止）
UA="fandhe-browser-harness/0.1 (+https://github.com/Fandhe-AI/fandhe-browser)"
# \A・\z で全体一致を強制する（`$` は末尾の改行を許すため使わない。check-matrix.sh と同方針）
ID_RE='\A[A-Za-z0-9][A-Za-z0-9._-]{0,63}\z'
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
    --total-timeout) [ $# -ge 2 ] || die "--total-timeout requires a value"; TOTAL_TIMEOUT="$2"; shift 2 ;;
    --bin) [ $# -ge 2 ] || die "--bin requires a value"; BIN_ARG="$2"; shift 2 ;;
    --validate-only) VALIDATE_ONLY=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

# lib.sh が jq 関数を定義するため `command -v jq` は常に成功する。バイナリ自体を `type -P` で探す
type -P jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"
if ! [[ "$TIMEOUT" =~ ^[1-9][0-9]?$ ]] || [ "$TIMEOUT" -gt 60 ]; then
  die "--timeout must be an integer in 1..60"
fi
if ! [[ "$TOTAL_TIMEOUT" =~ ^[1-9][0-9]{0,3}$ ]] || [ "$TOTAL_TIMEOUT" -gt 3600 ]; then
  die "--total-timeout must be an integer in 1..3600"
fi
[ -f "$TASKS" ] || die "tasks file not found: $TASKS"
size=$(wc -c <"$TASKS" | tr -d ' ')
[ "$size" -le "$MAX_BYTES" ] || die "tasks file too large: $size bytes"

# スキーマ検証。違反内容を 1 行ずつ jq が列挙する（不正 JSON は jq が非 0 で終わる）。
# id・cat は表示用に切り詰め、CI ログでワークフローコマンドとして解釈されない文字だけに制限済み。
# JSON 文書がちょうど 1 個であることを先に確認する。空ファイルは jq のフィルタが一度も走らず
# 終了コード 0・違反なしになり、複数文書は先頭以外が検証されないため、どちらも拒否する。
set +e
doc_count=$(jq -n '[inputs] | length' "$TASKS" 2>/dev/null)
doc_status=$?
set -e
[ "$doc_status" -eq 0 ] || die "tasks file is not valid JSON: $TASKS"
[ "$doc_count" = "1" ] || die "tasks file must contain exactly one JSON document (found $doc_count): $TASKS"

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
        ( if (.url | type) == "string" and (.url | test("\\Ahttps://[^[:space:][:cntrl:]]+\\z")) then empty else "invalid url" end ),
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
# --bin だけでなく環境変数 FANDHE_BROWSER_BIN のみの指定でも検証・記録する
# （resolve_bin は空引数なら環境変数を見る）
if [ -n "$BIN_ARG" ] || [ -n "${FANDHE_BROWSER_BIN:-}" ]; then
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

# MSYS2_ARG_CONV_EXCL: Windows（Git Bash）がネイティブ jq へ渡す /tmp/... 形式の --arg 値を
# C:/... へ変換して記録値がずれるのを防ぐ（この呼び出しはファイル引数を持たないため全除外してよい）
MSYS2_ARG_CONV_EXCL='*' jq -cn --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg ua "$UA" --argjson to "$TIMEOUT" --argjson tto "$TOTAL_TIMEOUT" \
  --arg sha "$(sha256_of "$TASKS")" --arg bin "$BIN_RESOLVED" \
  '{type:"meta",measured_at:$at,user_agent:$ua,timeout_sec:$to,total_timeout_sec:$tto,tasks_sha256:$sha,bin:(if $bin=="" then null else $bin end)}' >"$TMP"

MAX_REDIRS=5

# fetch_site <url>
#   リダイレクトを自前で最大 MAX_REDIRS 回追い、各ホップで url_check（scheme・ポート・内部名・IP リテラル）と
#   DNS 解決後アドレスの公開判定を行う（SSRF 対策。curl -L は使わない）。解決できたときは
#   --resolve で検証済み IPv4/IPv6 へ接続先を固定する（固定できないホストは取得前に拒否）。
#   プロキシは --noproxy '*' で無効化し、curl の remote_ip も事後検証する。
#   結果をグローバル F_CODE / F_RC / F_MS / F_BLOCKED（拒否理由。無ければ空）へ設定する。
fetch_site() {
  local cur="$1" hop=0 host ips ip out rc code t rip loc ms reason remaining max_time
  local pins=()
  F_CODE=0; F_RC=0; F_MS=0; F_BLOCKED=""
  while :; do
    # 全体期限（TOTAL_TIMEOUT）を超えたら以降の取得は行わず、記録だけ確定させる（curl_exit=28 は timeout 相当）
    remaining=$((TOTAL_TIMEOUT - SECONDS))
    if [ "$remaining" -le 0 ]; then
      F_BLOCKED="total time limit exceeded"; F_CODE=0; F_RC=28; return 0
    fi
    max_time=$TIMEOUT
    if [ "$remaining" -lt "$max_time" ]; then max_time=$remaining; fi
    if ! reason="$(url_check "$cur")"; then
      F_BLOCKED="$reason"; F_CODE=0; return 0
    fi
    host="$(url_host "$cur")"
    pins=()
    if ! [[ "$host" =~ ^[0-9.]+$ ]]; then
      # DNS 解決にも全体期限の残り時間を上限としてかける（getent 等は curl の --max-time の対象外）
      ips="$(resolve_host_ips "$host" "$max_time")"
      for ip in $ips; do
        if ! is_public_ip "$ip"; then
          F_BLOCKED="host resolves to a non-public address"; F_CODE=0; return 0
        fi
        case "$ip" in
          *:*) pins+=(--resolve "$host:443:[$ip]") ;;
          *) pins+=(--resolve "$host:443:$ip") ;;
        esac
      done
      # 固定できるアドレスが無いホストは再解決による接続先すり替えを防げないため取得前に拒否する
      if [ "${#pins[@]}" -eq 0 ]; then
        if [ "$((TOTAL_TIMEOUT - SECONDS))" -le 0 ]; then
          F_BLOCKED="total time limit exceeded"; F_CODE=0; F_RC=28; return 0
        fi
        F_BLOCKED="host could not be resolved for address pinning"; F_CODE=0; return 0
      fi
    fi
    # DNS 解決で時間を使った分を差し引いて残り時間を再計算し、期限切れなら curl を起動せず記録を確定する
    remaining=$((TOTAL_TIMEOUT - SECONDS))
    if [ "$remaining" -le 0 ]; then
      F_BLOCKED="total time limit exceeded"; F_CODE=0; F_RC=28; return 0
    fi
    max_time=$TIMEOUT
    if [ "$remaining" -lt "$max_time" ]; then max_time=$remaining; fi
    set +e
    # -q: .curlrc を読まない。--noproxy '*': 環境変数・設定のプロキシを無効化し、--resolve で固定した
    # 検証済みアドレスへ直接接続させる（プロキシ経由だと最終宛先を固定・検証できず SSRF 対策を迂回される）
    out=$(LC_ALL=C curl -q -sS --noproxy '*' -o /dev/null -w '%{http_code} %{time_total} %{remote_ip} %{redirect_url}' \
      --max-time "$max_time" --proto '=https' --proto-redir '=https' -A "$UA" \
      ${pins[@]+"${pins[@]}"} -- "$cur" 2>/dev/null)
    rc=$?
    set -e
    code="" t="" rip="" loc=""
    read -r code t rip loc <<<"$out" || true
    ms=$(LC_ALL=C awk -v t="$t" 'BEGIN { if (t ~ /^[0-9]+(\.[0-9]+)?$/) printf "%d", t * 1000; else print 0 }')
    F_MS=$((F_MS + ms))
    F_RC=$rc
    [[ "$code" =~ ^[0-9]{3}$ ]] || code=0
    code=$((10#$code))
    if [ "$rc" -ne 0 ]; then F_CODE=0; return 0; fi
    if [ -n "$rip" ] && ! is_public_ip "$rip"; then
      F_BLOCKED="connected to a non-public address"; F_CODE=0; return 0
    fi
    F_CODE=$code
    if [ "$code" -ge 300 ] && [ "$code" -lt 400 ] && [ -n "$loc" ]; then
      hop=$((hop + 1))
      if [ "$hop" -gt "$MAX_REDIRS" ]; then F_CODE=0; F_RC=47; return 0; fi
      cur="$loc"
      continue
    fi
    return 0
  done
}

REACHABLE=0
# bash 組み込みの SECONDS で全体期限を測る。ここから計時を始める
SECONDS=0
# @tsv はバックスラッシュ等をエスケープし read -r が復元しないため、スキーマで制御文字を除いた
# フィールドを US（0x1f）区切りで渡して値を変えない（url に 0x1f は含まれない）
while IFS=$'\x1f' read -r id cat url; do
  fetch_site "$url"
  reach=false
  if [ "$F_RC" -eq 0 ] && [ -z "$F_BLOCKED" ] && [ "$F_CODE" -ge 200 ] && [ "$F_CODE" -lt 400 ]; then
    reach=true
    REACHABLE=$((REACHABLE + 1))
  fi
  # Windows（Git Bash）の引数変換が url のバックスラッシュ・スラッシュを書き換えないよう全除外する
  MSYS2_ARG_CONV_EXCL='*' jq -cn --arg id "$id" --arg cat "$cat" --arg url "$url" --argjson st "$F_CODE" --argjson rc "$F_RC" \
    --argjson ms "$F_MS" --argjson reach "$reach" --arg bl "$F_BLOCKED" \
    '{type:"result",id:$id,cat:$cat,url:$url,http_status:$st,reachable:$reach,curl_exit:$rc,elapsed_ms:$ms,blocked:(if $bl=="" then null else $bl end)}' >>"$TMP"
  echo "[$id] status=$F_CODE reachable=$reach${F_BLOCKED:+ blocked=$F_BLOCKED}"
done < <(jq -r '.[] | [.id, .cat, .url] | join("\u001f")' "$TASKS")

mv -f "$TMP" "$OUT" || { echo "error: cannot write $OUT" >&2; exit 1; }
trap - EXIT
echo "reachable=$REACHABLE/$COUNT"

#!/usr/bin/env bash
#
# run_core.sh を複数回実行した結果 JSONL（core_results 形式）から、回ごとの成功数と動作率を集計して
# Markdown で出力する（TASK-59・`MEAS-4`・`COMPAT-4`。xos-measure.yml の compat-practical から呼ぶ）。
# 28 件（tasks.json 全体）と、PoC-1 の 22 件（28 件から 28 サイト化で追加した 6 件を除いたもの）の両方を出す。
# 追加 6 件は README.md「tasks.json」節の c4・d5・d7・d8・e3・e6。判定はしない（成功数の集計のみ。TASK-59 の判定は人間）。
#
# 使い方: summarize_repeats.sh [--tasks P] [--out-md P] [--total N] [--run N] RESULT.jsonl [[--run N] RESULT.jsonl ...]
#   --run N     直後の RESULT.jsonl の元の実行番号（1 以上）。省略時は直前の番号 + 1（先頭は 1）。失敗した回があって
#               成功した回のファイルだけを渡すときは、元の番号を保つために repeat_core.sh が必ず指定する
#   --total N   実行した回数（既定は渡した最大の実行番号）。結果ファイルの無い回は「欠測」行として表に残す
# 入力検証（fail-closed）: 各ファイルのメタ行がちょうど 1 行、結果行の id 集合が tasks.json と完全一致、
#   追加 6 件がすべて tasks.json にあること（22 件の定義が崩れていないこと）。違反は終了コード 1。引数エラーは 2。
# 終了コード: 0=集計完了、1=入力検証失敗、2=使用エラー
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TASKS="$SCRIPT_DIR/tasks.json"
OUT_MD=""
FILES=()
RUN_NUMS=()
NEXT_RUN=1
TOTAL=""

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --tasks) [ $# -ge 2 ] || die "--tasks requires a value"; TASKS="$2"; shift 2 ;;
    --out-md) [ $# -ge 2 ] || die "--out-md requires a value"; OUT_MD="$2"; shift 2 ;;
    --total) [ $# -ge 2 ] || die "--total requires a value"; TOTAL="$2"; shift 2 ;;
    --run) [ $# -ge 2 ] || die "--run requires a value"; NEXT_RUN="$2"; shift 2 ;;
    -*) die "unknown argument: $1" ;;
    *)
      [[ "$NEXT_RUN" =~ ^[1-9][0-9]?$ ]] || die "--run must be an integer in 1..99"
      FILES+=("$1"); RUN_NUMS+=("$NEXT_RUN"); NEXT_RUN=$((NEXT_RUN + 1)); shift ;;
  esac
done
[ "${#FILES[@]}" -ge 1 ] || die "at least one result file is required"
# 実行番号は昇順で重複しないこと（欠測行の位置と回番号を一意に対応させるため）
prev=0
for r in "${RUN_NUMS[@]}"; do
  [ "$r" -gt "$prev" ] || die "run numbers must be strictly increasing (got $r after $prev)"
  prev="$r"
done
[ -n "$TOTAL" ] || TOTAL="$prev"
{ [[ "$TOTAL" =~ ^[1-9][0-9]?$ ]] && [ "$TOTAL" -ge "$prev" ]; } || die "--total must be an integer in 1..99 and not less than the largest run number"
command -v jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"
[ -f "$TASKS" ] || die "tasks file not found: $TASKS"

# jq の出力から CR を除く（Windows のネイティブ jq は CRLF で出力するため）
jq() { command jq "$@" | tr -d '\r'; }

ADDED_JSON='["c4","d5","d7","d8","e3","e6"]'

bad() {
  echo "error: $*" >&2
  exit 1
}

# 追加 6 件が tasks.json にすべてあり、差し引いた残りが 22 件であること
jq -e --argjson added "$ADDED_JSON" '([.[].id] as $ids | ($added - $ids | length) == 0) and (([.[].id] - $added | length) == 22)' "$TASKS" >/dev/null \
  || bad "tasks.json does not match the 22-task definition (28 tasks minus c4,d5,d7,d8,e3,e6)"
TASK_IDS="$(jq -c '[.[].id] | sort' "$TASKS")"

ROWS="$(mktemp)"
trap 'rm -f "$ROWS"' EXIT
for idx in "${!FILES[@]}"; do
  f="${FILES[$idx]}"
  n="${RUN_NUMS[$idx]}"
  [ -f "$f" ] || bad "result file not found: $f"
  [ "$(jq -s '[.[] | select(.type=="meta")] | length' "$f")" = 1 ] || bad "$f: expected exactly one meta line"
  got="$(jq -s -c '[.[] | select(.type=="result") | .id] | sort' "$f")"
  [ "$got" = "$TASK_IDS" ] || bad "$f: result ids do not match tasks.json"
  jq -c --argjson run "$n" 'select(.type=="result") | {run:$run, id:.id, cat:.cat, success:(.success==true), reason:(.reason // null)}' "$f" >>"$ROWS"
done
DONE_RUNS="${#FILES[@]}"
META="$(jq -c 'select(.type=="meta") | {os, bin, measured_at, page_js_engine, page_modules_executed}' "${FILES[0]}")"

render() {
  jq -r -s --argjson added "$ADDED_JSON" --argjson total "$TOTAL" --argjson recorded "$DONE_RUNS" --argjson meta "$META" '
    def pct(s; t): if t == 0 then 0 else ((1000 * s / t) | round) / 10 end;
    . as $rows
    | ($rows | map(select(.run == $rows[0].run) | .id) | map(select(. as $i | ($added | index($i)) | not))) as $in22
    | "### compat-practical 結果（" + ($meta.os // "unknown") + "・" + ($total | tostring) + " 回中 " + ($recorded | tostring) + " 回記録）\n",
      "- 実行: `run_core.sh --page-js " + ($meta.page_js_engine // "none") + (if $meta.page_modules_executed then " --page-modules" else "" end) + "`",
      "- バイナリ: `" + ($meta.bin // "unknown") + "`",
      "- 22 件 = 28 件から c4・d5・d7・d8・e3・e6 を除いたもの（PoC-1 の定義）。TASK-59 の判定は人間が行う（本表は成功数の集計のみ）\n",
      "| 回 | 28 件 成功 | 28 件 動作率 | 22 件 成功 | 22 件 動作率 |",
      "| -- | ---------- | ------------ | ---------- | ------------ |",
      ( [range(1; $total + 1)] | map(. as $r
          | ($rows | map(select(.run == $r))) as $x
          | if ($x | length) == 0 then "| \($r) | 欠測 | 欠測 | 欠測 | 欠測 |" else
            ($x | map(select(.success)) | length) as $s28
          | ($x | map(select(.success and (.id as $i | $in22 | index($i)))) | length) as $s22
          | "| \($r) | \($s28)/\($x | length) | \(pct($s28; $x | length))% | \($s22)/\($in22 | length) | \(pct($s22; $in22 | length))% |" end) | .[] ),
      "",
      "### タスク別（1 回でも失敗したもの）\n",
      "| id | cat | 22 件に含む | 成功回数 | 失敗理由 |",
      "| -- | --- | ----------- | -------- | -------- |",
      ( $rows | group_by(.id) | map(
          {id: .[0].id, cat: .[0].cat, ok: (map(select(.success)) | length),
           reasons: (map(select(.success | not) | .reason // "none") | unique | join(", "))})
        | map(select(.ok < $recorded)) | sort_by(.id)
        | map(. as $t | "| \($t.id) | \($t.cat) | \(if ($in22 | index($t.id)) then "はい" else "いいえ" end) | \($t.ok)/\($recorded) | \($t.reasons) |") | .[] )
  ' "$ROWS"
}

if [ -n "$OUT_MD" ]; then
  mkdir -p "$(dirname "$OUT_MD")" || die "cannot create output directory"
  render >"$OUT_MD"
  cat "$OUT_MD"
else
  render
fi

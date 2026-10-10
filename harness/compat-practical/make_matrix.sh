#!/usr/bin/env bash
#
# fandhe-browser の実測（run_core.sh の core_results.jsonl）と PoC-9 の Chromium 実測
# （reference/chromium_results.json）を id で突合し、動作率マトリクス matrix.json を生成する
# （TASK-71.3・MEAS-4。関連 COMPAT-4・COMPAT-1・REPAIR-8）。
#
# 出力 [{id, cat, fandhe_browser_core, chromium}] は harness/compat-regression/check-matrix.sh の
# スキーマ契約（PoC-9 の matrix.json を踏襲）に従い、そのまま閾値判定へ渡せる。本スクリプトは
# 判定（ゲート）をしない: 動作率が低くても exit 0 で、成否の判定は check-matrix.sh の役割。
# 実測値は入力のまま写し、補正・推測・書き換えをしない（REPAIR-3: 達成を装わない）。
#
# 呼び出し元: 手動実行（再計測後の再生成）、self-test.sh（合成入力と実入力）、Makefile の
# check-compat-practical と CI の harness-* ジョブの [compat-regression] ステップ（--validate-only で入力の整合のみ検査）。
#
# 使い方: make_matrix.sh [--tasks P] [--core P] [--chromium P] [--out P] [--validate-only]
# 終了コード: 0=生成（--validate-only では検証）完了  1=書き込み失敗  2=入力・使用エラー
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"

TASKS="$SCRIPT_DIR/tasks.json"
CORE="$SCRIPT_DIR/results/core_results.jsonl"
CHROMIUM="$SCRIPT_DIR/reference/chromium_results.json"
OUT="$SCRIPT_DIR/results/matrix.json"
VALIDATE_ONLY=0
MAX_BYTES=1048576
TMP=""
trap '[ -z "$TMP" ] || rm -f "$TMP"' EXIT

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --tasks) [ $# -ge 2 ] || die "--tasks requires a value"; TASKS="$2"; shift 2 ;;
    --core) [ $# -ge 2 ] || die "--core requires a value"; CORE="$2"; shift 2 ;;
    --chromium) [ $# -ge 2 ] || die "--chromium requires a value"; CHROMIUM="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out requires a value"; OUT="$2"; shift 2 ;;
    --validate-only) VALIDATE_ONLY=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done

# lib.sh が jq 関数を定義するため `command -v jq` は常に成功する。バイナリ自体を `type -P` で探す
type -P jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"

# 入力ファイルの存在とサイズ上限（P0: 無制限読み込みの防止）
check_file() {
  local label="$1" path="$2" size
  [ -f "$path" ] || die "$label file not found: $path"
  size=$(wc -c <"$path" | tr -d ' ')
  [ "$size" -le "$MAX_BYTES" ] || die "$label file too large: $size bytes"
}
check_file tasks "$TASKS"
check_file core "$CORE"
check_file chromium "$CHROMIUM"

# tasks.json の規則は access_check.sh に一本化する（重複実装しない）
if ! tasks_msg=$(bash "$SCRIPT_DIR/access_check.sh" --validate-only --tasks "$TASKS" 2>&1); then
  die "tasks file is invalid: $tasks_msg"
fi

# Chromium 参照データは単一の JSON 文書でなければならない（複数文書は先頭以外が検証されない）
set +e
chromium_docs=$(jq -n '[inputs] | length' "$CHROMIUM" 2>/dev/null)
chromium_status=$?
set -e
[ "$chromium_status" -eq 0 ] || die "chromium file is not valid JSON"
[ "$chromium_docs" = "1" ] || die "chromium file must contain exactly one JSON document (found $chromium_docs)"
# core JSONL は各行が JSON であること（不正行は jq が非 0 で終わる）
set +e
core_lines=$(jq -n '[inputs] | length' "$CORE" 2>/dev/null)
core_status=$?
set -e
[ "$core_status" -eq 0 ] || die "core file is not valid JSONL"

tasks_sha=$(sha256_of "$TASKS")

# 検証と突合を 1 本の jq で行う。出力は {errors, matrix, summary}。
# エラー文にはページ由来テキストを含めない。id/cat は正規表現に適合したものだけを表示する
# （GitHub Actions のワークフローコマンドとして解釈される文字を出さない）。
read -r -d '' FILTER <<'JQ' || true
def idre: "\\A[A-Za-z0-9][A-Za-z0-9._-]{0,63}\\z";
def obj: type == "object";
def ok_id: type == "string" and test(idre);
def typ: if obj then (.type // null) else null end;
def safe(l): [l[] | select(ok_id)] | unique | join(",");

($tasks[0]) as $t
| ($core) as $lines
| ($chrom[0]) as $c
| ([$lines[] | select(typ == "meta")]) as $meta
| ([$lines[] | select(typ == "result")]) as $res
| ([$lines[] | select(typ != "meta" and typ != "result")] | length) as $other
| (if ($c | type) == "array" then $c else [] end) as $carr
| ([$t[].id]) as $tids
| ([$res[] | .id]) as $rids
| ([$carr[] | if obj then .id else null end]) as $cids
| (
    [ (if ($meta | length) != 1 then "core file must have exactly one meta line (found \($meta | length))" else empty end),
      (if ($meta | length) == 1 and ($meta[0].schema_version != 1) then "core meta schema_version must be 1" else empty end),
      (if ($meta | length) == 1 and ($meta[0].tasks_sha256 != $sha) then "core meta tasks_sha256 does not match tasks file (results were measured against a different task set)" else empty end),
      (if $other > 0 then "core file has \($other) line(s) that are neither meta nor result" else empty end),
      (if ($res | any(.id | ok_id | not)) then "core result has an invalid id" else empty end),
      (if ($rids | length) != ($rids | unique | length) then "core results have duplicate ids" else empty end),
      (if ($res | any(.success | type != "boolean")) then "core result success must be boolean" else empty end),
      (if ($c | type) != "array" then "chromium file must be a JSON array" else empty end),
      (if ($carr | any(obj | not)) then "chromium entry must be an object" else empty end),
      (if ($cids | any(ok_id | not)) then "chromium entry has an invalid id" else empty end),
      (if ($carr | any(obj and (.cat | ok_id | not))) then "chromium entry has an invalid cat" else empty end),
      (if ($cids | length) != ($cids | unique | length) then "chromium entries have duplicate ids" else empty end),
      (if ($carr | any(obj and (.success | type != "boolean"))) then "chromium entry success must be boolean" else empty end)
    ]
  ) as $e1
| (if ($e1 | length) > 0 then $e1 else
    ( [ (($tids - $rids) as $d | if ($d | length) > 0 then "ids missing from core results: \(safe($d))" else empty end),
        (($rids - $tids) as $d | if ($d | length) > 0 then "ids in core results but not in tasks: \(safe($d))" else empty end),
        (($tids - $cids) as $d | if ($d | length) > 0 then "ids missing from chromium results: \(safe($d))" else empty end),
        (($cids - $tids) as $d | if ($d | length) > 0 then "ids in chromium results but not in tasks: \(safe($d))" else empty end),
        ([$t[] | . as $x | select(([$res[] | select(.id == $x.id)][0].cat) != $x.cat) | .id] as $d
          | if ($d | length) > 0 then "cat differs between tasks and core results: \(safe($d))" else empty end),
        ([$t[] | . as $x | select(([$carr[] | select(.id == $x.id)][0].cat) != $x.cat) | .id] as $d
          | if ($d | length) > 0 then "cat differs between tasks and chromium results: \(safe($d))" else empty end)
      ]
    )
  end) as $errors
| if ($errors | length) > 0 then {errors: $errors, matrix: [], summary: []}
  else
    ( [ $t[] | . as $x
        | { id: .id, cat: .cat,
            fandhe_browser_core: ([$res[] | select(.id == $x.id)][0].success),
            chromium: ([$carr[] | select(.id == $x.id)][0].success) } ] ) as $m
    | { errors: [], matrix: $m,
        summary: (
          [ "overall core=\([$m[] | select(.fandhe_browser_core)] | length)/\($m | length) chromium=\([$m[] | select(.chromium)] | length)/\($m | length)" ]
          + [ ($m | map(.cat) | unique[]) as $k
              | [$m[] | select(.cat == $k)] as $g
              | "\($k) core=\([$g[] | select(.fandhe_browser_core)] | length)/\($g | length) chromium=\([$g[] | select(.chromium)] | length)/\($g | length)" ] ) }
  end
JQ

# --slurpfile は文書ごとの配列を返す（$tasks[0]・$chrom[0] が各文書、$core が JSONL の各行）。
# 巨大な値を引数文字列へ載せない（ARG_MAX 回避）ためにも --argjson は使わない
result=$(jq -c --slurpfile tasks "$TASKS" --slurpfile core "$CORE" --slurpfile chrom "$CHROMIUM" \
  --arg sha "$tasks_sha" "$FILTER" <<<'null') || die "failed to reconcile inputs"

errors=$(jq -r '.errors[]' <<<"$result")
if [ -n "$errors" ]; then
  while IFS= read -r line; do
    echo "error: $line" >&2
  done <<<"$errors"
  exit 2
fi

jq -r '.summary[]' <<<"$result"

if [ "$VALIDATE_ONLY" -eq 1 ]; then
  echo "ok: inputs validated (no file written)"
  exit 0
fi

OUT_DIR="$(dirname "$OUT")"
[ -d "$OUT_DIR" ] || { echo "error: output directory not found: $OUT_DIR" >&2; exit 1; }
# 出力先と同じディレクトリへ一時ファイルを作ってから置換する（途中失敗で既存の実測を壊さない）
TMP="$(mktemp "$OUT_DIR/.matrix.XXXXXX")" || { echo "error: cannot create temporary file in $OUT_DIR" >&2; exit 1; }
# 1 要素 1 行・末尾改行あり・LF 固定。各段の失敗を個別に検査し、全て成功した場合だけ置換する
# （コマンドグループを || の左辺にすると set -e が効かず、途中失敗が echo "]" に隠れるため）
rows=$(jq -c '.matrix[]' <<<"$result") || { echo "error: failed to extract matrix rows" >&2; exit 1; }
body=$(awk 'NR > 1 { print prev "," } { prev = "  " $0 } END { print prev }' <<<"$rows") \
  || { echo "error: failed to format matrix" >&2; exit 1; }
printf '[\n%s\n]\n' "$body" >"$TMP" || { echo "error: failed to write matrix" >&2; exit 1; }
mv -f "$TMP" "$OUT" || { echo "error: failed to replace $OUT" >&2; exit 1; }
TMP=""
echo "ok: wrote $(jq '.matrix | length' <<<"$result") entries"

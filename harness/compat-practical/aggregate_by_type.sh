#!/usr/bin/env bash
#
# 動作率マトリクス matrix.json を類型 (a)〜(e) 別に集計し、成功数・全体数・動作率を出力する
# （TASK-72.1・COMPAT-1。関連 COMPAT-4・MEAS-4・REPAIR-3）。
#
# 入力は make_matrix.sh が生成する [{id, cat, fandhe_browser_core, chromium}]（check-matrix.sh と同じ
# スキーマ契約）。目標類型は (a)(b)(d)＝static・spa・form、(c)(e)＝lazy・table は参考値として出す。
# 本スクリプトは集計器であり判定（70% 達成の合否・PASS/FAIL）をしない。動作率が低くても exit 0 で、
# 判定はゲートの check-matrix.sh と、考察を書く TASK-72.2（type-report.md）の役割。
# 実測値は入力のまま集計し、補正・推測をしない（REPAIR-3: 達成を装わない）。
#
# 呼び出し元: 手動実行、self-test.sh（合成入力と実入力）、TASK-72.2 のレポート作成。
# 動作率は整数演算で小数 1 桁（四捨五入）の文字列にする（jq の版・OS による浮動小数点表示差を避ける）。
#
# 使い方: aggregate_by_type.sh [--matrix P] [--key NAME] [--out P]
# 終了コード: 0=集計完了  1=--out の書き込み失敗  2=入力・使用エラー
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SCRIPT_DIR/lib.sh"

MATRIX="$SCRIPT_DIR/results/matrix.json"
KEY="fandhe_browser_core"
OUT=""
MAX_BYTES=1048576
MAX_ENTRIES=10000
TMP=""
trap '[ -z "$TMP" ] || rm -f "$TMP"' EXIT

die() {
  echo "error: $*" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --matrix) [ $# -ge 2 ] || die "--matrix requires a value"; MATRIX="$2"; shift 2 ;;
    --key) [ $# -ge 2 ] || die "--key requires a value"; KEY="$2"; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out requires a value"; OUT="$2"; shift 2 ;;
    *) die "unknown argument: $1" ;;
  esac
done

# lib.sh が jq 関数を定義するため `command -v jq` は常に成功する。バイナリ自体を `type -P` で探す
type -P jq >/dev/null 2>&1 || die "jq is required but was not found on PATH"

# --key は jq へ --arg で渡す（フィルタへ連結しない）が、ログ出力のため文字種も制限する
[[ "$KEY" =~ ^[A-Za-z0-9_]+$ ]] || die "--key must match ^[A-Za-z0-9_]+\$"

[ -f "$MATRIX" ] || die "matrix file not found"
size=$(wc -c <"$MATRIX" | tr -d ' ')
[ "$size" -le "$MAX_BYTES" ] || die "matrix file too large: $size bytes"

# 単一の JSON 文書でなければならない（複数文書は先頭以外が検証されない）
set +e
docs=$(jq -n '[inputs] | length' "$MATRIX" 2>/dev/null)
docs_status=$?
set -e
[ "$docs_status" -eq 0 ] || die "matrix file is not valid JSON"
[ "$docs" = "1" ] || die "matrix file must contain exactly one JSON document (found $docs)"

matrix_sha=$(sha256_of "$MATRIX")

# 検証と集計を 1 本の jq で行う。出力は {errors, types, overall}。
# エラー文にはページ由来テキストを含めない（GitHub Actions のワークフローコマンドとして
# 解釈される文字を CI ログへ出さない）。id / cat は \A..\z の正規表現で文字種を制限する。
read -r -d '' FILTER <<'JQ' || true
def idre: "\\A[A-Za-z0-9][A-Za-z0-9._-]{0,63}\\z";
def ok_id: type == "string" and test(idre);
def obj: type == "object";
# 類型対応（README「aggregate_by_type.sh」節と同一。PoC-9 の a〜e）
def tmap: [
  {type: "a", cat: "static", role: "target"},
  {type: "b", cat: "spa",    role: "target"},
  {type: "d", cat: "form",   role: "target"},
  {type: "c", cat: "lazy",   role: "reference"},
  {type: "e", cat: "table",  role: "reference"}
];
def rate(p; t):
  if t == 0 then null
  else ((p * 2000 + t) / (2 * t) | floor) as $x | "\($x / 10 | floor).\($x % 10)" end;
def core(g): {passed: ([g[] | select(.[$key] == true)] | length), total: (g | length)}
  | .rate_percent = rate(.passed; .total);
def chrom(g): [g[] | select(has("chromium"))] as $c
  | if ($c | length) == 0 then null
    else {passed: ([$c[] | select(.chromium == true)] | length), total: ($c | length)}
      | .rate_percent = rate(.passed; .total) end;

. as $m
| (if ($m | type) != "array" then ["matrix must be a JSON array"]
   elif ($m | length) < 1 then ["matrix must not be empty"]
   elif ($m | length) > $max then ["matrix has too many entries"]
   elif ($m | any(obj | not)) then ["matrix entry must be an object"]
   else
     [ (if ($m | any(.id | ok_id | not)) then "matrix entry has an invalid id" else empty end),
       (if ($m | map(.id) | length) != ($m | map(.id) | unique | length) then "matrix has duplicate ids" else empty end),
       (if ($m | any(.cat | ok_id | not)) then "matrix entry has an invalid cat" else empty end),
       (if ($m | any(.[$key] | type != "boolean")) then "matrix entry field \($key) must be boolean" else empty end),
       (if ($m | any(has("chromium") and (.chromium | type != "boolean"))) then "matrix entry chromium must be boolean" else empty end)
     ]
     + (if ($m | any(.cat | ok_id | not)) then [] else
         [ tmap[] | select(.role == "target") | .cat as $c
           | select([$m[] | select(.cat == $c)] | length == 0)
           | "no entries for target category \($c)" ] end)
   end) as $errors
| if ($errors | length) > 0 then {errors: $errors, types: [], overall: null}
  else
    ( [ tmap[] | . as $t
        | [$m[] | select(.cat == $t.cat)] as $g
        | $t + {core: core($g), chromium: chrom($g)} ]
      + [ ($m | map(.cat) | unique[]) as $k
          | select(tmap | map(.cat) | index($k) | not)
          | [$m[] | select(.cat == $k)] as $g
          | {type: null, cat: $k, role: "unmapped", core: core($g), chromium: chrom($g)} ]
    ) as $types
    | {errors: [], types: $types, overall: {core: core($m), chromium: chrom($m)}}
  end
JQ

result=$(jq -c --arg key "$KEY" --argjson max "$MAX_ENTRIES" "$FILTER" "$MATRIX") \
  || die "failed to aggregate matrix"

errors=$(jq -r '.errors[]' <<<"$result")
if [ -n "$errors" ]; then
  while IFS= read -r line; do
    echo "error: $line" >&2
  done <<<"$errors"
  exit 2
fi

# 要約（stdout・1 行 1 区分）。並びは target の a・b・d → reference の c・e → unmapped → overall
jq -r '
  def rr: if . == null then "n/a"
    else "\(.passed)/\(.total) rate=\(if .rate_percent == null then "n/a" else .rate_percent + "%" end)" end;
  (.types[] | "type=\(.type // "-") cat=\(.cat) role=\(.role) core=\(.core | rr) chromium=\(.chromium | rr)"),
  (.overall | "overall core=\(.core | rr) chromium=\(.chromium | rr)")
' <<<"$result"

[ -n "$OUT" ] || exit 0

OUT_DIR="$(dirname "$OUT")"
[ -d "$OUT_DIR" ] || { echo "error: output directory not found" >&2; exit 1; }
# 出力先と同じディレクトリへ一時ファイルを作ってから置換する（途中失敗で既存ファイルを壊さない）
TMP="$(mktemp "$OUT_DIR/.aggregate.XXXXXX")" || { echo "error: cannot create temporary file" >&2; exit 1; }
doc=$(jq --arg key "$KEY" --arg sha "$matrix_sha" \
  '{schema_version: 1, key: $key, matrix_sha256: $sha, types: .types, overall: .overall}' <<<"$result") \
  || { echo "error: failed to format output" >&2; exit 1; }
printf '%s\n' "$doc" >"$TMP" || { echo "error: failed to write output" >&2; exit 1; }
mv -f "$TMP" "$OUT" || { echo "error: failed to replace output" >&2; exit 1; }
TMP=""
echo "ok: wrote aggregate"

#!/usr/bin/env bash
#
# run_core.sh を指定回数だけ繰り返し、各回の結果 JSONL と集計 Markdown を出力ディレクトリへ置く
# （TASK-59・`MEAS-4`・`COMPAT-4`。xos-measure.yml の compat-practical の入口。集計は summarize_repeats.sh）。
# 繰り返す目的は、実サイトの応答揺れ・runner のネットワーク差による成否のぶれを見ること（TASK-59 の判定は人間）。
#
# 使い方: repeat_core.sh --repeat N --bin P --out-dir DIR [run_core.sh のオプション...]
#   --repeat    1〜5（既定なし）。run_core.sh の全体期限（既定 1200 秒）× 回数がジョブ時間の上限を決める
#   --bin       対象バイナリ（run_core.sh の --bin へそのまま渡す）
#   --out-dir   出力先。core_results_<n>.jsonl（各回）と summary.md（集計）を作る。前回の成果物を今回のものと
#               取り違えないよう、これらが既にある出力先は拒否する（使用エラー。新しい出力先か空のディレクトリを指定する）
#   以降の引数は run_core.sh へそのまま渡す（例: --page-js v8 --page-modules、Windows は --windows-localappdata-profile）。
#   ただし --tasks は拒否する（集計 summarize_repeats.sh が 28 件・22 件の定義に固定のため。使用エラー）
# 終了コード: 0=全回が記録完了、1=いずれかの回が失敗（完了した回だけで集計は出す）、2=使用エラー
# 環境変数 FC_REPEAT_RUN_CORE は自己テスト用（run_core.sh の代わりに呼ぶスクリプト。実測では設定しない）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN_CORE="${FC_REPEAT_RUN_CORE:-$SCRIPT_DIR/run_core.sh}"

die() {
  echo "error: $*" >&2
  exit 2
}

REPEAT=""
BIN=""
OUT_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --repeat) [ $# -ge 2 ] || die "--repeat requires a value"; REPEAT="$2"; shift 2 ;;
    --bin) [ $# -ge 2 ] || die "--bin requires a value"; BIN="$2"; shift 2 ;;
    --out-dir) [ $# -ge 2 ] || die "--out-dir requires a value"; OUT_DIR="$2"; shift 2 ;;
    *) break ;;
  esac
done
[[ "$REPEAT" =~ ^[1-5]$ ]] || die "--repeat must be an integer in 1..5"
[ -n "$BIN" ] || die "--bin is required"
[ -n "$OUT_DIR" ] || die "--out-dir is required"
# 出力名は固定文字列 + 回数だけで組み立てる（引数を連結しない）。--out は run_core.sh へ渡す引数に含めさせない
for a in "$@"; do
  case "$a" in
    --out | --bin) die "$a is managed by repeat_core.sh" ;;
    --tasks | --tasks=*) die "--tasks is not supported (repeat_core.sh is fixed to the 28/22-task definition of tasks.json)" ;;
  esac
done
# 同じ出力先の再利用で、今回失敗した回の位置に前回の成果物が残ると今回のものと誤認するため、管理対象があれば拒否する
if [ -d "$OUT_DIR" ]; then
  for stale in "$OUT_DIR"/core_results_*.jsonl "$OUT_DIR"/summary.md; do
    { [ ! -e "$stale" ] && [ ! -L "$stale" ]; } || die "output directory already has results: $stale (use a new --out-dir)"
  done
elif [ -e "$OUT_DIR" ]; then
  die "--out-dir exists and is not a directory: $OUT_DIR"
fi
mkdir -p "$OUT_DIR" || die "cannot create output directory"

FILES=()
FAILED=0
for ((i = 1; i <= REPEAT; i++)); do
  out="$OUT_DIR/core_results_$i.jsonl"
  echo "== run $i/$REPEAT =="
  if bash "$RUN_CORE" --bin "$BIN" --out "$out" "$@"; then
    # 失敗した回があっても回番号がずれないよう、元の実行番号を明示して渡す
    FILES+=(--run "$i" "$out")
  else
    echo "error: run $i failed (exit $?)" >&2
    FAILED=1
  fi
done

if [ "${#FILES[@]}" -gt 0 ]; then
  bash "$SCRIPT_DIR/summarize_repeats.sh" --out-md "$OUT_DIR/summary.md" --total "$REPEAT" "${FILES[@]}" || FAILED=1
else
  echo "error: no run was recorded" >&2
  FAILED=1
fi
exit "$FAILED"

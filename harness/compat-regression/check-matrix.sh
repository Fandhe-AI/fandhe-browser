#!/usr/bin/env bash
#
# 対象サイト群の動作率マトリクス（JSON）を読み込み、全体および指定した類型別の
# 動作率が閾値を下回っていれば非ゼロ終了する回帰チェッカー（TASK-9.2・REPAIR-8）。
# 閾値の根拠は COMPAT-4（全体動作率 70% 以上）・COMPAT-1（静的・SPA・フォーム等の
# 類型別動作率 70% 以上）。呼び出し元は .github/workflows/ci.yml の
# compat-regression ジョブと Makefile の check-compat-regression ターゲット。
# スキーマ契約・終了コードの詳細は同じディレクトリの README.md を参照。
#
# 依存は bash + jq のみ（新規 Cargo 依存・新規サードパーティ action を避けるため。
# .claude/rules/dependency-policy.md）。3 OS（Linux/macOS/Windows）の git-bash 上で
# 同一に動くことを前提にする。
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: check-matrix.sh --matrix <path> [--threshold <0-100>] [--key <name>] [--categories <csv>] [--all-categories] [--allow-missing]

  --matrix <path>       Path to the compat matrix JSON file (required).
  --threshold <0-100>   Minimum pass rate percentage, integer 0-100 (default: 70).
  --key <name>          Boolean field name to evaluate per entry (default: fandhe_browser_core).
  --categories <csv>    Comma-separated list of "cat" values that must each also meet the threshold
                         and must each have at least 1 matrix entry (use to require specific
                         categories to be present).
  --all-categories      Additionally judge every distinct "cat" value found in the matrix, not just
                         the ones listed in --categories. Catches categories the schema allows
                         (any string) but that --categories does not yet enumerate.
  --allow-missing       Exit 0 with a warning instead of failing when the matrix file does not exist.
EOF
}

MATRIX=""
THRESHOLD=70
KEY="fandhe_browser_core"
CATEGORIES=""
ALL_CATEGORIES=0
ALLOW_MISSING=0

while [ $# -gt 0 ]; do
  case "$1" in
    --matrix)
      [ $# -ge 2 ] || { echo "error: --matrix requires a value" >&2; usage; exit 2; }
      MATRIX="$2"
      shift 2
      ;;
    --threshold)
      [ $# -ge 2 ] || { echo "error: --threshold requires a value" >&2; usage; exit 2; }
      THRESHOLD="$2"
      shift 2
      ;;
    --key)
      [ $# -ge 2 ] || { echo "error: --key requires a value" >&2; usage; exit 2; }
      KEY="$2"
      shift 2
      ;;
    --categories)
      [ $# -ge 2 ] || { echo "error: --categories requires a value" >&2; usage; exit 2; }
      CATEGORIES="$2"
      shift 2
      ;;
    --all-categories)
      ALL_CATEGORIES=1
      shift
      ;;
    --allow-missing)
      ALLOW_MISSING=1
      shift
      ;;
    *)
      echo "error: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [ -z "$MATRIX" ]; then
  echo "error: --matrix is required" >&2
  usage
  exit 2
fi
# 先頭ゼロ（例: 070）を許すと後段の $((THRESHOLD * total)) が bash の 8 進数
# リテラルとして解釈され、期待と異なる値になる（070 は 10 進 56）。0 単体・
# 先頭が 1-9 の 1〜3 桁のみ許可し、先頭ゼロ付き複数桁を弾く。
if ! [[ "$THRESHOLD" =~ ^(0|[1-9][0-9]{0,2})$ ]] || [ "$THRESHOLD" -gt 100 ]; then
  echo "error: --threshold must be an integer between 0 and 100 with no leading zero (got: $THRESHOLD)" >&2
  exit 2
fi
if ! [[ "$KEY" =~ ^[A-Za-z0-9_]+$ ]]; then
  echo "error: --key must match ^[A-Za-z0-9_]+\$ (got: $KEY)" >&2
  exit 2
fi
if [ -n "$CATEGORIES" ] && ! [[ "$CATEGORIES" =~ ^[A-Za-z0-9_]+(,[A-Za-z0-9_]+)*$ ]]; then
  echo "error: --categories must match ^[A-Za-z0-9_]+(,[A-Za-z0-9_]+)*\$ (got: $CATEGORIES)" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required but was not found on PATH" >&2
  exit 2
fi

if [ ! -f "$MATRIX" ]; then
  if [ "$ALLOW_MISSING" -eq 1 ]; then
    # 実マトリクス（harness/compat-practical/results/matrix.json）は TASK-71.3（#312）
    # が生成予定でまだ存在しない。#312 がマトリクスをコミットしたらこのフラグを
    # ci.yml・Makefile から削除し、fail-closed（無フラグ時と同じ exit 2）へ戻す。
    echo "::warning::compat matrix not found at $MATRIX; regression gate inactive until TASK-71.3 (#312) lands"
    exit 0
  fi
  echo "error: matrix file not found at $MATRIX" >&2
  exit 2
fi

SIZE=$(wc -c <"$MATRIX" | tr -d '[:space:]')
if [ "$SIZE" -gt 1048576 ]; then
  echo "error: matrix file exceeds the 1 MiB size limit ($SIZE bytes)" >&2
  exit 2
fi

# jq はデフォルトで 1 ファイル内の複数のトップレベル JSON 値を順に処理できて
# しまう（JSON Text Sequences 的な挙動）。README.md のスキーマ契約は「トップ
# レベルは単一の JSON 配列」であり、以降のスキーマ検証・集計はすべて最初の
# トップレベル値だけを見る箇所がある（例: 187 行目以降の `read` は jq 出力の
# 先頭行だけを受け取る）ため、2 つ目以降の配列に不合格な内容を混入させても
# 検出されずに合格し得る（TASK-9.2 レビュー指摘。codex review, PR #452）。
# `jq -s`（slurp）でトップレベル値の個数を数え、1 個以外は拒否する。
# jq は Windows ネイティブ実行時に CRLF を出力しうるため、コマンド置換結果の
# 末尾に \r が残ると正当な単一ドキュメント（"1"）が "1\r" となり文字列比較で
# 不一致になる（windows-latest で compat-regression が誤って exit 2 になる。
# Cursor Bugbot 指摘。PR #452）。`set -o pipefail` 済みのため、jq の非ゼロ
# 終了は tr を挟んでもパイプライン全体の失敗として検出できる。
if ! DOC_COUNT=$(jq -s 'length' "$MATRIX" 2>&1 | tr -d '\r'); then
  echo "error: failed to parse matrix as JSON: $DOC_COUNT" >&2
  exit 2
fi
if [ "$DOC_COUNT" != "1" ]; then
  echo "error: matrix file must contain exactly one top-level JSON value (found $DOC_COUNT)" >&2
  exit 2
fi

# スキーマ検証。$key は --arg で jq へ渡し、フィルタ文字列へ連結しない
# （jq インジェクション対策。security.md「インジェクション」観点）。
# jq 自体が壊れた JSON に対して非ゼロ終了するため、その失敗もここで拾う。
if ! SCHEMA_ERR=$(jq -r --arg key "$KEY" '
    if (type != "array") then "top-level value must be a JSON array"
    elif (length < 1) then "matrix must contain at least 1 entry"
    elif (length > 10000) then "matrix must contain at most 10000 entries"
    elif ([.[] | select(type != "object")] | length) > 0 then
      "each entry must be a JSON object"
    elif ([.[] | select((.id? | type) != "string" or (.id | length) == 0)] | length) > 0 then
      "each entry requires a non-empty string id"
    elif (([.[].id] | length) != ([.[].id] | unique | length)) then
      "duplicate id detected in matrix"
    elif ([.[] | select((.cat? | type) != "string" or (.cat | length) == 0)] | length) > 0 then
      "each entry requires a non-empty string cat"
    elif ([.[] | select((.[$key]? | type) != "boolean")] | length) > 0 then
      ("each entry requires a boolean field: " + $key)
    elif ([.[] | select(has("chromium") and ((.chromium | type) != "boolean"))] | length) > 0 then
      "chromium field must be boolean when present"
    else
      empty
    end
  ' "$MATRIX" 2>&1); then
  echo "error: failed to parse matrix as JSON: $SCHEMA_ERR" >&2
  exit 2
fi
if [ -n "$SCHEMA_ERR" ]; then
  echo "error: $SCHEMA_ERR" >&2
  exit 2
fi

# --categories で列挙した類型は、集計前に「1 件も無い」を弾く（無言で 0/0 合格に
# なるのを防ぐ。fail-closed）。
if [ -n "$CATEGORIES" ]; then
  IFS=',' read -r -a CAT_LIST <<<"$CATEGORIES"
  for cat in "${CAT_LIST[@]}"; do
    ccount=$(jq -r --arg cat "$cat" '[.[] | select(.cat == $cat)] | length' "$MATRIX" | tr -d '\r')
    if [ "$ccount" -eq 0 ]; then
      echo "error: no matrix entries found for category: $cat" >&2
      exit 2
    fi
  done
fi

FAIL=0

judge() {
  # $1=label, $2=passed, $3=total
  local label="$1" passed="$2" total="$3" result
  if [ "$total" -eq 0 ]; then
    # --categories の事前検査で 0 件は既に弾いているため、ここに到達するのは
    # overall で total=0（スキーマ検証で長さ >= 1 を保証済みのため理論上到達しない）
    # の場合のみ。到達したら安全側で fail 扱いにする。
    result="FAIL"
  elif [ $((passed * 100)) -ge $((THRESHOLD * total)) ]; then
    result="PASS"
  else
    result="FAIL"
  fi
  echo "${label}: passed=${passed} total=${total} threshold=${THRESHOLD} result=${result}"
  if [ "$result" = "FAIL" ]; then
    echo "::error::${label} pass rate below threshold (passed=${passed} total=${total} threshold=${THRESHOLD})"
    FAIL=1
  fi
}

IFS=$'\t' read -r TOTAL PASSED < <(
  jq -r --arg key "$KEY" '
    [ length, ([.[] | select(.[$key] == true)] | length) ] | @tsv
  ' "$MATRIX" | tr -d '\r'
)
judge "overall" "$PASSED" "$TOTAL"

judge_category() {
  # $1=cat value（マトリクス内の "cat" 文字列そのまま）
  local cat="$1" ctotal cpassed
  IFS=$'\t' read -r ctotal cpassed < <(
    jq -r --arg cat "$cat" --arg key "$KEY" '
      [
        ([.[] | select(.cat == $cat)] | length),
        ([.[] | select(.cat == $cat and .[$key] == true)] | length)
      ] | @tsv
    ' "$MATRIX" | tr -d '\r'
  )
  judge "category ${cat}" "$cpassed" "$ctotal"
}

if [ -n "$CATEGORIES" ]; then
  for cat in "${CAT_LIST[@]}"; do
    judge_category "$cat"
  done
fi

# --all-categories: マトリクス内に実在する全ての "cat" 値を判定対象にする
# （COMPAT-1 は「静的・SPA・フォーム等の類型別」動作率を要求するが、スキーマ上
# "cat" は任意の文字列を許すため、--categories の固定 CSV だけでは呼び出し側が
# 列挙し忘れた類型〔例: lazy・table〕がマトリクスに含まれていても閾値未満のまま
# 検出されずに通過してしまう。TASK-9.2 レビュー指摘。--categories 側で既に
# 判定済みの値は重複判定を避けるため除外する）。
if [ "$ALL_CATEGORIES" -eq 1 ]; then
  # README.md のスキーマは "cat" に任意の非空文字列（改行・CR を含む）を
  # 許容する。改行区切りで jq 出力を読み `tr -d '\r'` で CR を除去する方式では、
  # 改行や CR を含む有効な cat 値がその場で複数の別カテゴリへ分割されてしまい、
  # 元の値のまま judge_category に渡らない（同名の分割後カテゴリが存在すると、
  # 元のカテゴリが閾値未満でも見逃し得る。COMPAT-1 の類型別回帰検出に反する。
  # codex review 指摘, PR #452）。NUL 区切り（jq -j で改行を一切挿入させず、
  # 明示的に \u0000 のみを区切りとして付与）で読み取り、値の中身（改行・CR
  # 含む）を無加工のまま judge_category へ渡す。NUL は bash のコマンド置換
  # ($(...)) を経由すると保持できないため、プロセス置換で直接読む。
  while IFS= read -r -d '' cat; do
    already_judged=0
    if [ -n "$CATEGORIES" ]; then
      for done_cat in "${CAT_LIST[@]}"; do
        if [ "$done_cat" = "$cat" ]; then
          already_judged=1
          break
        fi
      done
    fi
    if [ "$already_judged" -eq 0 ]; then
      judge_category "$cat"
    fi
  done < <(jq -j '[.[].cat] | unique | .[] | . + "\u0000"' "$MATRIX")
fi

if [ "$FAIL" -eq 1 ]; then
  exit 1
fi
exit 0

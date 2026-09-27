#!/usr/bin/env bash
#
# record.sh の自己テスト（TASK-84.2・Issue #212）。合成 fixture
# （record-fixtures/*.json。実サイトの計測結果ではない。harness/
# compat-regression/README.md の「fixture は合成データ」と同じ方針）に対して
# `--input` 経由で record.sh を実行し、終了コードと追記内容を具体値で
# 検証する。実際に `cargo bench` を実行しないため CI で高速に回せる。
#
# 呼び出し元: .github/workflows/ci.yml の bench-record-selftest ジョブ、
# Makefile の check-bench-record ターゲット。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RECORD="$SCRIPT_DIR/record.sh"
FIXTURES="$SCRIPT_DIR/record-fixtures"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required but was not found on PATH" >&2
  exit 2
fi

FAILURES=0
CASES=0

# 全ての作業（history ファイル・不正 JSON・symlink 候補）は mktemp -d の
# 中だけに作り、trap で必ず削除する（一時ファイル配置規則）。
WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/competitor-lightpanda-self-test.XXXXXX")
cleanup() {
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

HISTORY="$WORKDIR/history.jsonl"

# $1=case name, $2=expected exit code, remaining=record.sh args
LAST_OUTPUT=""
expect_exit() {
  local name="$1" expected="$2"
  shift 2
  CASES=$((CASES + 1))
  local out status
  set +e
  out=$(bash "$RECORD" "$@" 2>&1)
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

line_count() {
  if [ -f "$HISTORY" ]; then
    wc -l <"$HISTORY" | tr -d '[:space:]'
  else
    echo 0
  fi
}

# --- (a) all-skipped: 対象バイナリ・基準値ともに未設定 ---
expect_exit "all-skipped appends and exits 0" 0 \
  --history "$HISTORY" --input "$FIXTURES/all-skipped.json" --source ci
if [ "$(line_count)" != "1" ]; then
  echo "FAIL [all-skipped line count]: expected 1 line, got $(line_count)" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))
LAST_LINE=$(tail -n 1 "$HISTORY")
# jq は Windows ネイティブ実行時に CRLF を出力しうる（harness/compat-regression/
# check-matrix.sh 冒頭コメント・PR #452 と同じ既知の挙動）。以降の全ての jq 抽出
# 結果は `tr -d '\r'` で末尾の \r を除去してから文字列比較する。
STATUS=$(printf '%s' "$LAST_LINE" | jq -r '.result."fandhe-browser".perf6.status' | tr -d '\r')
if [ "$STATUS" != "skipped" ]; then
  echo "FAIL [all-skipped perf6 status]: expected skipped, got $STATUS" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))
SOURCE_VALUE=$(printf '%s' "$LAST_LINE" | jq -r '.source' | tr -d '\r')
if [ "$SOURCE_VALUE" != "ci" ]; then
  echo "FAIL [all-skipped source]: expected ci, got $SOURCE_VALUE" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# --- (b) measured-met ---
expect_exit "measured-met appends and exits 0" 0 \
  --history "$HISTORY" --input "$FIXTURES/measured-met.json" --source local
LAST_LINE=$(tail -n 1 "$HISTORY")
VERDICT=$(printf '%s' "$LAST_LINE" | jq -r '.result."fandhe-browser".perf6.verdict' | tr -d '\r')
if [ "$VERDICT" != "met" ]; then
  echo "FAIL [measured-met verdict]: expected met, got $VERDICT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# --- (c) measured-below: below_target は計測結果の一種であり exit 0 のまま ---
expect_exit "measured-below appends and exits 0" 0 \
  --history "$HISTORY" --input "$FIXTURES/measured-below.json" --source local
LAST_LINE=$(tail -n 1 "$HISTORY")
VERDICT=$(printf '%s' "$LAST_LINE" | jq -r '.result."fandhe-browser".perf6.verdict' | tr -d '\r')
if [ "$VERDICT" != "below_target" ]; then
  echo "FAIL [measured-below verdict]: expected below_target, got $VERDICT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# --- (d) bench exit 1 の入力: 追記したうえで exit 1 を伝播する ---
expect_exit "bench exit 1 is appended and propagated" 1 \
  --history "$HISTORY" --input "$FIXTURES/error-exit.json" --bench-exit-code 1 --source ci
LAST_LINE=$(tail -n 1 "$HISTORY")
BENCH_EXIT=$(printf '%s' "$LAST_LINE" | jq -r '.benchExitCode' | tr -d '\r')
if [ "$BENCH_EXIT" != "1" ]; then
  echo "FAIL [bench exit 1 benchExitCode]: expected 1, got $BENCH_EXIT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))
if [ "$(line_count)" != "4" ]; then
  echo "FAIL [line count after 4 successful appends]: expected 4, got $(line_count)" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# --- (e) 不正 JSON: 追記されず exit 2 ---
INVALID_JSON="$WORKDIR/invalid.json"
printf '{not valid json' >"$INVALID_JSON"
BEFORE=$(line_count)
expect_exit "invalid JSON is rejected without appending" 2 \
  --history "$HISTORY" --input "$INVALID_JSON"
AFTER=$(line_count)
if [ "$AFTER" != "$BEFORE" ]; then
  echo "FAIL [invalid JSON line count]: expected unchanged ($BEFORE), got $AFTER" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# 配列（オブジェクトでない）も同様に拒否する。
ARRAY_JSON="$WORKDIR/array.json"
printf '[]' >"$ARRAY_JSON"
expect_exit "non-object JSON is rejected" 2 \
  --history "$HISTORY" --input "$ARRAY_JSON"

# --- (f) --history の拡張子違い・シンボリックリンクは使用エラー ---
expect_exit "history with wrong extension is rejected" 2 \
  --history "$WORKDIR/history.txt" --input "$FIXTURES/all-skipped.json"

if ln -s "$HISTORY" "$WORKDIR/history-link.jsonl" 2>/dev/null && [ -L "$WORKDIR/history-link.jsonl" ]; then
  expect_exit "symlinked history is rejected" 2 \
    --history "$WORKDIR/history-link.jsonl" --input "$FIXTURES/all-skipped.json"
else
  # Windows（MSYS）等、シンボリックリンクを作成できない環境では
  # このケースを実行できない（`ln -s` が実体コピーになりテストの前提が
  # 崩れるため）。CASES には数えず、証跡として理由をログへ残す。
  echo "skip [symlinked history is rejected]: could not create a symlink in this environment"
fi

# --- 入力ファイル不在 ---
expect_exit "missing --input file is rejected" 2 \
  --history "$HISTORY" --input "$WORKDIR/does-not-exist.json"

# --- 必須引数不足 ---
expect_exit "missing --history" 2 --input "$FIXTURES/all-skipped.json"
expect_exit "unknown argument" 2 --history "$HISTORY" --bogus

# --- (g) 2 回連続実行すると行数が増え、各行が schemaVersion==1 を通る ---
FINAL_COUNT="$(line_count)"
expect_exit "final append for schema check" 0 \
  --history "$HISTORY" --input "$FIXTURES/measured-met.json" --source local
NEW_COUNT="$(line_count)"
if [ "$NEW_COUNT" != "$((FINAL_COUNT + 1))" ]; then
  echo "FAIL [append increments line count]: expected $((FINAL_COUNT + 1)), got $NEW_COUNT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))
while IFS= read -r line; do
  # `jq -e` は述語が false のとき exit 1 になる。`set -e` 下でそのまま呼ぶと
  # スキーマ不一致（本来 FAIL として報告したいケース）でスクリプト自体が
  # 中断してしまうため、`set +e`/`set -e` で一時的に無効化してから判定する。
  set +e
  SCHEMA=$(printf '%s' "$line" | jq -e '.schemaVersion == 1' 2>&1 | tr -d '\r')
  set -e
  if [ "$SCHEMA" != "true" ]; then
    echo "FAIL [schemaVersion check]: line did not satisfy schemaVersion==1: $line" >&2
    FAILURES=$((FAILURES + 1))
  fi
  CASES=$((CASES + 1))
done <"$HISTORY"

echo "----"
echo "competitor_lightpanda record self-test: ${CASES} cases, $((CASES - FAILURES)) passed, ${FAILURES} failed"
if [ "$FAILURES" -ne 0 ]; then
  exit 1
fi
exit 0

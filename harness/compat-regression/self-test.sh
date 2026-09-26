#!/usr/bin/env bash
#
# check-matrix.sh の自己テスト（TASK-9.2・REPAIR-8）。合成 fixture（fixtures/*.json）
# に対してチェッカーを実行し、終了コードと集計値（passed=/total=）を具体値で厳密比較する。
# 呼び出し元は .github/workflows/ci.yml の compat-regression ジョブと Makefile の
# check-compat-regression ターゲットで、実マトリクス（harness/compat-practical/
# results/matrix.json。TASK-71.3・#312 で導入予定）の判定に先立って毎回実行し、
# 「閾値未満で fail する」ことをログに証跡として残す。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$SCRIPT_DIR/check-matrix.sh"
FIXTURES="$SCRIPT_DIR/fixtures"

FAILURES=0
CASES=0
LAST_OUTPUT=""

# 一時ファイル（不正 JSON fixture）は mktemp で作り、trap で必ず削除する
# （不正 JSON をリポジトリへ常設すると markdownlint 等の対象外設定が別途必要になるため。
# 一時ファイル配置規則: スクリプト自身の cwd ではなく mktemp の絶対パスを使う）。
TMP_INVALID_JSON=""
cleanup() {
  if [ -n "$TMP_INVALID_JSON" ] && [ -f "$TMP_INVALID_JSON" ]; then
    rm -f "$TMP_INVALID_JSON"
  fi
}
trap cleanup EXIT

# $1=case name, $2=expected exit code, remaining=checker args
expect_exit() {
  local name="$1" expected="$2"
  shift 2
  CASES=$((CASES + 1))
  local out status
  set +e
  out=$(bash "$CHECKER" "$@" 2>&1)
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
  # 呼び出し元がアサーション文字列を追加検証できるよう出力を返す
  LAST_OUTPUT="$out"
}

# $1=haystack $2=needle $3=case name
expect_contains() {
  local haystack="$1" needle="$2" name="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    echo "FAIL [$name]: expected output to contain '$needle'" >&2
    echo "  output: $haystack" >&2
    FAILURES=$((FAILURES + 1))
  fi
}

# --- 合格系（具体値の passed=/total= まで比較。coding-rust.md「期待値は具体値」） ---

expect_exit "pass overall" 0 --matrix "$FIXTURES/pass.json"
expect_contains "$LAST_OUTPUT" "overall: passed=10 total=12 threshold=70 result=PASS" "pass overall counts"

expect_exit "pass categories" 0 --matrix "$FIXTURES/pass.json" --categories static,spa,form
expect_contains "$LAST_OUTPUT" "category static: passed=4 total=5 threshold=70 result=PASS" "pass static counts"
expect_contains "$LAST_OUTPUT" "category spa: passed=3 total=4 threshold=70 result=PASS" "pass spa counts"
expect_contains "$LAST_OUTPUT" "category form: passed=3 total=3 threshold=70 result=PASS" "pass form counts"

expect_exit "boundary 70 exact" 0 --matrix "$FIXTURES/boundary-70.json"
expect_contains "$LAST_OUTPUT" "overall: passed=7 total=10 threshold=70 result=PASS" "boundary counts"

expect_exit "boundary 70 with categories" 0 --matrix "$FIXTURES/boundary-70.json" --categories static
expect_contains "$LAST_OUTPUT" "category static: passed=7 total=10 threshold=70 result=PASS" "boundary category counts"

# --- 不合格系 ---

expect_exit "below overall" 1 --matrix "$FIXTURES/below-overall.json"
expect_contains "$LAST_OUTPUT" "overall: passed=6 total=10 threshold=70 result=FAIL" "below-overall counts"

expect_exit "below category without --categories passes" 0 --matrix "$FIXTURES/below-category.json"
expect_contains "$LAST_OUTPUT" "overall: passed=7 total=10 threshold=70 result=PASS" "below-category overall-only counts"

expect_exit "below category with --categories fails" 1 --matrix "$FIXTURES/below-category.json" --categories static,spa,form
expect_contains "$LAST_OUTPUT" "category form: passed=1 total=4 threshold=70 result=FAIL" "below-category form counts"

# --all-categories: --categories を指定しなくても、マトリクス内に実在する全ての
# cat 値（このフィクスチャでは static/spa/form）を判定対象にし、閾値未満の類型を
# 検出できることを確認する（TASK-9.2 レビュー指摘。COMPAT-1 の類型別回帰検出漏れ対策）。
expect_exit "below category with --all-categories fails without --categories" 1 --matrix "$FIXTURES/below-category.json" --all-categories
expect_contains "$LAST_OUTPUT" "category form: passed=1 total=4 threshold=70 result=FAIL" "all-categories form counts"

# --categories と --all-categories を併用した場合、--categories 側で既に判定した
# 類型を --all-categories 側で重複して二重出力しないことを確認する（同一 cat 値の
# judge 呼び出しが 1 回だけであることを出現回数で検証）。
expect_exit "categories and all-categories combined dedupe" 1 --matrix "$FIXTURES/below-category.json" --categories form --all-categories
FORM_LINE_COUNT=$(grep -c "^category form: " <<<"$LAST_OUTPUT")
if [ "$FORM_LINE_COUNT" -ne 1 ]; then
  echo "FAIL [categories/all-categories dedupe]: expected exactly 1 'category form:' line, got $FORM_LINE_COUNT" >&2
  echo "  output: $LAST_OUTPUT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))
expect_contains "$LAST_OUTPUT" "category static: passed=3 total=3 threshold=70 result=PASS" "all-categories discovers static too"

# cat 値に改行を含む場合、--all-categories のカテゴリ発見が改行区切りで壊れて
# 別カテゴリへ分割されないことを確認する（codex review 指摘, PR #452。改行を
# 含む cat 値「weird\ncase」の 4 件（passed=1）が 1 つのカテゴリとして集計され、
# 分割後の "weird"（0 件）"case"（0 件）へ分かれて total=0 のまま見逃されないこと
# を、集計結果の具体値まで比較して検証する）。
expect_exit "cat value containing a newline is not split during --all-categories discovery" 1 \
  --matrix "$FIXTURES/newline-cat.json" --all-categories
expect_contains "$LAST_OUTPUT" $'category weird\ncase: passed=1 total=4 threshold=70 result=FAIL' \
  "newline-containing cat value judged as a single category with correct counts"
if grep -qE "^category (weird|case): passed=0 total=0" <<<"$LAST_OUTPUT"; then
  echo "FAIL [newline cat not split]: cat value was split into separate zero-count categories" >&2
  echo "  output: $LAST_OUTPUT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# cat 値が末尾に改行を含む場合（例: "spa\n"）、コマンド置換 `$(...)` が出力の
# 末尾改行を剥ぎ取ってしまうと、末尾改行の無い別カテゴリ（"spa"）と誤って
# 同一視され、両者が誤結合されて集計されてしまう（codex review 指摘
# discussion_r4111644746, Windows self-test 失敗, PR #452）。上のケース
# （embedded newline）はコマンド置換で失われないため検出できず、この
# trailing newline のケースでのみ再現する。誤結合された場合、期待される
# 「spa\n」（passed=1 total=4）と「spa」（passed=1 total=1）が
# 「spa」（passed=2 total=5）1 本に化けるため、それぞれの具体値まで比較する。
expect_exit "cat value with a trailing newline is not merged with the same name without it" 1 \
  --matrix "$FIXTURES/trailing-newline-cat.json" --all-categories
expect_contains "$LAST_OUTPUT" $'category spa\n: passed=1 total=4 threshold=70 result=FAIL' \
  "trailing-newline cat value keeps its own count"
expect_contains "$LAST_OUTPUT" "category spa: passed=1 total=1 threshold=70 result=PASS" \
  "plain spa cat value is not merged with the trailing-newline one"
if grep -qE "^category spa: passed=2 total=5" <<<"$LAST_OUTPUT"; then
  echo "FAIL [trailing newline cat merged]: trailing-newline cat value was merged with plain 'spa'" >&2
  echo "  output: $LAST_OUTPUT" >&2
  FAILURES=$((FAILURES + 1))
fi
CASES=$((CASES + 1))

# cat 値に NUL 文字（\u0000）を含む場合はスキーマ検証で明示的に拒否する
# （codex review 指摘, PR #452。bash の変数・コマンド置換は NUL を保持できず、
# --all-categories の列挙・判定を通すと "static\u0000spa" が "staticspa" 相当に
# 化けて誤ったカテゴリへ結合され得るため、静かな誤判定ではなく exit 2 の
# 診断可能なエラーにする）。
expect_exit "cat value containing NUL is rejected by schema validation" 2 \
  --matrix "$FIXTURES/nul-cat.json"
expect_exit "cat value containing NUL is rejected with all-categories" 2 \
  --matrix "$FIXTURES/nul-cat.json" --all-categories

# --- 入力・使用エラー系（exit 2） ---

expect_exit "malformed non-boolean" 2 --matrix "$FIXTURES/malformed-non-boolean.json"
expect_exit "duplicate id" 2 --matrix "$FIXTURES/duplicate-id.json"
expect_exit "empty array" 2 --matrix "$FIXTURES/empty.json"
expect_exit "missing category entries" 2 --matrix "$FIXTURES/missing-category.json" --categories static,spa,form

# 空文字列の cat はスキーマ違反として拒否する（COMPAT-1 レビュー指摘。空文字列を
# 許容したまま --all-categories の走査で無条件に skip すると、cat: "" の失敗
# ケースが類型別判定から漏れて回帰ゲートを通過してしまう）。
expect_exit "empty cat rejected" 2 --matrix "$FIXTURES/empty-cat.json"
expect_exit "empty cat rejected with all-categories" 2 --matrix "$FIXTURES/empty-cat.json" --all-categories

# 1 ファイル内に複数のトップレベル JSON 値（配列）が連続する場合を拒否する
# ことを確認する（TASK-9.2 レビュー指摘。codex review, PR #452。1 つ目の配列
# が合格でも 2 つ目の配列を無視して合格してはならない。README.md の
# 「トップレベルは単一の JSON 配列」契約に対する検証）。
expect_exit "multiple top-level JSON values rejected" 2 --matrix "$FIXTURES/multi-document.json"
expect_contains "$LAST_OUTPUT" "exactly one top-level JSON value" "multi-document error message"

expect_exit "file not found without allow-missing" 2 --matrix "$FIXTURES/does-not-exist.json"
expect_exit "file not found with allow-missing" 0 --matrix "$FIXTURES/does-not-exist.json" --allow-missing
expect_contains "$LAST_OUTPUT" "::warning::" "allow-missing warning annotation"

expect_exit "threshold out of range" 2 --matrix "$FIXTURES/pass.json" --threshold 101
expect_exit "threshold non-numeric" 2 --matrix "$FIXTURES/pass.json" --threshold abc
expect_exit "threshold leading zero rejected" 2 --matrix "$FIXTURES/pass.json" --threshold 070
expect_exit "unknown argument" 2 --matrix "$FIXTURES/pass.json" --bogus
expect_exit "missing --matrix" 2 --threshold 70

# サフィックス無しの XXXXXX 末尾にする（GNU mktemp は末尾以外の X も許容するが、
# BSD/macOS の mktemp は末尾の X 列だけを置換対象とするため、macos-latest runner
# でも同じテンプレートで確実に動く形にする。check-matrix.sh は拡張子を見ないため
# .json サフィックスは不要）。
TMP_INVALID_JSON=$(mktemp "${TMPDIR:-/tmp}/compat-regression-invalid.XXXXXX")
printf '{not valid json' >"$TMP_INVALID_JSON"
expect_exit "invalid json" 2 --matrix "$TMP_INVALID_JSON"

echo "----"
echo "compat-regression self-test: ${CASES} cases, $((CASES - FAILURES)) passed, ${FAILURES} failed"
if [ "$FAILURES" -ne 0 ]; then
  exit 1
fi
exit 0

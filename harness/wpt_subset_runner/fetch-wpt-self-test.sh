#!/usr/bin/env bash
#
# fetch-wpt.sh の自己テスト（PLUG-10・TASK-101.2.2・Issue #554）。
# ネットワークへ接続せず、ローカルの一時 git リポジトリを公式 URL の代わりに使う
# （`FETCH_WPT_SELF_TEST=1` のときだけ有効な差し替え口）。次を確認する。
#   - 既存の作業ディレクトリ（悪意ある .git/config・ignored ファイル・symlink）を再利用せず、
#     filter・hooks・fsmonitor を実行せずに新規取得で置き換えること
#   - 既存の subset.tsv が symlink でも、リンク先を書き換えないこと
#   - 取得失敗時に既存の作業ディレクトリ・subset.tsv が保持されること
# 実行: bash harness/wpt_subset_runner/fetch-wpt-self-test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FETCH="${SCRIPT_DIR}/fetch-wpt.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fetch-wpt-self-test.XXXXXX")"
trap 'rm -rf "${TMP}"' EXIT

CASES=0
FAILURES=0

check() {
  local name="$1" cond="$2"
  CASES=$((CASES + 1))
  if eval "${cond}"; then
    echo "ok   ${name}"
  else
    echo "FAIL ${name}" >&2
    FAILURES=$((FAILURES + 1))
  fi
}

# ローカルの「公式」リポジトリ（取得対象ディレクトリ dom/・resources/ を持つ 1 コミット）。
SRC="${TMP}/src"
git init -q "${SRC}"
git -C "${SRC}" config user.email "t@example.invalid"
git -C "${SRC}" config user.name "t"
git -C "${SRC}" config uploadpack.allowFilter true
git -C "${SRC}" config uploadpack.allowAnySHA1InWant true
mkdir -p "${SRC}/dom" "${SRC}/resources"
printf '<!doctype html>\n' >"${SRC}/dom/a.html"
printf '// harness\n' >"${SRC}/resources/testharness.js"
git -C "${SRC}" add -A
git -C "${SRC}" commit -q -m init
REV="$(git -C "${SRC}" rev-parse HEAD)"

JSON="${TMP}/subset.json"
cat >"${JSON}" <<JSON_EOF
{
  "schemaVersion": 1,
  "source": {"wptRevision": "${REV}"},
  "totalSelected": 1,
  "subset": [{"harness": "testharness", "dir": "dom", "file": "dom/a.html"}],
  "perDirectorySummary": [{"dir": "dom", "picked": 1}],
  "harnessBreakdown": {"testharness": 1, "reftest": 0, "other": 0}
}
JSON_EOF

# $1=WPT_WORK_DIR, $2=取得元 URL。出力は ${RUN_OUT}、終了コードは ${RUN_RC} へ。
run_fetch() {
  RUN_RC=0
  RUN_OUT="$(FETCH_WPT_SELF_TEST=1 FETCH_WPT_SELF_TEST_URL="$2" FETCH_WPT_SELF_TEST_JSON="${JSON}" \
    WPT_WORK_DIR="$1" bash "${FETCH}" 2>&1)" || RUN_RC=$?
}
SRC_URL="file://${SRC}"

# 0. 自己テスト用の差し替え口は file:/// URL 以外を拒否する（任意ホストへ向けられない）。
RUN_RC=0
RUN_OUT="$(FETCH_WPT_SELF_TEST=1 FETCH_WPT_SELF_TEST_URL="https://example.invalid/x.git" \
  WPT_WORK_DIR="${TMP}/w0" bash "${FETCH}" 2>&1)" || RUN_RC=$?
check "self-test mode rejects non-file URL" '[ "${RUN_RC}" -eq 2 ] && [[ "${RUN_OUT}" == *"file:///"* ]]'

# 1. 新規取得: wpt/ と subset.tsv が作られ、HEAD が固定リビジョン。
work="${TMP}/fresh"
run_fetch "${work}" "${SRC_URL}"
check "fresh fetch succeeds" '[ "${RUN_RC}" -eq 0 ]'
check "fresh fetch checks out pinned revision" \
  '[ "$(git -C "${work}/wpt" rev-parse HEAD)" = "${REV}" ] && [ -f "${work}/wpt/dom/a.html" ]'
check "fresh fetch writes subset.tsv" \
  '[ "$(cat "${work}/subset.tsv")" = "$(printf "testharness\tdom/a.html")" ]'
check "staging directory is cleaned up" '[ -z "$(ls -A "${work}" | grep "^\.wpt-stage" || true)" ]'

# 2. P0: 既存の subset.tsv が symlink でも、リンク先を上書きしない。
work="${TMP}/tsv-symlink"
mkdir -p "${work}"
printf 'precious\n' >"${TMP}/precious.txt"
ln -s "${TMP}/precious.txt" "${work}/subset.tsv"
run_fetch "${work}" "${SRC_URL}"
check "symlinked subset.tsv: fetch succeeds" '[ "${RUN_RC}" -eq 0 ]'
check "symlinked subset.tsv: link target unchanged" '[ "$(cat "${TMP}/precious.txt")" = "precious" ]'
check "symlinked subset.tsv: replaced by regular file" \
  '[ ! -L "${work}/subset.tsv" ] && [ "$(cat "${work}/subset.tsv")" = "$(printf "testharness\tdom/a.html")" ]'

# 3. P0: 既存 WPT_DIR の悪意ある .git/config（filter・fsmonitor・hooks）は実行されず、置き換わる。
work="${TMP}/malicious"
mkdir -p "${work}/wpt/resources"
git init -q "${work}/wpt"
MARK="${TMP}/pwned"
git -C "${work}/wpt" config filter.evil.smudge "touch ${MARK}; cat"
git -C "${work}/wpt" config filter.evil.process "touch ${MARK}"
git -C "${work}/wpt" config core.fsmonitor "touch ${MARK}"
git -C "${work}/wpt" config remote.origin.url "https://example.invalid/evil/wpt.git"
git -C "${work}/wpt" config "url.https://example.invalid/.insteadOf" "file://"
mkdir -p "${work}/wpt/.git/hooks"
printf '#!/bin/sh\ntouch %s\n' "${MARK}" >"${work}/wpt/.git/hooks/post-checkout"
chmod +x "${work}/wpt/.git/hooks/post-checkout"
printf '* filter=evil\n' >"${work}/wpt/.git/info/attributes"
printf 'resources/evil.js\n' >"${work}/wpt/.git/info/exclude"
printf 'evil()\n' >"${work}/wpt/resources/evil.js"
run_fetch "${work}" "${SRC_URL}"
check "malicious clone: fetch succeeds" '[ "${RUN_RC}" -eq 0 ]'
check "malicious clone: no command executed" '[ ! -e "${MARK}" ]'
check "malicious clone: replaced by fresh checkout" \
  '[ ! -e "${work}/wpt/resources/evil.js" ] && [ "$(git -C "${work}/wpt" rev-parse HEAD)" = "${REV}" ] && ! git -C "${work}/wpt" config --get filter.evil.smudge >/dev/null'

# 4. 既存 WPT_DIR が symlink: リンク先を変更せず、リンク自体が実体ディレクトリに置き換わる。
work="${TMP}/wpt-symlink"
mkdir -p "${work}" "${TMP}/outside"
printf 'keep\n' >"${TMP}/outside/keep.txt"
ln -s "${TMP}/outside" "${work}/wpt"
run_fetch "${work}" "${SRC_URL}"
check "symlinked WPT_DIR: fetch succeeds" '[ "${RUN_RC}" -eq 0 ]'
check "symlinked WPT_DIR: link target untouched" \
  '[ "$(ls -A "${TMP}/outside")" = "keep.txt" ] && [ ! -L "${work}/wpt" ] && [ -f "${work}/wpt/dom/a.html" ]'

# 5. WORK_DIR 自体が symlink なら拒否する。
mkdir -p "${TMP}/real-work"
ln -s "${TMP}/real-work" "${TMP}/work-symlink"
run_fetch "${TMP}/work-symlink" "${SRC_URL}"
check "symlinked WORK_DIR is rejected" \
  '[ "${RUN_RC}" -eq 2 ] && [[ "${RUN_OUT}" == *"is a symlink"* ]] && [ -z "$(ls -A "${TMP}/real-work")" ]'

# 6. 取得失敗時: 既存の wpt/ と subset.tsv は変更されず、一時ディレクトリも残らない。
work="${TMP}/keep-on-failure"
mkdir -p "${work}/wpt"
printf 'old-content\n' >"${work}/wpt/marker.txt"
printf 'old-tsv\n' >"${work}/subset.tsv"
run_fetch "${work}" "file://${TMP}/does-not-exist"
check "fetch failure: exits 2" '[ "${RUN_RC}" -eq 2 ]'
check "fetch failure: existing directory and subset.tsv preserved" \
  '[ "$(cat "${work}/wpt/marker.txt")" = "old-content" ] && [ "$(cat "${work}/subset.tsv")" = "old-tsv" ]'
check "fetch failure: staging directory removed" '[ -z "$(ls -A "${work}" | grep "^\.wpt-stage" || true)" ]'

echo "${CASES} cases, ${FAILURES} failures"
[ "${FAILURES}" -eq 0 ]

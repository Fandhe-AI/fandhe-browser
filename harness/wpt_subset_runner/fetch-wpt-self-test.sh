#!/usr/bin/env bash
#
# fetch-wpt.sh の既存作業ディレクトリ検証の自己テスト（PLUG-10・TASK-101.2.2・Issue #554）。
# ローカルの一時 git リポジトリだけを使い、ネットワークへ接続しない（検証は取得より前に
# 失敗する）。取得元が公式 URL でないクローン・symlink・gitdir 参照ファイルを再利用せず
# exit 2 で拒否することを確認する。実行: bash harness/wpt_subset_runner/fetch-wpt-self-test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FETCH="${SCRIPT_DIR}/fetch-wpt.sh"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fetch-wpt-self-test.XXXXXX")"
trap 'rm -rf "${TMP}"' EXIT

CASES=0
FAILURES=0

# $1=case name, $2=WPT_WORK_DIR, $3=期待するエラー断片
expect_reject() {
  local name="$1" work="$2" fragment="$3" out rc=0
  CASES=$((CASES + 1))
  out="$(WPT_WORK_DIR="${work}" bash "${FETCH}" 2>&1)" || rc=$?
  if [ "${rc}" -ne 2 ] || [[ "${out}" != *"${fragment}"* ]]; then
    echo "FAIL ${name}: rc=${rc} output=${out}" >&2
    FAILURES=$((FAILURES + 1))
  else
    echo "ok   ${name}"
  fi
}

# 1. origin が公式 URL でないクローンは再利用しない。
work="${TMP}/origin-mismatch"
mkdir -p "${work}/wpt"
git init -q "${work}/wpt"
git -C "${work}/wpt" remote add origin "https://example.invalid/evil/wpt.git"
expect_reject "origin mismatch is rejected" "${work}" "origin of"

# 2. origin が無いクローンも拒否する。
work="${TMP}/no-origin"
mkdir -p "${work}/wpt"
git init -q "${work}/wpt"
expect_reject "missing origin is rejected" "${work}" "origin of"

# 3. WPT_DIR が symlink（リポジトリ外を指す）なら拒否する。
work="${TMP}/wpt-symlink"
mkdir -p "${work}" "${TMP}/outside"
git init -q "${TMP}/outside"
ln -s "${TMP}/outside" "${work}/wpt"
expect_reject "symlinked WPT_DIR is rejected" "${work}" "is a symlink"

# 4. WORK_DIR 自体が symlink なら拒否する。
mkdir -p "${TMP}/real-work"
ln -s "${TMP}/real-work" "${TMP}/work-symlink"
expect_reject "symlinked WORK_DIR is rejected" "${TMP}/work-symlink" "is a symlink"

# 5. `.git` が gitdir 参照ファイルなら拒否する。
work="${TMP}/gitfile"
mkdir -p "${work}/wpt"
git init -q "${TMP}/gitdir-target"
printf 'gitdir: %s\n' "${TMP}/gitdir-target/.git" >"${work}/wpt/.git"
expect_reject "gitdir file is rejected" "${work}" "gitdir file is not accepted"

# 6. `.git` が symlink なら拒否する。
work="${TMP}/git-symlink"
mkdir -p "${work}/wpt"
ln -s "${TMP}/gitdir-target/.git" "${work}/wpt/.git"
expect_reject "symlinked .git is rejected" "${work}" ".git is a symlink"

# 7. 親リポジトリの一部（トップレベルでない）ディレクトリは拒否する。
work="${TMP}/nested"
git init -q "${work}"
mkdir -p "${work}/wpt/.git"
expect_reject "non-toplevel directory is rejected" "${work}" "not the top level"

echo "${CASES} cases, ${FAILURES} failures"
[ "${FAILURES}" -eq 0 ]

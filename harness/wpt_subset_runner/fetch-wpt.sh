#!/usr/bin/env bash
#
# WPT サブセット（PLUG-10・TASK-101.2.2・Issue #554）の取得スクリプト。
# `wpt-subset.json` の `source.wptRevision`（リビジョンの正本）を、公式 WPT リポジトリから
# sparse checkout で取得し、ランナー入力 `subset.tsv`（1 行 `<harness>\t<file>`）を書き出す。
# Rust 側の `wpt_subset_runner::runner`（`parse_subset_tsv`）が subset.tsv を読む。
#
# 取得元 URL はハードコードし、引数・環境変数では変えられない（自己テスト専用の差し替え口は
# 下記 `FETCH_WPT_SELF_TEST` 参照）。既存の作業ディレクトリは一切再利用せず、毎回新しい一時
# ディレクトリへ取得・検証し、成功後にだけ置き換える（PLUG-10）。ネットワークが必要なため
# CI には組み込まない（CI 連携は #278 の判断）。依存は bash + jq + git のみ
# （新規 Cargo 依存を避けるため。.claude/rules/dependency-policy.md）。
#
# 使い方: bash fetch-wpt.sh   （出力先の上書きは環境変数 WPT_WORK_DIR）
# 終了コード: 0 成功 / 2 入力・前提不正または取得失敗
set -euo pipefail

readonly WPT_URL_OFFICIAL="https://github.com/web-platform-tests/wpt.git"
WPT_URL="${WPT_URL_OFFICIAL}"
ALLOWED_PROTOCOL="https"

die() {
  echo "error: $*" >&2
  exit 2
}

command -v jq >/dev/null 2>&1 || die "jq is required"
command -v git >/dev/null 2>&1 || die "git is required"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUBSET_JSON="${SCRIPT_DIR}/wpt-subset.json"
# 自己テスト専用の差し替え口（fetch-wpt-self-test.sh がローカルの一時リポジトリを使うため）。
# `FETCH_WPT_SELF_TEST=1` のときだけ有効で、取得元は `file://` のローカルパスに限る。
# 通常の利用（変数未設定）では公式 URL・同梱 JSON 以外を受け付けない。
if [ "${FETCH_WPT_SELF_TEST:-}" = "1" ]; then
  [[ "${FETCH_WPT_SELF_TEST_URL:-}" == file:///* ]] || die "self-test mode requires a file:/// URL"
  WPT_URL="${FETCH_WPT_SELF_TEST_URL}"
  ALLOWED_PROTOCOL="file"
  SUBSET_JSON="${FETCH_WPT_SELF_TEST_JSON:-${SUBSET_JSON}}"
  echo "warning: self-test mode; fetching from ${WPT_URL}" >&2
fi
WORK_DIR="${WPT_WORK_DIR:-${SCRIPT_DIR}/wpt-work}"
WPT_DIR="${WORK_DIR}/wpt"

[ -f "${SUBSET_JSON}" ] || die "wpt-subset.json not found"

schema="$(jq -r '.schemaVersion' "${SUBSET_JSON}" | tr -d '\r')"
[ "${schema}" = "1" ] || die "unsupported schemaVersion: ${schema}"

REV="$(jq -r '.source.wptRevision // ""' "${SUBSET_JSON}" | tr -d '\r')"
[[ "${REV}" =~ ^[0-9a-f]{40}$ ]] || die "source.wptRevision must be a 40-char lowercase hex SHA"

# README「スキーマ契約」の件数・対応関係を取得前に再検証する（違反時は fail-closed）。
# 上限（1 MiB・10000 件）、totalSelected と実件数の一致、file の一意性、
# file が `<dir>/` で始まること、dir が perDirectorySummary に存在すること、
# picked・harnessBreakdown が subset の集計と一致することを確認する。
size_bytes="$(wc -c <"${SUBSET_JSON}" | tr -d '[:space:]')"
[ "${size_bytes}" -le 1048576 ] || die "wpt-subset.json exceeds 1 MiB"
contract_err="$(jq -r '
  def cnt(f): [.subset[] | select(f)] | length;
  if (.subset | type) != "array" then "subset must be an array"
  elif (.perDirectorySummary | type) != "array" then "perDirectorySummary must be an array"
  elif (.subset | length) > 10000 then "subset exceeds 10000 entries"
  elif (.totalSelected | type) != "number" or .totalSelected != (.subset | length) then
    "totalSelected does not match subset length"
  elif ([.subset[] | .file | type] | all(. == "string") | not) then "subset[].file must be a string"
  elif ([.subset[] | .dir | type] | all(. == "string") | not) then "subset[].dir must be a string"
  elif ([.subset[].file] | length) != ([.subset[].file] | unique | length) then
    "subset[].file is not unique"
  elif ([.subset[] | select(. as $e | ($e.file | startswith($e.dir + "/")) | not)] | length) > 0 then
    "subset[].file does not start with its dir"
  elif ([.perDirectorySummary[].dir] | length) != ([.perDirectorySummary[].dir] | unique | length) then
    "perDirectorySummary[].dir is not unique"
  elif ([.perDirectorySummary[].dir] as $dirs
        | [.subset[] | select(. as $e | $dirs | index($e.dir) == null)] | length) > 0 then
    "subset[].dir is not listed in perDirectorySummary"
  elif (. as $r | [.perDirectorySummary[]
        | select(.picked != (.dir as $d | [$r.subset[] | select(.dir == $d)] | length))]
        | length) > 0 then
    "perDirectorySummary[].picked does not match subset"
  elif ((.harnessBreakdown.testharness // -1) != cnt(.harness == "testharness")
        or (.harnessBreakdown.reftest // -1) != cnt(.harness == "reftest")
        or (.harnessBreakdown.other // -1) != cnt(.harness == "other")) then
    "harnessBreakdown does not match subset"
  else "" end
' "${SUBSET_JSON}")" || die "failed to validate wpt-subset.json contract"
[ -z "${contract_err}" ] || die "wpt-subset.json contract violation: ${contract_err}"

# パス規則（README のスキーマ契約と同じ）: 許可文字のみ・先頭 '/' と '..' セグメント禁止。
valid_path() {
  local p="$1"
  [[ "${p}" =~ ^[A-Za-z0-9][A-Za-z0-9._/-]*$ ]] || return 1
  # 空セグメント（'//'・末尾 '/'）はランナーの validate_relative_path が拒否するため、
  # 書き出す TSV がランナー側の規則を必ず満たすようここでも拒否する。
  case "/${p}/" in
    */../* | *//* ) return 1 ;;
  esac
  case "${p}" in
    */) return 1 ;;
  esac
  return 0
}

# testharness.js と、テストが共通で参照するサポートスクリプトの置き場も取得する。
PATTERNS=("/resources/" "/common/" "/css/support/")
# プロセス置換では jq の失敗を set -e / pipefail で検出できないため、出力を変数へ
# 取り込んで終了状態を明示的に確認してから処理する。
dirs_out="$(jq -r '.subset[].dir' "${SUBSET_JSON}")" || die "failed to read .subset[].dir"
dirs_sorted="$(printf '%s\n' "${dirs_out}" | sort -u)" || die "failed to sort dirs"
while IFS= read -r d; do
  d="${d%$'\r'}"
  [ -n "${d}" ] || continue
  valid_path "${d}" || die "invalid dir in wpt-subset.json: ${d}"
  PATTERNS+=("/${d}/")
done <<<"${dirs_sorted}"
[ "${#PATTERNS[@]}" -gt 1 ] || die "no directories in wpt-subset.json"

# 作業ディレクトリ（WORK_DIR）は symlink を拒否する。取得は WORK_DIR 直下の一時ディレクトリ
# （同一ファイルシステム＝mv が rename になる）で行い、失敗時は trap で消す。既存の
# WPT_DIR・subset.tsv は成功するまで変更しない。
[ ! -L "${WORK_DIR}" ] || die "${WORK_DIR} is a symlink; refusing to use it"
mkdir -p "${WORK_DIR}"
STAGE="$(mktemp -d "${WORK_DIR}/.wpt-stage.XXXXXX")" || die "failed to create staging directory"
trap 'rm -rf "${STAGE}"' EXIT
NEW_DIR="${STAGE}/wpt"
TSV_TMP="${STAGE}/subset.tsv"

# subset.tsv は検証済みの行だけを書く（harness は列挙値、file は valid_path）。
files_out="$(jq -r '.subset[] | [.harness, .file] | @tsv' "${SUBSET_JSON}")" || die "failed to read .subset[]"
while IFS=$'\t' read -r harness file; do
  [ -n "${harness}${file}" ] || continue
  file="${file%$'\r'}"
  case "${harness}" in
    testharness | reftest | other) ;;
    *) die "invalid harness in wpt-subset.json: ${harness}" ;;
  esac
  valid_path "${file}" || die "invalid file in wpt-subset.json: ${file}"
  printf '%s\t%s\n' "${harness}" "${file}" >>"${TSV_TMP}"
done <<<"${files_out}"
[ -s "${TSV_TMP}" ] || die "no entries in wpt-subset.json"

# git はユーザー・システム設定や環境変数経由の GIT_DIR 等に影響されないようにし、
# hooks・fsmonitor を無効化し、使用プロトコルを取得元の種別に限る。新規に作った一時
# リポジトリだけを触るため、既存クローンの .git/config（filter.*・url.* 等）は読まない。
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
export GIT_ALLOW_PROTOCOL="${ALLOWED_PROTOCOL}"
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_CONFIG_PARAMETERS GIT_CONFIG_COUNT GIT_CONFIG
git_new() {
  git -c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.excludesFile=/dev/null \
    -C "${NEW_DIR}" "$@"
}

mkdir "${NEW_DIR}"
git_new init -q || die "git init failed"
git_new remote add origin "${WPT_URL}" || die "git remote add failed"
git_new sparse-checkout set --no-cone -- "${PATTERNS[@]}" || die "sparse-checkout failed"
git_new fetch -q --depth 1 --filter=blob:none origin "${REV}" || die "git fetch ${REV} failed"
git_new checkout -q --detach "${REV}" || die "git checkout ${REV} failed"
[ "$(git_new rev-parse --verify HEAD)" = "${REV}" ] || die "HEAD is not ${REV}"

# 取得と検証が成功した後にだけ置き換える。置き換え先が symlink なら辿らずリンク自体を除去する
# （mv は symlink 先のディレクトリの中へ移動してしまうため）。既存ディレクトリは退避してから
# 差し替え、差し替えに失敗したら元へ戻す。
if [ -L "${WPT_DIR}" ]; then
  rm -f "${WPT_DIR}"
elif [ -e "${WPT_DIR}" ]; then
  [ -d "${WPT_DIR}" ] || die "${WPT_DIR} exists but is not a directory"
  mv "${WPT_DIR}" "${STAGE}/old" || die "failed to move aside ${WPT_DIR}"
fi
if ! mv "${NEW_DIR}" "${WPT_DIR}"; then
  [ ! -e "${STAGE}/old" ] || mv "${STAGE}/old" "${WPT_DIR}"
  die "failed to install ${WPT_DIR}"
fi

# subset.tsv も同じ手順（symlink を辿らず rename で置き換える）。
OUT_TSV="${WORK_DIR}/subset.tsv"
if [ -L "${OUT_TSV}" ]; then
  rm -f "${OUT_TSV}"
elif [ -d "${OUT_TSV}" ]; then
  die "${OUT_TSV} is a directory"
fi
mv -f "${TSV_TMP}" "${OUT_TSV}" || die "failed to write ${OUT_TSV}"
echo "WPT ${REV} ready at ${WPT_DIR}; wrote ${OUT_TSV}"

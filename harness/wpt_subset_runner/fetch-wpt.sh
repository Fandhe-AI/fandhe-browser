#!/usr/bin/env bash
#
# WPT サブセット（PLUG-10・TASK-101.2.2・Issue #554）の取得スクリプト。
# `wpt-subset.json` の `source.wptRevision`（リビジョンの正本）を、公式 WPT リポジトリから
# sparse checkout で取得し、ランナー入力 `subset.tsv`（1 行 `<harness>\t<file>`）を書き出す。
# Rust 側の `wpt_subset_runner::runner`（`parse_subset_tsv`）が subset.tsv を読む。
#
# 取得元 URL はハードコードし、引数・環境変数では変えられない。ネットワークが必要なため
# CI には組み込まない（CI 連携は #278 の判断）。依存は bash + jq + git のみ
# （新規 Cargo 依存を避けるため。.claude/rules/dependency-policy.md）。
#
# 使い方: bash fetch-wpt.sh   （出力先の上書きは環境変数 WPT_WORK_DIR）
# 終了コード: 0 成功 / 2 入力・前提不正または取得失敗
set -euo pipefail

readonly WPT_URL="https://github.com/web-platform-tests/wpt.git"

die() {
  echo "error: $*" >&2
  exit 2
}

command -v jq >/dev/null 2>&1 || die "jq is required"
command -v git >/dev/null 2>&1 || die "git is required"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUBSET_JSON="${SCRIPT_DIR}/wpt-subset.json"
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

# subset.tsv は検証済みの行だけを書く（harness は列挙値、file は valid_path）。
TSV_TMP="$(mktemp "${TMPDIR:-/tmp}/wpt-subset-tsv.XXXXXX")"
trap 'rm -f "${TSV_TMP}"' EXIT
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

# 既存の作業ディレクトリは信頼しない（fail-closed。PLUG-10）。別の取得元・別リポジトリ・
# リポジトリ外を指す symlink を再利用すると、固定リビジョンと無関係なコードを JS ランナーへ
# 渡してしまうため、次を全て満たす場合だけ再利用する。
#   - WORK_DIR・WPT_DIR・WPT_DIR/.git が symlink でなく実体のディレクトリ
#     （`.git` がファイル＝gitdir 参照の場合も、外部の gitdir を指し得るため拒否する）
#   - WPT_DIR 自身がリポジトリのトップレベル（親リポジトリの一部ではない）
#   - origin の URL（.git/config の生の値）が公式 WPT_URL と完全一致し、url.* の書き換え設定が無い
# 再利用するクローンの .git/config に仕込まれたコマンド（fsmonitor・hooks）を実行しないよう、
# WPT_DIR に対する git 呼び出しは常に無効化オプション付きで行う。
git_wpt() {
  git -c core.fsmonitor=false -c core.hooksPath=/dev/null \
    -c core.excludesFile=/dev/null -C "${WPT_DIR}" "$@"
}

# 追跡ファイルの変更・未追跡ファイルに加え、無視対象（.gitignore・.git/info/exclude・
# グローバル ignore）のファイルも検出して拒否する。`--porcelain` 単体は無視対象を列挙せず、
# 固定リビジョンに属さない JS が resources/ 等に残っていても清潔と誤判定するため
# `--ignored=matching` を付け、ユーザー環境の ignore 設定に依存しないよう excludesFile を無効化する。
assert_clean() {
  local out
  out="$(git_wpt status --porcelain --untracked-files=all \
    --ignored=matching)" || die "git status failed"
  [ -z "${out}" ] || die "${WPT_DIR} has local, untracked or ignored files; remove it and re-run for a fresh checkout"
}

[ ! -L "${WORK_DIR}" ] || die "${WORK_DIR} is a symlink; refusing to use it"
mkdir -p "${WORK_DIR}"
[ ! -L "${WPT_DIR}" ] || die "${WPT_DIR} is a symlink; refusing to reuse it"

if [ -e "${WPT_DIR}" ]; then
  [ -d "${WPT_DIR}" ] || die "${WPT_DIR} exists but is not a directory"
  [ ! -L "${WPT_DIR}/.git" ] || die "${WPT_DIR}/.git is a symlink; refusing to reuse it"
  [ -d "${WPT_DIR}/.git" ] || die "${WPT_DIR}/.git is not a directory (gitdir file is not accepted)"
  top="$(git_wpt rev-parse --show-toplevel 2>/dev/null || true)"
  [ -n "${top}" ] && [ "$(cd "${top}" && pwd -P)" = "$(cd "${WPT_DIR}" && pwd -P)" ] \
    || die "${WPT_DIR} is not the top level of its own git repository"
  origin_raw="$(git_wpt config --local --get remote.origin.url 2>/dev/null || true)"
  [ "${origin_raw}" = "${WPT_URL}" ] \
    || die "origin of ${WPT_DIR} is not ${WPT_URL}; remove it and re-run for a fresh clone"
  # クローン内の `url.<base>.insteadOf` は URL を別ホストへ書き換え得るため拒否する
  # （ユーザーのグローバル設定の insteadOf は信頼するため、--local のみ検査する）。
  [ -z "$(git_wpt config --local --get-regexp '^url\.' 2>/dev/null || true)" ] \
    || die "${WPT_DIR} defines url rewrite rules (url.*); remove it and re-run for a fresh clone"
  # 取得・チェックアウトの前にも検査する（無視対象を残したクローンを触らず拒否する）。
  assert_clean
else
  git clone --filter=blob:none --no-checkout --sparse -- "${WPT_URL}" "${WPT_DIR}" \
    || die "git clone failed"
fi

# sparse パターンはリビジョンの一致に関わらず毎回設定する（クローン直後の HEAD が
# 固定リビジョンと一致していても作業ツリーを必ず構築するため）。
git_wpt sparse-checkout set --no-cone -- "${PATTERNS[@]}" \
  || die "sparse-checkout failed"
# 取得元は `origin` 名ではなく検証済みの定数 URL を明示する。
if [ "$(git_wpt rev-parse --verify HEAD 2>/dev/null || true)" != "${REV}" ]; then
  git_wpt fetch --filter=blob:none "${WPT_URL}" "${REV}" || die "git fetch ${REV} failed"
fi
git_wpt checkout --detach "${REV}" || die "git checkout ${REV} failed"
# 既存クローンを再利用する場合、追跡ファイルの変更・未追跡ファイルが残っていると、
# 固定リビジョンと異なる testharness.js やテストを実行できてしまう。チェックアウト後に
# HEAD が固定リビジョンであり、作業ツリーが清潔であることを確認し、違えば失敗させる
# （手動で wpt-work を削除して取得し直す）。
[ "$(git_wpt rev-parse --verify HEAD)" = "${REV}" ] || die "HEAD is not ${REV}"
assert_clean

cp "${TSV_TMP}" "${WORK_DIR}/subset.tsv"
echo "WPT ${REV} ready at ${WPT_DIR}; wrote ${WORK_DIR}/subset.tsv"

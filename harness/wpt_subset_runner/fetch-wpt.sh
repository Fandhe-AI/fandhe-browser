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

# パス規則（README のスキーマ契約と同じ）: 許可文字のみ・先頭 '/' と '..' セグメント禁止。
valid_path() {
  local p="$1"
  [[ "${p}" =~ ^[A-Za-z0-9][A-Za-z0-9._/-]*$ ]] || return 1
  case "/${p}/" in
    */../*) return 1 ;;
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

mkdir -p "${WORK_DIR}"

if [ ! -d "${WPT_DIR}/.git" ]; then
  git clone --filter=blob:none --no-checkout --sparse -- "${WPT_URL}" "${WPT_DIR}" \
    || die "git clone failed"
fi

# sparse パターンはリビジョンの一致に関わらず毎回設定する（クローン直後の HEAD が
# 固定リビジョンと一致していても作業ツリーを必ず構築するため）。
git -C "${WPT_DIR}" sparse-checkout set --no-cone -- "${PATTERNS[@]}" \
  || die "sparse-checkout failed"
if [ "$(git -C "${WPT_DIR}" rev-parse --verify HEAD 2>/dev/null || true)" != "${REV}" ]; then
  git -C "${WPT_DIR}" fetch --filter=blob:none origin "${REV}" || die "git fetch ${REV} failed"
fi
git -C "${WPT_DIR}" checkout --detach "${REV}" || die "git checkout ${REV} failed"

cp "${TSV_TMP}" "${WORK_DIR}/subset.tsv"
echo "WPT ${REV} ready at ${WPT_DIR}; wrote ${WORK_DIR}/subset.tsv"

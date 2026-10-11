#!/usr/bin/env bash
# ドキュメント／設定ファイル系 lint（CI の lint-docs ジョブと同等）。
# 使い方: scripts/lint-docs.sh <md|yaml|editorconfig|commits>
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

usage() {
  echo "usage: $(basename "$0") <md|yaml|editorconfig|commits>" >&2
  exit 2
}

[ "$#" -eq 1 ] || usage

lint_md() {
  npx --yes "markdownlint-cli@${MARKDOWNLINT_VERSION}" --ignore-path .markdownlintignore "**/*.md"
}

# yamllint は Python 製のため npx で賄えない。導入済みの実体（brew / pip）を優先し、
# uvx があれば固定版のワンショット実行で代替する。いずれも無ければ fail-closed で
# 導入方法を案内して失敗する（silent skip は CI との false-green 乖離になるため行わない）。
lint_yaml() {
  if command -v yamllint >/dev/null 2>&1; then
    yamllint .
  elif command -v uvx >/dev/null 2>&1; then
    uvx "yamllint==${YAMLLINT_VERSION}" .
  else
    echo "error: yamllint not found. Install: brew install yamllint / pip install yamllint==${YAMLLINT_VERSION}" >&2
    exit 1
  fi
}

lint_editorconfig() {
  npx --yes "editorconfig-checker@${EC_NPM_VERSION}"
}

# main からの分岐点以降のコミットを CI（lint-docs の commitlint ステップ）と同じ extends 構成で
# 検証する。origin/main が未取得の環境では範囲を決められないためスキップする。
# `git rev-parse --verify --quiet` は「参照が存在しない」場合に終了コード 1 を返す。これだけを
# skip 条件にし、それ以外の失敗（`fatal: detected dubious ownership` 等は典型的に 128）は
# エラーを表示して非 0 終了する（fail-closed。無音 skip で実エラーを見逃さない）。
lint_commits() {
  local out st base
  if out="$(git rev-parse --verify --quiet refs/remotes/origin/main 2>&1 >/dev/null)"; then
    st=0
  else
    st=$?
  fi
  if [ "${st}" -eq 1 ]; then
    skip "origin/main is not fetched; commitlint skipped"
  elif [ "${st}" -ne 0 ]; then
    printf '%s\n' "${out}" >&2
    echo "error: failed to resolve origin/main (git rev-parse exit=${st})" >&2
    exit 1
  fi
  base="$(git merge-base origin/main HEAD)" || {
    echo "error: git merge-base failed" >&2
    exit 1
  }
  npx --yes -p "@commitlint/cli@${COMMITLINT_VERSION}" -p "@commitlint/config-conventional@${COMMITLINT_CONFIG_VERSION}" \
    commitlint --extends @commitlint/config-conventional --from "${base}" --to HEAD
}

case "$1" in
  md) lint_md ;;
  yaml) lint_yaml ;;
  editorconfig) lint_editorconfig ;;
  commits) lint_commits ;;
  *) usage ;;
esac

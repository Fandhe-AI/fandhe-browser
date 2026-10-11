#!/usr/bin/env bash
# scripts/*.sh が共有するヘルパー。直接実行せず source して使う:
#   DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "${DIR}/lib.sh"
#
# 呼び出し元は `set -euo pipefail` を引き継ぐため、失敗は終了コードとして
# 呼び出し元（人・`make`・CI）へそのまま伝播する。出力文字列は英語（japanese-style.md）。
set -euo pipefail

_lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${_lib_dir}/.." && pwd)"
# shellcheck disable=SC2034 # source した側のスクリプトで参照する
readonly ROOT_DIR
# shellcheck source=./versions.sh
source "${_lib_dir}/versions.sh"

# cwd に依存せず常にリポジトリルートで動かす（`scripts/x.sh` 直叩きでも `make` 経由でも同じ対象）
cd "${ROOT_DIR}"

# require_cmd <name> [hint]: PATH 上に無ければ失敗する。導入は一切試みない。
require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command not found on PATH: $1${2:+ ($2)}" >&2
    return 1
  fi
}

# reject_args <script-name> "$@": 引数を取らないスクリプトで未知の入力を黙って無視せず終了コード 2 で止める。
reject_args() {
  local script_name="$1"
  shift
  if [ "$#" -ne 0 ]; then
    echo "usage: ${script_name} (no arguments)" >&2
    exit 2
  fi
}

# has_cargo: ルートに Cargo.toml があるか。
has_cargo() { [ -f Cargo.toml ]; }

# has_members: workspace に member crate（crates/*/Cargo.toml）が 1 つでもあるか。
# member が 0 件の仮想 workspace では fmt / clippy / test / tree / deny が
# 「対象パッケージが無い」エラーで落ちる（cargo の仕様）ため、対象コードが実在しない間はスキップする。
has_members() {
  local f
  for f in crates/*/Cargo.toml; do
    [ -f "$f" ] && return 0
  done
  return 1
}

# log <message...>: 進捗メッセージ（標準エラー）。
log() {
  echo "[$(basename "$0")] $*" >&2
}

# skip <reason>: 前提未整備でスキップする（終了コード 0）。
skip() {
  echo "skip: $*"
  exit 0
}

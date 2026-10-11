#!/usr/bin/env bash
# lefthook の git hooks を導入する（未導入なら lefthook 本体も導入）。
# lefthook（Go 製。crates.io には存在しないため cargo フォールバックは置かない）は
# brew（バージョン固定不可だが常用導線）を優先し、無ければ npm 配布版を exact 固定の
# npx ワンショットで実行する（生成される hook スクリプトは PATH → npx の順で本体を解決する）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
reject_args "$(basename "$0")" "$@"

if command -v lefthook >/dev/null 2>&1; then
  lefthook install
elif command -v brew >/dev/null 2>&1; then
  echo "installing lefthook via brew"
  brew install lefthook && lefthook install
elif command -v npx >/dev/null 2>&1; then
  echo "installing hooks via lefthook@${LEFTHOOK_VERSION} (npx)"
  npx --yes "lefthook@${LEFTHOOK_VERSION}" install
else
  echo "error: neither brew nor npx found. See https://lefthook.dev/installation/" >&2
  exit 1
fi

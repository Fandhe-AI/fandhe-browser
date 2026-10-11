#!/usr/bin/env bash
# 開発環境を一括構築する。順序（submodule → rustup → hooks）を明示的に保証する。
# Makefile の依存ターゲット並記では -j 実行時に順序が保証されず、cargo フォールバックを持つ
# hooks が rustup より先に走りうるため、ここで直列に実行する（rust-ai-library と同一方針）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
reject_args "$(basename "$0")" "$@"

"${DIR}/submodule.sh"
"${DIR}/rustup-check.sh"
"${DIR}/install-hooks.sh"
echo "setup complete"

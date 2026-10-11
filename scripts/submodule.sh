#!/usr/bin/env bash
# docs/spec（fandhe-browser-spec）サブモジュールを初期化・更新する。
# private リポジトリのためアクセス権が無い環境では取得に失敗する。ビルド・テストは
# docs/spec 抜きでも成立させる方針（CLAUDE.md）のため、失敗しても警告のみで 0 終了する。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
reject_args "$(basename "$0")" "$@"

git submodule update --init || \
  echo "warning: failed to fetch docs/spec (private). Expected without access; build and tests do not need it"

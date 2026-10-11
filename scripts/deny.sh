#!/usr/bin/env bash
# cargo deny check advisories bans licenses sources（依存監査）。cargo-deny 未導入なら固定版を導入する。
# Cargo.toml・deny.toml・member crate のいずれかが未整備の間は skip。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
reject_args "$(basename "$0")" "$@"

if ! has_cargo || [ ! -f deny.toml ] || ! has_members; then
  skip "Cargo.toml, deny.toml or a member crate is missing; deny skipped"
fi
export PATH="${HOME}/.cargo/bin:${PATH}"
if ! command -v cargo-deny >/dev/null 2>&1; then
  echo "installing cargo-deny ${CARGO_DENY_VERSION}"
  cargo install "cargo-deny@${CARGO_DENY_VERSION}" --locked
fi
cargo deny --locked check advisories bans licenses sources

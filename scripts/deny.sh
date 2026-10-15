#!/usr/bin/env bash
# cargo deny check advisories bans licenses sources（依存監査）。cargo-deny 未導入なら固定版を導入する。
# 使い方: scripts/deny.sh [servo-host]
#   引数なし: 本体 workspace と Servo ホスト（servo-host/。別 workspace・別 Cargo.lock）の両方を検査する
#   servo-host: ホストのみ検査する（それ以外の引数は exit 2）
# 本体は Cargo.toml・deny.toml・member crate のいずれかが未整備なら skip。ホストは servo-host/Cargo.toml が
# 無ければ skip、あるのに servo-host/deny.toml が無ければ fail-closed でエラーにする（TASK-38・#991。OSS-2・OSS-3）。
# CI の rust-lint は cargo deny を直接呼び本スクリプトを通らない（ホストの CI ステップは #995）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

mode="all"
case "$#:${1:-}" in
  0:) ;;
  1:servo-host) mode="servo-host" ;;
  *)
    echo "usage: $(basename "$0") [servo-host]" >&2
    exit 2
    ;;
esac

ensure_cargo_deny() {
  export PATH="${HOME}/.cargo/bin:${PATH}"
  if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "installing cargo-deny ${CARGO_DENY_VERSION}"
    cargo install "cargo-deny@${CARGO_DENY_VERSION}" --locked
  fi
}

deny_main() {
  if ! has_cargo || [ ! -f deny.toml ] || ! has_members; then
    echo "skip: Cargo.toml, deny.toml or a member crate is missing; main deny skipped"
    return 0
  fi
  ensure_cargo_deny
  echo "== cargo deny (main workspace)"
  cargo deny --locked check advisories bans licenses sources
}

deny_servo_host() {
  if [ ! -f servo-host/Cargo.toml ]; then
    echo "skip: servo-host/Cargo.toml is missing; servo-host deny skipped"
    return 0
  fi
  if [ ! -f servo-host/deny.toml ]; then
    echo "error: servo-host/Cargo.toml exists but servo-host/deny.toml is missing" >&2
    return 1
  fi
  ensure_cargo_deny
  echo "== cargo deny (servo-host)"
  # --config を明示し、deny.toml 欠落時に本体の設定へ暗黙に落ちないようにする
  cargo deny --manifest-path servo-host/Cargo.toml --config servo-host/deny.toml --locked \
    check advisories bans licenses sources
}

if [ "${mode}" = "all" ]; then
  deny_main
fi
deny_servo_host

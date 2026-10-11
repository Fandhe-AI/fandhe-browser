#!/usr/bin/env bash
# 受入基準 2（許可外ライセンスが混入したら fail することをログで示す。TASK-9.1・REPAIR-8・OSS-3・
# RENDER-1）の negative test。deny.toml の [licenses] allow は実依存グラフの現状に依存するため、
# `deny`（実 workspace の依存監査）だけでは「許可外ライセンスを実際に reject できるか」を継続的に
# 検証できない。本スクリプトは依存ゼロの canary crate を `mktemp -d`（リポジトリ外）に都度生成し、
# 本リポの deny.toml をそのまま当てて `cargo deny check licenses` を実行して fail-closed 性自体を検証する。
#
# - `--config` はグローバルオプションのため `check` より前に置く（実機検証済み）
# - canary は依存ゼロなので `--locked --offline` で実行でき、ネットワークも advisory DB も使わない
# - canary の Cargo.toml には `publish` を書かない（脅威モデルはサードパーティ crate の混入であり、
#   公開可能な crate と同じ扱いで監査対象に含めるため）
# - MPL-2.0 を negative ケースに含めるのは Servo を fandhe-browser-render に隔離する方針
#   （RENDER-1・licensing.md）を裏付けるため
# - 陽性対照（MIT OR Apache-2.0）を先に走らせ、deny.toml の破損や実行環境の問題を
#   「reject された」と取り違えないようにする
# - `warning[license-not-encountered]` は正常なノイズ。判定は行全体で `error[rejected]` /
#   `error[unlicensed]` を含むかで行う
# - cargo-deny の出力は抑止せず `error[` 周辺を標準出力へ残す（受入基準 2 の CI ログ証跡）
# - cargo-deny を上げて診断文字列が変わる場合は本スクリプトも確認する
#   （CARGO_DENY_VERSION は scripts/versions.sh が単一の版管理箇所）
# - `cargo deny` は `if out=$(...); then code=0; else code=$?; fi` の形で呼ぶ（errexit 下でも
#   reject 期待ケースの非 0 終了から診断アサーションへ到達させるため）
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
reject_args "$(basename "$0")" "$@"

# GPL/AGPL/LGPL の代表と MPL-2.0。陽性対照は空白を含むため含めない。
DENY_LICENSE_REJECT_CASES="GPL-3.0-only AGPL-3.0-only LGPL-2.1-only MPL-2.0"

[ -f deny.toml ] || skip "no deny.toml; check-deny-license-reject skipped"

export PATH="${HOME}/.cargo/bin:${PATH}"
if ! command -v cargo-deny >/dev/null 2>&1; then
  echo "installing cargo-deny ${CARGO_DENY_VERSION}"
  cargo install "cargo-deny@${CARGO_DENY_VERSION}" --locked
fi

tmp="$(mktemp -d)" || { echo "error: mktemp -d failed" >&2; exit 1; }
trap 'rm -rf "${tmp}"' EXIT
deny_cfg="${ROOT_DIR}/deny.toml"

# make_canary <dir-name> <license-or-empty>
make_canary() {
  local dir="${tmp}/$1"
  mkdir -p "${dir}/src"
  {
    echo '[package]'
    echo 'name = "deny-license-canary"'
    echo 'version = "0.0.0"'
    echo 'edition = "2024"'
    if [ -n "$2" ]; then echo "license = \"$2\""; fi
    echo ''
    echo '[workspace]'
  } > "${dir}/Cargo.toml"
  touch "${dir}/src/lib.rs"
  cargo generate-lockfile --offline --manifest-path "${dir}/Cargo.toml" >/dev/null 2>&1 || {
    echo "error: failed to generate Cargo.lock for canary $1" >&2
    exit 1
  }
}

# run_case <label> <expected-diagnostic-or-empty> <manifest>
run_case() {
  local label="$1" expect_diag="$2" manifest="$3" out code
  if out="$(cargo deny --manifest-path "${manifest}" --config "${deny_cfg}" --locked --offline check licenses 2>&1)"; then
    code=0
  else
    code=$?
  fi
  printf '%s\n' "${out}" | grep -F -A6 'error[' || true
  if [ -z "${expect_diag}" ]; then
    if [ "${code}" -ne 0 ]; then
      echo "error: positive control (${label}) failed (exit=${code}); deny.toml syntax or the environment may be broken" >&2
      exit 1
    fi
    echo "OK: ${label} ok (exit=${code})"
  else
    if [ "${code}" -eq 0 ] || ! printf '%s\n' "${out}" | grep -qF "${expect_diag}"; then
      echo "error: ${label} was not rejected (exit=${code}); expected diagnostic ${expect_diag} is missing" >&2
      exit 1
    fi
    echo "OK: ${label} rejected (exit=${code})"
  fi
}

make_canary positive "MIT OR Apache-2.0"
run_case "MIT OR Apache-2.0" "" "${tmp}/positive/Cargo.toml"
for lic in ${DENY_LICENSE_REJECT_CASES}; do
  make_canary "case-${lic}" "${lic}"
  run_case "${lic}" "error[rejected]" "${tmp}/case-${lic}/Cargo.toml"
done
make_canary unlicensed ""
run_case "unlicensed (no license field)" "error[unlicensed]" "${tmp}/unlicensed/Cargo.toml"

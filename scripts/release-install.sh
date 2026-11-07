#!/usr/bin/env bash
# 公開直後の crates.io から CLI とホストを `cargo install` する（#1543。release.yml の smoke-macos・smoke-ubuntu が呼ぶ）。
# 使い方: scripts/release-install.sh <install-root>
# 入力は環境変数: RELEASE_CRATE（all または 9 件の名前）・RELEASE_VERSION
#   - CLI（fandhe-browser-cli）は crate が all か CLI のとき、ホスト（fandhe-browser-servo-host）は all か
#     ホストのときに導入する。対象外でも、その版が公開済みなら導入する（スモークは CLI とホストの組を確かめる）
#   - 対象なのに未公開なら失敗し、対象外で未公開なら notice を出して飛ばす（例: CLI だけの再公開でホストが
#     その版で未公開のとき）
#   - 導入できたものを HAVE_CLI=1・HAVE_HOST=1 として GITHUB_ENV（未設定なら標準出力）へ出す
# 公開直後は index への反映待ちがあり得るため、対象 crate の公開済み判定と導入の双方を最大 6 回再試行する
# （RELEASE_INSTALL_SLEEP 秒間隔。既定 60）。対象外 crate の未公開判定は待つ理由がないので再試行せず即 skip する。
# `--self-test` で再試行の挙動を偽 release.sh・偽 cargo で検証する（release-install-selftest.sh）。
# `--locked` で配布物の Cargo.lock を使う。出力文字列は英語（japanese-style.md）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"
# shellcheck source=./publish-crates.sh
source "${DIR}/publish-crates.sh"

if [ "${1:-}" = --self-test ]; then
  exec bash "${DIR}/release-install-selftest.sh"
fi

[ "$#" -eq 1 ] || {
  echo "usage: $(basename "$0") <install-root>" >&2
  exit 2
}
root="$1"
: "${RELEASE_CRATE:?RELEASE_CRATE is required}" "${RELEASE_VERSION:?RELEASE_VERSION is required}"
cli=fandhe-browser-cli

emit() {
  echo "$1=$2"
  if [ -n "${GITHUB_ENV:-}" ]; then echo "$1=$2" >>"${GITHUB_ENV}"; fi
}

# install_with_retry NAME
install_with_retry() {
  local attempt
  for attempt in 1 2 3 4 5 6; do
    if cargo install "$1" --version "${RELEASE_VERSION}" --locked --root "${root}"; then
      return 0
    fi
    echo "install attempt ${attempt} of $1 failed; retrying in ${RELEASE_INSTALL_SLEEP:-60}s"
    sleep "${RELEASE_INSTALL_SLEEP:-60}"
  done
  echo "::error::cargo install $1 failed" >&2
  return 1
}

# required NAME: この実行で公開されているはずの crate か（all か名前一致）。
required() {
  case "${RELEASE_CRATE}" in
    all | "$1") return 0 ;;
  esac
  return 1
}

# published_state NAME: is-published の結果（yes / no）を出力する。対象 crate は no の間、最大 6 回再試行する
# （公開直後の sparse index の反映遅延で偽の未公開にならないように）。is-published 自体の失敗は再試行せず致命。
published_state() {
  local name="$1" state attempt
  for attempt in 1 2 3 4 5 6; do
    state="$(bash "${RELEASE_SH:-${DIR}/release.sh}" is-published "${name}")" || return 1
    if [ "${state}" = yes ] || ! required "${name}"; then
      break
    fi
    if [ "${attempt}" -lt 6 ]; then
      echo "${name} ${RELEASE_VERSION} is not in the index yet (attempt ${attempt} of 6); retrying in ${RELEASE_INSTALL_SLEEP:-60}s" >&2
      sleep "${RELEASE_INSTALL_SLEEP:-60}"
    fi
  done
  echo "${state}"
}

# handle NAME VAR: 導入する（または飛ばす）。
handle() {
  local name="$1" var="$2" state
  state="$(published_state "${name}")" || exit 1
  if [ "${state}" = yes ]; then
    install_with_retry "${name}" || exit 1
    emit "${var}" 1
    return 0
  fi
  if required "${name}"; then
    echo "::error::${name} ${RELEASE_VERSION} is not published" >&2
    exit 1
  fi
  echo "::notice::${name} ${RELEASE_VERSION} is not published; skipped"
}

handle "${cli}" HAVE_CLI
handle "${HOST_CRATE}" HAVE_HOST

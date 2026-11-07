#!/usr/bin/env bash
# release.yml が Servo ホスト（`servo-host/`）をビルドする runner の前提を整える（#1543）。
# 使い方: scripts/release-host-env.sh <ubuntu|macos|record-isolation|real-host HOST>
#   ubuntu            apt で system 依存・libclang・bubblewrap を導入し、LIBCLANG_PATH を GITHUB_ENV へ書く
#   macos             Xcode 同梱の objdump を xcrun で探し、LLVM_OBJDUMP を GITHUB_ENV へ書く
#   real-host HOST    公開された（または検証ビルドの）ホストで render の実ホスト結合テスト
#                     `real_host` を実行する（loopback のページを load して撮影し画素まで確かめる。
#                     ライブラリ層で private 宛を許可するテスト専用の経路。CLI の SSRF 既定は緩めない）。
#                     成功件数が 4 件未満なら失敗（0 件実行で通さない。ci.yml の servo-host-ubuntu と同じ）
#   record-isolation  Linux の egress 隔離（bubblewrap）の可否を、判定に使う環境情報とともに記録する。
#                     隔離できなくても失敗にしない（`best-effort` の挙動の記録。RENDER-3）
# 呼び出し元: release.yml の verify（ubuntu）・verify-macos・smoke-macos・smoke-ubuntu。
# `.github/workflows/ci.yml` の servo-host-{ubuntu,macos} の手順の複製（ci.yml は required checks に
# 関わるため本変更では触らない。統合は別 issue）。使い捨ての GitHub-hosted runner 専用で、sudo を使う。
# sysctl は読むだけで書き換えない。出力文字列は英語（japanese-style.md）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

need() { # need CMD...: PATH に無ければ失敗（fail-closed）
  local t
  for t in "$@"; do
    command -v "${t}" >/dev/null 2>&1 || {
      echo "error: ${t} is required but was not found on PATH" >&2
      exit 1
    }
  done
}

export_env() { # export_env KEY VALUE: GITHUB_ENV があれば追記し、表示もする
  echo "$1=$2"
  if [ -n "${GITHUB_ENV:-}" ]; then echo "$1=$2" >>"${GITHUB_ENV}"; fi
}

setup_ubuntu() {
  df -h /
  sudo apt-get update -qq
  # docs/setup/linux.md の apt パッケージ（Ubuntu 公式リポジトリのみ）。bubblewrap は egress 隔離の記録用
  sudo apt-get install -y --no-install-recommends \
    libfontconfig1-dev libfreetype-dev libgl1-mesa-dev libx11-dev libssl-dev \
    llvm bubblewrap
  # mozjs_sys の build.rs が llvm-objdump を要求する
  need clang cmake pkg-config python3 make llvm-objdump
  clang --version | head -1
  # bindgen（clang-sys）が別版 libclang を拾うと mozjs_sys が落ちるため、clang と同じ major の libclang に固定する
  local major libdir
  major="$(clang --version | sed -n 's/.*clang version \([0-9]*\)\..*/\1/p' | head -1)"
  [[ "${major}" =~ ^[0-9]+$ ]] || {
    echo "error: could not read the clang major version" >&2
    exit 1
  }
  sudo apt-get install -y --no-install-recommends "libclang-${major}-dev"
  libdir="/usr/lib/llvm-${major}/lib"
  if ! ls "${libdir}"/libclang*.so* >/dev/null 2>&1; then
    echo "error: libclang for LLVM ${major} not found under ${libdir}" >&2
    exit 1
  fi
  export_env LIBCLANG_PATH "${libdir}"
}

setup_macos() {
  # mozjs_sys の configure は llvm-objdump を要求する。Xcode 同梱の objdump を xcrun で探して渡す
  # （PATH は変えない。新規導入もしない）
  local objdump="" name found
  for name in llvm-objdump objdump; do
    if found="$(xcrun --find "${name}" 2>/dev/null)" && [ -x "${found}" ]; then
      objdump="${found}"
      break
    fi
  done
  [ -n "${objdump}" ] || {
    echo "error: neither llvm-objdump nor objdump was found via xcrun" >&2
    exit 1
  }
  export_env LLVM_OBJDUMP "${objdump}"
  "${objdump}" --version | head -2
  need clang cmake pkg-config python3 make
  clang --version | head -1
}

record_isolation() {
  local out="" rc
  out+="kernel: $(uname -r)"$'\n'
  if command -v bwrap >/dev/null 2>&1; then
    out+="bwrap: $(bwrap --version 2>&1 | head -1)"$'\n'
    rc=0
    bwrap --unshare-net --ro-bind / / true >/dev/null 2>&1 || rc=$?
    out+="bwrap --unshare-net probe rc: ${rc}"$'\n'
  else
    out+="bwrap: not installed"$'\n'
  fi
  if [ -r /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]; then
    out+="apparmor_restrict_unprivileged_userns: $(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)"$'\n'
  else
    out+="apparmor_restrict_unprivileged_userns: n/a"$'\n'
  fi
  printf '%s' "${out}"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
      echo "### egress isolation environment (informational)"
      echo '```'
      printf '%s' "${out}"
      echo '```'
    } >>"${GITHUB_STEP_SUMMARY}"
  fi
}

real_host() {
  local host="$1" log passed
  [ -x "${host}" ] || {
    echo "error: render host binary not found: ${host}" >&2
    exit 1
  }
  require_cmd cargo "required for the real_host test"
  log="$(mktemp)"
  FANDHE_BROWSER_RENDER_HOST="${host}" \
    cargo test -p fandhe-browser-render --locked --test real_host -- --test-threads=1 2>&1 | tee "${log}" || {
    rm -f "${log}"
    echo "error: real_host failed" >&2
    exit 1
  }
  passed="$(grep -Eo 'test result: ok\. [0-9]+ passed' "${log}" | grep -Eo '[0-9]+' | head -1 || true)"
  rm -f "${log}"
  if [ -z "${passed}" ] || [ "${passed}" -lt 4 ]; then
    echo "error: real_host ran ${passed:-0} tests (expected >= 4)" >&2
    exit 1
  fi
  echo "OK: real_host passed ${passed} tests"
}

USAGE="usage: $(basename "$0") <ubuntu|macos|record-isolation|real-host HOST>"
case "${1:-}" in
  ubuntu | macos | record-isolation) [ "$#" -eq 1 ] || { echo "${USAGE}" >&2; exit 2; } ;;
  real-host) [ "$#" -eq 2 ] || { echo "${USAGE}" >&2; exit 2; } ;;
  *) echo "${USAGE}" >&2; exit 2 ;;
esac
case "$1" in
  ubuntu) setup_ubuntu ;;
  macos) setup_macos ;;
  record-isolation) record_isolation ;;
  real-host) real_host "$2" ;;
esac

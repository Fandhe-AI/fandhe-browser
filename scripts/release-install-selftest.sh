#!/usr/bin/env bash
# release-install.sh の自己テスト（#1543）。偽の release.sh（is-published）と偽の cargo を使い、
# index 反映遅延（数回 no の後 yes）・長すぎる遅延・対象外の未公開の挙動を固定する。
# 実ネットワーク・実 cargo は使わない。`scripts/release-install.sh --self-test` から呼ばれる。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
fail=0

# 偽 release.sh: state-<name> に書いた「no を返す残り回数」を消費し、0 になったら yes を返す。
cat >"${tmp}/fake-release.sh" <<'FAKE'
#!/usr/bin/env bash
name="$2"
f="${FAKE_DIR}/state-${name}"
n="$(cat "${f}" 2>/dev/null || echo 0)"
echo "${name}" >>"${FAKE_DIR}/calls"
if [ "${n}" -gt 0 ]; then
  echo $((n - 1)) >"${f}"
  echo no
else
  echo yes
fi
FAKE
mkdir -p "${tmp}/bin"
# shellcheck disable=SC2016 # 偽 cargo の中で展開させるため単引用符にする
printf '#!/usr/bin/env bash\necho "$*" >>"${FAKE_DIR}/installs"\n' >"${tmp}/bin/cargo"
chmod +x "${tmp}/bin/cargo" "${tmp}/fake-release.sh"

# run LABEL EXPECT(ok|fail) CRATE LAG_CLI LAG_HOST
run() {
  local label="$1" expect="$2" crate="$3" lag_cli="$4" lag_host="$5" rc=0
  : >"${tmp}/calls"
  : >"${tmp}/installs"
  echo "${lag_cli}" >"${tmp}/state-fandhe-browser-cli"
  echo "${lag_host}" >"${tmp}/state-fandhe-browser-servo-host"
  FAKE_DIR="${tmp}" PATH="${tmp}/bin:${PATH}" RELEASE_SH="${tmp}/fake-release.sh" RELEASE_INSTALL_SLEEP=0 \
    RELEASE_CRATE="${crate}" RELEASE_VERSION=0.0.0 \
    bash "${DIR}/release-install.sh" "${tmp}/root" >"${tmp}/out" 2>&1 || rc=$?
  if { [ "${expect}" = ok ] && [ "${rc}" -eq 0 ]; } || { [ "${expect}" = fail ] && [ "${rc}" -ne 0 ]; }; then
    echo "OK(self-test): ${label}"
  else
    echo "NG(self-test): ${label} (rc=${rc})" >&2
    sed 's/^/    /' "${tmp}/out" >&2
    fail=1
  fi
}

# count FILE PATTERN WANT LABEL
count() {
  local n
  n="$(grep -c -- "$2" "$1" || true)"
  if [ "${n}" = "$3" ]; then
    echo "OK(self-test): $4"
  else
    echo "NG(self-test): $4 (got ${n}, want $3)" >&2
    fail=1
  fi
}

run "index lag on required crates recovers" ok all 3 2
count "${tmp}/calls" '^fandhe-browser-cli$' 4 "cli polled until published (3 misses + 1 hit)"
count "${tmp}/installs" 'install' 2 "both crates installed"
run "index lag longer than the retry budget fails" fail all 99 0
count "${tmp}/calls" '^fandhe-browser-cli$' 6 "required crate polled at most 6 times"
run "non-required unpublished host is skipped without polling" ok fandhe-browser-cli 0 99
count "${tmp}/calls" '^fandhe-browser-servo-host$' 1 "non-required crate checked once"
run "single host required with lag recovers" ok fandhe-browser-servo-host 99 2

[ "${fail}" -eq 0 ] || { echo "NG: release-install.sh self-test failed" >&2; exit 1; }
echo "OK: release-install.sh self-test passed"

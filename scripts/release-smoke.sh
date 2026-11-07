#!/usr/bin/env bash
# リリース成果物（`fandhe-browser` バイナリと、任意で Servo ホスト）のスモーク確認（#1531・#1543）。
# 使い方: scripts/release-smoke.sh <binary> [<servo-host binary>]
# 呼び出し元: `.github/workflows/release.yml` の verify-macos（dry-run でビルドされたバイナリ。ホストなし）と
# smoke-macos・smoke-ubuntu（`cargo install` した CLI とホスト）。ローカルでも
# `scripts/release-smoke.sh target/debug/fandhe-browser` のように実行できる。
#
# 確認する内容（ネットワークに依存しない。cli の引数体系は `crates/fandhe-browser-cli` を正とする）:
#   1. `query --file`: ローカル HTML を照会し、出力が期待値と完全一致する（`CLI-1`・`PERF-7`）
#   2. `launch --port 0`: CDP 互換サーバーが起動し、`/json/version` が応答する（`CDP-1`）。確認後に停止する
#   3. `screenshot`: 描画ホスト（`fandhe-browser-servo-host`）が無い環境では成功を装わず
#      `rendering is not available` で非 0 終了する（`RENDER-1`。ホストは別配布）。
#      URL は SSRF 検証を通る公開 IP リテラルで、描画ホスト解決の前に失敗するため通信は発生しない。
#      CLI は一時ディレクトリへコピーして実行する（`cargo install --root` では CLI の隣にホストが入り、
#      隣接ホストが解決されてしまうため）
# ホストを渡したときだけ追加で確認する（ネットワークには出ない）:
#   4. ホストの `--selftest`: 標準出力が `selftest ok:` で始まる
#   5. `FANDHE_BROWSER_RENDER_HOST` にホストを渡した `launch --port 0` が起動する（CLI がホストの版を
#      自身と照合して受け入れた。`RENDER-3`・#1540）
#   6. 版を偽るスタブホストを渡した `launch` が `render host version mismatch` で非 0 終了する
# 実ホストでのページ描画は `fandhe-browser-render` の `real_host` 結合テスト（ワークフローのステップ）が担う。
# CLI の `screenshot` はローカル・プライベート宛を SSRF の既定で拒否し切り替え手段が無いため、
# loopback のページを撮る確認はここでは行わない（`RENDER-3`・`SEC-2`）。# 失敗は非 0 で終了する。出力文字列は英語（japanese-style.md）。
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./lib.sh
source "${DIR}/lib.sh"

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || {
  echo "usage: $(basename "$0") <binary> [<servo-host binary>]" >&2
  exit 2
}
bin="$1"
host="${2:-}"
# lib.sh はリポジトリルートへ cd するため、相対パスは呼び出し時の cwd ではなくルート基準になる。
# 呼び出し元は絶対パス（または `target/...` などルート相対）を渡す。
[ -x "${bin}" ] || {
  echo "error: not an executable file: ${bin}" >&2
  exit 2
}
if [ -n "${host}" ]; then
  [ -x "${host}" ] || {
    echo "error: not an executable file: ${host}" >&2
    exit 2
  }
  # 子プロセスの解決は絶対パスだけを受け付けるため、絶対パスへ正規化する
  host="$(cd "$(dirname "${host}")" && pwd -P)/$(basename "${host}")"
fi
require_cmd curl "required for the launch check"

# macOS の mktemp は /var/folders/... を返し、/var は /private/var への symlink になる。プロファイルの
# パス検証（PROF-1）は経路上の symlink を拒否するため、symlink を解決した実パスを作業ディレクトリにする。
work="$(cd "$(mktemp -d)" && pwd -P)"
server_pid=""
cleanup() {
  if [ -n "${server_pid}" ]; then
    kill "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
  fi
  rm -rf "${work}"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# 他の環境変数が結果を左右しないよう、描画ホストの指定は外す。
unset FANDHE_BROWSER_RENDER_HOST FANDHE_BROWSER_CONFIG || true

# --- 1. query --file ---
cat >"${work}/page.html" <<'HTML'
<!doctype html>
<html><body>
<h1 id="title">Release smoke</h1>
<ul><li class="item">alpha</li><li class="item">beta</li></ul>
<a class="link" href="/docs">docs</a>
</body></html>
HTML

got="$("${bin}" query --file "${work}/page.html" -- '#title')"
[ "${got}" = '"Release smoke"' ] || fail "query text: expected '\"Release smoke\"', got '${got}'"
got="$("${bin}" query --all --file "${work}/page.html" -- 'li.item')"
[ "${got}" = '"alpha"
"beta"' ] || fail "query --all: unexpected output: ${got}"
got="$("${bin}" query --attr href --file "${work}/page.html" -- 'a.link')"
[ "${got}" = '"/docs"' ] || fail "query --attr: expected '\"/docs\"', got '${got}'"
echo "OK: query --file"

# --- 2. launch ---
"${bin}" launch --port 0 --profile-dir "${work}/profile" >"${work}/launch.out" 2>"${work}/launch.err" &
server_pid=$!
port=""
for _ in $(seq 1 100); do
  if line="$(grep -m1 'DevTools listening on ws://127.0.0.1:' "${work}/launch.err" 2>/dev/null)"; then
    port="${line#*ws://127.0.0.1:}"
    port="${port%%/*}"
    break
  fi
  kill -0 "${server_pid}" 2>/dev/null || fail "launch exited early: $(cat "${work}/launch.err")"
  sleep 0.3
done
[[ "${port}" =~ ^[0-9]{1,5}$ ]] || fail "launch did not report a listening port: $(cat "${work}/launch.err")"
version="$(curl -fsS --max-time 10 "http://127.0.0.1:${port}/json/version")" \
  || fail "GET /json/version failed on port ${port}"
[[ "${version}" == *'"webSocketDebuggerUrl"'* ]] || fail "/json/version has no webSocketDebuggerUrl: ${version}"
kill "${server_pid}" 2>/dev/null || true
wait "${server_pid}" 2>/dev/null || true
server_pid=""
echo "OK: launch (/json/version on port ${port})"

# --- 3. screenshot without a render host ---
mkdir -p "${work}/nohost"
cp "${bin}" "${work}/nohost/fandhe-browser"
rc=0
err="$("${work}/nohost/fandhe-browser" screenshot http://93.184.216.34/ "${work}/shot.png" 2>&1 >/dev/null)" || rc=$?
[ "${rc}" -ne 0 ] || fail "screenshot succeeded without a render host"
[[ "${err}" == *'rendering is not available'* ]] || fail "screenshot: expected 'rendering is not available', got '${err}'"
[ ! -e "${work}/shot.png" ] || fail "screenshot left an output file"
echo "OK: screenshot reports rendering is not available (rc=${rc})"

if [ -n "${host}" ]; then
  # --- 4. host --selftest ---
  selftest="$("${host}" --selftest)" || fail "host --selftest failed"
  [[ "${selftest}" == "selftest ok:"* ]] || fail "host --selftest: unexpected output: ${selftest}"
  echo "OK: host --selftest"

  # --- 5. launch with the real host (CLI accepts the host version) ---
  FANDHE_BROWSER_RENDER_HOST="${host}" "${bin}" launch --port 0 --profile-dir "${work}/profile-host" \
    >"${work}/launch-host.out" 2>"${work}/launch-host.err" &
  server_pid=$!
  port=""
  for _ in $(seq 1 100); do
    if line="$(grep -m1 'DevTools listening on ws://127.0.0.1:' "${work}/launch-host.err" 2>/dev/null)"; then
      port="${line#*ws://127.0.0.1:}"
      port="${port%%/*}"
      break
    fi
    kill -0 "${server_pid}" 2>/dev/null || fail "launch with the host exited early: $(cat "${work}/launch-host.err")"
    sleep 0.3
  done
  [[ "${port}" =~ ^[0-9]{1,5}$ ]] || fail "launch with the host did not report a listening port: $(cat "${work}/launch-host.err")"
  kill "${server_pid}" 2>/dev/null || true
  wait "${server_pid}" 2>/dev/null || true
  server_pid=""
  echo "OK: launch with the host (version accepted, port ${port})"

  # --- 6. launch with a host that reports another version ---
  printf '#!/bin/sh\necho "fandhe-browser-servo-host 0.0.0"\n' >"${work}/stub-host"
  chmod 755 "${work}/stub-host"
  rc=0
  err="$(FANDHE_BROWSER_RENDER_HOST="${work}/stub-host" "${bin}" launch --port 0 --profile-dir "${work}/profile-stub" 2>&1 >/dev/null)" || rc=$?
  [ "${rc}" -ne 0 ] || fail "launch accepted a host with a mismatched version"
  [[ "${err}" == *'render host version mismatch'* ]] || fail "launch: expected 'render host version mismatch', got '${err}'"
  echo "OK: launch rejects a host version mismatch (rc=${rc})"
fi

echo "OK: release smoke passed for ${bin}"

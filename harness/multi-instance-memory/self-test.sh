#!/usr/bin/env bash
#
# harness/multi-instance-memory の自己テスト（TASK-81・Issue #319、ビヘイビア PERF-4・MEAS-6）。
# 小さな N で measure.sh の起動・プロセスツリー走査・PSS 合算・JSON 出力・後始末を確認する。
# 実測値の妥当性や 50 インスタンスの結果は検証しない（それはオーナーの実測）。CI には組み込まない。
# Linux 専用。
#
# 終了コード:
#   0  すべて検証できた（Chromium 比較経路を含む）
#   1  失敗
#   3  Chromium が見つからず、Chromium 側の経路だけ未検証（他は合格。skip ではなく明示的に区別する）
#   4  Linux 以外
# 環境変数: FANDHE_BIN（実バイナリ。未設定なら実バイナリ部分は未検証と表示）、CHROMIUM_BIN。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEASURE="$SCRIPT_DIR/measure.sh"
[ "$(uname -s)" = "Linux" ] || { echo "error: Linux only" >&2; exit 4; }

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

bash -n "$MEASURE" || fail "syntax error in measure.sh"
if command -v node >/dev/null 2>&1; then node --check "$SCRIPT_DIR/navigate.mjs" || fail "navigate.mjs syntax"; fi
if command -v shellcheck >/dev/null 2>&1; then shellcheck "$MEASURE" "$0" || fail "shellcheck"; fi

# 入力検証: 範囲外・非数は起動前に終了コード 2
for bad in 0 201 abc ""; do
  st=0
  "$MEASURE" -n "$bad" --fandhe-bin /bin/true --skip-chromium >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "N='$bad' must exit 2 (got $st)"
done
ok "instance count validation (0, 201, abc, empty -> exit 2)"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/fandhe-mim-selftest.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# 使用エラー（計測開始前に終了コード 2）: PoC-1 基準値（正の有限値のみ・PSS 専用・両方指定）と URL
expect_usage_error() { # $1=説明、以降=measure.sh 追加引数
  local what="$1" st=0; shift
  "$MEASURE" --fandhe-bin /bin/true --skip-chromium --conditions idle "$@" >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "$what must exit 2 (got $st)"
}
for bad in 0 0.0 -1 nan inf abc 1e3; do
  expect_usage_error "poc1 fandhe '$bad'" --poc1-chromium-kib 100 --poc1-fandhe-kib "$bad"
  expect_usage_error "poc1 chromium '$bad'" --poc1-chromium-kib "$bad" --poc1-fandhe-kib 100
done
expect_usage_error "poc1 single value" --poc1-chromium-kib 100
ok "PoC-1 baseline validation (0, 0.0, -1, nan, inf, non-number, single -> exit 2)"
for bad in file:///etc/hostname http://127.0.0.1:18080/ http://localhost/ http://10.0.0.1/ \
  http://192.168.1.1/ http://172.16.0.1/ "http://[::1]/" ftp://example.com/; do
  expect_usage_error "url '$bad'" --url "$bad"
done
ok "--url rejects file:, loopback, private and IPv6-literal hosts (SSRF guard parity)"

# URL の JSON エスケープ: " \ 制御文字を JSON 規則でエスケープし、元の URL を復元できる
EVIL_URL='https://example.com/a"b\c?q=x'
printf '#!/bin/sh\nexec sleep 300\n' >"$TMP/sleeper"; chmod +x "$TMP/sleeper"
OUT="$("$MEASURE" -n 1 --fandhe-bin "$TMP/sleeper" --skip-chromium --conditions idle --settle 0 --net-mode host \
  --no-ready-check --url "$EVIL_URL" 2>/dev/null)" || true
if command -v jq >/dev/null 2>&1 && [ -n "$OUT" ]; then
  [ "$(jq -r .url <<<"$OUT")" = "$EVIL_URL" ] || fail "url must round-trip through JSON escaping"
  ok "URL with quote and backslash round-trips through JSON"
else
  echo "note: jq or output missing; URL JSON round-trip NOT verified"
fi

# navigate.mjs: result.errorText があれば非 0 終了、無ければ 0（WebSocket・fetch を差し替えて検証）
if command -v node >/dev/null 2>&1; then
  cat >"$TMP/stub.mjs" <<'STUB_EOF'
globalThis.fetch = async () => ({ json: async () => ({ webSocketDebuggerUrl: "ws://stub/" }) });
globalThis.WebSocket = class {
  constructor() { queueMicrotask(() => this.onopen && this.onopen()); }
  send(data) {
    const { id } = JSON.parse(data);
    const result = JSON.parse(process.env.STUB_RESULT);
    queueMicrotask(() => this.onmessage({ data: JSON.stringify({ id, result }) }));
  }
  close() {}
};
STUB_EOF
  st=0
  STUB_RESULT='{"frameId":"f","errorText":"net::ERR_ADDRESS_INVALID"}' \
    node --import "$TMP/stub.mjs" "$SCRIPT_DIR/navigate.mjs" http://stub https://example.com/ >/dev/null 2>&1 || st=$?
  [ "$st" -eq 1 ] || fail "navigate.mjs must exit 1 on errorText (got $st)"
  STUB_RESULT='{"frameId":"f"}' \
    node --import "$TMP/stub.mjs" "$SCRIPT_DIR/navigate.mjs" http://stub https://example.com/ >/dev/null 2>&1 \
    || fail "navigate.mjs must exit 0 without errorText"
  ok "navigate.mjs: errorText -> exit 1, success -> exit 0"

  # 組み込み WebSocket が Origin ヘッダを送らないこと（cdp の ws.rs は Origin 付きを 403 で拒否する）と、
  # ハンドシェイクに応答しないサーバーへの接続が期限内に非 0 で終了すること（短い期限は環境変数で上書き）
  cat >"$TMP/origin-check.mjs" <<'ORIGIN_EOF'
import net from "node:net";
const srv = net.createServer((c) => {
  c.once("data", (d) => {
    const head = String(d).toLowerCase();
    console.log(head.includes("\r\norigin:") ? "origin-sent" : "origin-absent");
    c.destroy(); srv.close();
  });
});
srv.listen(0, "127.0.0.1", () => {
  const w = new WebSocket(`ws://127.0.0.1:${srv.address().port}/x`);
  w.onerror = () => {};
});
ORIGIN_EOF
  [ "$(timeout 10 node "$TMP/origin-check.mjs")" = "origin-absent" ] \
    || fail "built-in WebSocket must not send an Origin header"
  ok "built-in WebSocket sends no Origin header (compatible with cdp check_handshake)"
  cat >"$TMP/hang.mjs" <<'HANG_EOF'
import net from "node:net";
const srv = net.createServer(() => {}).listen(0, "127.0.0.1", () => console.log(srv.address().port));
HANG_EOF
  node "$TMP/hang.mjs" >"$TMP/hang.port" & hang_pid=$!
  for _ in $(seq 1 50); do [ -s "$TMP/hang.port" ] && break; sleep 0.1; done
  st=0
  NAVIGATE_CONNECT_TIMEOUT_MS=500 timeout 20 node "$SCRIPT_DIR/navigate.mjs" \
    "http://127.0.0.1:$(cat "$TMP/hang.port")" https://example.com/ >/dev/null 2>&1 || st=$?
  kill "$hang_pid" 2>/dev/null || true
  [ "$st" -eq 1 ] || fail "navigate.mjs must exit 1 on connect timeout (got $st)"
  ok "navigate.mjs: unresponsive endpoint -> exit 1 within the deadline"
else
  echo "note: node not found; navigate.mjs errorText handling NOT verified"
fi

# 偽バイナリ: 親 sh + 子 sleep 2 本（プロセスツリー走査を検証）。ポートは使わない。
FAKE="$TMP/fake-fandhe"
cat >"$FAKE" <<'FAKE_EOF'
#!/bin/sh
sleep 300 &
sleep 300 &
wait
FAKE_EOF
chmod +x "$FAKE"

json_num() { # $1=JSON $2=キー（最初の出現）
  grep -o "\"$2\":[0-9.]*" <<<"$1" | head -1 | cut -d: -f2
}
OUT="$("$MEASURE" -n 2 --fandhe-bin "$FAKE" --skip-chromium --conditions idle --settle 1 \
  --net-mode host --no-ready-check --allow-shared-port 2>/dev/null)" || fail "fake run failed"
procs="$(json_num "$OUT" processes)"; pss="$(json_num "$OUT" pss_kib)"
[ "$procs" = "6" ] || fail "expected 6 processes (2 x (sh + 2 sleep)), got '$procs'"
[ -n "$pss" ] && [ "$pss" -gt 0 ] || fail "pss_kib must be positive (got '$pss')"
if command -v jq >/dev/null 2>&1; then
  jq -e '.task == "TASK-81" and .instances == 2 and (.results | length) == 1' <<<"$OUT" >/dev/null \
    || fail "JSON shape"
fi
ok "fake binary x2: processes=$procs pss_kib=$pss (process tree walked, JSON emitted)"

# 後始末: 偽バイナリが 1 つも残っていない
if pgrep -f "$FAKE" >/dev/null 2>&1; then fail "leftover processes after run"; fi
ok "no leftover processes"

# 実バイナリ（N=1 host。固定ポート 9333 のため。N>=2 は netns が必要で、環境次第のため任意）
if [ -n "${FANDHE_BIN:-}" ] && [ -x "$FANDHE_BIN" ]; then
  OUT="$("$MEASURE" -n 1 --fandhe-bin "$FANDHE_BIN" --skip-chromium --conditions idle --settle 1 \
    --net-mode host 2>/dev/null)" || fail "real fandhe-browser run failed"
  pss="$(json_num "$OUT" pss_kib)"
  [ -n "$pss" ] && [ "$pss" -gt 0 ] || fail "real binary pss must be positive"
  ok "real fandhe-browser x1 (host): pss_kib=$pss"
  if unshare -Urn true 2>/dev/null || [ "$(id -u)" -eq 0 ]; then
    OUT="$("$MEASURE" -n 2 --fandhe-bin "$FANDHE_BIN" --skip-chromium --conditions idle --settle 1 \
      --net-mode netns 2>/dev/null)" || fail "real fandhe-browser x2 (netns) failed"
    ok "real fandhe-browser x2 (netns): pss_kib=$(json_num "$OUT" pss_kib)"
  else
    echo "note: netns unavailable here (needs root or unprivileged userns); x2 netns run NOT verified"
  fi
else
  echo "note: FANDHE_BIN not set; real fandhe-browser run NOT verified"
fi

# Chromium: 無い場合は skip 扱いで 0 にせず、終了コード 3 で区別する
CHROMIUM="${CHROMIUM_BIN:-}"
if [ -z "$CHROMIUM" ]; then
  for c in chromium chromium-browser google-chrome google-chrome-stable; do
    if command -v "$c" >/dev/null 2>&1; then CHROMIUM="$(command -v "$c")"; break; fi
  done
fi
if [ -z "$CHROMIUM" ]; then
  echo "NOT VERIFIED: Chromium not found (set CHROMIUM_BIN). Chromium comparison path was not exercised." >&2
  exit 3
fi
OUT="$("$MEASURE" -n 2 --fandhe-bin "$FAKE" --chromium-bin "$CHROMIUM" --chromium-extra-args "--no-sandbox" \
  --conditions idle --settle 3 --net-mode host --no-ready-check --allow-shared-port 2>/dev/null)" || fail "chromium run failed"
grep -q '"reduction_pct"' <<<"$OUT" || fail "comparison missing"
ok "chromium x2 + comparison emitted"

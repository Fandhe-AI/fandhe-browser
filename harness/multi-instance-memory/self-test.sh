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

# 数値引数の先頭ゼロ・桁あふれ・値欠落は起動前に終了コード 2（八進数解釈や set -e 経由の終了コード 1 を防ぐ）
for bad in 050 08 01 007 99999999999999999999; do
  st=0
  "$MEASURE" -n "$bad" --fandhe-bin /bin/true --skip-chromium >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "N='$bad' must exit 2 (got $st)"
done
for bad in 010 00 05 -1 1.5 abc 99999; do
  st=0
  "$MEASURE" -n 1 --settle "$bad" --fandhe-bin /bin/true --skip-chromium >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "--settle '$bad' must exit 2 (got $st)"
done
for bad in 1.1.1.01 01.1.1.1 256.1.1.1 1.1.1 a.b.c.d; do
  st=0
  "$MEASURE" -n 1 --dns "$bad" --fandhe-bin /bin/true --skip-chromium >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "--dns '$bad' must exit 2 (got $st)"
done
for opt in -n --settle --url --out --fandhe-bin --conditions; do
  st=0
  "$MEASURE" "$opt" >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "'$opt' without a value must exit 2 (got $st)"
done
for bad in 050 00100 01.5 .5; do
  st=0
  "$MEASURE" --fandhe-bin /bin/true --skip-chromium --conditions idle \
    --poc1-chromium-kib "$bad" --poc1-fandhe-kib 100 >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "poc1 '$bad' must exit 2 (got $st)"
done
for bad in "--remote-debugging-port=9999" "--no-sandbox --user-data-dir=/tmp/x" "--remote-debugging-address=0.0.0.0"; do
  st=0
  "$MEASURE" --fandhe-bin /bin/true --chromium-bin /bin/true --chromium-extra-args "$bad" --conditions idle >/dev/null 2>&1 || st=$?
  [ "$st" -eq 2 ] || fail "chromium-extra-args '$bad' must exit 2 (got $st)"
done
ok "numeric args reject leading zeros/overflow/missing values; chromium extra args cannot override debug port or profile"

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
for bad in "" ","; do
  expect_usage_error "conditions '$bad'" --conditions "$bad"
done
ok "--conditions rejects an empty list (exit 2; never an empty results with exit 0)"
ok "PoC-1 baseline validation (0, 0.0, -1, nan, inf, non-number, single -> exit 2)"
for bad in file:///etc/hostname http://127.0.0.1:18080/ http://localhost/ http://10.0.0.1/ \
  http://192.168.1.1/ http://172.16.0.1/ "http://[::1]/" ftp://example.com/; do
  expect_usage_error "url '$bad'" --url "$bad"
done
ok "--url rejects file:, loopback, private and IPv6-literal hosts (SSRF guard parity)"

# URL の JSON エスケープ: " \ 制御文字を JSON 規則でエスケープし、元の URL を復元できる
EVIL_URL='https://example.com/a"b\c?q=x'
printf '#!/bin/sh\nexec sleep 300\n' >"$TMP/sleeper"; chmod +x "$TMP/sleeper"
st=0
OUT="$("$MEASURE" -n 1 --fandhe-bin "$TMP/sleeper" --skip-chromium --conditions idle --settle 0 --net-mode host \
  --no-ready-check --url "$EVIL_URL" 2>/dev/null)" || st=$?
if ! command -v jq >/dev/null 2>&1; then
  # 環境要因の未検証（skip）。計測側の失敗とは区別する
  echo "note: jq not found; URL JSON round-trip NOT verified (skip)"
else
  [ "$st" -eq 0 ] || fail "measure.sh must exit 0 for the URL round-trip run (got $st)"
  [ -n "$OUT" ] || fail "measure.sh produced empty output for the URL round-trip run"
  [ "$(jq -r .url <<<"$OUT")" = "$EVIL_URL" ] || fail "url must round-trip through JSON escaping"
  ok "URL with quote and backslash round-trips through JSON"
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

  # --page-target（Chromium 向け）: /json/list の page ターゲットへ接続し、Page.navigate 応答の loaderId と
  # 一致する Page.lifecycleEvent(load) まで待つ。STUB_NAV は navigate の result、STUB_EVENTS は遷移時に
  # 送るイベント列（JSON）、STUB_BEFORE=1 なら応答より前に送る
  cat >"$TMP/stub-page.mjs" <<'STUBP_EOF'
globalThis.fetch = async (u) => ({
  json: async () => (String(u).endsWith("/json/list")
    ? [{ type: "page", webSocketDebuggerUrl: "ws://stub/page" }]
    : {}),
});
globalThis.WebSocket = class {
  constructor(u) { if (u !== "ws://stub/page") throw new Error("unexpected ws url"); queueMicrotask(() => this.onopen()); }
  send(data) {
    const { id, method } = JSON.parse(data);
    const isNav = method === "Page.navigate";
    const result = isNav ? JSON.parse(process.env.STUB_NAV) : {};
    const events = isNav && !result.errorText ? JSON.parse(process.env.STUB_EVENTS || "[]") : [];
    const emit = () => { for (const e of events) this.onmessage({ data: JSON.stringify(e) }); };
    queueMicrotask(() => {
      if (process.env.STUB_BEFORE === "1") emit();
      this.onmessage({ data: JSON.stringify({ id, result }) });
      if (process.env.STUB_BEFORE !== "1") emit();
    });
  }
  close() {}
};
STUBP_EOF
  life() { # $1=loaderId $2=name $3=frameId
    printf '{"method":"Page.lifecycleEvent","params":{"frameId":"%s","loaderId":"%s","name":"%s","timestamp":1}}' "${3:-f}" "$1" "$2"
  }
  run_page() { # 環境変数 STUB_* を引き継いで --page-target を実行し終了コードを返す
    local st=0
    NAVIGATE_LOAD_TIMEOUT_MS=700 node --import "$TMP/stub-page.mjs" "$SCRIPT_DIR/navigate.mjs" \
      http://stub https://example.com/ --page-target >/dev/null 2>&1 || st=$?
    echo "$st"
  }
  NAV_OK='{"frameId":"f","loaderId":"NEW"}'
  [ "$(STUB_NAV='{"frameId":"f","loaderId":"NEW","errorText":"net::ERR_NAME_NOT_RESOLVED"}' STUB_EVENTS="[$(life NEW load)]" run_page)" = 1 ] \
    || fail "--page-target must exit 1 on errorText"
  [ "$(STUB_NAV="$NAV_OK" STUB_EVENTS="[$(life NEW load)]" run_page)" = 0 ] \
    || fail "--page-target must exit 0 after the matching lifecycle load"
  # 遅れて届いた旧ドキュメント（about:blank）の load の後に本物の load が来ても、本物で解決する
  [ "$(STUB_NAV="$NAV_OK" STUB_EVENTS="[$(life OLD load),$(life NEW DOMContentLoaded),$(life NEW load)]" run_page)" = 0 ] \
    || fail "--page-target must resolve on the matching load after a stale load"
  # 応答より先にイベントが届いても取りこぼさない
  [ "$(STUB_BEFORE=1 STUB_NAV="$NAV_OK" STUB_EVENTS="[$(life NEW load)]" run_page)" = 0 ] \
    || fail "--page-target must not miss a load event that precedes the navigate response"
  # 旧ドキュメントの load だけ・別 frame の load・loadEventFired だけでは解決せず、期限で非 0
  [ "$(STUB_NAV="$NAV_OK" STUB_EVENTS="[$(life OLD load)]" run_page)" = 1 ] \
    || fail "--page-target must not resolve on a stale (about:blank) load"
  [ "$(STUB_NAV="$NAV_OK" STUB_EVENTS="[$(life NEW load other)]" run_page)" = 1 ] \
    || fail "--page-target must not resolve on a load from a different frame"
  [ "$(STUB_NAV="$NAV_OK" STUB_EVENTS='[{"method":"Page.loadEventFired","params":{"timestamp":1}}]' run_page)" = 1 ] \
    || fail "--page-target must not resolve on a bare Page.loadEventFired"
  [ "$(STUB_NAV='{"frameId":"f"}' STUB_EVENTS="[$(life NEW load)]" run_page)" = 1 ] \
    || fail "--page-target must exit 1 when Page.navigate returns no loaderId"
  ok "navigate.mjs --page-target: resolves only on the lifecycle load matching the navigate loaderId"

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
  pkill -f "$TMP/hang.mjs" 2>/dev/null || true # node がラッパー経由だと $! が実プロセスと一致しない
  [ "$st" -eq 1 ] || fail "navigate.mjs must exit 1 on connect timeout (got $st)"
  ok "navigate.mjs: unresponsive endpoint -> exit 1 within the deadline"
else
  echo "note: node not found; navigate.mjs errorText handling NOT verified"
fi

# Chromium 経路（オフライン）: 偽 Chromium が受け取った引数を記録し、起動引数の組み立てを検証する
cat >"$TMP/fake-chromium" <<'FAKECR_EOF'
#!/bin/sh
printf '%s\n' "$@" >"$ARGS_DIR/args.$$"
exec sleep 300
FAKECR_EOF
chmod +x "$TMP/fake-chromium"
mkdir -p "$TMP/cargs"
ARGS_DIR="$TMP/cargs" "$MEASURE" -n 2 --fandhe-bin /bin/true --skip-fandhe --chromium-bin "$TMP/fake-chromium" \
  --conditions idle --settle 0 >/dev/null 2>&1 || fail "fake chromium idle run failed"
cat "$TMP"/cargs/args.* >"$TMP/cargs.all"
for want in --remote-debugging-address=127.0.0.1 --remote-debugging-port=9401 --remote-debugging-port=9402 about:blank; do
  grep -qxF -- "$want" "$TMP/cargs.all" || fail "chromium args must include '$want'"
done
ok "chromium launch args: loopback-only remote debugging port per instance, about:blank"

# ポート競合・接続先の取り違え（ポート 9401 を使う別プロセス）。node が必要
if command -v node >/dev/null 2>&1; then
  cat >"$TMP/listener.mjs" <<'LISTEN_EOF'
// 指定ポートで /json/version に 200 を返すだけの待ち受け。argv: port [delay_ms] または偽 Chromium としての引数
import http from "node:http";
const arg = process.argv.slice(2).find((a) => a.startsWith("--remote-debugging-port="));
const port = Number(arg ? arg.split("=")[1] : process.argv[2]);
const delay = arg ? 0 : Number(process.argv[3] || 0);
setTimeout(() => http.createServer((_, r) => r.end("{}")).listen(port, "127.0.0.1"), delay);
LISTEN_EOF
  port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
  if port_busy 9401; then
    echo "note: 127.0.0.1:9401 is already in use; chromium port-conflict checks NOT verified"
  else
    # (a) 既に使用中 -> 起動前に終了コード 2
    node "$TMP/listener.mjs" 9401 & lp=$!
    for _ in $(seq 1 50); do port_busy 9401 && break; sleep 0.1; done
    st=0
    "$MEASURE" -n 1 --fandhe-bin /bin/true --skip-fandhe --chromium-bin "$TMP/fake-chromium" \
      --conditions idle --settle 0 >/dev/null 2>&1 || st=$?
    # node がラッパー経由だと $! が実プロセスと一致しないため、スクリプトパスで止める
    pkill -f "$TMP/listener.mjs" 2>/dev/null || true; wait "$lp" 2>/dev/null || true
    for _ in $(seq 1 50); do port_busy 9401 || break; sleep 0.1; done
    [ "$st" -eq 2 ] || fail "chromium port in use must exit 2 before launch (got $st)"
    ok "chromium: busy debug port is rejected before launch (exit 2)"

    # (b) 起動後に別プロセスが同ポートで応答 -> 所有照合に失敗して終了コード 1
    # 偽 Chromium 自身は待ち受けず、起動の 1 秒後に木の外（setsid・孤児化）の別プロセスが 9401 で応答する
    cat >"$TMP/fake-chromium-foreign" <<FAKEFOREIGN_EOF
#!/bin/sh
(setsid sh -c 'sleep 1; exec node "$TMP/listener.mjs" 9401' >/dev/null 2>&1 &)
exec sleep 300
FAKEFOREIGN_EOF
    chmod +x "$TMP/fake-chromium-foreign"
    st=0
    MEASURE_READY_TIMEOUT_SEC=8 "$MEASURE" -n 1 --fandhe-bin /bin/true --skip-fandhe \
      --chromium-bin "$TMP/fake-chromium-foreign" --conditions loaded --url https://example.com/ --settle 0 \
      >/dev/null 2>"$TMP/foreign.err" || st=$?
    pkill -f "$TMP/listener.mjs" 2>/dev/null || true
    [ "$st" -eq 1 ] || fail "foreign CDP responder must exit 1 (got $st): $(cat "$TMP/foreign.err")"
    grep -q 'is not owned by chromium instance 1' "$TMP/foreign.err" || fail "foreign responder must be reported as not owned"
    ok "chromium: a CDP answered by a foreign process is not accepted (ownership mismatch -> exit 1)"

    # (c) 自分のツリーが待ち受けていれば所有照合を通り、次段（遷移）まで進む（偽サーバーは WebSocket 無しで遷移は失敗）
    cat >"$TMP/fake-chromium-listen" <<FAKELISTEN_EOF
#!/bin/sh
exec node "$TMP/listener.mjs" "\$@"
FAKELISTEN_EOF
    chmod +x "$TMP/fake-chromium-listen"
    st=0
    NAVIGATE_CONNECT_TIMEOUT_MS=500 MEASURE_READY_TIMEOUT_SEC=8 "$MEASURE" -n 1 --fandhe-bin /bin/true --skip-fandhe \
      --chromium-bin "$TMP/fake-chromium-listen" --conditions loaded --url https://example.com/ --settle 0 \
      >/dev/null 2>"$TMP/own.err" || st=$?
    [ "$st" -eq 1 ] || fail "own listener run must still exit 1 at navigation (got $st)"
    grep -q 'navigation failed for chromium instance 1' "$TMP/own.err" || fail "own listener must pass the ownership check and reach navigation"
    ok "chromium: a listener owned by the launched tree passes the ownership check"
  fi
  # fandhe 側（host モード・固定ポート 9333）も同じ所有照合: 自分のツリーが待ち受ければ成功、別プロセスなら失敗
  if port_busy 9333; then
    echo "note: 127.0.0.1:9333 is already in use; fandhe ownership checks NOT verified"
  else
    printf '#!/bin/sh\nexec node "%s" 9333\n' "$TMP/listener.mjs" >"$TMP/fake-fandhe-listen"
    chmod +x "$TMP/fake-fandhe-listen"
    "$MEASURE" -n 1 --fandhe-bin "$TMP/fake-fandhe-listen" --skip-chromium --conditions idle --settle 0 \
      --net-mode host >/dev/null 2>&1 || fail "fandhe listener owned by the launched tree must pass"
    printf '#!/bin/sh\n(setsid sh -c '"'"'sleep 1; exec node "%s" 9333'"'"' >/dev/null 2>&1 &)\nexec sleep 300\n' \
      "$TMP/listener.mjs" >"$TMP/fake-fandhe-foreign"
    chmod +x "$TMP/fake-fandhe-foreign"
    st=0
    MEASURE_READY_TIMEOUT_SEC=8 "$MEASURE" -n 1 --fandhe-bin "$TMP/fake-fandhe-foreign" --skip-chromium \
      --conditions idle --settle 0 --net-mode host >/dev/null 2>"$TMP/ffe.err" || st=$?
    pkill -f "$TMP/listener.mjs" 2>/dev/null || true
    [ "$st" -eq 1 ] || fail "fandhe foreign responder must exit 1 (got $st)"
    grep -q 'is not owned by fandhe instance' "$TMP/ffe.err" || fail "fandhe foreign responder must be reported as not owned"
    ok "fandhe (host): ownership check accepts own listener and rejects a foreign responder"
  fi
fi

# loaded: 遷移確認ができない（偽 Chromium は CDP に応答しない）場合は計測を中断し終了コード 1。
# URL は起動引数に渡さない
rm -f "$TMP"/cargs/args.*
st=0
ARGS_DIR="$TMP/cargs" NAVIGATE_CONNECT_TIMEOUT_MS=500 MEASURE_READY_TIMEOUT_SEC=2 "$MEASURE" -n 1 --fandhe-bin /bin/true --skip-fandhe \
  --chromium-bin "$TMP/fake-chromium" --conditions loaded --url https://example.com/ --settle 0 >/dev/null 2>&1 || st=$?
[ "$st" -eq 1 ] || fail "chromium loaded without a verified navigation must exit 1 (got $st)"
if grep -q 'example.com' "$TMP"/cargs/args.* 2>/dev/null; then fail "URL must not be a chromium launch argument"; fi
ok "chromium loaded: unverified navigation aborts the measurement (exit 1)"
if pgrep -f "$TMP/fake-chromium" >/dev/null 2>&1; then fail "leftover fake chromium processes"; fi

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

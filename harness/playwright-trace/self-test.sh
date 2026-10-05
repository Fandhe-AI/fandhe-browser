#!/usr/bin/env bash
#
# harness/playwright-trace の自己テスト（TASK-43.1・#246、ビヘイビア CDP-2）。
# 呼び出し元は Makefile の check-playwright-trace。Playwright・ネットワーク不要で、
# lib.mjs の判定ロジックと trace.mjs の引数検証（拒否系の終了コード）を確認する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v node >/dev/null 2>&1; then
  echo "error: node not found" >&2
  exit 1
fi

node "$SCRIPT_DIR/self-test.mjs"

# trace.mjs が不正な引数を playwright 読み込み前に exit 2 で拒否すること。
expect_exit2() {
  local name="$1"
  shift
  local status=0
  node "$SCRIPT_DIR/trace.mjs" "$@" >/dev/null 2>&1 || status=$?
  if [ "$status" -ne 2 ]; then
    echo "FAIL: $name (expected exit 2, got $status)" >&2
    exit 1
  fi
  echo "ok: $name"
}
expect_exit2 "non-loopback endpoint is rejected" --endpoint http://example.com:9333 --out /dev/null
expect_exit2 "missing endpoint is rejected" --out /dev/null
expect_exit2 "option-like out path is rejected" --endpoint http://127.0.0.1:9333 --out -x
expect_exit2 "missing module-dir is rejected" --endpoint http://127.0.0.1:9333 --out /nonexistent-dir/x.jsonl
expect_exit2 "unknown argument is rejected" --bogus

# CDP メッセージが 0 件（接続全失敗）の場合は JSONL を書かず exit 1 すること（CDP-2・fail-closed）。
# 偽の playwright-core（常に connectOverCDP が reject）と、閉じたポートを使う。
FAKE_DIR="$(mktemp -d)"
trap 'rm -rf "$FAKE_DIR"' EXIT
mkdir -p "$FAKE_DIR/node_modules/playwright-core"
cat >"$FAKE_DIR/node_modules/playwright-core/index.js" <<'JS'
exports.chromium = { connectOverCDP: async () => { throw new Error("connect refused"); } };
JS
status=0
node "$SCRIPT_DIR/trace.mjs" --endpoint http://127.0.0.1:9 --out "$FAKE_DIR/out.jsonl" \
  --module-dir "$FAKE_DIR" --playwright-version 0.0.0 >/dev/null 2>"$FAKE_DIR/err.txt" || status=$?
if [ "$status" -ne 1 ] || [ -e "$FAKE_DIR/out.jsonl" ]; then
  echo "FAIL: zero-message trace must exit 1 without output (got $status)" >&2
  exit 1
fi
grep -q "no CDP messages were captured" "$FAKE_DIR/err.txt" || { echo "FAIL: missing error message" >&2; exit 1; }
echo "ok: zero-message traces exit 1 without writing a trace"

# いずれかの段階がタイムアウトしたら JSONL を書かず exit 1 すること（CDP-2・fail-closed）。
# connectOverCDP が決して解決しない偽 playwright-core と、短い段階タイムアウトを使う。
cat >"$FAKE_DIR/node_modules/playwright-core/index.js" <<'JS'
exports.chromium = { connectOverCDP: () => new Promise(() => {}) };
JS
status=0
node "$SCRIPT_DIR/trace.mjs" --endpoint http://127.0.0.1:9 --out "$FAKE_DIR/out2.jsonl" \
  --module-dir "$FAKE_DIR" --playwright-version 0.0.0 --stage-timeout-ms 200 >/dev/null 2>"$FAKE_DIR/err2.txt" || status=$?
if [ "$status" -ne 1 ] || [ -e "$FAKE_DIR/out2.jsonl" ]; then
  echo "FAIL: timed-out stage must exit 1 without output (got $status)" >&2
  exit 1
fi
grep -q "stage timed out" "$FAKE_DIR/err2.txt" || { echo "FAIL: missing timeout message" >&2; exit 1; }
echo "ok: timed-out stage exits 1 without writing a trace"

# 末尾が改行前に途切れた pw:protocol 行は欠落になるため exit 1 すること（CDP-2・fail-closed）。
cat >"$FAKE_DIR/node_modules/playwright-core/index.js" <<'JS'
exports.chromium = { connectOverCDP: async () => {
  process.stderr.write('t pw:protocol SEND \u25ba {"id":1,"method":"A.b"}\n');
  process.stderr.write('t pw:protocol \u25c0 RECV {"id":1');
  throw new Error("connect refused");
} };
JS
status=0
node "$SCRIPT_DIR/trace.mjs" --endpoint http://127.0.0.1:9 --out "$FAKE_DIR/out3.jsonl" \
  --module-dir "$FAKE_DIR" --playwright-version 0.0.0 >/dev/null 2>"$FAKE_DIR/err3.txt" || status=$?
if [ "$status" -ne 1 ] || [ -e "$FAKE_DIR/out3.jsonl" ]; then
  echo "FAIL: incomplete protocol line must exit 1 without output (got $status)" >&2
  exit 1
fi
grep -q "incomplete pw:protocol line" "$FAKE_DIR/err3.txt" || { echo "FAIL: missing incomplete-line message" >&2; exit 1; }
echo "ok: incomplete protocol line exits 1 without writing a trace"

# --force は既存ファイルを置き換え、一時ファイルを残さないこと。
cat >"$FAKE_DIR/node_modules/playwright-core/index.js" <<'JS'
exports.chromium = { connectOverCDP: async () => {
  process.stderr.write('t pw:protocol SEND \u25ba {"id":1,"method":"A.b"}\n');
  throw new Error("connect refused");
} };
JS
echo "old" >"$FAKE_DIR/out4.jsonl"
node "$SCRIPT_DIR/trace.mjs" --endpoint http://127.0.0.1:9 --out "$FAKE_DIR/out4.jsonl" --force \
  --module-dir "$FAKE_DIR" --playwright-version 0.0.0 >/dev/null 2>&1
grep -q '"method":"A.b"' "$FAKE_DIR/out4.jsonl" || { echo "FAIL: --force did not replace the trace" >&2; exit 1; }
if ls "$FAKE_DIR"/out4.jsonl.tmp-* >/dev/null 2>&1; then
  echo "FAIL: temporary file left behind" >&2
  exit 1
fi
echo "ok: --force replaces the trace without leaving temporary files"

# 収集終了後（browser.close 中）の解析不能な protocol 行では失敗せず、取得済みトレースを保存すること。
cat >"$FAKE_DIR/node_modules/playwright-core/index.js" <<'JS'
exports.chromium = { connectOverCDP: async () => {
  process.stderr.write('t pw:protocol SEND \u25ba {"id":1,"method":"A.b"}\n');
  return {
    newContext: async () => ({ newPage: async () => ({}) }),
    close: async () => { process.stderr.write("t pw:protocol SENT garbage\n"); },
  };
} };
JS
status=0
node "$SCRIPT_DIR/trace.mjs" --endpoint http://127.0.0.1:9 --out "$FAKE_DIR/out5.jsonl" \
  --module-dir "$FAKE_DIR" --playwright-version 0.0.0 >/dev/null 2>&1 || status=$?
if [ "$status" -ne 0 ] || ! grep -q '"method":"A.b"' "$FAKE_DIR/out5.jsonl"; then
  echo "FAIL: post-capture garbage must not fail the run (got $status)" >&2
  exit 1
fi
echo "ok: protocol lines after capture stops are discarded"
echo "playwright-trace self-test: all passed"

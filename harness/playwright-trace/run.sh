#!/usr/bin/env bash
#
# Playwright の connectOverCDP → newContext → newPage 実行中の CDP トレースを収集する
# （TASK-43.1・#246、ビヘイビア CDP-2・MS-4）。呼び出し元は Makefile の trace-playwright。
# cdp crate の examples/trace_server を一時プロファイル・空きポート（127.0.0.1 限定）で
# 起動し、playwright-core（exact 固定版）を mktemp -d へ導入して trace.mjs を実行する。
# ネットワーク（npm レジストリ）と node/npm が必要。CI では実行しない（手動）。
#
# 使い方: run.sh <playwright-core の exact バージョン> <出力 JSONL パス> [--force]
set -euo pipefail

if [ "$#" -lt 2 ]; then
  echo "usage: run.sh <playwright-version> <out.jsonl> [--force]" >&2
  exit 2
fi
PW_VERSION="$1"
OUT="$2"
shift 2
if ! [[ "$PW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: playwright version must be exact x.y.z: $PW_VERSION" >&2
  exit 2
fi
for cmd in node npm cargo; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: $cmd not found" >&2; exit 1; }
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

(cd "$REPO_ROOT" && cargo build -p fandhe-browser-cdp --example trace_server)
TARGET_DIR="$(cd "$REPO_ROOT" && cargo metadata --format-version 1 --no-deps \
  | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).target_directory))')"

# playwright-core のみ・ブラウザ本体は取得しない・install script は実行しない。
(cd "$WORK" && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm install --no-save --no-audit --no-fund --ignore-scripts \
  "playwright-core@$PW_VERSION" >/dev/null)

# 一時プロファイルは trace_server が temp_dir 配下へ作る。SIGTERM 停止では自前の削除に
# 到達しないため、TMPDIR を $WORK 配下へ向けて cleanup の rm -rf で一緒に消す。
mkdir -p "$WORK/tmp"
TMPDIR="$WORK/tmp" "$TARGET_DIR/debug/examples/trace_server" >"$WORK/server.out" 2>"$WORK/server.err" &
SERVER_PID=$!

ADDR=""
for _ in $(seq 1 100); do
  if [ -s "$WORK/server.out" ]; then
    ADDR="$(sed -n 's/^listening \(127\.0\.0\.1:[0-9][0-9]*\)$/\1/p' "$WORK/server.out" | head -n 1)"
    [ -n "$ADDR" ] && break
  fi
  sleep 0.1
done
if [ -z "$ADDR" ]; then
  echo "error: trace_server did not report a listening address" >&2
  exit 1
fi

node "$SCRIPT_DIR/trace.mjs" --endpoint "http://$ADDR" --out "$OUT" \
  --module-dir "$WORK" --playwright-version "$PW_VERSION" "$@"
echo "trace written: $OUT"

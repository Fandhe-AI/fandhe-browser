#!/usr/bin/env bash
#
# harness/puppeteer-connect の自己テスト（TASK-45.2・#481、ビヘイビア CDP-3）。
# 呼び出し元は Makefile の check-puppeteer-connect。Puppeteer・ネットワーク不要で、
# stages.mjs の段階判定と connect.mjs のエンドポイント拒否（結果行 + exit 1）を確認する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v node >/dev/null 2>&1; then
  echo "error: node not found" >&2
  exit 1
fi

node --check "$SCRIPT_DIR/connect.mjs"
node --check "$SCRIPT_DIR/stages.mjs"
node "$SCRIPT_DIR/self-test.mjs"

# 非 loopback エンドポイントは puppeteer 読み込み前に結果行つきで exit 1 になること。
status=0
out="$(FANDHE_CDP_WS_ENDPOINT=ws://example.com:9222/x node "$SCRIPT_DIR/connect.mjs" 2>/dev/null)" || status=$?
if [ "$status" -ne 1 ] || ! grep -q '^FANDHE_SCRIPT_RESULT {"ok":false,"step":"connect"' <<<"$out"; then
  echo "FAIL: non-loopback endpoint must be rejected (got $status: $out)" >&2
  exit 1
fi
echo "ok: non-loopback endpoint is rejected with a result line"

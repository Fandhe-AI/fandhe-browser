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
echo "playwright-trace self-test: all passed"

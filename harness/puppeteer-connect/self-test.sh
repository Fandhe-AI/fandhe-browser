#!/usr/bin/env bash
#
# harness/puppeteer-connect の自己テスト（TASK-45.2・#481、ビヘイビア CDP-3）。
# 呼び出し元は Makefile の check-puppeteer-connect。Puppeteer・ネットワーク不要で、
# stages.mjs の段階判定と Rust 側契約テスト（tests/puppeteer_contract.rs の --ignored。0 件実行は fail）、connect.mjs のエンドポイント拒否（結果行 + exit 1）を確認する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v node >/dev/null 2>&1; then
  echo "error: node not found" >&2
  exit 1
fi

node --check "$SCRIPT_DIR/connect.mjs"
node --check "$SCRIPT_DIR/stages.mjs"
node --check "$SCRIPT_DIR/contract-sample.mjs"
node "$SCRIPT_DIR/self-test.mjs"

# 非 loopback エンドポイントは puppeteer 読み込み前に結果行つきで exit 1 になること。
status=0
out="$(FANDHE_CDP_WS_ENDPOINT=ws://example.com:9222/x node "$SCRIPT_DIR/connect.mjs" 2>/dev/null)" || status=$?
if [ "$status" -ne 1 ] || ! grep -q '^FANDHE_SCRIPT_RESULT {"ok":false,"step":"connect"' <<<"$out"; then
  echo "FAIL: non-loopback endpoint must be rejected (got $status: $out)" >&2
  exit 1
fi
echo "ok: non-loopback endpoint is rejected with a result line"

# stages.mjs の結果行が Rust 側パーサー（script_harness）の契約を満たすこと（node 必須のため
# cargo test の既定実行から外し、ここで --ignored 付きで実行する）。
cd "$SCRIPT_DIR/../.."
contract_out="$(cargo test -p fandhe-browser-cdp --test puppeteer_contract -- --ignored --exact cdp3_stages_mjs_result_line_satisfies_rust_contract 2>&1)" || {
  printf '%s\n' "$contract_out" >&2
  exit 1
}
printf '%s\n' "$contract_out"
# 0 件一致（テストがコンパイルされない・名前不一致）を成功扱いにしない（fail-closed）。
if ! grep -qE '^test result: ok\. 1 passed;' <<<"$contract_out"; then
  echo "FAIL: contract test did not run exactly once (0 tests matched?)" >&2
  exit 1
fi

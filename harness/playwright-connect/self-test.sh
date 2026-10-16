#!/usr/bin/env bash
#
# harness/playwright-connect の自己テスト（TASK-44.1・#476、ビヘイビア CDP-2）。
# 呼び出し元は Makefile の check-playwright-connect。Playwright・ネットワーク不要で、
# stages.mjs の段階判定・connect.mjs のエンドポイント拒否（結果行 + exit 1）・Rust 側契約テスト
# （tests/playwright_contract.rs の --ignored。0 件実行は fail）を確認する。
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

# 非 loopback・非 http のエンドポイントは playwright 読み込み前に結果行つきで exit 1 になること。
for ep in "http://example.com:9222" "ws://127.0.0.1:9222/x" "http://127.0.0.1:1@evil.example:80"; do
  status=0
  out="$(FANDHE_CDP_WS_ENDPOINT="$ep" FANDHE_PLAYWRIGHT_MODULE_DIR=/nonexistent node "$SCRIPT_DIR/connect.mjs" 2>/dev/null)" || status=$?
  if [ "$status" -ne 1 ] || ! grep -q '^FANDHE_SCRIPT_RESULT {"ok":false,"step":"connect"' <<<"$out"; then
    echo "FAIL: endpoint $ep must be rejected (got $status: $out)" >&2
    exit 1
  fi
done
echo "ok: non-loopback / non-http endpoints are rejected with a result line"

# fixture URL も同様に（外部ホスト・ローカルファイル・ユーザー情報偽装・https・未設定は goto 段階で結果行つき exit 1）。
for fx in "http://example.com:80/x" "file:///tmp/x" "http://127.0.0.1:1@evil.example:80/" "https://127.0.0.1:1/" ""; do
  status=0
  out="$(FANDHE_CDP_WS_ENDPOINT="http://127.0.0.1:1" FANDHE_FIXTURE_URL="$fx" FANDHE_PLAYWRIGHT_MODULE_DIR=/nonexistent node "$SCRIPT_DIR/connect.mjs" 2>/dev/null)" || status=$?
  if [ "$status" -ne 1 ] || ! grep -q '^FANDHE_SCRIPT_RESULT {"ok":false,"step":"goto"' <<<"$out"; then
    echo "FAIL: fixture url '$fx' must be rejected (got $status: $out)" >&2
    exit 1
  fi
done
echo "ok: non-loopback / non-http fixture urls are rejected with a result line"

# stages.mjs の結果行が Rust 側パーサー（script_harness）の契約を満たすこと。
cd "$SCRIPT_DIR/../.."
contract_out="$(cargo test -p fandhe-browser-cdp --test playwright_contract -- --ignored --exact cdp2_stages_mjs_result_line_satisfies_rust_contract 2>&1)" || {
  printf '%s\n' "$contract_out" >&2
  exit 1
}
printf '%s\n' "$contract_out"
# 0 件一致を成功扱いにしない（fail-closed）。
if ! grep -qE '^test result: ok\. 1 passed;' <<<"$contract_out"; then
  echo "FAIL: contract test did not run exactly once (0 tests matched?)" >&2
  exit 1
fi

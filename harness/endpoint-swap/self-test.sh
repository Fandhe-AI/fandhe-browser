#!/usr/bin/env bash
#
# harness/endpoint-swap のオフライン自己テスト（TASK-46・#252、ビヘイビア CDP-4）。
# 呼び出し元は人手（`bash harness/endpoint-swap/self-test.sh`）。CI・`make ci` には含めない。
# npm の playwright-core / puppeteer-core・Chromium・fandhe-browser・実サイトは使わない。
# 偽ライブラリ・既存の偽 CDP サーバーで判定・集計・CLI の配線を確かめる（実ライブラリでの接続は対象外）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
command -v node >/dev/null 2>&1 || { echo "error: node not found" >&2; exit 1; }
node -e 'process.exit(typeof WebSocket === "function" ? 0 : 1)' || { echo "error: Node 22+ is required" >&2; exit 1; }

for f in swap_lib adapters run_swap summarize verify_install self-test; do
  node --check "$SCRIPT_DIR/$f.mjs"
done
bash -n "$SCRIPT_DIR/run.sh"
node "$SCRIPT_DIR/self-test.mjs"

# --- run.sh の引数検証（起動・npm 導入の前に exit 2 になること）---
fail=0
expect_rc() { # $1=name $2=期待 exit $3...=引数
  local name="$1" want="$2" got=0
  shift 2
  bash "$SCRIPT_DIR/run.sh" "$@" >/dev/null 2>&1 || got=$?
  if [ "$got" -ne "$want" ]; then
    echo "FAIL [run.sh $name]: expected exit $want, got $got" >&2
    fail=1
  else
    echo "ok [run.sh $name]: exit=$got"
  fi
}
expect_rc "no args" 2
expect_rc "relative out-dir" 2 --out-dir rel
expect_rc "bad libraries" 2 --out-dir /tmp/x --libraries selenium
expect_rc "bad targets" 2 --out-dir /tmp/x --targets firefox
expect_rc "page-modules without page-js" 2 --out-dir /tmp/x --targets chromium --chromium /bin/sh --page-modules
expect_rc "chromium target without --chromium" 2 --out-dir /tmp/x --targets chromium
expect_rc "chromium not executable" 2 --out-dir /tmp/x --targets chromium --chromium /nonexistent
expect_rc "bad stage timeout" 2 --out-dir /tmp/x --targets chromium --chromium /bin/sh --stage-timeout 0
expect_rc "bad only" 2 --out-dir /tmp/x --targets chromium --chromium /bin/sh --only 'a1;rm'
expect_rc "unknown argument" 2 --out-dir /tmp/x --user-agent x
expect_rc "validate-only" 0 --out-dir /tmp/x --targets chromium --chromium /bin/sh --validate-only
[ "$fail" -eq 0 ] || exit 1

# --- 静的ガード ---
# SEC-2: UA・ヘッダ・フィンガープリントの変更や anti-bot 回避を持ち込んでいないこと
# （本ファイルと self-test.mjs は検査語を含むため除く）
if grep -rniE "user-?agent|extraHTTPHeaders|setExtraHTTPHeaders|stealth|puppeteer-extra|addInitScript|evaluateOnNewDocument" \
  "$SCRIPT_DIR" --include='*.mjs' --include='*.sh' --include='*.md' --exclude='self-test.mjs' --exclude='self-test.sh' --exclude='README.md'; then
  echo "FAIL: identity-changing or anti-bot code found in harness/endpoint-swap (SEC-2)" >&2
  exit 1
fi
echo "ok [static]: no identity-changing code"
# リポに npm の成果物を残さない（測定のときだけ一時導入する運用）
for p in package.json package-lock.json node_modules; do
  if [ -e "$SCRIPT_DIR/$p" ] || [ -e "$REPO_ROOT/$p" ]; then
    echo "FAIL: $p must not exist in the repository (npm packages are installed to a temporary directory)" >&2
    exit 1
  fi
done
echo "ok [static]: no npm artifacts in the repository"
echo "all endpoint-swap self-tests passed"

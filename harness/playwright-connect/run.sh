#!/usr/bin/env bash
#
# Playwright 接続試験の実行入口（TASK-44.1・#476、ビヘイビア CDP-2。手動・CI 対象外）。
# 呼び出し元は Makefile の test-playwright-connect。playwright-core（exact 固定版）を
# mktemp -d へ導入して verify-install.mjs で版・integrity・単独導入を検証し、
# FANDHE_PLAYWRIGHT_MODULE_DIR に渡して実 Playwright 実行テスト（goto・セレクタ取得まで。TASK-44.3。
# ループバック fixture を許す feature test-support が必要）
# （tests/playwright_flow.rs の cdp2_real_playwright_goto_and_selector）を実行する。
# TASK-44.9・#1220: 評価コンテキストを有効にするためエンジン feature（js-v8。cli の既定と同じ）を
# 付け、テストは独自 main（harness = false）でケース名の引数で実行する。成功時の完了マーカー行が
# ちょうど 1 件であることを検査する（0 件・2 件以上は失敗。fail-closed）。
# ブラウザ本体は取得せず、install script も実行しない。要 node/npm/cargo とネットワーク。
# 導入の承認状況（測定時の一時導入のみ承認済み・CI 組み込みは未承認）は README の「導入状況」を参照。
#
# 使い方: run.sh <playwright-core の exact バージョン> <npm integrity>
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: run.sh <playwright-version> <integrity>" >&2
  exit 2
fi
PW_VERSION="$1"
PW_INTEGRITY="$2"
if ! [[ "$PW_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: playwright version must be exact x.y.z: $PW_VERSION" >&2
  exit 2
fi
if ! [[ "$PW_INTEGRITY" =~ ^sha512-[A-Za-z0-9+/]+=*$ ]]; then
  echo "error: integrity must be an sha512 SRI string" >&2
  exit 2
fi
for cmd in node npm cargo; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "error: $cmd not found" >&2; exit 1; }
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

(cd "$WORK" && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm install --no-save --no-audit --no-fund --ignore-scripts \
  "playwright-core@$PW_VERSION" >/dev/null)
node "$SCRIPT_DIR/../playwright-trace/verify-install.mjs" "$WORK" playwright-core "$PW_VERSION" "$PW_INTEGRITY"

cd "$REPO_ROOT"
out="$(FANDHE_PLAYWRIGHT_MODULE_DIR="$WORK" FANDHE_PLAYWRIGHT_VERSION="$PW_VERSION" \
  cargo test -p fandhe-browser-cdp --features test-support,js-v8 --test playwright_flow -- cdp2_real_playwright_goto_and_selector 2>&1)" || {
  printf '%s\n' "$out" >&2
  exit 1
}
printf '%s\n' "$out"
# 0 件一致を成功扱いにしない（fail-closed）。
if [ "$(grep -cE '^playwright_flow: real playwright case passed \(1 case\)$' <<<"$out" || true)" != "1" ]; then
  echo "FAIL: real playwright test did not run exactly once" >&2
  exit 1
fi

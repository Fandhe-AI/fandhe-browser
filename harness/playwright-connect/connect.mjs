// Playwright 接続試験スクリプト（TASK-44.1・#476、ビヘイビア CDP-2・MS-4）。
//
// 役割: テスト基盤（crates/fandhe-browser-cdp/tests/script_harness）が起動した
// fandhe-browser の CDP サーバーへ playwright-core で接続し、結果を stdout の 1 行
// （FANDHE_SCRIPT_RESULT + JSON）で報告する。段階（stages。ロジックは stages.mjs）は
// connect → newContext → newPage → goto → selector。外部 URL へはアクセスしない（fixture URL もループバックのみ許可）。
//
// 入力（環境変数）:
//   FANDHE_CDP_WS_ENDPOINT        ループバックの http:// URL（connectOverCDP は HTTP エンドポイントを取る）
//   FANDHE_PLAYWRIGHT_MODULE_DIR  playwright-core を導入したディレクトリ（node_modules を含む）
//   FANDHE_FIXTURE_URL            goto 先の fixture ページ URL（ループバック http:// のみ。必須。TASK-44.3）
// UA・フィンガープリントを偽装するオプションは指定しない。

import { createRequire } from "node:module";
import { join } from "node:path";
import { runStages, toError } from "./stages.mjs";

const PREFIX = "FANDHE_SCRIPT_RESULT ";

// 結果行の書き込み完了まで待つ（完了前の process.exit で結果行が失われないため）。
function emit(result) {
  return new Promise((resolve) => {
    process.stdout.write(`${PREFIX}${JSON.stringify(result)}\n`, () => resolve());
  });
}

function report(ok, step, err) {
  return emit({ ok, step, error: err ? toError(err) : null });
}

let currentStage = "connect";
process.on("unhandledRejection", async (e) => {
  await report(false, currentStage, e);
  process.exit(1);
});

// 接続先・fixture URL がループバック IP リテラルの http:// URL（パスは許可） であることを URL 解析で厳密に検証する
// （前方一致ではユーザー情報でホストを偽装できる）。
function isLoopbackHttpUrl(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.protocol !== "http:") return false;
  if (url.username !== "" || url.password !== "") return false;
  if (!["127.0.0.1", "[::1]"].includes(url.hostname)) return false;
  const port = Number(url.port);
  return url.port !== "" && Number.isInteger(port) && port >= 1 && port <= 65535;
}

const endpoint = process.env.FANDHE_CDP_WS_ENDPOINT ?? "";
if (!isLoopbackHttpUrl(endpoint)) {
  await report(false, "connect", new Error("endpoint must be a loopback http:// URL"));
  process.exit(1);
}
// goto 先もループバック http:// に限る（外部・file: へ出さない。SSRF 防止）。パスは許可する。
const fixtureUrl = process.env.FANDHE_FIXTURE_URL ?? "";
if (!isLoopbackHttpUrl(fixtureUrl)) {
  await report(false, "goto", new Error("fixture url must be a loopback http:// URL"));
  process.exit(1);
}
const moduleDir = process.env.FANDHE_PLAYWRIGHT_MODULE_DIR ?? "";
if (moduleDir === "") {
  await report(false, "connect", new Error("FANDHE_PLAYWRIGHT_MODULE_DIR is not set"));
  process.exit(1);
}

try {
  const { chromium } = createRequire(join(moduleDir, "noop.js"))("playwright-core");
  const result = await runStages({
    chromium,
    endpoint,
    fixtureUrl,
    onStage: (name) => {
      currentStage = name;
    },
  });
  await emit(result);
  process.exit(result.ok ? 0 : 1);
} catch (e) {
  await report(false, currentStage, e);
  process.exit(1);
}

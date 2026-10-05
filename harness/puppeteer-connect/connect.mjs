// Puppeteer 接続試験スクリプト（TASK-45.1・#480、ビヘイビア CDP-3・MS-4）。
//
// 役割: テスト基盤（crates/fandhe-browser-cdp/tests/script_harness）が起動した
// fandhe-browser の CDP サーバーへ puppeteer-core で接続し、結果を stdout の 1 行
// （FANDHE_SCRIPT_RESULT + JSON）で報告する。接続後の操作（newPage・goto 等）は #481 の範囲。
// UA・フィンガープリントを偽装するオプションは指定しない。

import { createRequire } from "node:module";

const PREFIX = "FANDHE_SCRIPT_RESULT ";
const MAX_LEN = 500;

function report(ok, step, err) {
  const error = err
    ? {
        name: String(err?.name ?? "Error").slice(0, MAX_LEN),
        message: String(err?.message ?? err).slice(0, MAX_LEN),
      }
    : null;
  process.stdout.write(`${PREFIX}${JSON.stringify({ ok, step, error })}\n`);
}

process.on("unhandledRejection", (e) => {
  report(false, "connect", e);
  process.exit(1);
});

// 接続先がループバックの ws:// URL であることを URL 解析で厳密に検証する。
// 正規表現の前方一致では `ws://localhost:80@evil.example/` のようにユーザー情報で
// ホストを偽装できるため、hostname・port・userinfo を個別に確認する。
function isLoopbackWsEndpoint(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.protocol !== "ws:") return false;
  if (url.username !== "" || url.password !== "") return false;
  if (!["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)) return false;
  const port = Number(url.port);
  return url.port !== "" && Number.isInteger(port) && port >= 1 && port <= 65535;
}

const endpoint = process.env.FANDHE_CDP_WS_ENDPOINT ?? "";
if (!isLoopbackWsEndpoint(endpoint)) {
  report(false, "connect", new Error("endpoint must be a loopback ws:// URL"));
  process.exit(1);
}

try {
  const puppeteer = createRequire(import.meta.url)("puppeteer-core");
  const browser = await puppeteer.connect({ browserWSEndpoint: endpoint });
  await browser.disconnect();
  report(true, "connect", null);
} catch (e) {
  report(false, "connect", e);
  process.exit(1);
}

// Puppeteer 接続試験スクリプト（TASK-45.1・#480 / TASK-45.2・#481、ビヘイビア CDP-3・MS-4）。
//
// 役割: テスト基盤（crates/fandhe-browser-cdp/tests/script_harness）が起動した
// fandhe-browser の CDP サーバーへ puppeteer-core で接続し、結果を stdout の 1 行
// （FANDHE_SCRIPT_RESULT + JSON）で報告する。接続 → newPage → goto → セレクタ取得の
// 段階別到達結果（stages。ロジックは stages.mjs）を最終 1 行に載せる。
// UA・フィンガープリントを偽装するオプションは指定しない。

import { createRequire } from "node:module";
import { runStages, toError } from "./stages.mjs";

const PREFIX = "FANDHE_SCRIPT_RESULT ";

// 結果行の書き込み完了（パイプへのフラッシュ）まで待つ Promise を返す。完了前の
// process.exit で結果行が失われないよう、終了前に必ず await する。
function emit(result) {
  return new Promise((resolve) => {
    process.stdout.write(`${PREFIX}${JSON.stringify(result)}\n`, () => resolve());
  });
}

function report(ok, step, err) {
  return emit({ ok, step, error: err ? toError(err) : null });
}

// 実行中の段階名（想定外の reject を正しい段階へ帰属させる）。
let currentStage = "connect";
process.on("unhandledRejection", async (e) => {
  await report(false, currentStage, e);
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
  await report(false, "connect", new Error("endpoint must be a loopback ws:// URL"));
  process.exit(1);
}

try {
  const puppeteer = createRequire(import.meta.url)("puppeteer-core");
  const result = await runStages({
    puppeteer,
    endpoint,
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

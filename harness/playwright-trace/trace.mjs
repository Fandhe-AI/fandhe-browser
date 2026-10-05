// Playwright の connectOverCDP → newContext → newPage 実行中の CDP 送受信トレース収集
// （TASK-43.1・#246、ビヘイビア CDP-2・MS-4）。
//
// 呼び出し元は run.sh（一時サーバー起動後に実行）。段階ごとに成否を記録し、失敗した段階で
// 止める（newPage より先へ進まない・goto しない）。メッセージは DEBUG=pw:protocol の
// stderr 出力を横取りして取得する（playwright-core は logger オプションを公開しないため）。
// 出力は JSONL（スキーマは README・lib.mjs）。ポートは <PORT> へ正規化して決定的にする。
//
// 使い方: node trace.mjs --endpoint http://127.0.0.1:<port> --out <path>
//         --module-dir <playwright-core を install した dir> --playwright-version <x.y.z> [--force]

import { createRequire } from "node:module";
import { existsSync, writeFileSync } from "node:fs";
import path from "node:path";
import {
  SCHEMA_VERSION,
  MAX_LINE_BYTES,
  MAX_RECORDS,
  normalizeString,
  normalizeValue,
  parseProtocolLine,
  toJsonl,
  validateEndpoint,
  validateOutPath,
} from "./lib.mjs";

const STAGE_TIMEOUT_MS = 10_000;

function parseArgs(argv) {
  const args = { force: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--force") args.force = true;
    else if (["--endpoint", "--out", "--module-dir", "--playwright-version"].includes(a)) {
      args[a.slice(2)] = argv[++i];
    } else throw new Error(`unknown argument: ${a}`);
  }
  return args;
}

let args;
let endpoint;
try {
  args = parseArgs(process.argv.slice(2));
  endpoint = validateEndpoint(args.endpoint ?? "");
  validateOutPath(args.out);
  if (typeof args["module-dir"] !== "string" || typeof args["playwright-version"] !== "string") {
    throw new Error("--module-dir and --playwright-version are required");
  }
  if (existsSync(args.out) && !args.force) {
    throw new Error(`output exists (use --force to overwrite): ${args.out}`);
  }
} catch (e) {
  console.error(`error: ${e.message}`);
  process.exit(2);
}

const port = endpoint.port;
const records = [];
const push = (r) => {
  if (records.length >= MAX_RECORDS) throw new Error("record limit exceeded");
  records.push(r);
};

// playwright-core の debug 出力を有効化し、stderr への書き込みを横取りする（エコーしない）。
process.env.DEBUG = "pw:protocol";
process.env.DEBUG_COLORS = "no";
let pending = "";
const origWrite = process.stderr.write.bind(process.stderr);
process.stderr.write = (chunk, ...rest) => {
  pending += chunk.toString();
  // 改行が来ないまま蓄積が上限を超えたら収集を中断する（無制限バッファ防止）。
  if (Buffer.byteLength(pending) > MAX_LINE_BYTES * 2 && pending.indexOf("\n") < 0) {
    origWrite("error: protocol log line exceeds buffer limit; aborting\n");
    process.exit(1);
  }
  let idx;
  while ((idx = pending.indexOf("\n")) >= 0) {
    const line = pending.slice(0, idx);
    pending = pending.slice(idx + 1);
    try {
      const parsed = parseProtocolLine(line);
      if (parsed) push({ kind: "cdp", dir: parsed.dir, message: normalizeValue(parsed.message, port) });
    } catch (e) {
      origWrite(`warn: unparsable protocol line: ${e.message}\n`);
    }
  }
  const cb = rest.find((x) => typeof x === "function");
  if (cb) cb();
  return true;
};

// ESM は NODE_PATH を見ないため、install 先 dir から CJS require で解決する。
const req = createRequire(path.join(path.resolve(args["module-dir"]), "noop.js"));
const { chromium } = req("playwright-core");

push({
  kind: "meta",
  schema: SCHEMA_VERSION,
  purpose: "CDP trace until chromium.connectOverCDP/newContext/newPage (CDP-2, TASK-43.1)",
  playwright: args["playwright-version"],
  node: process.version,
});

const withTimeout = (p, label) =>
  Promise.race([
    p,
    new Promise((_, rej) =>
      setTimeout(() => rej(new Error(`${label} timed out after ${STAGE_TIMEOUT_MS}ms`)), STAGE_TIMEOUT_MS),
    ),
  ]);

/** 段階を実行して stage レコードを積む。成功なら値、失敗なら undefined を返す。 */
async function stage(name, fn) {
  try {
    const v = await withTimeout(fn(), name);
    push({ kind: "stage", name, ok: true });
    return { ok: true, value: v };
  } catch (e) {
    push({ kind: "stage", name, ok: false, error: normalizeString(String(e.message), port) });
    return { ok: false };
  }
}

// HTTP discovery の状態を残す（CDP が 0 件で終わった場合の原因切り分け用）。
// Playwright は /json/version/（末尾スラッシュ付き）を要求するため両方を調べる。
const origin = `http://${endpoint.hostname}:${port}`;
let wsUrl = null;
for (const p of ["/json/version", "/json/version/"]) {
  try {
    const res = await fetch(origin + p, { redirect: "manual", signal: AbortSignal.timeout(STAGE_TIMEOUT_MS) });
    push({ kind: "http", method: "GET", path: p, status: res.status });
    if (res.status === 200 && p === "/json/version") {
      const body = await res.json();
      if (typeof body.webSocketDebuggerUrl === "string") wsUrl = body.webSocketDebuggerUrl;
    }
  } catch (e) {
    push({ kind: "http", method: "GET", path: p, error: normalizeString(String(e.message), port) });
  }
}

let browser;
if (endpoint.protocol === "http:") {
  const r = await stage("connectOverCDP(http)", () => chromium.connectOverCDP(origin, { timeout: STAGE_TIMEOUT_MS }));
  if (r.ok) browser = r.value;
}
if (!browser) {
  // http 経由が失敗した場合は、discovery が返した ws:// で CDP メッセージ列の収集を試みる。
  const target = endpoint.protocol === "ws:" ? endpoint.href : wsUrl;
  if (target) {
    const wsEndpoint = validateEndpoint(target);
    const r = await stage("connectOverCDP(ws)", () =>
      chromium.connectOverCDP(wsEndpoint.href, { timeout: STAGE_TIMEOUT_MS }),
    );
    if (r.ok) browser = r.value;
  }
}

if (browser) {
  // 既存 context の有無に関わらず newContext から newPage までの CDP 列を必ず収集する。
  let context;
  const r = await stage("newContext", () => browser.newContext());
  if (r.ok) context = r.value;
  if (context) {
    // newPage 到達後は goto 等へ進まない（スコープは newPage まで）。
    await stage("newPage", () => context.newPage());
  }
  await Promise.race([browser.close().catch(() => {}), new Promise((r) => setTimeout(r, 2000))]);
}

writeFileSync(args.out, toJsonl(records), { flag: args.force ? "w" : "wx" });
process.stderr.write = origWrite;
process.exit(0);

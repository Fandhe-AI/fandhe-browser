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
import { existsSync, renameSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import {
  SCHEMA_VERSION,
  checkCollection,
  drainLines,
  MAX_RECORDS,
  normalizeString,
  normalizeValue,
  parseProtocolLine,
  toJsonl,
  readBodyLimited,
  validateDiscoveredWs,
  validateEndpoint,
  validateOutPath,
} from "./lib.mjs";

let STAGE_TIMEOUT_MS = 10_000;

function parseArgs(argv) {
  const args = { force: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--force") args.force = true;
    else if (["--endpoint", "--out", "--module-dir", "--playwright-version", "--stage-timeout-ms"].includes(a)) {
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
  if (args["stage-timeout-ms"] !== undefined) {
    const n = Number(args["stage-timeout-ms"]);
    if (!Number.isInteger(n) || n < 1 || n > 60_000) throw new Error("--stage-timeout-ms must be an integer in 1..60000");
    STAGE_TIMEOUT_MS = n;
  }
  if (existsSync(args.out) && !args.force) {
    throw new Error(`output exists (use --force to overwrite): ${args.out}`);
  }
} catch (e) {
  console.error(`error: ${e.message}`);
  process.exit(2);
}

const port = endpoint.port;
const host = endpoint.hostname;
const records = [];
const origWrite = process.stderr.write.bind(process.stderr);
// 欠落・切り詰めたトレースを正常成果物として残さないため、致命的エラーは JSONL を書かずに
// 即座に非 0 終了する（出力は最後の writeFileSync でのみ行う）。
const fatal = (msg) => {
  origWrite(`error: ${msg}; aborting without writing trace\n`);
  process.exit(1);
};
const push = (r) => {
  if (records.length >= MAX_RECORDS) fatal("record limit exceeded");
  records.push(r);
};

// playwright-core の debug 出力を有効化し、stderr への書き込みを横取りする（エコーしない）。
process.env.DEBUG = "pw:protocol";
process.env.DEBUG_COLORS = "no";
let pending = "";
// newPage 到達後の終了処理（browser.close）の CDP は収集対象外（README の契約: newPage まで）。
let capturing = true;
// 収集を止める。改行前に途切れた pw:protocol 行が残っていればメッセージ欠落になるため失敗させる。
const stopCapture = () => {
  if (!capturing) return;
  capturing = false;
  if (pending.trim() !== "") {
    let incomplete = true;
    try {
      incomplete = parseProtocolLine(pending) !== null;
    } catch {
      incomplete = true;
    }
    if (incomplete) fatal("incomplete pw:protocol line at end of capture");
  }
};
process.stderr.write = (chunk, ...rest) => {
  const cb0 = rest.find((x) => typeof x === "function");
  // 収集終了後（browser.close 中など）のログは解析も保持もせず破棄する。
  if (!capturing) {
    if (cb0) cb0();
    return true;
  }
  let lines;
  try {
    ({ lines, rest: pending } = drainLines(pending, chunk.toString()));
  } catch (e) {
    fatal(e.message);
  }
  for (const line of lines) {
    let rec = null;
    try {
      const parsed = parseProtocolLine(line);
      if (parsed) rec = { kind: "cdp", dir: parsed.dir, message: normalizeValue(parsed.message, port, host) };
    } catch (e) {
      // 解析不能行はレコード欠落になるため収集全体を失敗させる。
      fatal(`unparsable protocol line: ${e.message}`);
    }
    if (rec && capturing) push(rec);
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

const withTimeout = (p, label) => {
  let timer;
  const timeout = new Promise((_, rej) => {
    timer = setTimeout(() => rej(new Error(`${label} timed out after ${STAGE_TIMEOUT_MS}ms`)), STAGE_TIMEOUT_MS);
  });
  // 段階が先に決着してもタイマーを残さない（後からの reject・プロセス延命を防ぐ）。
  return Promise.race([p, timeout]).finally(() => clearTimeout(timer));
};

/** 段階を実行して stage レコードを積む。成功なら値、失敗なら undefined を返す。 */
let timedOutStage = null;
async function stage(name, fn) {
  try {
    const v = await withTimeout(fn(), name);
    push({ kind: "stage", name, ok: true });
    return { ok: true, value: v };
  } catch (e) {
    if (/timed out after/.test(String(e.message))) timedOutStage ??= name;
    push({ kind: "stage", name, ok: false, error: normalizeString(String(e.message), port, host) });
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
      const body = JSON.parse(await readBodyLimited(res));
      if (typeof body.webSocketDebuggerUrl === "string") wsUrl = body.webSocketDebuggerUrl;
    }
  } catch (e) {
    push({ kind: "http", method: "GET", path: p, error: normalizeString(String(e.message), port, host) });
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
    const wsEndpoint = validateDiscoveredWs(target, endpoint);
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
  stopCapture();
  await Promise.race([browser.close().catch(() => {}), new Promise((r) => setTimeout(r, 2000))]);
}

// タイムアウトした段階は fn() が中止されず CDP 送受信が続き得るため、段階の成否とメッセージ列が
// 一致しない。保存せず、進行中の接続を best-effort（上限時間付き）で閉じてから失敗終了する。
if (timedOutStage) {
  stopCapture();
  if (browser) await Promise.race([browser.close().catch(() => {}), new Promise((r) => setTimeout(r, 2000))]);
  fatal(`stage timed out: ${timedOutStage}`);
}

stopCapture();

// CDP 0 件は収集失敗（接続失敗でも CDP 送受信があれば調査用トレースとして保存する）。成功扱いの JSONL を残さず非 0 終了する。
const verdict = checkCollection({ cdpCount: records.filter((r) => r.kind === "cdp").length });
if (!verdict.ok) fatal(verdict.reason);

// --force 時は別ファイルへ書き終えてから置き換え、途中失敗で既存の成果物を壊さない。
const text = toJsonl(records);
if (args.force) {
  const tmp = `${args.out}.tmp-${process.pid}`;
  try {
    writeFileSync(tmp, text, { flag: "wx" });
    renameSync(tmp, args.out);
  } catch (e) {
    rmSync(tmp, { force: true });
    fatal(`failed to write trace: ${e.message}`);
  }
} else {
  writeFileSync(args.out, text, { flag: "wx" });
}
process.stderr.write = origWrite;
process.exit(0);

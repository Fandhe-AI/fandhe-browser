// harness/endpoint-swap のオフライン自己テスト本体（TASK-46・#252、ビヘイビア CDP-4）。self-test.sh から実行する。
// npm の playwright-core / puppeteer-core は使わない。偽ライブラリ（ページ内関数は本物の PAGE_FN_* をそのまま実行する）と
// 既存の偽 CDP サーバー（compat-practical/fake_cdp_server.mjs。/json/version の解決の検証用）だけで、
// 判定・集計・CLI の配線を確かめる。実 Playwright / Puppeteer・実 Chromium・実 fandhe-browser との接続は対象外。
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { playwrightAdapter, puppeteerAdapter, parseLoopbackEndpoint, resolveWsEndpoint } from "./adapters.mjs";
import {
  executeTask, formatSummary, inPoc22, POC22_EXCLUDED, ratePercent, runAll, summarize, validateTasks, verifyInstall, verifyMetaConsistency, verifyResultFile,
} from "./swap_lib.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const TASKS_PATH = join(here, "..", "compat-practical", "tasks.json");
const FAKE_SERVER = join(here, "..", "compat-practical", "fake_cdp_server.mjs");
const work = mkdtempSync(join(tmpdir(), "endpoint-swap-selftest-"));
let cases = 0;
const t = async (name, fn) => {
  cases++;
  try {
    await fn();
    console.log(`ok [${name}]`);
  } catch (e) {
    console.error(`FAIL [${name}]: ${e.message}`);
    process.exitCode = 1;
  }
};

// ---- 偽ライブラリ ----
// WORLD: url -> { status?, sel: { selector: { text?, fields? } } }。特殊 URL: fetcherr（goto が例外）・hang（goto が戻らない）
const WORLD = {
  "https://ok.test/": {
    status: 200,
    sel: {
      h1: { text: "Hello" },
      "p.blank": { text: "  \n\t " },
      "a.first": { text: "first link" },
      "form#f": { fields: [{ name: "user" }, { name: "" }, {}] },
      "form#nameless": { fields: [{}, { name: "" }] },
    },
  },
  "https://forbidden.test/": { status: 403, sel: { h1: { text: "Forbidden" }, a: { text: "Sign in" } } },
  "https://servererr.test/": { status: 503, sel: { h1: { text: "Unavailable" } } },
  "https://redirected.test/": { status: 304, sel: { h1: { text: "Cached" } } },
  // 応答が取れない（Playwright の goto が null を返す / fandhe-browser の http_status: null 相当）
  "https://nostatus.test/": { sel: { h1: { text: "No status" } } },
  "https://nostatus-empty.test/": { sel: { h1: { text: "  " } } },
  "https://fetcherr.test/": { sel: {} },
  "https://hang.test/": { sel: {} },
  "https://evalerr.test/": { status: 200, sel: { h1: { text: "x", evalError: true } } },
};

// stageTimeoutMs(60) より後に解決させる遅延（期限後に完了する接続・ページ作成の再現用）
const LATE_MS = 150;
const sleepMs = (ms) => new Promise((r) => setTimeout(r, ms));

function fakeNode(spec) {
  return {
    textContent: spec.text ?? null,
    querySelectorAll: () => (spec.fields ?? []).map((f) => ({ getAttribute: (k) => (k === "name" ? (f.name ?? null) : null) })),
  };
}

function fakePage(calls) {
  let url = "";
  return {
    async goto(u, opts) {
      calls.goto.push({ u, opts });
      url = u;
      if (u.includes("fetcherr")) throw Object.assign(new Error("net::ERR_NAME_NOT_RESOLVED"), { name: "Error" });
      if (u.includes("hang")) return new Promise(() => {});
      const s = WORLD[u]?.status;
      return s === undefined ? null : { status: () => s };
    },
    async $(selector) {
      const spec = WORLD[url]?.sel?.[selector];
      if (!spec) return null;
      return {
        async evaluate(fn) {
          if (spec.evalError) throw Object.assign(new Error("Execution context was destroyed"), { name: "Error" });
          return fn(fakeNode(spec));
        },
      };
    },
    async close() {
      calls.closed.push("page");
    },
  };
}

function fakePlaywright(calls, fail = {}) {
  return {
    async connectOverCDP(endpoint, opts) {
      calls.connect.push({ endpoint, opts });
      if (fail.connect === "throw") throw new Error("connect refused");
      if (fail.connect === "hang") return new Promise(() => {});
      const browser = {
        version: () => "FakeChrome/1.0",
        async newContext(o) {
          calls.context.push(o);
          if (fail.newContext === "late") await sleepMs(LATE_MS);
          else if (fail.newContext) throw new Error("newContext failed");
          return {
            async newPage() {
              if (fail.newPage === "late") await sleepMs(LATE_MS);
              else if (fail.newPage) throw new Error("newPage failed");
              return fakePage(calls);
            },
            async close() {
              calls.closed.push("context");
            },
          };
        },
        async close() {
          calls.closed.push("browser");
        },
      };
      if (fail.connect === "late") await sleepMs(LATE_MS);
      return browser;
    },
  };
}

function fakePuppeteer(calls, fail = {}) {
  return {
    async connect(opts) {
      calls.connect.push({ opts });
      if (fail.connect === "throw") throw new Error("connect refused");
      if (fail.connect === "hang") return new Promise(() => {});
      const browser = {
        async version() {
          return "FakeChrome/1.0";
        },
        async newPage() {
          if (fail.newPage === "late") await sleepMs(LATE_MS);
          else if (fail.newPage) throw new Error("newPage failed");
          return fakePage(calls);
        },
        async disconnect() {
          calls.closed.push("disconnect");
        },
      };
      if (fail.connect === "late") await sleepMs(LATE_MS);
      return browser;
    },
  };
}

const newCalls = () => ({ connect: [], context: [], goto: [], closed: [] });
const OPTS = { stageTimeoutMs: 60, settleMs: 0, target: "t" };
// ws エンドポイントを直接渡す（ネットワークに出ない）。http の解決は別ケースで偽サーバーを使う
const WS = "ws://127.0.0.1:9/devtools/browser/x";
function adapterFor(lib, calls, fail) {
  return lib === "playwright"
    ? playwrightAdapter({ chromium: fakePlaywright(calls, fail), endpoint: "http://127.0.0.1:9", stageTimeoutMs: 60 })
    : puppeteerAdapter({ puppeteer: fakePuppeteer(calls, fail), endpoint: WS, stageTimeoutMs: 60 });
}
const task = (id, kind, selector, url = "https://ok.test/") => ({ id, cat: "static", url, selector, kind });
const run = async (lib, tk, fail, calls = newCalls()) => ({ r: await executeTask(adapterFor(lib, calls, fail), tk, { ...OPTS, library: lib }), calls });

// ---- 判定（run_core.mjs と同じ基準。CDP-4）----
for (const lib of ["playwright", "puppeteer"]) {
  await t(`${lib}: text success`, async () => {
    const { r } = await run(lib, task("x1", "text", "h1"));
    assert.equal(r.success, true);
    assert.equal(r.output_sample, "Hello");
    assert.equal(r.failed_stage, null);
    assert.equal(r.reason, null);
    assert.equal(r.reached_stage, "selector");
    assert.equal(r.browser, "FakeChrome/1.0");
    assert.equal(r.http_status, 200);
    assert.equal(r.method, "first_match");
    assert.equal(r.match_count, null);
  });
  await t(`${lib}: texts judges the first match only`, async () => {
    const { r } = await run(lib, task("x2", "texts", "a.first"));
    assert.equal(r.success, true);
    assert.equal(r.method, "first_match");
    assert.equal(r.match_count, null);
  });
  await t(`${lib}: whitespace-only text is empty_result at selector`, async () => {
    const { r } = await run(lib, task("x3", "text", "p.blank"));
    assert.deepEqual([r.success, r.failed_stage, r.reason, r.reached_stage], [false, "selector", "empty_result", "goto"]);
  });
  await t(`${lib}: no element is no_match at selector`, async () => {
    const { r } = await run(lib, task("x4", "text", "h9"));
    assert.deepEqual([r.success, r.failed_stage, r.reason], [false, "selector", "no_match"]);
  });
  await t(`${lib}: form counts only descendants with a non-empty name`, async () => {
    const { r } = await run(lib, task("x5", "form", "form#f"));
    assert.equal(r.success, true);
    assert.equal(r.match_count, 1);
    assert.equal(r.method, "form_fields");
    assert.equal(r.output_sample, "1 named fields");
  });
  await t(`${lib}: form without named fields is empty_result`, async () => {
    const { r } = await run(lib, task("x6", "form", "form#nameless"));
    assert.deepEqual([r.success, r.reason, r.match_count], [false, "empty_result", 0]);
  });
  await t(`${lib}: goto exception is fetch_error at goto`, async () => {
    const { r } = await run(lib, task("x7", "text", "h1", "https://fetcherr.test/"));
    assert.deepEqual([r.success, r.failed_stage, r.reason, r.reached_stage], [false, "goto", "fetch_error", "newPage"]);
    assert.match(r.detail, /ERR_NAME_NOT_RESOLVED/);
  });
  await t(`${lib}: goto that never returns is timeout at goto`, async () => {
    const { r } = await run(lib, task("x8", "text", "h1", "https://hang.test/"));
    assert.deepEqual([r.failed_stage, r.reason], ["goto", "timeout"]);
  });
  await t(`${lib}: connect failure is attributed to connect`, async () => {
    const { r } = await run(lib, task("x9", "text", "h1"), { connect: "throw" });
    assert.deepEqual([r.failed_stage, r.reason, r.reached_stage], ["connect", "cdp_error", null]);
  });
  await t(`${lib}: connect hang is timeout at connect`, async () => {
    const { r } = await run(lib, task("x10", "text", "h1"), { connect: "hang" });
    assert.deepEqual([r.failed_stage, r.reason], ["connect", "timeout"]);
  });
  await t(`${lib}: newPage failure is attributed to newPage`, async () => {
    const { r } = await run(lib, task("x11", "text", "h1"), { newPage: true });
    assert.deepEqual([r.failed_stage, r.reason], ["newPage", "cdp_error"]);
  });
  await t(`${lib}: evaluate failure is cdp_error at selector`, async () => {
    const { r } = await run(lib, task("x12", "text", "h1", "https://evalerr.test/"));
    assert.deepEqual([r.failed_stage, r.reason], ["selector", "cdp_error"]);
  });
  await t(`${lib}: a 403 page with a matching non-empty element is a failure (http_error at goto)`, async () => {
    // c1 のセレクタ `a` のように、エラーページにも要素と非空テキストがある場合を成功に数えない
    const { r } = await run(lib, task("x13", "text", "a", "https://forbidden.test/"));
    assert.deepEqual([r.success, r.failed_stage, r.reason, r.reached_stage], [false, "goto", "http_error", "goto"]);
    assert.equal(r.http_status, 403);
    assert.equal(r.http_status_check, "failed");
    assert.equal(r.detail, "HTTP 403");
    assert.equal(r.output_sample, "");
  });
  await t(`${lib}: a 5xx page and a form task are failures too`, async () => {
    const a = (await run(lib, task("x13b", "text", "h1", "https://servererr.test/"))).r;
    assert.deepEqual([a.success, a.failed_stage, a.reason, a.http_status], [false, "goto", "http_error", 503]);
    const b = (await run(lib, task("x13c", "form", "form#f", "https://servererr.test/"))).r;
    assert.deepEqual([b.success, b.failed_stage, b.reason], [false, "goto", "http_error"]);
  });
  await t(`${lib}: 2xx and 3xx statuses pass the status check`, async () => {
    const ok = (await run(lib, task("x13d", "text", "h1"))).r;
    assert.deepEqual([ok.success, ok.http_status, ok.http_status_check], [true, 200, "passed"]);
    const nm = (await run(lib, task("x13e", "text", "h1", "https://redirected.test/"))).r;
    assert.deepEqual([nm.success, nm.http_status, nm.http_status_check], [true, 304, "passed"]);
  });
  await t(`${lib}: null status skips only the status check and is recorded (no inflation, no penalty)`, async () => {
    const ok = (await run(lib, task("x13f", "text", "h1", "https://nostatus.test/"))).r;
    assert.deepEqual([ok.success, ok.http_status, ok.http_status_check], [true, null, "skipped_no_status"]);
    // 要素の判定は通常どおり行う（ステータスが無くても空テキスト・要素なしは失敗のまま）
    const empty = (await run(lib, task("x13g", "text", "h1", "https://nostatus-empty.test/"))).r;
    assert.deepEqual([empty.success, empty.reason, empty.http_status_check], [false, "empty_result", "skipped_no_status"]);
    const none = (await run(lib, task("x13h", "text", "h9", "https://nostatus.test/"))).r;
    assert.deepEqual([none.success, none.reason, none.http_status_check], [false, "no_match", "skipped_no_status"]);
  });
  await t(`${lib}: goto failure leaves http_status_check null`, async () => {
    const { r } = await run(lib, task("x13i", "text", "h1", "https://fetcherr.test/"));
    assert.equal(r.http_status_check, null);
  });
  await t(`${lib}: connect that resolves after the stage deadline is released`, async () => {
    const calls = newCalls();
    const { r } = await run(lib, task("x16", "text", "h1"), { connect: "late" }, calls);
    assert.deepEqual([r.failed_stage, r.reason], ["connect", "timeout"]);
    assert.deepEqual(calls.closed, []);
    await sleepMs(LATE_MS + 100);
    assert.deepEqual(calls.closed, [lib === "playwright" ? "browser" : "disconnect"]);
  });
  await t(`${lib}: newPage that resolves after the stage deadline is released`, async () => {
    const calls = newCalls();
    const { r } = await run(lib, task("x17", "text", "h1"), { newPage: "late" }, calls);
    assert.deepEqual([r.failed_stage, r.reason], ["newPage", "timeout"]);
    await sleepMs(LATE_MS + 100);
    assert.ok(calls.closed.includes("page"), `late page must be closed: ${calls.closed}`);
    assert.ok(calls.closed.includes(lib === "playwright" ? "browser" : "disconnect"));
  });
  await t(`${lib}: cleanup runs (page and connection are released)`, async () => {
    const { calls } = await run(lib, task("x14", "text", "h1"));
    assert.ok(calls.closed.includes("page"));
    assert.ok(calls.closed.includes(lib === "playwright" ? "browser" : "disconnect"));
  });
  await t(`${lib}: options contain no identity-changing keys (SEC-2)`, async () => {
    const { calls } = await run(lib, task("x15", "text", "h1"));
    const all = JSON.stringify([calls.connect, calls.context, calls.goto.map((g) => g.opts)]);
    assert.doesNotMatch(all, /userAgent|extraHTTPHeaders|locale|timezone|args|stealth|headers/i);
  });
}
await t("playwright: spec-sanctioned options (CDP-2) are passed", async () => {
  const { calls } = await run("playwright", task("y1", "text", "h1"));
  assert.equal(calls.connect[0].opts.noDefaults, true);
  assert.deepEqual(calls.context[0], { acceptDownloads: false, viewport: null });
  assert.equal(calls.goto[0].opts.waitUntil, "domcontentloaded");
});
await t("playwright: newContext that resolves after the stage deadline is released", async () => {
  const calls = newCalls();
  const { r } = await run("playwright", task("y2b", "text", "h1"), { newContext: "late" }, calls);
  assert.deepEqual([r.failed_stage, r.reason], ["newContext", "timeout"]);
  await sleepMs(LATE_MS + 100);
  assert.ok(calls.closed.includes("context"), `late context must be closed: ${calls.closed}`);
  assert.ok(calls.closed.includes("browser"));
});
await t("playwright: newContext failure is attributed to newContext", async () => {
  const { r } = await run("playwright", task("y2", "text", "h1"), { newContext: true });
  assert.deepEqual([r.failed_stage, r.reached_stage], ["newContext", "connect"]);
});
await t("puppeteer: defaultViewport null by default, omitted with 'default'", async () => {
  const a = newCalls();
  await executeTask(puppeteerAdapter({ puppeteer: fakePuppeteer(a), endpoint: WS, stageTimeoutMs: 60 }), task("y3", "text", "h1"), { ...OPTS, library: "puppeteer" });
  assert.equal(a.connect[0].opts.defaultViewport, null);
  assert.equal(a.connect[0].opts.browserWSEndpoint, WS);
  const b = newCalls();
  await executeTask(puppeteerAdapter({ puppeteer: fakePuppeteer(b), endpoint: WS, stageTimeoutMs: 60, viewport: "default" }), task("y3", "text", "h1"), { ...OPTS, library: "puppeteer" });
  assert.equal("defaultViewport" in b.connect[0].opts, false);
});
await t("puppeteer: browserURL mode passes the origin", async () => {
  const c = newCalls();
  await executeTask(puppeteerAdapter({ puppeteer: fakePuppeteer(c), endpoint: "http://127.0.0.1:9", stageTimeoutMs: 60, connectMode: "url" }), task("y4", "text", "h1"), { ...OPTS, library: "puppeteer" });
  assert.equal(c.connect[0].opts.browserURL, "http://127.0.0.1:9");
});
await t("same judgment for both libraries (no per-library or per-target logic)", async () => {
  for (const [tk] of [[task("z1", "text", "h1")], [task("z2", "form", "form#f")], [task("z3", "text", "h9")], [task("z4", "text", "a", "https://forbidden.test/")], [task("z5", "text", "h1", "https://nostatus.test/")]]) {
    const a = (await run("playwright", tk)).r;
    const b = (await run("puppeteer", tk)).r;
    for (const k of ["success", "failed_stage", "reason", "output_sample", "match_count", "method", "http_status", "http_status_check"]) assert.equal(a[k], b[k], `${tk.id}.${k}`);
  }
});
await t("runAll: tasks after the total deadline are blocked, not run", async () => {
  const calls = newCalls();
  const tasks = [task("b1", "text", "h1", "https://hang.test/"), task("b2", "text", "h1")];
  const results = await runAll(() => adapterFor("playwright", calls), tasks, { stageTimeoutMs: 40, settleMs: 0, totalTimeoutMs: 20, library: "playwright", target: "t" });
  assert.deepEqual(results.map((r) => [r.id, r.reason]), [["b1", "timeout"], ["b2", "blocked"]]);
  assert.equal(results[1].failed_stage, null);
  assert.equal(calls.connect.length, 1);
});

// ---- 定義・検証 ----
const realTasks = validateTasks(JSON.parse(readFileSync(TASKS_PATH, "utf8")));
await t("tasks.json: 28 tasks, 22 in the PoC set, excluded ids all exist", async () => {
  assert.equal(realTasks.length, 28);
  assert.equal(realTasks.filter((x) => inPoc22(x.id)).length, 22);
  for (const id of POC22_EXCLUDED) assert.ok(realTasks.some((x) => x.id === id), id);
});
await t("validateTasks rejects bad input", async () => {
  assert.throws(() => validateTasks([]));
  assert.throws(() => validateTasks([{ id: "a", cat: "x", url: "http://x", selector: "h1", kind: "text" }]));
  assert.throws(() => validateTasks([{ id: "a", cat: "x", url: "https://x", selector: "", kind: "text" }]));
  assert.throws(() => validateTasks([{ id: "a", cat: "x", url: "https://x", selector: "h1", kind: "bogus" }]));
  const d = { id: "a", cat: "x", url: "https://x", selector: "h1", kind: "text" };
  assert.throws(() => validateTasks([d, d]));
});
await t("parseLoopbackEndpoint accepts loopback only", async () => {
  assert.equal(parseLoopbackEndpoint("http://127.0.0.1:9333").port, "9333");
  assert.equal(parseLoopbackEndpoint("ws://localhost:1/x").hostname, "localhost");
  for (const bad of ["http://example.com:80", "https://127.0.0.1:1", "http://127.0.0.1", "http://127.0.0.1:1@evil.example:80", "file:///x", "nope", "http://user:pw@127.0.0.1:9"]) {
    assert.throws(() => parseLoopbackEndpoint(bad), undefined, bad);
  }
});

// ---- 集計 ----
function synthetic(library, target, failIds) {
  return realTasks.map((x) => ({ type: "result", library, target, id: x.id, success: !failIds.includes(x.id), failed_stage: failIds.includes(x.id) ? "selector" : null, reason: failIds.includes(x.id) ? "no_match" : null }));
}
await t("ratePercent rounds to one decimal with integers", async () => {
  assert.deepEqual([ratePercent(17, 28), ratePercent(21, 28), ratePercent(1, 3), ratePercent(2, 3), ratePercent(0, 0), ratePercent(28, 28)], ["60.7", "75.0", "33.3", "66.7", "n/a", "100.0"]);
});
await t("summarize: 28 and 22 denominators per library and target; a6/c1 stay in the denominator", async () => {
  const lines = [
    ...synthetic("playwright", "fandhe-browser", ["a6", "c1", "c4", "d5"]),
    ...synthetic("playwright", "chromium", ["c1", "c4"]),
    ...synthetic("puppeteer", "fandhe-browser", realTasks.map((x) => x.id).filter((id) => !["a1", "a2", "b1"].includes(id))),
  ];
  const g = summarize(lines, realTasks);
  assert.equal(g.length, 3);
  const pf = g.find((x) => x.library === "playwright" && x.target === "fandhe-browser");
  assert.deepEqual(pf.sets, { 28: { success: 24, total: 28, rate: "85.7" }, 22: { success: 20, total: 22, rate: "90.9" } });
  assert.deepEqual(pf.failed_ids, ["a6", "c1", "c4", "d5"]);
  const pc = g.find((x) => x.library === "playwright" && x.target === "chromium");
  assert.equal(pc.sets[28].success, 26);
  assert.deepEqual(pf.of_chromium_success, { success: 24, total: 26, rate: "92.3" });
  const pp = g.find((x) => x.library === "puppeteer");
  assert.deepEqual([pp.sets[28].success, pp.sets[22].success], [3, 3]);
  assert.equal(pp.of_chromium_success, undefined);
  const text = formatSummary(g);
  assert.match(text, /^library=playwright target=fandhe-browser set=28 success=24\/28 rate=85\.7%$/m);
  assert.match(text, /^library=playwright target=fandhe-browser set=22 success=20\/22 rate=90\.9%$/m);
  assert.match(text, /^library=puppeteer target=fandhe-browser failed_stage selector=25$/m);
});
await t("summarize: successes without a status are listed separately; http_error counts as a failed goto", async () => {
  const rows = synthetic("playwright", "fandhe-browser", ["a6"]).map((r) => {
    if (r.id === "a6") return { ...r, failed_stage: "goto", reason: "http_error", http_status: 403, http_status_check: "failed" };
    if (r.id === "a1") return { ...r, http_status: null, http_status_check: "skipped_no_status" };
    return { ...r, http_status: 200, http_status_check: "passed" };
  });
  const [g] = summarize(rows, realTasks);
  assert.deepEqual(g.status_unchecked_success_ids, ["a1"]);
  assert.deepEqual(g.failed_stage, { goto: 1 });
  assert.deepEqual(g.reason, { http_error: 1 });
  assert.match(formatSummary([g]), /^library=playwright target=fandhe-browser status_unchecked_success a1 /m);
});
await t("summarize: incomplete, duplicate or unknown ids are rejected (fail-closed)", async () => {
  const full = synthetic("playwright", "chromium", []);
  assert.throws(() => summarize(full.slice(1), realTasks), /missing: a1/);
  assert.throws(() => summarize([...full, full[0]], realTasks), /duplicate/);
  assert.throws(() => summarize([...full.slice(1), { ...full[0], id: "zz" }], realTasks), /unknown: zz/);
  assert.throws(() => summarize([], realTasks), /no result lines/);
  assert.throws(() => summarize([{ ...full[0], library: "selenium" }], realTasks), /unknown library/);
  assert.throws(() => summarize([{ ...full[0], success: "yes" }], realTasks), /non-boolean/);
});
await t("verifyInstall: single package for playwright, transitive allowed for puppeteer", async () => {
  const mk = (extra = {}) => ({ packages: { "": {}, "node_modules/pw": { version: "1.0.0", integrity: "sha512-A" }, ...extra } });
  assert.deepEqual(verifyInstall(mk(), { name: "pw", version: "1.0.0", integrity: "sha512-A" }), ["pw@1.0.0"]);
  const withDep = mk({ "node_modules/ws": { version: "8.0.0" } });
  assert.throws(() => verifyInstall(withDep, { name: "pw", version: "1.0.0", integrity: "sha512-A" }), /unexpected packages/);
  assert.deepEqual(verifyInstall(withDep, { name: "pw", version: "1.0.0", integrity: "sha512-A", allowTransitive: true }), ["pw@1.0.0", "ws@8.0.0"]);
  assert.throws(() => verifyInstall(mk(), { name: "pw", version: "1.0.1", integrity: "sha512-A" }), /version mismatch/);
  assert.throws(() => verifyInstall(mk(), { name: "pw", version: "1.0.0", integrity: "sha512-B" }), /integrity mismatch/);
  assert.throws(() => verifyInstall(mk(), { name: "other", version: "1.0.0", integrity: "sha512-A", allowTransitive: true }), /not installed/);
});

// ---- 偽 CDP サーバー（/json/version の解決）と CLI の配線 ----
async function startFake(...args) {
  const child = spawn(process.execPath, [FAKE_SERVER, ...args], { stdio: ["ignore", "pipe", "ignore"] });
  const port = await new Promise((resolve, reject) => {
    let buf = "";
    child.stdout.on("data", (d) => {
      buf += d;
      if (buf.includes("\n")) resolve(buf.trim());
    });
    child.on("exit", () => reject(new Error("fake server exited")));
    setTimeout(() => reject(new Error("fake server start timeout")), 15000);
  });
  return { child, port };
}
const fakes = [];
process.on("exit", () => {
  for (const f of fakes) f.kill();
  rmSync(work, { recursive: true, force: true });
});

const good = await startFake();
fakes.push(good.child);
const bad = await startFake("--ws-host-mismatch");
fakes.push(bad.child);

await t("resolveWsEndpoint: takes webSocketDebuggerUrl from /json/version on the same host and port", async () => {
  const ws = await resolveWsEndpoint(`http://127.0.0.1:${good.port}`, 3000);
  assert.match(ws, new RegExp(`^ws://127\\.0\\.0\\.1:${good.port}/`));
});
await t("resolveWsEndpoint: rejects a webSocketDebuggerUrl that points elsewhere", async () => {
  await assert.rejects(resolveWsEndpoint(`http://127.0.0.1:${bad.port}`, 3000), /does not match endpoint/);
});

// 偽モジュール（CJS）: node_modules/<pkg>/index.js。ワールドは環境変数 FAKE_WORLD のファイルから読む
function writeFakeModule(root, pkg, body) {
  const dir = join(root, "node_modules", pkg);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: pkg, version: "0.0.0-fake", main: "index.js" }));
  writeFileSync(join(dir, "index.js"), body);
}
const FAKE_COMMON = `
const fs = require("node:fs");
const world = () => JSON.parse(fs.readFileSync(process.env.FAKE_WORLD, "utf8"));
function page() {
  let url = "";
  return {
    async goto(u) { url = u; const w = world()[u]; if (!w) throw new Error("net::ERR_NAME_NOT_RESOLVED"); return { status: () => w.status }; },
    async $(sel) {
      const spec = (world()[url] || { sel: {} }).sel[sel];
      if (!spec) return null;
      return { async evaluate(fn) { return fn({ textContent: spec.text ?? null, querySelectorAll: () => (spec.fields || []).map((f) => ({ getAttribute: (k) => (k === "name" ? f.name ?? null : null) })) }); } };
    },
    async close() {},
  };
}
`;
const modRoot = { pw: join(work, "pw"), pp: join(work, "pp"), ppbad: join(work, "ppbad") };
writeFakeModule(modRoot.pw, "playwright-core", `${FAKE_COMMON}
exports.chromium = { async connectOverCDP(ep, o) {
  if (o.noDefaults !== true) throw new Error("noDefaults missing");
  return { version: () => "FakeChromium/9.9", async newContext() { return { async newPage() { return page(); }, async close() {} }; }, async close() {} };
} };`);
const PP = (port) => `${FAKE_COMMON}
exports.connect = async (o) => {
  if (!/^ws:\\/\\/127\\.0\\.0\\.1:${port}\\//.test(o.browserWSEndpoint || "")) throw new Error("unexpected endpoint: " + o.browserWSEndpoint);
  if (o.defaultViewport !== null) throw new Error("defaultViewport must be null");
  return { async version() { return "FakeChromium/9.9"; }, async newPage() { return page(); }, async disconnect() {} };
};`;
writeFakeModule(modRoot.pp, "puppeteer-core", PP(good.port));

const miniTasks = [
  { id: "m1", cat: "static", url: "https://ok.test/", selector: "h1", kind: "text" },
  { id: "m2", cat: "static", url: "https://ok.test/", selector: "h9", kind: "texts" },
  { id: "c4", cat: "lazy", url: "https://gone.test/", selector: "h1", kind: "text" },
  { id: "m4", cat: "form", url: "https://ok.test/", selector: "form#f", kind: "form" },
];
const tasksFile = join(work, "mini-tasks.json");
const worldFile = join(work, "world.json");
writeFileSync(tasksFile, JSON.stringify(miniTasks));
writeFileSync(worldFile, JSON.stringify({ "https://ok.test/": WORLD["https://ok.test/"] }));
const swap = (args, env = {}) => spawnSync(process.execPath, [join(here, "run_swap.mjs"), ...args], { encoding: "utf8", env: { ...process.env, FAKE_WORLD: worldFile, ...env } });
const base = (lib, target, out, mod) => ["--library", lib, "--target", target, "--endpoint", `http://127.0.0.1:${good.port}`, "--module-dir", mod, "--out", out, "--tasks", tasksFile, "--settle-ms", "0", "--stage-timeout", "5"];

await t("run_swap: writes meta + results for both libraries and both targets, summarize counts them separately", async () => {
  const outs = [];
  for (const [lib, mod] of [["playwright", modRoot.pw], ["puppeteer", modRoot.pp]]) {
    for (const target of ["fandhe-browser", "chromium"]) {
      const out = join(work, `${lib}-${target}.jsonl`);
      const r = swap([...base(lib, target, out, mod), "--context-json", '{"bin_sha256":"ab12","page_modules":true}']);
      assert.equal(r.status, 0, r.stderr);
      assert.match(r.stdout, /^success=2\/4$/m);
      outs.push(out);
    }
  }
  const lines = readFileSync(outs[0], "utf8").trim().split("\n").map((l) => JSON.parse(l));
  assert.equal(lines.length, 5);
  assert.equal(lines[0].type, "meta");
  assert.deepEqual([lines[0].library, lines[0].target, lines[0].library_version, lines[0].browser, lines[0].task_count], ["playwright", "fandhe-browser", "0.0.0-fake", "FakeChromium/9.9", 4]);
  assert.match(lines[0].tasks_sha256, /^[0-9a-f]{64}$/);
  assert.deepEqual(lines[0].context, { bin_sha256: "ab12", page_modules: true });
  const byId = Object.fromEntries(lines.slice(1).map((l) => [l.id, l]));
  assert.equal(byId.m1.success, true);
  assert.deepEqual([byId.m2.failed_stage, byId.m2.reason], ["selector", "no_match"]);
  assert.deepEqual([byId.c4.failed_stage, byId.c4.reason], ["goto", "fetch_error"]);
  assert.equal(byId.m4.success, true);
  const s = spawnSync(process.execPath, [join(here, "summarize.mjs"), "--tasks", tasksFile, ...outs], { encoding: "utf8" });
  assert.equal(s.status, 0, s.stderr);
  assert.match(s.stdout, /^library=puppeteer target=chromium set=28 success=2\/4 rate=50\.0%$/m);
  assert.match(s.stdout, /^library=puppeteer target=chromium set=22 success=2\/3 rate=66\.7%$/m);
  assert.match(s.stdout, /does not judge/);
});
await t("run_swap: refuses to overwrite --out", async () => {
  const out = join(work, "playwright-chromium.jsonl");
  const r = swap(base("playwright", "chromium", out, modRoot.pw));
  assert.equal(r.status, 2);
  assert.match(r.stderr, /already exists/);
});
await t("run_swap: puppeteer connect fails at the connect stage when discovery points elsewhere", async () => {
  writeFakeModule(modRoot.ppbad, "puppeteer-core", PP(bad.port));
  const out = join(work, "pp-bad.jsonl");
  const a = base("puppeteer", "fandhe-browser", out, modRoot.ppbad);
  a[a.indexOf("--endpoint") + 1] = `http://127.0.0.1:${bad.port}`;
  const r = swap(a);
  assert.equal(r.status, 0, r.stderr);
  const rows = readFileSync(out, "utf8").trim().split("\n").map((l) => JSON.parse(l)).filter((l) => l.type === "result");
  assert.ok(rows.every((x) => x.failed_stage === "connect" && x.reason === "cdp_error"));
  assert.match(rows[0].detail, /does not match endpoint/);
});
await t("run_swap: input validation exits 2", async () => {
  const out = join(work, "never.jsonl");
  const ok = base("playwright", "chromium", out, modRoot.pw);
  const withArg = (k, v) => {
    const a = [...ok];
    const i = a.indexOf(k);
    a[i + 1] = v;
    return a;
  };
  for (const [name, args] of [
    ["non-loopback endpoint", withArg("--endpoint", "http://example.com:9222")],
    ["https endpoint", withArg("--endpoint", "https://127.0.0.1:9222")],
    ["relative out", withArg("--out", "rel.jsonl")],
    ["bad library", withArg("--library", "selenium")],
    ["bad target label", withArg("--target", "Bad Label")],
    ["missing module", withArg("--module-dir", join(work, "nope"))],
    ["bad stage timeout", withArg("--stage-timeout", "0")],
    ["unknown argument", [...ok, "--user-agent", "x"]],
  ]) {
    const r = swap(args);
    assert.equal(r.status, 2, `${name}: ${r.stderr}`);
  }
  assert.equal(swap(["--validate-only", "--library", "playwright", "--target", "x", "--endpoint", "http://127.0.0.1:1", "--tasks", TASKS_PATH]).stdout.trim(), "ok: 28 tasks validated");
  assert.equal(swap(["--only", "a1,a1", "--validate-only", "--library", "playwright", "--target", "x", "--endpoint", "http://127.0.0.1:1"]).status, 2);
});
await t("summarize CLI: incomplete results exit 2", async () => {
  const p = join(work, "partial.jsonl");
  writeFileSync(p, JSON.stringify({ type: "result", library: "playwright", target: "x", id: "a1", success: true }) + "\n");
  const s = spawnSync(process.execPath, [join(here, "summarize.mjs"), p], { encoding: "utf8" });
  assert.equal(s.status, 2);
  assert.match(s.stderr, /exactly one meta line, found 0/);
});

// ---- 集計入力の検証（古い測定・別のタスク定義の結果を受理しない。fail-closed）----
const jl = (path) => readFileSync(path, "utf8").trim().split("\n").map((l) => JSON.parse(l));
const writeJl = (path, rows) => writeFileSync(path, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
const sumCli = (args) => spawnSync(process.execPath, [join(here, "summarize.mjs"), ...args], { encoding: "utf8" });
await t("verifyResultFile: meta line count, tasks_sha256, task_count, library and target are all checked", async () => {
  const sha = "a".repeat(64);
  const rows = synthetic("playwright", "chromium", []);
  const meta = { type: "meta", schema_version: 1, library: "playwright", target: "chromium", tasks_sha256: sha, task_count: 28 };
  const ok = (lines) => verifyResultFile(lines, { tasks: realTasks, tasksSha256: sha, label: "f.jsonl" });
  assert.equal(ok([meta, ...rows]).results.length, 28);
  assert.throws(() => ok(rows), /exactly one meta line, found 0/);
  assert.throws(() => ok([meta, meta, ...rows]), /exactly one meta line, found 2/);
  assert.throws(() => ok([...rows, meta]), /meta line must be the first line/);
  assert.throws(() => ok([{ ...meta, tasks_sha256: "b".repeat(64) }, ...rows]), /tasks_sha256 mismatch/);
  assert.throws(() => ok([{ ...meta, tasks_sha256: undefined }, ...rows]), /tasks_sha256 mismatch/);
  assert.throws(() => ok([{ ...meta, task_count: 4 }, ...rows]), /task_count mismatch/);
  assert.throws(() => ok([meta, ...rows.slice(1)]), /does not match task_count/);
  assert.throws(() => ok([{ ...meta, target: "other" }, ...rows]), /different from the meta line/);
  assert.throws(() => ok([{ ...meta, library: "puppeteer" }, ...rows]), /different from the meta line/);
  assert.throws(() => ok([{ ...meta, schema_version: 2 }, ...rows]), /schema_version/);
  assert.throws(() => ok([meta, { type: "note" }, ...rows]), /unexpected line type/);
});
await t("verifyMetaConsistency: library_version, options and duplicate targets are rejected", async () => {
  const m = (target, extra = {}) => ({ library: "playwright", target, library_version: "1.0.0", options: { settle_ms: 0 }, ...extra });
  verifyMetaConsistency([m("chromium"), m("fandhe-browser"), { ...m("chromium"), library: "puppeteer" }]);
  assert.throws(() => verifyMetaConsistency([m("chromium"), m("chromium")]), /more than one results file/);
  assert.throws(() => verifyMetaConsistency([m("chromium"), m("fandhe-browser", { library_version: "2.0.0" })]), /library_version differs/);
  assert.throws(() => verifyMetaConsistency([m("chromium"), m("fandhe-browser", { options: { settle_ms: 5 } })]), /options differ/);
});
await t("summarize CLI: results measured with a different tasks file are rejected even if the ids are the same", async () => {
  const outs = ["playwright-fandhe-browser", "playwright-chromium"].map((n) => join(work, `${n}.jsonl`));
  assert.equal(sumCli(["--tasks", tasksFile, ...outs]).status, 0);
  // id は同じまま selector だけ変えたタスク定義（旧定義の結果を新定義の結果として受理しない）
  const changed = join(work, "mini-tasks-changed.json");
  writeFileSync(changed, JSON.stringify(miniTasks.map((x) => (x.id === "m1" ? { ...x, selector: "h2" } : x))));
  const s = sumCli(["--tasks", changed, ...outs]);
  assert.equal(s.status, 2);
  assert.match(s.stderr, /tasks_sha256 mismatch/);
});
await t("summarize CLI: a file without a meta line, or with a mismatched meta, exits 2", async () => {
  const src = join(work, "playwright-chromium.jsonl");
  const rows = jl(src);
  const other = join(work, "playwright-fandhe-browser.jsonl");
  const noMeta = join(work, "nometa.jsonl");
  writeJl(noMeta, rows.slice(1));
  let s = sumCli(["--tasks", tasksFile, noMeta, other]);
  assert.equal(s.status, 2);
  assert.match(s.stderr, /nometa\.jsonl: expected exactly one meta line, found 0/);
  const wrongTarget = join(work, "wrong-target.jsonl");
  writeJl(wrongTarget, [{ ...rows[0], target: "fandhe-browser" }, ...rows.slice(1)]);
  s = sumCli(["--tasks", tasksFile, wrongTarget, other]);
  assert.equal(s.status, 2);
  assert.match(s.stderr, /different from the meta line/);
  const staleOptions = join(work, "stale-options.jsonl");
  writeJl(staleOptions, [{ ...rows[0], options: { ...rows[0].options, settle_ms: 999 } }, ...rows.slice(1)]);
  s = sumCli(["--tasks", tasksFile, staleOptions, other]);
  assert.equal(s.status, 2);
  assert.match(s.stderr, /options differ between targets/);
  const dup = join(work, "dup-target.jsonl");
  copyFileSync(src, dup);
  s = sumCli(["--tasks", tasksFile, src, dup]);
  assert.equal(s.status, 2);
  assert.match(s.stderr, /more than one results file/);
});

// ---- run.sh の集計対象（前回の測定が残る出力ディレクトリでも今回生成した分だけを集計する）----
// npm は偽の実行ファイルで差し替え、Chromium は偽 CDP サーバーを起動する薄いシェルで代替する。実サイト・実 npm には出ない。
await t("run.sh: summarizes only the files generated in this run, not stale jsonl left in --out-dir", async () => {
  const bin = join(work, "fakebin");
  mkdirSync(bin, { recursive: true });
  const fakeNpm = join(bin, "npm");
  writeFileSync(fakeNpm, `#!/usr/bin/env bash
pkg="\${@: -1}"; name="\${pkg%@*}"; ver="\${pkg##*@}"
mkdir -p "node_modules/$name"
cat >"node_modules/$name/package.json" <<EOF2
{"name":"$name","version":"$ver","main":"index.js"}
EOF2
cat >"node_modules/$name/index.js" <<'EOF2'
exports.chromium = { async connectOverCDP() { throw new Error("fake: no browser"); } };
EOF2
cat >node_modules/.package-lock.json <<EOF2
{"packages":{"node_modules/$name":{"version":"$ver","integrity":"$PLAYWRIGHT_INTEGRITY"}}}
EOF2
`);
  chmodSync(fakeNpm, 0o755);
  const fakeChromium = join(bin, "fake-chromium");
  writeFileSync(fakeChromium, `#!/bin/sh
for a in "$@"; do case "$a" in --remote-debugging-port=*) port="\${a#*=}" ;; esac; done
exec "${process.execPath}" "${FAKE_SERVER}" --port "$port"
`);
  chmodSync(fakeChromium, 0o755);
  const outDir = join(work, "runsh-out");
  mkdirSync(outDir, { recursive: true });
  // 前回の測定が残っている状況（Puppeteer × Chromium のファイル。中身は今回のタスク定義と無関係）
  writeJl(join(outDir, "puppeteer-chromium.jsonl"), [{ type: "meta", library: "puppeteer", target: "chromium", tasks_sha256: "0".repeat(64) }, ...synthetic("puppeteer", "chromium", [])]);
  const r = spawnSync("bash", [join(here, "run.sh"), "--out-dir", outDir, "--libraries", "playwright", "--targets", "chromium", "--chromium", fakeChromium, "--settle-ms", "0", "--stage-timeout", "2"], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, PLAYWRIGHT_INTEGRITY: "sha512-AAAA" },
    timeout: 120000,
  });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /^library=playwright target=chromium set=28 success=0\/28 rate=0\.0%$/m);
  assert.doesNotMatch(r.stdout, /library=puppeteer/);
  const summary = JSON.parse(readFileSync(join(outDir, "summary.json"), "utf8"));
  assert.deepEqual(summary.groups.map((g) => `${g.library}/${g.target}`), ["playwright/chromium"]);
  assert.ok(existsSync(join(outDir, "playwright-chromium.jsonl")));
});

console.log(cases > 0 && process.exitCode !== 1 ? `all ${cases} cases passed` : "FAILED");
// 偽サーバーの子プロセスがイベントループを保持するため明示的に終了する（exit ハンドラが後始末する）
process.exit(process.exitCode ?? 0);

// stages.mjs のオフライン自己テスト（TASK-44.1・#476、ビヘイビア CDP-2）。
// 偽 chromium で段階別結果を具体値検証する。Playwright・ネットワーク不要。
import assert from "node:assert/strict";
import { FIXTURE_EXPECT, runStages } from "./stages.mjs";

function fake({ failAt, hangAt, lateRejectAt, text = FIXTURE_EXPECT.text, attr = FIXTURE_EXPECT.attribute.value, count = 1 } = {}) {
  const calls = { connectArgs: null, contextArgs: null, gotoArgs: null, attrArgs: null };
  const maybe = async (name, v) => {
    if (hangAt === name) await new Promise(() => {});
    if (lateRejectAt === name) {
      await new Promise((r) => setTimeout(r, 120));
      throw new Error(`${name} late boom`);
    }
    if (failAt === name) {
      const e = new Error(`${name} boom`);
      e.name = "ProtocolError";
      throw e;
    }
    return v;
  };
  const locator = {
    count: () => maybe("selector", count),
    first: () => locator,
    textContent: () => maybe("selector", text),
    getAttribute: (n, o) => {
      calls.attrArgs = [n, o];
      return maybe("selector", attr);
    },
  };
  const page = {
    goto: (u, o) => {
      calls.gotoArgs = [u, o];
      return maybe("goto", null);
    },
    locator: (sel) => {
      calls.selector = sel;
      return locator;
    },
  };
  const context = { newPage: () => maybe("newPage", page) };
  const browser = {
    newContext: (o) => {
      calls.contextArgs = o;
      return maybe("newContext", context);
    },
  };
  return {
    calls,
    chromium: {
      connectOverCDP: (url, o) => {
        calls.connectArgs = [url, o];
        return maybe("connect", browser);
      },
    },
  };
}

const statuses = (r) => r.stages.map((s) => `${s.name}:${s.status}`);
const run = (f, ms = 200) =>
  runStages({
    chromium: f.chromium,
    endpoint: "http://127.0.0.1:1",
    fixtureUrl: "http://127.0.0.1:2/index.html",
    stageTimeoutMs: ms,
  });
const ALL = ["connect", "newContext", "newPage", "goto", "selector"];

{
  const f = fake();
  const r = await run(f);
  assert.equal(r.ok, true);
  assert.equal(r.step, "selector");
  assert.deepEqual(statuses(r), ALL.map((n) => `${n}:ok`));
  assert.deepEqual(f.calls.gotoArgs, ["http://127.0.0.1:2/index.html", { timeout: 200 }]);
  assert.equal(f.calls.selector, "#target");
  assert.deepEqual(f.calls.attrArgs, ["data-role", { timeout: 200 }]);
  // 既知の失敗回避オプションが渡ること（harness/playwright-trace/README.md）。
  assert.deepEqual(f.calls.connectArgs, ["http://127.0.0.1:1", { noDefaults: true, timeout: 200 }]);
  assert.deepEqual(f.calls.contextArgs, { acceptDownloads: false, viewport: null });
}
for (const at of ALL) {
  const r = await run(fake({ failAt: at }));
  const i = ALL.indexOf(at);
  const expected = ALL.map((n, k) => `${n}:${k < i ? "ok" : k === i ? "failed" : "not_reached"}`);
  assert.equal(r.ok, false);
  assert.equal(r.step, at);
  assert.deepEqual(statuses(r), expected);
  assert.deepEqual(r.error, { name: "ProtocolError", message: `${at} boom` });
}
{
  const r = await run(fake({ hangAt: "goto" }), 50);
  assert.equal(r.step, "goto");
  assert.equal(r.error.name, "StageTimeout");
  assert.equal(r.error.message, "stage timed out after 50 ms");
}
{
  // 期限後に遅れて reject する段階が unhandledRejection を起こさないこと。
  let unhandled = 0;
  process.on("unhandledRejection", () => {
    unhandled++;
  });
  const r = await run(fake({ lateRejectAt: "newPage" }), 50);
  await new Promise((res) => setTimeout(res, 200));
  assert.equal(unhandled, 0);
  assert.deepEqual(statuses(r), ["connect:ok", "newContext:ok", "newPage:failed", "goto:not_reached", "selector:not_reached"]);
}
{
  // セレクタ段階: テキスト不一致・属性不一致・要素なしは具体値のエラーで失敗する。
  let r = await run(fake({ text: "  Bye " }));
  assert.equal(r.step, "selector");
  assert.deepEqual(r.error, { name: "SelectorMismatch", message: 'expected text "Hello fixture" got "Bye"' });
  r = await run(fake({ attr: "other" }));
  assert.deepEqual(r.error, { name: "SelectorMismatch", message: 'expected attribute data-role "greeting" got "other"' });
  r = await run(fake({ attr: null }));
  assert.deepEqual(r.error, { name: "SelectorMismatch", message: 'expected attribute data-role "greeting" got "null"' });
  r = await run(fake({ count: 0 }));
  assert.deepEqual(r.error, { name: "SelectorNotFound", message: "no element matches #target" });
  // 前後の空白は無視する（trim）。
  r = await run(fake({ text: "\n Hello fixture \n" }));
  assert.equal(r.ok, true);
}
{
  const seen = [];
  await runStages({
    chromium: fake().chromium,
    endpoint: "http://127.0.0.1:1",
    fixtureUrl: "http://127.0.0.1:2/index.html",
    stageTimeoutMs: 200,
    onStage: (n) => seen.push(n),
  });
  assert.deepEqual(seen, ALL);
}
console.log("playwright-connect self-test: all passed");
process.exit(0);

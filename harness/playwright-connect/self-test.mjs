// stages.mjs のオフライン自己テスト（TASK-44.1・#476、ビヘイビア CDP-2）。
// 偽 chromium で段階別結果を具体値検証する。Playwright・ネットワーク不要。
import assert from "node:assert/strict";
import { runStages } from "./stages.mjs";

function fake({ failAt, hangAt, lateRejectAt } = {}) {
  const calls = { connectArgs: null, contextArgs: null };
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
  const context = { newPage: () => maybe("newPage", {}) };
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
  runStages({ chromium: f.chromium, endpoint: "http://127.0.0.1:1", stageTimeoutMs: ms });

{
  const f = fake();
  const r = await run(f);
  assert.equal(r.ok, true);
  assert.equal(r.step, "newPage");
  assert.deepEqual(statuses(r), ["connect:ok", "newContext:ok", "newPage:ok"]);
  // 既知の失敗回避オプションが渡ること（harness/playwright-trace/README.md）。
  assert.deepEqual(f.calls.connectArgs, ["http://127.0.0.1:1", { noDefaults: true, timeout: 200 }]);
  assert.deepEqual(f.calls.contextArgs, { acceptDownloads: false, viewport: null });
}
for (const [at, expected] of [
  ["connect", ["connect:failed", "newContext:not_reached", "newPage:not_reached"]],
  ["newContext", ["connect:ok", "newContext:failed", "newPage:not_reached"]],
  ["newPage", ["connect:ok", "newContext:ok", "newPage:failed"]],
]) {
  const r = await run(fake({ failAt: at }));
  assert.equal(r.ok, false);
  assert.equal(r.step, at);
  assert.deepEqual(statuses(r), expected);
  assert.deepEqual(r.error, { name: "ProtocolError", message: `${at} boom` });
}
{
  const r = await run(fake({ hangAt: "newContext" }), 50);
  assert.equal(r.step, "newContext");
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
  assert.deepEqual(statuses(r), ["connect:ok", "newContext:ok", "newPage:failed"]);
}
{
  const seen = [];
  await runStages({
    chromium: fake().chromium,
    endpoint: "http://127.0.0.1:1",
    stageTimeoutMs: 200,
    onStage: (n) => seen.push(n),
  });
  assert.deepEqual(seen, ["connect", "newContext", "newPage"]);
}
console.log("playwright-connect self-test: all passed");
process.exit(0);

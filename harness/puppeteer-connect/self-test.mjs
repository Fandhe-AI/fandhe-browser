// stages.mjs のオフライン自己テスト（TASK-45.2・#481、ビヘイビア CDP-3）。
// 偽 puppeteer で段階別結果を具体値検証する。Puppeteer・ネットワーク不要。
import assert from "node:assert/strict";
import { runStages } from "./stages.mjs";

function fake({ failAt, nullBody, hangAt, lateRejectAt, hangDisconnect, failDisconnect } = {}) {
  const calls = { disconnect: 0 };
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
  const page = {
    goto: () => maybe("goto"),
    $: () => maybe("selector", nullBody ? null : {}),
  };
  const browser = {
    newPage: () => maybe("newPage", page),
    disconnect: async () => {
      calls.disconnect++;
      if (hangDisconnect) await new Promise(() => {});
      if (failDisconnect) throw new Error("disconnect boom");
    },
  };
  return { calls, puppeteer: { connect: () => maybe("connect", browser) } };
}

const statuses = (r) => r.stages.map((s) => `${s.name}:${s.status}`);
const run = (f, ms = 200) =>
  runStages({ puppeteer: f.puppeteer, endpoint: "ws://127.0.0.1:1/x", stageTimeoutMs: ms });

{
  const f = fake();
  const r = await run(f);
  assert.equal(r.ok, true);
  assert.equal(r.step, "disconnect");
  assert.deepEqual(statuses(r), ["connect:ok", "newPage:ok", "goto:ok", "selector:ok", "disconnect:ok"]);
  assert.equal(f.calls.disconnect, 1);
}
for (const [at, expected] of [
  ["connect", ["connect:failed", "newPage:not_reached", "goto:not_reached", "selector:not_reached", "disconnect:not_reached"]],
  ["newPage", ["connect:ok", "newPage:failed", "goto:not_reached", "selector:not_reached", "disconnect:not_reached"]],
  ["goto", ["connect:ok", "newPage:ok", "goto:failed", "selector:not_reached", "disconnect:not_reached"]],
  ["selector", ["connect:ok", "newPage:ok", "goto:ok", "selector:failed", "disconnect:not_reached"]],
]) {
  const f = fake({ failAt: at });
  const r = await run(f);
  assert.equal(r.ok, false);
  assert.equal(r.step, at);
  assert.deepEqual(statuses(r), expected);
  assert.deepEqual(r.error, { name: "ProtocolError", message: `${at} boom` });
  // connect 失敗時は browser が無いので disconnect は呼ばれない。
  assert.equal(f.calls.disconnect, at === "connect" ? 0 : 1);
}
{
  const r = await run(fake({ nullBody: true }));
  assert.equal(r.step, "selector");
  assert.equal(r.error.name, "SelectorNotFound");
}
{
  const f = fake({ hangAt: "goto" });
  const r = await run(f, 50);
  assert.equal(r.step, "goto");
  assert.equal(r.error.name, "StageTimeout");
  assert.equal(r.error.message, "stage timed out after 50 ms");
  assert.equal(f.calls.disconnect, 1);
}
{
  // ガード（#691 Bugbot。Promise.race が敗者を購読するため修正前も通る防御的テスト）: 期限後に遅れて reject する段階が unhandledRejection を起こさず、
  // stages（StageTimeout の到達結果）が保持されること。
  let unhandled = 0;
  process.on("unhandledRejection", () => {
    unhandled++;
  });
  const f = fake({ lateRejectAt: "goto" });
  const r = await run(f, 50);
  await new Promise((res) => setTimeout(res, 200));
  assert.equal(unhandled, 0);
  assert.equal(r.step, "goto");
  assert.equal(r.error.name, "StageTimeout");
  assert.deepEqual(statuses(r), ["connect:ok", "newPage:ok", "goto:failed", "selector:not_reached", "disconnect:not_reached"]);
}
{
  // 切断が応答しなくても stages を返す（切断にも期限。#691 codex）。
  const f = fake({ failAt: "goto", hangDisconnect: true });
  const r = await run(f, 50);
  assert.equal(r.step, "goto");
  assert.deepEqual(statuses(r), ["connect:ok", "newPage:ok", "goto:failed", "selector:not_reached", "disconnect:not_reached"]);
  assert.equal(f.calls.disconnect, 1);
}
{
  // 全段階成功でも切断の失敗・時間切れは成功扱いにしない（#691 codex）。
  const r = await run(fake({ failDisconnect: true }));
  assert.equal(r.ok, false);
  assert.equal(r.step, "disconnect");
  assert.deepEqual(r.error, { name: "Error", message: "disconnect boom" });
  const h = await run(fake({ hangDisconnect: true }), 50);
  assert.equal(h.ok, false);
  assert.equal(h.step, "disconnect");
  assert.equal(h.error.name, "StageTimeout");
}
console.log("puppeteer-connect self-test: all passed");
process.exit(0);

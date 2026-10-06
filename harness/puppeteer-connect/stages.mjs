// Puppeteer 基本操作の段階実行ロジック（TASK-45.2・#481、ビヘイビア CDP-3・MS-4）。
//
// 役割: connect.mjs から呼ばれ、接続 → newPage → goto → セレクタ取得を順に実行して
// 段階別の到達結果（stages）を返す純粋ロジック。puppeteer を注入できるため、
// self-test.mjs が偽クライアントでオフライン検証する。最初の失敗で止め、残りは not_reached。
// UA・フィンガープリントを変えるオプションは渡さない（SEC-2）。

const MAX_LEN = 500;
export const STAGE_NAMES = ["connect", "newPage", "goto", "selector", "disconnect"];

export function toError(err) {
  return {
    name: String(err?.name ?? "Error").slice(0, MAX_LEN),
    message: String(err?.message ?? err).slice(0, MAX_LEN),
  };
}

// 段階を期限と競わせる（イベント待ちで止まる場合の保護）。
// 敗者側（期限後に遅れて reject する段階 promise）は no-op の catch で購読しておく。
// 未処理だと unhandledRejection となり、connect.mjs が結果行を二重に出して stages を失う。
async function withTimeout(promise, ms) {
  promise.catch(() => {});
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => {
      const e = new Error(`stage timed out after ${ms} ms`);
      e.name = "StageTimeout";
      reject(e);
    }, ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

// onStage は実行中の段階名の通知（unhandledRejection 時の報告用）。
export async function runStages({ puppeteer, endpoint, stageTimeoutMs = 10000, onStage = () => {} }) {
  const stages = STAGE_NAMES.map((name) => ({ name, status: "not_reached", error: null }));
  let browser;
  let page;
  const steps = [
    async () => {
      browser = await puppeteer.connect({
        browserWSEndpoint: endpoint,
        protocolTimeout: stageTimeoutMs,
      });
    },
    async () => {
      page = await browser.newPage();
    },
    // about:blank はネットワーク取得なしで確定遷移として扱われる（SSRF 防御を緩めない）。
    async () => {
      await page.goto("about:blank", { timeout: stageTimeoutMs });
    },
    async () => {
      const el = await page.$("body");
      if (el === null || el === undefined) {
        const e = new Error("selector `body` matched no element");
        e.name = "SelectorNotFound";
        throw e;
      }
    },
  ];
  try {
    for (let i = 0; i < steps.length; i++) {
      onStage(stages[i].name);
      try {
        await withTimeout(steps[i](), stageTimeoutMs);
        stages[i].status = "ok";
      } catch (e) {
        stages[i].status = "failed";
        stages[i].error = toError(e);
        break;
      }
    }
  } finally {
    // 切断は最後の段階。切断が応答しなくても stages を返せるよう期限を設ける。先行段階が
    // 失敗済みなら切断は後始末のみ（失敗・時間切れは先行段階の結果を上書きしない）。
    onStage("disconnect");
    const allOk = stages.slice(0, -1).every((s) => s.status === "ok");
    try {
      if (browser) {
        await withTimeout(Promise.resolve().then(() => browser.disconnect()), stageTimeoutMs);
        if (allOk) stages[stages.length - 1].status = "ok";
      }
    } catch (e) {
      if (allOk) {
        stages[stages.length - 1].status = "failed";
        stages[stages.length - 1].error = toError(e);
      }
    }
  }
  const bad = stages.find((s) => s.status !== "ok");
  const last = stages[stages.length - 1];
  return {
    ok: bad === undefined,
    step: (bad ?? last).name,
    error: bad?.error ?? null,
    stages,
  };
}

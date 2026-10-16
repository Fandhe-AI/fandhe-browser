// Playwright 基本操作の段階実行ロジック（TASK-44.1・#476、ビヘイビア CDP-2・MS-4）。
//
// 役割: connect.mjs から呼ばれ、connectOverCDP → newContext → newPage を順に実行して
// 段階別の到達結果（stages）を返す純粋ロジック。chromium を注入できるため、self-test.mjs と
// contract-sample.mjs が偽クライアントでオフライン検証する。最初の失敗で止め、残りは not_reached。
// goto・セレクタ取得の段階は TASK-44.3（#478）で追加する。UA・フィンガープリントを変える
// オプションは渡さない（SEC-2）。Playwright 既定オプションは Browser.setDownloadBehavior 等で
// 失敗するため noDefaults / acceptDownloads:false / viewport:null を指定する
// （harness/playwright-trace/README.md）。

const MAX_LEN = 500;
export const STAGE_NAMES = ["connect", "newContext", "newPage"];

export function toError(err) {
  return {
    name: String(err?.name ?? "Error").slice(0, MAX_LEN),
    message: String(err?.message ?? err).slice(0, MAX_LEN),
  };
}

// 段階を期限と競わせる（イベント待ちで止まる場合の保護）。敗者側の遅延 reject は購読して
// unhandledRejection による結果行の二重出力を防ぐ。
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
export async function runStages({ chromium, endpoint, stageTimeoutMs = 10000, onStage = () => {} }) {
  const stages = STAGE_NAMES.map((name) => ({ name, status: "not_reached", error: null }));
  let browser;
  let context;
  const steps = [
    async () => {
      browser = await chromium.connectOverCDP(endpoint, { noDefaults: true, timeout: stageTimeoutMs });
    },
    async () => {
      context = await browser.newContext({ acceptDownloads: false, viewport: null });
    },
    async () => {
      await context.newPage();
    },
  ];
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
  const bad = stages.find((s) => s.status !== "ok");
  const last = stages[stages.length - 1];
  return {
    ok: bad === undefined,
    step: (bad ?? last).name,
    error: bad?.error ?? null,
    stages,
  };
}

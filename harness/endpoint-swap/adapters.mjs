// Playwright / Puppeteer のアダプタ（TASK-46・#252、ビヘイビア CDP-4。関連 CDP-2・CDP-3・SEC-2）。
// 呼び出し元は run_swap.mjs（実ライブラリ）と self-test.mjs（偽ライブラリ）。swap_lib.mjs の executeTask が
// 期待する形（stages / connect / newContext / newPage / goto / queryFirst / evaluate / version / cleanup）を返す。
//
// 接続先（fandhe-browser か Chromium か）はアダプタに渡す endpoint の URL だけが異なり、
// アダプタ内に接続先ごとの分岐は無い。UA・ヘッダ・フィンガープリントを変えるオプションは渡さない（SEC-2）。
//
// 接続オプション（両接続先に同じものを渡す。測定条件として README に明記する）:
//   Playwright: connectOverCDP(endpoint, { noDefaults: true })・newContext({ acceptDownloads: false, viewport: null })。
//     spec の CDP-2（2026-10-27 オーナー判断。根拠 #943）が適合判定にこの指定を使うと定めている。
//   Puppeteer : connect({ defaultViewport: null })（Playwright の viewport: null に相当する公開オプション。
//     既定の 800x600 エミュレーションが送る Emulation.* を避ける。--puppeteer-viewport default で既定値に戻せる）。

const MAX_DISCOVERY_BYTES = 64 * 1024;

export const STAGES_PLAYWRIGHT = Object.freeze(["connect", "newContext", "newPage", "goto", "selector"]);
export const STAGES_PUPPETEER = Object.freeze(["connect", "newPage", "goto", "selector"]);

/** 接続先がループバックの http(s) / ws(s) URL（userinfo なし・ポートあり）かを URL 解析で検証して URL を返す。違反は Error。 */
export function parseLoopbackEndpoint(value) {
  let u;
  try {
    u = new URL(value);
  } catch {
    throw new Error("endpoint is not a valid URL");
  }
  if (!["http:", "ws:"].includes(u.protocol)) throw new Error("endpoint must use http: or ws:");
  if (u.username !== "" || u.password !== "") throw new Error("endpoint must not contain userinfo");
  if (!["127.0.0.1", "localhost", "[::1]"].includes(u.hostname)) throw new Error("endpoint must be a loopback host");
  const port = Number(u.port);
  if (u.port === "" || !Number.isInteger(port) || port < 1 || port > 65535) throw new Error("endpoint must have an explicit port");
  return u;
}

async function readLimited(res, limit) {
  const reader = res.body.getReader();
  const chunks = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.length;
    if (total > limit) {
      await reader.cancel();
      throw new Error("discovery response too large");
    }
    chunks.push(value);
  }
  return Buffer.concat(chunks).toString("utf8");
}

/**
 * http エンドポイントの /json/version から webSocketDebuggerUrl を得る（Puppeteer の browserWSEndpoint 用）。
 * 返された ws URL は endpoint と同じホスト・ポートの ws: だけ受け付ける（別の接続先へ誘導されない）。
 */
export async function resolveWsEndpoint(endpoint, timeoutMs) {
  const base = parseLoopbackEndpoint(endpoint);
  if (base.protocol === "ws:") return base.href;
  const res = await fetch(new URL("/json/version", base), { redirect: "error", signal: AbortSignal.timeout(timeoutMs) });
  if (!res.ok) throw new Error(`discovery failed: HTTP ${res.status}`);
  const j = JSON.parse(await readLimited(res, MAX_DISCOVERY_BYTES));
  let w;
  try {
    w = new URL(j.webSocketDebuggerUrl);
  } catch {
    throw new Error("webSocketDebuggerUrl is missing or invalid");
  }
  if (w.protocol !== "ws:" || w.hostname !== base.hostname || w.port !== base.port || w.username || w.password) {
    throw new Error("webSocketDebuggerUrl does not match endpoint");
  }
  return w.href;
}

// 後始末は個別に期限を付け、失敗を握りつぶす（接続が壊れていても後続の後始末を続ける）
async function quietly(fn, ms) {
  let timer;
  try {
    await Promise.race([
      Promise.resolve().then(fn),
      new Promise((_, rej) => {
        timer = setTimeout(() => rej(new Error("cleanup timeout")), ms);
      }),
    ]);
  } catch {
    // 後始末の失敗は成否に影響しない
  } finally {
    clearTimeout(timer);
  }
}

export function playwrightAdapter({ chromium, endpoint, stageTimeoutMs, waitUntil = "domcontentloaded" }) {
  let browser;
  let context;
  let page;
  // cleanup 開始後は true。executeTask の期限（withTimeout）は元の処理を中断しないため、期限後に遅れて得た
  // browser・context・page は代入せず、その場で解放する（解放漏れで接続先にセッションが残らないようにする）
  let cleaned = false;
  return {
    stages: STAGES_PLAYWRIGHT,
    async connect() {
      const b = await chromium.connectOverCDP(endpoint, { noDefaults: true, timeout: stageTimeoutMs });
      if (cleaned) return await quietly(() => b.close(), 3000);
      browser = b;
    },
    async newContext() {
      const c = await browser.newContext({ acceptDownloads: false, viewport: null });
      if (cleaned) return await quietly(() => c.close(), 3000);
      context = c;
    },
    async newPage() {
      const p = await context.newPage();
      if (cleaned) return await quietly(() => p.close(), 3000);
      page = p;
    },
    async goto(url) {
      const response = await page.goto(url, { waitUntil, timeout: stageTimeoutMs });
      return response ? response.status() : null;
    },
    async queryFirst(selector) {
      return await page.$(selector);
    },
    async evaluate(handle, fn) {
      return await handle.evaluate(fn);
    },
    version() {
      try {
        return String(browser.version());
      } catch {
        return null;
      }
    },
    async cleanup() {
      cleaned = true;
      if (page) await quietly(() => page.close(), 3000);
      if (context) await quietly(() => context.close(), 3000);
      // connectOverCDP の browser.close() は切断のみ（接続先のブラウザプロセスは終了しない）
      if (browser) await quietly(() => browser.close(), 3000);
    },
  };
}

export function puppeteerAdapter({ puppeteer, endpoint, stageTimeoutMs, waitUntil = "domcontentloaded", viewport = "null", connectMode = "ws" }) {
  let browser;
  let page;
  // Playwright 側と同じ。cleanup 後に遅れて得たリソースは代入せず即時に解放する
  let cleaned = false;
  return {
    stages: STAGES_PUPPETEER,
    async connect() {
      const base = parseLoopbackEndpoint(endpoint);
      const opts = { protocolTimeout: stageTimeoutMs };
      if (viewport === "null") opts.defaultViewport = null;
      if (base.protocol === "ws:") opts.browserWSEndpoint = base.href;
      else if (connectMode === "url") opts.browserURL = base.origin;
      else opts.browserWSEndpoint = await resolveWsEndpoint(endpoint, stageTimeoutMs);
      const b = await puppeteer.connect(opts);
      if (cleaned) return await quietly(() => b.disconnect(), 3000);
      browser = b;
    },
    async newPage() {
      const p = await browser.newPage();
      if (cleaned) return await quietly(() => p.close(), 3000);
      page = p;
    },
    async goto(url) {
      const response = await page.goto(url, { waitUntil, timeout: stageTimeoutMs });
      return response ? response.status() : null;
    },
    async queryFirst(selector) {
      return await page.$(selector);
    },
    async evaluate(handle, fn) {
      return await handle.evaluate(fn);
    },
    async version() {
      try {
        return String(await browser.version());
      } catch {
        return null;
      }
    },
    async cleanup() {
      cleaned = true;
      if (page) await quietly(() => page.close(), 3000);
      // disconnect は接続先のブラウザを終了しない
      if (browser) await quietly(() => browser.disconnect(), 3000);
    },
  };
}

// 22 タスクを fandhe-browser の CDP サーバーへ流して成否を判定するクライアント（TASK-71.2・MEAS-4）。
// 呼び出し元は run_core.sh のみ（バイナリの起動・停止・meta 行・JSONL の書き出しは sh 側が担う）。
// 本ファイルは CDP の `Page.navigate` → `DOM.getDocument` → `DOM.querySelector` →
// `DOM.requestChildNodes` だけを使い、タスク 1 件ごとに結果 1 行（JSON）を stdout へ出す。
//
// 入力は環境変数（URL を引数に載せない）: FC_ENDPOINT・FC_TASKS・FC_TASK_TIMEOUT_MS・
// FC_TOTAL_TIMEOUT_MS。依存ゼロ（Node 22 以降の組み込み WebSocket / fetch）。
//
// 重要な前提（REPAIR-3・JS-2）: core の `Page.navigate` は fetch して HTML を保存するだけで、
// ページ内 JS は実行されない（js_stub / JsRuntime はナビゲーション経路に未配線）。そのため
// b5・d2・d3 のような JS 必須ページの「V8 統合による解消」はここでは示せず、実測のまま記録する。
// 配線された時点で meta の page_js_executed と本コメントを見直すこと。
//
// 非信頼データの扱い: ページ由来テキストは output_sample としてのみ出力し、300 文字で切り詰め、
// 制御文字を空白へ置換する。stderr・進捗には出さない（CI ログでのワークフローコマンド注入対策）。
import { readFileSync } from "node:fs";

const endpoint = process.env.FC_ENDPOINT ?? "";
const tasksPath = process.env.FC_TASKS ?? "";
const taskTimeoutMs = Number(process.env.FC_TASK_TIMEOUT_MS ?? "45000");
const totalTimeoutMs = Number(process.env.FC_TOTAL_TIMEOUT_MS ?? "1200000");

const MAX_DISCOVERY_BYTES = 64 * 1024;
const MAX_MESSAGE_BYTES = 8 * 1024 * 1024;
const SAMPLE_MAX = 300;

function emit(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function fatal(msg) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(2);
}

function sanitizeSample(s) {
  // eslint-disable-next-line no-control-regex
  return s.replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim().slice(0, SAMPLE_MAX);
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

// discovery が返す WebSocket URL が endpoint と同じ loopback ホスト・ポートかを検証する
// （不一致の接続先へ誘導されない。harness/playwright-trace の validateDiscoveredWs と同じ考え方）。
function validateWs(wsUrl, base) {
  let u;
  try {
    u = new URL(wsUrl);
  } catch {
    return false;
  }
  return u.protocol === "ws:" && u.hostname === base.hostname && u.port === base.port && !u.username && !u.password;
}

async function discover() {
  const base = new URL(endpoint);
  const res = await fetch(new URL("/json/version", base), { redirect: "error", signal: AbortSignal.timeout(10000) });
  if (!res.ok) throw new Error(`discovery failed: HTTP ${res.status}`);
  const j = JSON.parse(await readLimited(res, MAX_DISCOVERY_BYTES));
  if (typeof j.webSocketDebuggerUrl !== "string" || !validateWs(j.webSocketDebuggerUrl, base)) {
    throw new Error("webSocketDebuggerUrl does not match endpoint");
  }
  const browser = typeof j.Browser === "string" ? j.Browser.slice(0, 100) : "";
  return { ws: j.webSocketDebuggerUrl, browser };
}

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.events = [];
    this.waiters = [];
    this.closed = false;
    ws.addEventListener("message", (ev) => this.onMessage(ev.data));
    ws.addEventListener("close", () => this.onClose());
    ws.addEventListener("error", () => this.onClose());
  }
  onClose() {
    if (this.closed) return;
    this.closed = true;
    for (const p of this.pending.values()) p.reject(new Error("connection closed"));
    this.pending.clear();
    for (const w of this.waiters) w.reject(new Error("connection closed"));
    this.waiters = [];
  }
  onMessage(data) {
    const text = typeof data === "string" ? data : String(data);
    if (text.length > MAX_MESSAGE_BYTES) {
      // 巨大メッセージは解釈せず、対応する要求がどれか不明なため全 pending をエラーにする
      for (const p of this.pending.values()) p.reject(new Error("message too large"));
      this.pending.clear();
      return;
    }
    let m;
    try {
      m = JSON.parse(text);
    } catch {
      return;
    }
    if (typeof m.id === "number") {
      const p = this.pending.get(m.id);
      if (!p) return;
      this.pending.delete(m.id);
      if (m.error) p.reject(Object.assign(new Error(String(m.error.message ?? "error")), { cdp: true }));
      else p.resolve(m.result ?? {});
    } else if (typeof m.method === "string") {
      this.events.push(m);
      const ws = this.waiters;
      this.waiters = [];
      for (const w of ws) w.resolve();
    }
  }
  send(method, params, timeoutMs) {
    if (this.closed) return Promise.reject(new Error("connection closed"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => {
        this.pending.delete(id);
        reject(Object.assign(new Error("timeout"), { timeout: true }));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => (clearTimeout(t), resolve(v)),
        reject: (e) => (clearTimeout(t), reject(e)),
      });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }
  // 条件に合うイベントを待つ（先に届いて溜まっているものも対象。見つけたら取り除く）
  async waitEvent(pred, timeoutMs) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const i = this.events.findIndex(pred);
      if (i >= 0) return this.events.splice(i, 1)[0];
      const left = deadline - Date.now();
      if (left <= 0) throw Object.assign(new Error("timeout"), { timeout: true });
      await new Promise((resolve, reject) => {
        const w = { resolve: () => (clearTimeout(t), resolve()), reject: (e) => (clearTimeout(t), reject(e)) };
        const t = setTimeout(() => {
          this.waiters = this.waiters.filter((x) => x !== w);
          resolve();
        }, left);
        this.waiters.push(w);
      });
    }
  }
}

// 部分木から text ノードの nodeValue を連結する
function collectText(nodes) {
  let out = "";
  const stack = [...nodes].reverse();
  while (stack.length > 0) {
    const n = stack.pop();
    if (n.nodeType === 3 && typeof n.nodeValue === "string") out += n.nodeValue + " ";
    if (Array.isArray(n.children)) for (let i = n.children.length - 1; i >= 0; i--) stack.push(n.children[i]);
  }
  return out;
}

// 部分木のうち name 属性を持つ input・textarea・select の件数を数える
function countFormFields(nodes) {
  let count = 0;
  const stack = [...nodes];
  while (stack.length > 0) {
    const n = stack.pop();
    if (n.nodeType === 1 && ["input", "textarea", "select"].includes(n.localName) && Array.isArray(n.attributes)) {
      for (let i = 0; i + 1 < n.attributes.length; i += 2) {
        if (n.attributes[i] === "name" && n.attributes[i + 1] !== "") count++;
      }
    }
    // 展開引数の上限（RangeError）を避けるため 1 件ずつ積む（CDP は 1 文書 20 万ノードまで許容）
    if (Array.isArray(n.children)) {
      for (const c of n.children) stack.push(c);
    }
  }
  return count;
}

function classifyError(e) {
  if (e?.timeout) return "timeout";
  const msg = String(e?.message ?? "");
  if (msg === "unsupported params") return "selector_unsupported";
  if (msg === "document too large") return "document_too_large";
  return "cdp_error";
}

async function runTask(cdp, t, budgetMs) {
  const base = { type: "result", id: t.id, cat: t.cat, url: t.url, kind: t.kind, selector: t.selector };
  const fail = (reason, detail = null, extra = {}) => ({
    ...base, success: false, reason, detail, method: "first_match", match_count: null, output_sample: "", ...extra,
  });
  // タスク期限は開始時に 1 回だけ確定する（--task-timeout と --total-timeout の残りの小さい方）。
  // 各 CDP 操作へは「期限までの残り時間」を渡し、操作ごとに予算が再付与されないようにする
  const startedAt = Date.now();
  const totalDeadline = startedAt + budgetMs;
  const taskDeadline = startedAt + Math.min(taskTimeoutMs, budgetMs);
  const stepTimeout = () => {
    const now = Date.now();
    if (now >= totalDeadline) throw Object.assign(new Error("total time limit exceeded"), { totalExceeded: true });
    if (now >= taskDeadline) throw Object.assign(new Error("timeout"), { timeout: true });
    return taskDeadline - now;
  };
  try {
    const nav = await cdp.send("Page.navigate", { url: t.url }, stepTimeout());
    if (typeof nav.errorText === "string" && nav.errorText !== "") {
      return fail("fetch_error", nav.errorText.slice(0, 60));
    }
    const doc = await cdp.send("DOM.getDocument", { depth: 1 }, stepTimeout());
    const rootId = doc?.root?.nodeId;
    if (typeof rootId !== "number") return fail("cdp_error", "no root node");
    const q = await cdp.send("DOM.querySelector", { nodeId: rootId, selector: t.selector }, stepTimeout());
    if (!q.nodeId) return fail("no_match");
    cdp.events.length = 0;
    await cdp.send("DOM.requestChildNodes", { nodeId: q.nodeId, depth: -1 }, stepTimeout());
    const ev = await cdp.waitEvent((m) => m.method === "DOM.setChildNodes" && m.params?.parentId === q.nodeId, stepTimeout());
    const nodes = Array.isArray(ev.params?.nodes) ? ev.params.nodes : [];
    if (t.kind === "form") {
      const n = countFormFields(nodes);
      return n > 0
        ? { ...base, success: true, reason: null, detail: null, method: "form_fields", match_count: n, output_sample: `${n} named fields` }
        : fail("empty_result", null, { method: "form_fields", match_count: 0 });
    }
    const text = collectText(nodes);
    if (text.replace(/\s+/g, "") === "") return fail("empty_result");
    return { ...base, success: true, reason: null, detail: null, method: "first_match", match_count: null, output_sample: sanitizeSample(text) };
  } catch (e) {
    if (cdp.closed) return fail("cdp_error", "connection closed", { closed: true });
    if (e?.totalExceeded || Date.now() >= totalDeadline) return fail("blocked", "total time limit exceeded");
    return fail(classifyError(e));
  }
}

async function main() {
  if (typeof WebSocket !== "function") fatal("this Node.js has no built-in WebSocket (Node 22+ is required)");
  let tasks;
  try {
    tasks = JSON.parse(readFileSync(tasksPath, "utf8"));
  } catch {
    fatal("cannot read tasks file");
  }
  let disc;
  try {
    disc = await discover();
  } catch (e) {
    fatal(`discovery failed: ${String(e.message).slice(0, 100)}`);
  }
  const ws = new WebSocket(disc.ws);
  // 接続待ちにも期限を適用する（/json/version は応答するが handshake が完了しないサーバー対策）。
  // 期限は --total-timeout と --task-timeout の小さい方で、接続に使った時間は総予算から差し引く
  const start = Date.now();
  const connectMs = Math.min(taskTimeoutMs, totalTimeoutMs);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("websocket connect timeout")), connectMs);
    ws.addEventListener("open", () => { clearTimeout(timer); resolve(); }, { once: true });
    ws.addEventListener("error", () => { clearTimeout(timer); reject(new Error("websocket connect failed")); }, { once: true });
  }).catch((e) => {
    try {
      ws.close();
    } catch {
      // 切断失敗は致命エラー処理に影響しない
    }
    fatal(e.message);
  });
  emit({ type: "browser", cdp_browser: disc.browser });
  const cdp = new Cdp(ws);
  for (const t of tasks) {
    const left = totalTimeoutMs - (Date.now() - start);
    let r;
    if (left <= 0) {
      r = { type: "result", id: t.id, cat: t.cat, url: t.url, kind: t.kind, selector: t.selector, success: false, reason: "blocked", detail: "total time limit exceeded", method: "first_match", match_count: null, output_sample: "" };
    } else if (cdp.closed) {
      r = { type: "result", id: t.id, cat: t.cat, url: t.url, kind: t.kind, selector: t.selector, success: false, reason: "cdp_error", detail: "connection closed", method: "first_match", match_count: null, output_sample: "" };
    } else {
      const t0 = Date.now();
      r = await runTask(cdp, t, left);
      r.elapsed_ms = Date.now() - t0;
      delete r.closed;
    }
    if (r.elapsed_ms === undefined) r.elapsed_ms = 0;
    emit(r);
    process.stderr.write(`[${t.id}] success=${r.success} reason=${r.reason ?? "-"}\n`);
  }
  try {
    ws.close();
  } catch {
    // 切断失敗は結果に影響しない
  }
  process.exit(0);
}

main();

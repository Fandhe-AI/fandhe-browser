// fandhe-browser の CDP エンドポイントで同一 URL を読み込む（TASK-81・PERF-4・MEAS-6）。
// 呼び出し元は measure.sh の loaded 条件。node 22 以降の組み込み WebSocket のみを使い、
// npm 依存を持たない。使い方: node navigate.mjs http://127.0.0.1:9333 <url>
// 手順: /json/version の browser WebSocket へ接続して Page.navigate を送る。error 応答なら
// 非 0 で終了する（成功を装わない）。
const [endpoint, url] = process.argv.slice(2);
if (!endpoint || !url) {
  console.error("usage: node navigate.mjs <http-endpoint> <url>");
  process.exit(2);
}
if (typeof WebSocket === "undefined") {
  console.error("error: global WebSocket is unavailable (node 22+ required)");
  process.exit(2);
}

const version = await (await fetch(`${endpoint}/json/version`)).json();
const ws = new WebSocket(version.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  ws.onopen = resolve;
  ws.onerror = () => reject(new Error("websocket connect failed"));
});

let nextId = 1;
const pending = new Map();
ws.onmessage = (ev) => {
  const msg = JSON.parse(String(ev.data));
  const p = pending.get(msg.id);
  if (!p) return;
  pending.delete(msg.id);
  if (msg.error) p.reject(new Error(`${p.method}: ${JSON.stringify(msg.error)}`));
  else p.resolve(msg.result ?? {});
};

function send(method, params = {}, sessionId) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`${method}: timeout`)), 30000);
    pending.set(id, {
      method,
      resolve: (v) => (clearTimeout(timer), resolve(v)),
      reject: (e) => (clearTimeout(timer), reject(e)),
    });
    ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  });
}

try {
  // 現行の fandhe-browser は Target.createTarget が未実装（method not implemented）のため、
  // browser WebSocket 上で Page.navigate を直接送る。Target.* が実装されたら本手順を見直す。
  const nav = await send("Page.navigate", { url });
  console.log(JSON.stringify({ ok: true, nav }));
  ws.close();
} catch (e) {
  console.error(`error: ${e.message}`);
  process.exit(1);
}

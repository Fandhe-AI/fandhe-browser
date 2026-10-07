// run_core の自己テスト用の偽 CDP サーバー（TASK-71.2・MEAS-4）。オフライン専用。
// run_core.sh の自己テスト（self-test.sh）が起動し、実ネットワーク・実バイナリを使わずに
// run_core.mjs の判定ロジック・期限・検証経路を確認するために使う。本物のサーバー挙動の代替では
// なく、core の応答形（応答 → DOM.setChildNodes イベントの順）を模しただけの固定応答である。
//
// 使い方: node fake_cdp_server.mjs [--ws-host-mismatch] [--port N]
//   127.0.0.1 のエフェメラルポートで listen し、ポート番号を stdout へ 1 行出す。
// セレクタ文字列で挙動を切り替える（合成 tasks 側で指定）:
//   nomatch=一致なし / unsupported=-32602 / toolarge=requestChildNodes が document too large /
//   emptytext=空テキスト / emptyform=name 付き入力なし / form*=フォーム / ctl=制御文字と長文 /
//   それ以外=テキスト "Hello"。URL に fetcherr を含むと navigate が errorText、hang を含むと無応答。
import http from "node:http";
import crypto from "node:crypto";

const mismatch = process.argv.includes("--ws-host-mismatch");
// --port N: 固定ポートで listen する（run_core.sh の起動経路テストで、スタブバイナリ経由で起動するため）
const portIdx = process.argv.indexOf("--port");
const listenPort = portIdx >= 0 ? Number(process.argv[portIdx + 1]) : 0;

function frame(text) {
  const payload = Buffer.from(text, "utf8");
  let header;
  if (payload.length < 126) header = Buffer.from([0x81, payload.length]);
  else if (payload.length < 65536) header = Buffer.from([0x81, 126, payload.length >> 8, payload.length & 255]);
  else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  return Buffer.concat([header, payload]);
}

function* parseFrames(state) {
  for (;;) {
    const b = state.buf;
    if (b.length < 2) return;
    const opcode = b[0] & 0x0f;
    let len = b[1] & 0x7f;
    let off = 2;
    if (len === 126) {
      if (b.length < 4) return;
      len = b.readUInt16BE(2);
      off = 4;
    } else if (len === 127) {
      if (b.length < 10) return;
      len = Number(b.readBigUInt64BE(2));
      off = 10;
    }
    const masked = (b[1] & 0x80) !== 0;
    const need = off + (masked ? 4 : 0) + len;
    if (b.length < need) return;
    const mask = masked ? b.subarray(off, off + 4) : null;
    const data = Buffer.from(b.subarray(off + (masked ? 4 : 0), need));
    if (mask) for (let i = 0; i < data.length; i++) data[i] ^= mask[i % 4];
    state.buf = b.subarray(need);
    yield { opcode, data };
  }
}

const textNode = (id, v) => ({ nodeId: id, nodeType: 3, nodeName: "#text", nodeValue: v });
const inputNode = (id, name) => ({
  nodeId: id, nodeType: 1, nodeName: "INPUT", localName: "input", nodeValue: "",
  attributes: name === null ? ["type", "submit"] : ["type", "text", "name", name],
});

function childrenFor(selector) {
  if (selector.includes("emptytext")) return [textNode(11, "   ")];
  if (selector.includes("emptyform")) return [inputNode(11, null)];
  if (selector.includes("form")) return [inputNode(11, "user"), inputNode(12, "pass"), inputNode(13, null)];
  if (selector.includes("ctl")) return [textNode(11, "a\u0000b\nc::error::x" + "z".repeat(500))];
  return [textNode(11, "Hello")];
}

const server = http.createServer((req, res) => {
  if (req.url === "/json/version") {
    const port = server.address().port;
    const wsPort = mismatch ? port + 1 : port;
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ Browser: "FakeCdp/0.0", webSocketDebuggerUrl: `ws://127.0.0.1:${wsPort}/devtools/browser/fake` }));
    return;
  }
  res.statusCode = 404;
  res.end();
});

server.on("upgrade", (req, socket) => {
  const key = req.headers["sec-websocket-key"];
  const accept = crypto.createHash("sha1").update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest("base64");
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  const state = { buf: Buffer.alloc(0) };
  let selectorSeen = "";
  socket.on("data", (d) => {
    state.buf = Buffer.concat([state.buf, d]);
    for (const f of parseFrames(state)) {
      if (f.opcode === 8) {
        socket.end();
        return;
      }
      if (f.opcode !== 1) continue;
      const m = JSON.parse(f.data.toString("utf8"));
      const reply = (result) => socket.write(frame(JSON.stringify({ id: m.id, result })));
      const error = (message) => socket.write(frame(JSON.stringify({ id: m.id, error: { code: -32000, message } })));
      if (m.method === "Page.navigate") {
        if (m.params.url.includes("hang")) return;
        if (m.params.url.includes("fetcherr")) reply({ frameId: "f", loaderId: "l", errorText: "net::ERR_FAILED" });
        else reply({ frameId: "f", loaderId: "l" });
      } else if (m.method === "DOM.getDocument") {
        reply({ root: { nodeId: 1, nodeType: 9, nodeName: "#document", childNodeCount: 1 } });
      } else if (m.method === "DOM.querySelector") {
        selectorSeen = m.params.selector;
        if (selectorSeen.includes("unsupported")) error("unsupported params");
        else if (selectorSeen.includes("nomatch")) reply({ nodeId: 0 });
        else reply({ nodeId: 10 });
      } else if (m.method === "DOM.requestChildNodes") {
        if (selectorSeen.includes("toolarge")) {
          error("document too large");
        } else {
          reply({});
          socket.write(frame(JSON.stringify({ method: "DOM.setChildNodes", params: { parentId: m.params.nodeId, nodes: childrenFor(selectorSeen) } })));
        }
      } else {
        socket.write(frame(JSON.stringify({ id: m.id, error: { code: -32601, message: "method not implemented" } })));
      }
    }
  });
  socket.on("error", () => {});
});

server.listen(listenPort, "127.0.0.1", () => {
  process.stdout.write(`${server.address().port}\n`);
});

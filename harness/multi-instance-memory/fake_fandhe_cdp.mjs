// measure.sh の自己テスト用の偽 fandhe-browser（TASK-81・PERF-4・MEAS-6）。オフライン専用。
// 127.0.0.1:9333 で /json/version と最小の WebSocket（Page.navigate のみ）を提供し、
// 実バイナリ・実ネットワークなしで「ページ JS 有効時に JS ワーカー子プロセスが起動する」経路を模す。
// 本物の挙動の代替ではない。self-test.sh だけが、シェルのラッパー経由（exec node ...）で起動する。
//
// 環境変数:
//   FAKE_SPAWN=worker  Page.navigate で FANDHE_BROWSER_JS_WORKER 付きの子（メモリを確保して約 1 秒で終了）を起動する
//   FAKE_SPAWN=none    何も起動しない（ワーカー未起動の検知確認用）
//   FAKE_RECORD_DIR    あれば、受け取った FANDHE_BROWSER_CONFIG（無ければ "<unset>"）を config.txt に記録する
// 最小の WebSocket フレーム処理は harness/compat-practical/fake_cdp_server.mjs と同じ形（ハーネス間の import はしない）。
import http from "node:http";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";

const spawnMode = process.env.FAKE_SPAWN ?? "none";

if (process.env.FAKE_RECORD_DIR) {
  const cfg = process.env.FANDHE_BROWSER_CONFIG;
  fs.writeFileSync(path.join(process.env.FAKE_RECORD_DIR, `config-${process.pid}.txt`), cfg ?? "<unset>");
}

function frame(text) {
  const payload = Buffer.from(text, "utf8");
  return Buffer.concat([Buffer.from([0x81, payload.length]), payload]);
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
      return; // この偽サーバーが受ける要求は小さい
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

function startWorker() {
  // 約 32 MiB を確保して約 1 秒で終了する子（親が回収する）。PSS ピークの観測用
  const child = spawn("/bin/sh", ["-c", 'x=$(head -c 33554432 /dev/zero | tr "\\0" a); sleep 1'], {
    env: { FANDHE_BROWSER_JS_WORKER: "1", PATH: process.env.PATH ?? "/usr/bin:/bin" },
    stdio: "ignore",
  });
  child.on("error", () => {});
}

const server = http.createServer((req, res) => {
  if (req.url === "/json/version") {
    res.setHeader("content-type", "application/json");
    res.end(JSON.stringify({ Browser: "FakeFandhe/0.0", webSocketDebuggerUrl: "ws://127.0.0.1:9333/devtools/browser/fake" }));
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
  socket.on("data", (d) => {
    state.buf = Buffer.concat([state.buf, d]);
    for (const f of parseFrames(state)) {
      if (f.opcode === 8) return void socket.end();
      if (f.opcode !== 1) continue;
      const m = JSON.parse(f.data.toString("utf8"));
      if (m.method === "Page.navigate") {
        if (spawnMode === "worker") startWorker();
        socket.write(frame(JSON.stringify({ id: m.id, result: { frameId: "f", loaderId: "l" } })));
      } else {
        socket.write(frame(JSON.stringify({ id: m.id, error: { code: -32601, message: "method not implemented" } })));
      }
    }
  });
  socket.on("error", () => {});
});
server.listen(9333, "127.0.0.1");

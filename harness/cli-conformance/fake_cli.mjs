// run.sh の自己テスト用の偽 fandhe-browser（TASK-49・CLI-1）。実バイナリの固定文言・終了コードを
// docs/design/cli-conformance-tests.md の 3・4 章から引き写した最小の模倣で、ネットワークには出ない
// （example.com の応答は埋め込み値）。環境変数 FAKE_MODE（カンマ区切り）で故意に挙動を崩し、
// run.sh の判定ロジックが pass / fail / 前提未充足を正しく出し分けることを確かめる。
//   bad-title     query の title が違う値を返す（CT-QUERY を fail にする）
//   bad-nav-exit  navigate が終了コード 3 で失敗する（CT-NAV を fail にする）
//   bad-devtools  launch の DevTools 行を壊す（CT-LAUNCH を fail にする）
//   bad-png       screenshot の PNG の高さを 600 にする（CT-SHOT を fail にする）
//   no-host       描画ホスト未解決のシグネチャを返す（CT-SHOT を前提未充足にする）
//   crlf          stdout を CRLF 区切りで出す（正規化されて合格し、差分メモに残る）
// 描画ホストは環境変数 FANDHE_BROWSER_RENDER_HOST が設定されているときだけ「ある」ものとする。
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import crypto from "node:crypto";

const modes = new Set((process.env.FAKE_MODE || "").split(",").filter(Boolean));
const PORT = Number(process.env.CT_LAUNCH_PORT || 9333);
const USAGE = "; run 'fandhe-browser --help' for usage";
const NL = modes.has("crlf") ? "\r\n" : "\n";
const P_TEXT =
  '"This domain is for use in documentation examples without needing permission. This is not a service; avoid relying on it for testing and monitoring purposes."';

function out(line) {
  process.stdout.write(line + NL);
}
function fail(code, msg) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(code);
}
function usage(msg) {
  fail(2, msg + USAGE);
}

const args = process.argv.slice(2);

// --profile-dir PATH は全コマンド共通
let profileDir = null;
const pi = args.indexOf("--profile-dir");
if (pi >= 0) {
  if (pi + 1 >= args.length) usage("missing required argument 'PATH'");
  profileDir = args[pi + 1];
  args.splice(pi, 2);
}

function defaultRoot() {
  if (process.platform === "win32") return path.join(process.env.LOCALAPPDATA, "fandhe-browser");
  if (process.platform === "darwin") return path.join(process.env.HOME, "Library", "Application Support", "fandhe-browser");
  return path.join(process.env.XDG_DATA_HOME || path.join(process.env.HOME, ".local", "share"), "fandhe-browser");
}

// プロファイルを開く。祖先の symlink 拒否・symlink の後の `..` の拒否・Windows の LOCALAPPDATA 制約・ロックを模倣する
let lockFile = null;
function openProfile(raw) {
  if (process.platform === "win32") {
    const la = path.resolve(process.env.LOCALAPPDATA).toLowerCase();
    if (!path.resolve(raw).toLowerCase().startsWith(la + path.sep)) {
      fail(1, "on Windows, the profile root must be located under %LOCALAPPDATA% (XOS-7, XOS-10)");
    }
  }
  const parts = raw.split(/[\\/]/);
  let cur = parts[0] === "" ? "/" : parts[0];
  let sawSymlink = false;
  for (const part of parts.slice(1)) {
    if (part === "..") {
      if (sawSymlink) fail(2, "invalid profile directory: '..' after a symbolic link is not allowed");
      cur = path.dirname(cur);
      continue;
    }
    if (part === "") continue;
    cur = path.join(cur, part);
    try {
      if (fs.lstatSync(cur).isSymbolicLink()) {
        sawSymlink = true;
        if (!raw.includes("..")) {
          fail(1, "invalid profile directory: a path component is a symlink; specify the real path without symlinks");
        }
      }
    } catch {
      // 未作成の末尾要素は後で作る
    }
  }
  fs.mkdirSync(raw, { recursive: true, mode: 0o700 });
  const lock = path.join(raw, "profile.lock");
  if (fs.existsSync(lock)) {
    const pid = Number(fs.readFileSync(lock, "utf8").trim());
    if (pid > 0) {
      let alive = false;
      try {
        process.kill(pid, 0);
        alive = true;
      } catch (e) {
        alive = e.code === "EPERM";
      }
      if (alive) fail(1, "profile is already in use");
    }
  }
  fs.writeFileSync(lock, String(process.pid));
  for (const d of ["cookies", "storage", "cache", "history"]) fs.mkdirSync(path.join(raw, d), { recursive: true });
  lockFile = lock;
}
function closeProfile() {
  if (lockFile) fs.writeFileSync(lockFile, "");
}

function checkUrl(url) {
  if (/^file:/i.test(url)) fail(1, "unsupported URL scheme (only http, https and about:blank are allowed)");
  if (/^https?:\/\/(127\.|localhost|10\.|192\.168\.)/i.test(url)) {
    fail(1, "navigation to private or loopback address is not allowed");
  }
}

const cmd = args[0];
if (cmd === "--help" || cmd === "-h" || cmd === "help") {
  out("Usage: fandhe-browser [COMMAND] [ARGS]...");
  out("Commands: launch, navigate, query, screenshot, help");
  out("Options: --profile-dir PATH");
} else if (cmd === undefined || cmd === "launch") {
  const rest = cmd === "launch" ? args.slice(1) : [];
  for (const a of rest) if (a.startsWith("-") && a !== "--port") usage(`unknown option '${a}'`);
  openProfile(profileDir || defaultRoot());
  const id = crypto.randomBytes(16).toString("hex").toUpperCase();
  const ws = `ws://127.0.0.1:${PORT}/devtools/browser/${id}`;
  const server = http.createServer((req, res) => {
    if (req.url === "/json/version") {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify({ Browser: "fake/0", webSocketDebuggerUrl: ws }));
    } else {
      res.statusCode = 404;
      res.end();
    }
  });
  server.on("error", () => fail(1, "failed to bind listener"));
  server.listen(PORT, "127.0.0.1", () => {
    process.stderr.write(modes.has("bad-devtools") ? `listening ${ws}\n` : `DevTools listening on ${ws}\n`);
  });
  const stop = () => {
    closeProfile();
    process.exit(0);
  };
  process.on("SIGTERM", stop);
  process.on("SIGINT", stop);
  // node の shim（Volta・asdf 等）は本体を子プロセスとして起動し、shim だけが TERM で死ぬと本体が孤児として残る。
  // 親が消えたら自分も終了して、自己テストが待ち受けポートを漏らさないようにする
  setInterval(() => {
    try {
      process.kill(process.ppid, 0);
    } catch {
      stop();
    }
  }, 200).unref();
} else if (cmd === "navigate") {
  const rest = args.slice(1);
  for (const a of rest) if (a.startsWith("-")) usage(`unknown option '${a}'`);
  if (rest.length < 1) usage("missing required argument 'URL'");
  checkUrl(rest[0]);
  if (profileDir) openProfile(profileDir);
  if (modes.has("bad-nav-exit")) fail(3, "boom");
  out(rest[0] === "about:blank" ? "url=about:blank" : `status=200 url=${rest[0]}`);
  closeProfile();
} else if (cmd === "query") {
  let all = false;
  let attr = null;
  const pos = [];
  for (let i = 1; i < args.length; i++) {
    if (args[i] === "--all") all = true;
    else if (args[i] === "--attr") attr = args[++i];
    else if (args[i].startsWith("-")) usage(`unknown option '${args[i]}'`);
    else pos.push(args[i]);
  }
  if (all && attr !== null) usage("options '--all' and '--attr' cannot be used together");
  if (pos.length < 2) usage("missing required argument 'SELECTOR'");
  if (pos[0] === "about:blank") fail(1, "query requires a fetched document (about:blank has no content)");
  const sel = pos[1];
  if (sel === "title") out(modes.has("bad-title") ? '"Wrong"' : '"Example Domain"');
  else if (sel === "html" && attr === "lang") out('"en"');
  else if (sel === "script" && attr === "src") out('"/s.js"');
  else if (sel === "p" && all) out(P_TEXT);
  else fail(1, "no element matches the selector");
} else if (cmd === "screenshot") {
  const pos = [];
  for (let i = 1; i < args.length; i++) {
    if (args[i] === "--full-page") usage("option '--full-page' is not supported yet");
    else if (args[i] === "--timeout") {
      const v = args[++i];
      if (!/^[0-9]+$/.test(v || "") || Number(v) < 1 || Number(v) > 60) usage("invalid value for option '--timeout'");
    } else if (args[i].startsWith("-")) usage(`unknown option '${args[i]}'`);
    else pos.push(args[i]);
  }
  if (pos.length < 2) usage("missing required argument 'OUTPUT'");
  if (pos[0] === "about:blank") fail(1, "screenshot requires a fetched document (about:blank has no content)");
  if (fs.existsSync(pos[1])) fail(1, "output file already exists");
  if (modes.has("no-host") || !process.env.FANDHE_BROWSER_RENDER_HOST) {
    fail(1, "rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)");
  }
  const buf = Buffer.alloc(64);
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]).copy(buf, 0);
  buf.write("IHDR", 12, "latin1");
  buf.writeUInt32BE(1280, 16);
  buf.writeUInt32BE(modes.has("bad-png") ? 600 : 800, 20);
  fs.writeFileSync(pos[1], buf);
  out(`width=1280 height=800 bytes=${buf.length} url=${pos[0]}`);
} else {
  usage(`unknown command '${cmd}'`);
}

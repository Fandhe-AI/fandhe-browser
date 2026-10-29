// 28 タスクを Playwright / Puppeteer の API で 1 つの接続先に対して実行し、JSONL へ記録する（TASK-46・#252、
// ビヘイビア CDP-4。関連 CDP-2・CDP-3・MEAS-2・SEC-2）。手動専用（実サイトへ出る）。
// 呼び出し元は run.sh（接続先の起動・停止と npm の一時導入を担う）。単体でも起動済みの任意のループバック
// エンドポイントに対して実行できる。
//
// 「エンドポイント差し替えのみ」: 接続先は --endpoint と --target（記録用ラベル）だけが異なり、
// fandhe-browser 用・Chromium 用の分岐やオプションの違いは無い。
//
// 使い方:
//   node run_swap.mjs --library playwright|puppeteer --target <label> --endpoint http://127.0.0.1:<port>
//        --module-dir <導入先> --out <絶対パス> [--tasks P] [--only id,id] [--stage-timeout SEC] [--total-timeout SEC]
//        [--settle-ms N] [--wait-until domcontentloaded|load] [--puppeteer-connect ws|url]
//        [--puppeteer-viewport null|default] [--context-json '{...}'] [--validate-only]
// 終了コード: 0 = 記録完了（失敗タスクがあっても 0。成否は計測結果でありゲートではない）、
//            1 = 実行・書き込み失敗、2 = 入力・使用エラー（ライブラリ未導入を含む）。
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { playwrightAdapter, puppeteerAdapter, parseLoopbackEndpoint } from "./adapters.mjs";
import { ID_RE, LIBRARIES, runAll, validateTasks } from "./swap_lib.mjs";

const PACKAGE_OF = { playwright: "playwright-core", puppeteer: "puppeteer-core" };
const here = dirname(fileURLToPath(import.meta.url));

function usage(msg) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(2);
}

function intIn(name, v, lo, hi) {
  if (!/^[0-9]{1,6}$/.test(String(v))) usage(`${name} must be an integer in ${lo}..${hi}`);
  const n = Number(v);
  if (n < lo || n > hi) usage(`${name} must be an integer in ${lo}..${hi}`);
  return n;
}

export function parseArgs(argv) {
  const a = {
    library: null, target: null, endpoint: null, moduleDir: null, out: null, tasks: null, only: null,
    stageTimeout: 30, totalTimeout: 1800, settleMs: 2500, waitUntil: "domcontentloaded",
    puppeteerConnect: "ws", puppeteerViewport: "null", contextJson: null, validateOnly: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    const need = () => {
      if (i + 1 >= argv.length) usage(`${k} requires a value`);
      return argv[++i];
    };
    switch (k) {
      case "--library": a.library = need(); break;
      case "--target": a.target = need(); break;
      case "--endpoint": a.endpoint = need(); break;
      case "--module-dir": a.moduleDir = need(); break;
      case "--out": a.out = need(); break;
      case "--tasks": a.tasks = need(); break;
      case "--only": a.only = need(); break;
      case "--stage-timeout": a.stageTimeout = intIn(k, need(), 1, 120); break;
      case "--total-timeout": a.totalTimeout = intIn(k, need(), 1, 7200); break;
      case "--settle-ms": a.settleMs = intIn(k, need(), 0, 30000); break;
      case "--wait-until": a.waitUntil = need(); break;
      case "--puppeteer-connect": a.puppeteerConnect = need(); break;
      case "--puppeteer-viewport": a.puppeteerViewport = need(); break;
      case "--context-json": a.contextJson = need(); break;
      case "--validate-only": a.validateOnly = true; break;
      default: usage(`unknown argument: ${k}`);
    }
  }
  if (!LIBRARIES.includes(a.library)) usage("--library must be playwright or puppeteer");
  if (a.target === null || !/^[a-z0-9][a-z0-9-]{0,31}$/.test(a.target)) usage("--target must match ^[a-z0-9][a-z0-9-]{0,31}$");
  if (a.endpoint === null) usage("--endpoint is required");
  try {
    parseLoopbackEndpoint(a.endpoint);
  } catch (e) {
    usage(e.message);
  }
  if (!["domcontentloaded", "load"].includes(a.waitUntil)) usage("--wait-until must be domcontentloaded or load");
  if (!["ws", "url"].includes(a.puppeteerConnect)) usage("--puppeteer-connect must be ws or url");
  if (!["null", "default"].includes(a.puppeteerViewport)) usage("--puppeteer-viewport must be null or default");
  if (a.only !== null) {
    const ids = a.only.split(",");
    if (ids.some((x) => !ID_RE.test(x)) || new Set(ids).size !== ids.length) usage("--only has an invalid or duplicate id");
    a.ids = ids;
  }
  a.context = {};
  if (a.contextJson !== null) {
    if (a.contextJson.length > 2048) usage("--context-json is too long");
    try {
      const o = JSON.parse(a.contextJson);
      if (!o || typeof o !== "object" || Array.isArray(o)) throw new Error("not an object");
      for (const [key, v] of Object.entries(o)) {
        if (!/^[a-z][a-z0-9_]{0,39}$/.test(key) || !["string", "number", "boolean"].includes(typeof v) || String(v).length > 200) throw new Error("bad entry");
        a.context[key] = v;
      }
    } catch {
      usage("--context-json must be a flat JSON object of short scalar values");
    }
  }
  if (!a.validateOnly) {
    if (a.moduleDir === null) usage("--module-dir is required");
    if (a.out === null) usage("--out is required");
  }
  if (a.out !== null && !isAbsolute(a.out)) usage("--out must be an absolute path");
  return a;
}

function loadLibrary(library, moduleDir) {
  const pkg = PACKAGE_OF[library];
  const req = createRequire(join(resolve(moduleDir), "noop.js"));
  let mod;
  let version = null;
  try {
    mod = req(pkg);
    version = JSON.parse(readFileSync(join(resolve(moduleDir), "node_modules", pkg, "package.json"), "utf8")).version ?? null;
  } catch {
    usage(`cannot load ${pkg} from --module-dir (install it first; see README)`);
  }
  const lib = library === "playwright" ? mod.chromium : typeof mod.connect === "function" ? mod : mod.default;
  if (!lib) usage(`${pkg} has an unexpected export shape`);
  return { lib, version };
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  let tasks;
  const tasksPath = resolve(a.tasks ?? join(here, "..", "compat-practical", "tasks.json"));
  let raw;
  try {
    raw = readFileSync(tasksPath);
    tasks = validateTasks(JSON.parse(raw.toString("utf8")));
  } catch (e) {
    usage(`invalid tasks file: ${String(e.message).slice(0, 100)}`);
  }
  if (a.ids) {
    const picked = a.ids.map((id) => tasks.find((t) => t.id === id));
    if (picked.some((t) => t === undefined)) usage("--only has an id not in tasks");
    tasks = picked;
  }
  if (a.validateOnly) {
    process.stdout.write(`ok: ${tasks.length} tasks validated\n`);
    return;
  }
  if (existsSync(a.out)) usage("--out already exists (refusing to overwrite)");
  const { lib, version } = loadLibrary(a.library, a.moduleDir);
  const stageTimeoutMs = a.stageTimeout * 1000;
  const makeAdapter = () =>
    a.library === "playwright"
      ? playwrightAdapter({ chromium: lib, endpoint: a.endpoint, stageTimeoutMs, waitUntil: a.waitUntil })
      : puppeteerAdapter({
          puppeteer: lib, endpoint: a.endpoint, stageTimeoutMs, waitUntil: a.waitUntil,
          viewport: a.puppeteerViewport, connectMode: a.puppeteerConnect,
        });
  const lines = [];
  const results = await runAll(makeAdapter, tasks, {
    stageTimeoutMs, settleMs: a.settleMs, totalTimeoutMs: a.totalTimeout * 1000, library: a.library, target: a.target,
    // 進捗にはページ由来テキストを出さない（CI ログへのコマンド注入対策）。id・成否・理由コードのみ
    onResult: (r) => process.stderr.write(`[${a.library}/${a.target}/${r.id}] success=${r.success} stage=${r.failed_stage ?? "-"} reason=${r.reason ?? "-"}\n`),
  });
  const browsers = [...new Set(results.map((r) => r.browser).filter((b) => b))];
  lines.push({
    type: "meta", schema_version: 1, measured_at: new Date().toISOString(), library: a.library, library_package: PACKAGE_OF[a.library],
    library_version: version, target: a.target, browser: browsers[0] ?? null,
    tasks_sha256: createHash("sha256").update(raw).digest("hex"), task_count: tasks.length,
    options: {
      stage_timeout_sec: a.stageTimeout, total_timeout_sec: a.totalTimeout, settle_ms: a.settleMs, wait_until: a.waitUntil,
      puppeteer_connect: a.library === "puppeteer" ? a.puppeteerConnect : null,
      puppeteer_viewport: a.library === "puppeteer" ? a.puppeteerViewport : null,
    },
    os: process.platform, node: process.version, context: a.context,
  });
  lines.push(...results);
  writeFileSync(a.out, lines.map((l) => JSON.stringify(l)).join("\n") + "\n", { flag: "wx" });
  const ok = results.filter((r) => r.success).length;
  process.stdout.write(`success=${ok}/${results.length}\n`);
  // 切断しきれないソケットがあってもプロセスを残さない
  process.exit(0);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((e) => {
    process.stderr.write(`error: ${String(e).slice(0, 300)}\n`);
    process.exit(1);
  });
}

// tasks.json の判定を Chromium（playwright-core）で実行し、Chromium 参照データを作る（TASK-71.3・TASK-89・MEAS-2。
// 関連 COMPAT-4）。手動専用（実サイトへ出る）。CI では引数検証だけを self-test.sh が確かめる。
//
// 役割: reference/chromium_results.json のうち PoC-9 の生データを転記できない id（28 サイト化で追加した
// c4・d5・d7・d8・e3・e6）を、PoC-9 の run_chromium.mjs と同じ手順・同じ判定で作る。出力は
// make_matrix.sh が読む {id, cat, kind, success, error} の配列（PoC-9 転記分と同じ形。sample・url は載せない）。
// 判定は PoC-9 と同一: domcontentloaded 後に 2500ms 待機（timeout 15000）、text は innerText 非空、
// texts は一致件数 > 0、form は配下の input/select/textarea が 1 件以上。
//
// 依存: playwright-core（scripts/versions.sh の exact 固定版。harness/playwright-connect/run.sh と同じ
// 手順で mktemp -d へ一時導入し、FANDHE_PLAYWRIGHT_MODULE_DIR に渡す。新規依存ではない）。
// ブラウザ本体は取得せず、FANDHE_CHROMIUM_EXECUTABLE（または --executable）で既存の Chromium を指す。
// UA は変更しない（Chromium 既定のまま。偽装・anti-bot 回避はしない。SEC-2）。
//
// 使い方:
//   node chromium_reference.mjs --only c4,d5 --out /abs/path/out.json [--tasks P] [--module-dir D]
//        [--executable P] [--probe]
//   --probe: 一致件数・先頭一致の要素の要約を stderr だけへ出す（selector 確認用。ページ由来テキストを含みうる
//            ため、コミット・レポートへ転記しない）。
// 終了コード: 0 = 記録完了（success=false でも 0）、1 = 実行・書き込み失敗、2 = 入力・使用エラー。
import { createRequire } from "node:module";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
const KINDS = new Set(["text", "texts", "form"]);

function usage(msg) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(2);
}

/** 引数を検証して返す。実ブラウザには触れない（self-test.sh から --validate-only で呼ぶ）。 */
export function parseArgs(argv) {
  const a = { only: null, out: null, tasks: null, moduleDir: null, executable: null, probe: false, validateOnly: false };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    const need = () => {
      if (i + 1 >= argv.length) usage(`${k} requires a value`);
      return argv[++i];
    };
    if (k === "--only") a.only = need();
    else if (k === "--out") a.out = need();
    else if (k === "--tasks") a.tasks = need();
    else if (k === "--module-dir") a.moduleDir = need();
    else if (k === "--executable") a.executable = need();
    else if (k === "--probe") a.probe = true;
    else if (k === "--validate-only") a.validateOnly = true;
    else usage(`unknown argument: ${k}`);
  }
  if (a.only === null) usage("--only <id,id,...> is required (PoC-9 raw data is not regenerated here)");
  const ids = a.only.split(",");
  if (ids.length === 0 || ids.some((x) => !ID_RE.test(x))) usage("--only has an invalid id");
  if (new Set(ids).size !== ids.length) usage("--only has duplicate ids");
  a.ids = ids;
  if (!a.validateOnly && a.out === null) usage("--out is required");
  if (a.out !== null && !isAbsolute(a.out)) usage("--out must be an absolute path");
  return a;
}

export function loadTasks(path, ids) {
  let tasks;
  try {
    tasks = JSON.parse(readFileSync(path, "utf8"));
  } catch {
    usage("tasks file is not readable JSON");
  }
  if (!Array.isArray(tasks)) usage("tasks file must be a JSON array");
  const picked = ids.map((id) => tasks.find((t) => t && t.id === id));
  const missing = ids.filter((_, i) => picked[i] === undefined);
  if (missing.length > 0) usage(`ids not in tasks: ${missing.join(",")}`);
  for (const t of picked) {
    if (!KINDS.has(t.kind) || typeof t.selector !== "string" || t.selector === "" || !/^https:\/\//.test(t.url) || !ID_RE.test(t.cat)) {
      usage(`task ${t.id} has an invalid field`);
    }
  }
  return picked;
}

async function runTask(browser, t, probe) {
  const page = await browser.newPage();
  let success = false;
  let error = null;
  try {
    await page.goto(t.url, { waitUntil: "domcontentloaded", timeout: 15000 });
    await page.waitForTimeout(2500);
    if (t.kind === "text") {
      const el = await page.$(t.selector);
      if (el) success = (await el.innerText()).trim().length > 0;
    } else if (t.kind === "texts") {
      const els = await page.$$(t.selector);
      success = els.length > 0;
      if (probe) process.stderr.write(`[probe ${t.id}] matches=${els.length}\n`);
    } else {
      const el = await page.$(t.selector);
      if (el) success = (await el.$$("input, select, textarea")).length > 0;
    }
    if (probe) {
      const el = await page.$(t.selector);
      const head = el ? (await el.evaluate((n) => n.outerHTML)).replace(/\s+/g, " ").slice(0, 200) : "(no match)";
      process.stderr.write(`[probe ${t.id}] title=${JSON.stringify(await page.title())} first=${head}\n`);
    }
  } catch (e) {
    error = String(e).slice(0, 200);
  }
  await page.close();
  return { id: t.id, cat: t.cat, kind: t.kind, success, error };
}

async function main() {
  const a = parseArgs(process.argv.slice(2));
  const here = dirname(fileURLToPath(import.meta.url));
  const tasks = loadTasks(resolve(a.tasks ?? join(here, "tasks.json")), a.ids);
  if (a.validateOnly) {
    process.stdout.write(`ok: ${tasks.length} tasks validated\n`);
    return;
  }
  if (existsSync(a.out)) usage("--out already exists (refusing to overwrite)");
  const moduleDir = a.moduleDir ?? process.env.FANDHE_PLAYWRIGHT_MODULE_DIR ?? "";
  if (moduleDir === "") usage("--module-dir or FANDHE_PLAYWRIGHT_MODULE_DIR is required");
  const executable = a.executable ?? process.env.FANDHE_CHROMIUM_EXECUTABLE ?? "";
  if (executable === "") usage("--executable or FANDHE_CHROMIUM_EXECUTABLE is required");
  const { chromium } = createRequire(join(resolve(moduleDir), "noop.js"))("playwright-core");
  const browser = await chromium.launch({ executablePath: executable });
  const results = [];
  try {
    for (const t of tasks) {
      const r = await runTask(browser, t, a.probe);
      process.stderr.write(`[${r.id}] success=${r.success}${r.error ? " ERROR" : ""}\n`);
      results.push(r);
    }
  } finally {
    await browser.close();
  }
  writeFileSync(a.out, `[\n${results.map((r) => `  ${JSON.stringify(r)}`).join(",\n")}\n]\n`, { flag: "wx" });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((e) => {
    process.stderr.write(`error: ${String(e).slice(0, 300)}\n`);
    process.exit(1);
  });
}

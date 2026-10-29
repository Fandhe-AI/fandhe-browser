// run_swap.mjs / run.sh の JSONL をライブラリ別・接続先別に集計する（TASK-46・#252、ビヘイビア CDP-4）。
// 呼び出し元は run.sh（実行後）と人手。ネットワーク・ライブラリ不要。60% の達成判定はしない
// （判定は人間の役割。率の高低によらず exit 0）。Playwright と Puppeteer は別々の分母で集計する。
// a6（403）・c1（anti-bot）などの失敗は分母に残す（回避しない。SEC-2）。
//
// 使い方: node summarize.mjs [--tasks P] [--json-out ABS_PATH] <results.jsonl>...
// 終了コード: 0 = 集計完了、1 = --json-out の書き込み失敗、2 = 入力・使用エラー（id 集合の欠落・重複を含む）。
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { formatSummary, summarize, THRESHOLD_PERCENT, validateTasks } from "./swap_lib.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const MAX_BYTES = 8 * 1024 * 1024;

function usage(msg) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(2);
}

export function readJsonl(path) {
  let text;
  try {
    const buf = readFileSync(path);
    if (buf.length > MAX_BYTES) throw new Error("too large");
    text = buf.toString("utf8");
  } catch {
    usage("cannot read a results file (missing or larger than 8 MiB)");
  }
  const out = [];
  for (const line of text.split("\n")) {
    if (line.trim() === "") continue;
    try {
      out.push(JSON.parse(line));
    } catch {
      usage("a results file has a line that is not JSON");
    }
  }
  return out;
}

function main() {
  const argv = process.argv.slice(2);
  let tasksPath = join(here, "..", "compat-practical", "tasks.json");
  let jsonOut = null;
  const files = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--tasks") {
      if (i + 1 >= argv.length) usage("--tasks requires a value");
      tasksPath = argv[++i];
    } else if (argv[i] === "--json-out") {
      if (i + 1 >= argv.length) usage("--json-out requires a value");
      jsonOut = argv[++i];
    } else if (argv[i].startsWith("--")) {
      usage(`unknown argument: ${argv[i]}`);
    } else {
      files.push(argv[i]);
    }
  }
  if (files.length === 0) usage("at least one results file is required");
  if (jsonOut !== null && !isAbsolute(jsonOut)) usage("--json-out must be an absolute path");
  if (jsonOut !== null && existsSync(jsonOut)) usage("--json-out already exists (refusing to overwrite)");
  let tasks;
  try {
    tasks = validateTasks(JSON.parse(readFileSync(resolve(tasksPath), "utf8")));
  } catch (e) {
    usage(`invalid tasks file: ${String(e.message).slice(0, 100)}`);
  }
  const lines = files.flatMap((f) => readJsonl(f));
  let groups;
  try {
    groups = summarize(lines, tasks);
  } catch (e) {
    usage(String(e.message).slice(0, 300));
  }
  process.stdout.write(formatSummary(groups));
  process.stdout.write(`note: the ${THRESHOLD_PERCENT}% criterion (CDP-4) is judged separately per library by a human; this script does not judge\n`);
  if (jsonOut !== null) {
    try {
      writeFileSync(jsonOut, JSON.stringify({ schema_version: 1, threshold_percent_reference: THRESHOLD_PERCENT, groups }, null, 2) + "\n", { flag: "wx" });
    } catch {
      process.stderr.write("error: cannot write --json-out\n");
      process.exit(1);
    }
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();

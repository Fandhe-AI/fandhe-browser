// エンドポイント差し替え測定の共通ロジック（TASK-46・#252、ビヘイビア CDP-4。関連 CDP-2・CDP-3・MEAS-2・SEC-2）。
// 呼び出し元は run_swap.mjs（実行）・summarize.mjs（集計）・self-test.mjs（偽クライアントでのオフライン検証）。
//
// 役割: tasks.json の 1 タスクを「ライブラリの API だけ」で実行して成否を判定する純粋ロジック。
// ライブラリ差（Playwright / Puppeteer）は adapters.mjs のアダプタが吸収し、本ファイルは接続先を知らない。
// つまり fandhe-browser と Chromium は同一のコードパスで判定され、接続先ごとの分岐・回避は存在しない。
//
// 成功判定は compat-practical/run_core.mjs と同じ基準（同一 selector・同一の期待）:
//   text  : 先頭一致要素の textContent が空白除去後に非空
//   texts : 先頭一致 1 件の textContent が空白除去後に非空（件数は捏造しない。method: "first_match"）
//   form  : 先頭一致要素の子孫に name 属性が非空の input / textarea / select が 1 件以上
// reason の語彙も run_core と揃える: no_match / empty_result / timeout / fetch_error / cdp_error / blocked。
// 追加の reason は http_error（goto の応答が 4xx・5xx。下記）。
// 追加の failed_stage は connect / newContext（Playwright のみ）/ newPage / goto / selector。
//
// HTTP ステータス条件（両接続先・両ライブラリに共通。403・anti-bot の失敗を失敗として数える契約。SEC-2・SEC-3）:
//   goto の応答が 4xx・5xx なら要素取得の結果にかかわらず失敗（failed_stage: goto・reason: http_error・detail: "HTTP <status>"）。
//   ステータスが取れない（応答 null。fandhe-browser は現状 http_status: null）場合は「判定不能」としてステータス条件だけを
//   スキップし、結果に http_status_check: "skipped_no_status" を残す。null を一律失敗にすると接続先によって
//   構造的に不利になり、一律成功にすると 403 を水増しするため、スキップを記録して後から数え直せるようにする。
//   記録値は http_status_check: "passed"（2xx・3xx）/ "failed"（4xx・5xx）/ "skipped_no_status"（取得不能）/ null（goto 未到達）。

export const POC22_EXCLUDED = Object.freeze(["c4", "d5", "d7", "d8", "e3", "e6"]);
export const LIBRARIES = Object.freeze(["playwright", "puppeteer"]);
export const ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
export const KINDS = new Set(["text", "texts", "form"]);
export const SAMPLE_MAX = 300;
export const THRESHOLD_PERCENT = 60;

/** goto の応答ステータスを正規化する。HTTP として妥当な 100〜599 の整数だけを返し、それ以外は null（判定不能）。 */
export function normalizeHttpStatus(status) {
  return Number.isInteger(status) && status >= 100 && status <= 599 ? status : null;
}

/** ステータス条件の判定結果（"passed" / "failed" / "skipped_no_status"）。null は判定不能でスキップ。 */
export function httpStatusCheck(status) {
  if (status === null) return "skipped_no_status";
  return status >= 400 ? "failed" : "passed";
}

// ページ内で評価する関数。toString で直列化されるため、閉包・外部参照を持たない自己完結の形にする。
// 全アダプタ・全接続先で同じ関数を渡す（接続先ごとに差し替えない）。
export const PAGE_FN_TEXT = (n) => n.textContent ?? "";
export const PAGE_FN_FORM_FIELDS = (n) => {
  let c = 0;
  for (const e of n.querySelectorAll("input,textarea,select")) {
    const v = e.getAttribute("name");
    if (v !== null && v !== "") c++;
  }
  return c;
};

/** tasks.json を読み込んだ配列を検証して返す。違反は Error（呼び出し側が exit 2 にする）。 */
export function validateTasks(tasks) {
  if (!Array.isArray(tasks) || tasks.length === 0) throw new Error("tasks must be a non-empty array");
  const seen = new Set();
  for (const t of tasks) {
    if (!t || typeof t !== "object") throw new Error("task must be an object");
    if (!ID_RE.test(String(t.id)) || !ID_RE.test(String(t.cat))) throw new Error(`task has invalid id/cat: ${String(t.id).slice(0, 20)}`);
    if (seen.has(t.id)) throw new Error(`duplicate task id: ${t.id}`);
    seen.add(t.id);
    if (!KINDS.has(t.kind)) throw new Error(`task ${t.id} has invalid kind`);
    if (typeof t.selector !== "string" || t.selector === "") throw new Error(`task ${t.id} has empty selector`);
    if (typeof t.url !== "string" || !/^https:\/\/[^\s\u0000-\u001f]+$/.test(t.url)) throw new Error(`task ${t.id} has invalid url`);
  }
  return tasks;
}

/** PoC-1 由来の 22 件（28 件から 28 サイト化で追加した 6 件を除いたもの）に含まれるか。 */
export function inPoc22(id) {
  return !POC22_EXCLUDED.includes(id);
}

export function sanitizeSample(s) {
  // eslint-disable-next-line no-control-regex
  return String(s).replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim().slice(0, SAMPLE_MAX);
}

export function errorSummary(e) {
  const name = String(e?.name ?? "Error").replace(/[^A-Za-z0-9_.$-]/g, "").slice(0, 60) || "Error";
  const message = sanitizeSample(e?.message ?? e).slice(0, 160);
  return { name, message };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function isTimeoutError(e) {
  return e?.name === "StageTimeout" || e?.name === "TimeoutError";
}

/** 段階を期限と競わせる。敗者側の遅延 reject は購読して unhandledRejection を防ぐ。 */
export async function withTimeout(promise, ms) {
  Promise.resolve(promise).catch(() => {});
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

/** goto 後の要素取得と成否判定。rec の success / failed_stage / reason / match_count / output_sample を更新する。 */
async function judgeSelector(adapter, task, rec, stageTimeoutMs) {
  const handle = await withTimeout(adapter.queryFirst(task.selector), stageTimeoutMs);
  if (handle === null || handle === undefined) {
    rec.failed_stage = "selector";
    rec.reason = "no_match";
  } else if (task.kind === "form") {
    const n = await withTimeout(adapter.evaluate(handle, PAGE_FN_FORM_FIELDS), stageTimeoutMs);
    rec.match_count = Number.isInteger(n) ? n : 0;
    if (rec.match_count > 0) {
      rec.success = true;
      rec.output_sample = `${rec.match_count} named fields`;
    } else {
      rec.failed_stage = "selector";
      rec.reason = "empty_result";
    }
  } else {
    const text = String(await withTimeout(adapter.evaluate(handle, PAGE_FN_TEXT), stageTimeoutMs));
    if (text.replace(/\s+/g, "") === "") {
      rec.failed_stage = "selector";
      rec.reason = "empty_result";
    } else {
      rec.success = true;
      rec.output_sample = sanitizeSample(text);
    }
  }
}

/**
 * 1 タスクを実行して結果レコード（JSON 化可能）を返す。例外は投げない。
 * adapter: { stages, connect, newContext?, newPage, goto, queryFirst, evaluate, version, cleanup }（adapters.mjs）
 * 1 タスク = 1 本の独立したスクリプト（接続 → ページ作成 → goto → 取得 → 切断）として実行する。
 */
export async function executeTask(adapter, task, { stageTimeoutMs, settleMs, library, target }) {
  const t0 = performance.now();
  const rec = {
    type: "result", library, target, id: task.id, cat: task.cat, kind: task.kind, url: task.url, selector: task.selector,
    in_poc22: inPoc22(task.id), success: false, failed_stage: null, reached_stage: null, reason: null, detail: null,
    browser: null, http_status: null, http_status_check: null, method: task.kind === "form" ? "form_fields" : "first_match",
    match_count: null, elapsed_ms: 0, output_sample: "",
  };
  let stage = "connect";
  try {
    stage = "connect";
    await withTimeout(adapter.connect(), stageTimeoutMs);
    rec.reached_stage = "connect";
    try {
      const v = await withTimeout(Promise.resolve(adapter.version()), 3000);
      rec.browser = typeof v === "string" ? sanitizeSample(v).slice(0, 80) : null;
    } catch {
      // バージョン取得の失敗は成否に影響しない（記録が null になるだけ）
    }
    if (adapter.stages.includes("newContext")) {
      stage = "newContext";
      await withTimeout(adapter.newContext(), stageTimeoutMs);
      rec.reached_stage = "newContext";
    }
    stage = "newPage";
    await withTimeout(adapter.newPage(), stageTimeoutMs);
    rec.reached_stage = "newPage";
    stage = "goto";
    const status = await withTimeout(adapter.goto(task.url), stageTimeoutMs);
    rec.http_status = normalizeHttpStatus(status);
    rec.http_status_check = httpStatusCheck(rec.http_status);
    rec.reached_stage = "goto";
    if (rec.http_status_check === "failed") {
      // HTTP エラー応答は要素の有無にかかわらず失敗（エラーページの `a` 等を成功に数えない）
      rec.failed_stage = "goto";
      rec.reason = "http_error";
      rec.detail = `HTTP ${rec.http_status}`;
    } else {
      if (settleMs > 0) await sleep(settleMs);
      stage = "selector";
      await judgeSelector(adapter, task, rec, stageTimeoutMs);
      if (rec.success) rec.reached_stage = "selector";
    }
  } catch (e) {
    rec.failed_stage = stage;
    rec.reason = isTimeoutError(e) ? "timeout" : stage === "goto" ? "fetch_error" : "cdp_error";
    const s = errorSummary(e);
    rec.detail = `${s.name}: ${s.message}`;
  }
  // 後始末は期限付き・失敗は無視（先行段階の結果を上書きしない）
  try {
    await withTimeout(adapter.cleanup(), Math.min(stageTimeoutMs, 10000));
  } catch {
    // 切断失敗は成否に影響しない
  }
  rec.elapsed_ms = Math.round(performance.now() - t0);
  return rec;
}

/** 全タスクを直列実行する。全体期限を超えたら以降は実行せず reason: "blocked" で記録する。 */
export async function runAll(makeAdapter, tasks, { stageTimeoutMs, settleMs, totalTimeoutMs, library, target, onResult = () => {} }) {
  const deadline = performance.now() + totalTimeoutMs;
  const results = [];
  for (const task of tasks) {
    let r;
    if (performance.now() >= deadline) {
      r = {
        type: "result", library, target, id: task.id, cat: task.cat, kind: task.kind, url: task.url, selector: task.selector,
        in_poc22: inPoc22(task.id), success: false, failed_stage: null, reached_stage: null, reason: "blocked",
        detail: "total time limit exceeded", browser: null, http_status: null, http_status_check: null, method: task.kind === "form" ? "form_fields" : "first_match",
        match_count: null, elapsed_ms: 0, output_sample: "",
      };
    } else {
      r = await executeTask(makeAdapter(), task, { stageTimeoutMs, settleMs, library, target });
    }
    results.push(r);
    onResult(r);
  }
  return results;
}

// ---- 集計（summarize.mjs が使う）----

/** 成功数・全体数から小数 1 桁の率（文字列）を整数演算で返す。全体 0 は "n/a"。 */
export function ratePercent(success, total) {
  if (total === 0) return "n/a";
  const x = Math.round((success * 1000) / total);
  return `${Math.floor(x / 10)}.${x % 10}`;
}

/**
 * JSONL の行（パース済みオブジェクト）を (library, target) ごとに集計する。
 * 各グループが tasks の id 集合とちょうど一致しない（欠落・重複・未知 id）場合は Error（fail-closed。
 * 欠けた結果を分母から黙って落とさない）。a6（403）・c1（anti-bot）などの失敗も分母に残す。
 */
export function summarize(lines, tasks) {
  const wantIds = tasks.map((t) => t.id);
  const groups = new Map();
  for (const l of lines) {
    if (!l || l.type !== "result") continue;
    if (!LIBRARIES.includes(l.library)) throw new Error(`unknown library: ${String(l.library).slice(0, 20)}`);
    if (typeof l.target !== "string" || l.target === "") throw new Error("result without target");
    if (typeof l.success !== "boolean") throw new Error(`result ${l.id} has non-boolean success`);
    const key = `${l.library}\u0000${l.target}`;
    if (!groups.has(key)) groups.set(key, { library: l.library, target: l.target, rows: [] });
    groups.get(key).rows.push(l);
  }
  if (groups.size === 0) throw new Error("no result lines");
  const out = [];
  for (const g of groups.values()) {
    const ids = g.rows.map((r) => r.id);
    const dup = ids.filter((id, i) => ids.indexOf(id) !== i);
    if (dup.length > 0) throw new Error(`${g.library}/${g.target}: duplicate ids: ${[...new Set(dup)].join(",")}`);
    const missing = wantIds.filter((id) => !ids.includes(id));
    const unknown = ids.filter((id) => !wantIds.includes(id));
    if (missing.length > 0 || unknown.length > 0) {
      throw new Error(`${g.library}/${g.target}: id set mismatch (missing: ${missing.join(",") || "-"}; unknown: ${unknown.join(",") || "-"})`);
    }
    const sets = {};
    for (const [name, pred] of [["28", () => true], ["22", (r) => inPoc22(r.id)]]) {
      const rows = g.rows.filter(pred);
      const success = rows.filter((r) => r.success).length;
      sets[name] = { success, total: rows.length, rate: ratePercent(success, rows.length) };
    }
    const failures = g.rows.filter((r) => !r.success);
    const count = (key) => {
      const m = {};
      for (const f of failures) {
        const k = f[key] ?? "none";
        m[k] = (m[k] ?? 0) + 1;
      }
      return Object.fromEntries(Object.entries(m).sort(([a], [b]) => a.localeCompare(b)));
    };
    out.push({
      library: g.library, target: g.target, sets,
      failed_stage: count("failed_stage"), reason: count("reason"),
      failed_ids: failures.map((f) => f.id).sort(),
      success_ids: g.rows.filter((r) => r.success).map((r) => r.id).sort(),
      // 成功のうちステータスが取れず HTTP エラー判定をスキップした id（成功率の読み手が数え直せるよう別掲する）
      status_unchecked_success_ids: g.rows.filter((r) => r.success && r.http_status_check === "skipped_no_status").map((r) => r.id).sort(),
    });
  }
  out.sort((a, b) => a.library.localeCompare(b.library) || a.target.localeCompare(b.target));
  // 参考値: 同一ライブラリで基準側（target=chromium）が成功した id のうち、他の接続先も成功した件数。判定の分母には使わない
  for (const g of out) {
    const base = out.find((o) => o.library === g.library && o.target === "chromium");
    if (base && g.target !== "chromium") {
      const hit = base.success_ids.filter((id) => g.success_ids.includes(id)).length;
      g.of_chromium_success = { success: hit, total: base.success_ids.length, rate: ratePercent(hit, base.success_ids.length) };
    }
  }
  return out;
}

/** summarize の結果を 1 行 1 区分のテキストにする（判定はしない。60% との照合は人間の役割）。 */
export function formatSummary(groups) {
  const lines = [];
  for (const g of groups) {
    const head = `library=${g.library} target=${g.target}`;
    for (const name of ["28", "22"]) {
      const s = g.sets[name];
      lines.push(`${head} set=${name} success=${s.success}/${s.total} rate=${s.rate}%`);
    }
    const fmt = (m) => Object.entries(m).map(([k, v]) => `${k}=${v}`).join(",") || "-";
    lines.push(`${head} failed_stage ${fmt(g.failed_stage)}`);
    lines.push(`${head} reason ${fmt(g.reason)}`);
    lines.push(`${head} failed_ids ${g.failed_ids.join(",") || "-"}`);
    lines.push(`${head} status_unchecked_success ${g.status_unchecked_success_ids.join(",") || "-"} (HTTP status unavailable; error-status check skipped)`);
    if (g.of_chromium_success) {
      const c = g.of_chromium_success;
      lines.push(`${head} of_chromium_success=${c.success}/${c.total} rate=${c.rate}% (reference only; not a denominator)`);
    }
  }
  return lines.join("\n") + "\n";
}

/**
 * npm install 後の .package-lock.json を検証する。
 * playwright-core は単独導入（推移的依存なし）を要求する。puppeteer-core は推移的依存を持つため
 * allowTransitive で許し、最上位の版と integrity だけを固定検証する（推移的依存は結果の meta に記録する）。
 */
export function verifyInstall(lock, { name, version, integrity, allowTransitive }) {
  const pkgs = lock && typeof lock === "object" ? lock.packages : null;
  if (!pkgs || typeof pkgs !== "object") throw new Error("lockfile has no packages");
  const want = `node_modules/${name}`;
  const keys = Object.keys(pkgs).filter((k) => k !== "");
  const extra = keys.filter((k) => k !== want);
  if (!allowTransitive && extra.length > 0) throw new Error(`unexpected packages installed: ${extra.slice(0, 5).join(", ")}`);
  const entry = pkgs[want];
  if (!entry) throw new Error(`${name} was not installed`);
  if (entry.version !== version) throw new Error(`version mismatch: expected ${version}, got ${entry.version}`);
  if (entry.integrity !== integrity) throw new Error("integrity mismatch");
  return keys
    .map((k) => `${k.replace(/^.*node_modules\//, "")}@${pkgs[k]?.version ?? "?"}`)
    .sort();
}

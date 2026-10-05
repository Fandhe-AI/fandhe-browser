// Playwright トレース収集の純粋関数群（TASK-43.1・#246、ビヘイビア CDP-2・MS-4）。
//
// trace.mjs（実 Playwright 駆動）と self-test.mjs（オフライン自己テスト）の共通部品で、
// ネットワーク・プロセス起動・ファイル I/O を持たない。ここに判定ロジックを集めることで、
// Playwright 非導入でも引数検証・ログ解析・正規化を自己テストできる。

export const SCHEMA_VERSION = 1;
export const PLACEHOLDER_PORT = "<PORT>";
// 1 行あたり・総行数の上限（無制限確保の防止。security.md「不安全な設計」）。
export const MAX_LINE_BYTES = 64 * 1024;
export const MAX_RECORDS = 1024;
const ANSI = /\u001b\[[0-9;]*m/g;

/** loopback の IP リテラル（127.0.0.1 / [::1]）かを判定する。ホスト名（localhost 含む）は拒否。 */
export function isLoopbackHost(hostname) {
  return hostname === "127.0.0.1" || hostname === "[::1]";
}

/**
 * `--endpoint` を検証する。http(s)/ws(s) の loopback IP リテラルのみ許可し、
 * 外部ホストへの接続（SSRF）を防ぐ。不正なら Error を投げる。
 */
export function validateEndpoint(raw) {
  let url;
  try {
    url = new URL(raw);
  } catch {
    throw new Error(`invalid endpoint URL: ${raw}`);
  }
  if (!["http:", "ws:"].includes(url.protocol)) {
    throw new Error(`endpoint scheme must be http or ws: ${url.protocol}`);
  }
  if (!isLoopbackHost(url.hostname)) {
    throw new Error(`endpoint host must be a loopback IP literal: ${url.hostname}`);
  }
  if (url.port === "") {
    throw new Error("endpoint must include an explicit port");
  }
  return url;
}

/** `--out` を検証する。空・NUL・`-` 始まり（オプション誤認）を拒否する。 */
export function validateOutPath(raw) {
  if (typeof raw !== "string" || raw === "" || raw.includes("\0") || raw.startsWith("-")) {
    throw new Error(`invalid output path: ${JSON.stringify(raw)}`);
  }
  return raw;
}

/**
 * DEBUG=pw:protocol の 1 行（`<ISO 時刻> pw:protocol SEND ► {json}` /
 * `... ◀ RECV {json}`）を解析する。対象外の行は null、JSON 不正は Error。
 */
export function parseProtocolLine(rawLine) {
  // TTY では debug が namespace と SEND/RECV の間へ色リセットを挟むため、照合前に ANSI を除去する。
  const line = rawLine.replace(ANSI, "");
  const m = /pw:protocol (SEND ►|◀ RECV) (\{.*\})\s*$/.exec(line);
  if (m === null) {
    // pw:protocol 行なのに形式が合わない（ログ形式の変更等）まま捨てると CDP メッセージが
    // 欠落した JSONL が残るため、対象外ではなく解析失敗として扱う。
    if (line.includes("pw:protocol")) throw new Error("unrecognized pw:protocol line format");
    return null;
  }
  if (Buffer.byteLength(m[2]) > MAX_LINE_BYTES) {
    throw new Error("protocol line exceeds size limit");
  }
  return { dir: m[1] === "SEND ►" ? "send" : "recv", message: JSON.parse(m[2]) };
}

// 1 行（タイムスタンプ等の接頭辞込み）の許容バイト数。
export const MAX_RAW_LINE_BYTES = MAX_LINE_BYTES * 2;
/** 1 回の stderr 書き込みとして受け付ける最大バイト数（分割前の割り当て量を抑える）。 */
export const MAX_CHUNK_BYTES = 1024 * 1024;

/**
 * stderr チャンクを残余バッファへ連結し、完結した行と新しい残余を返す。
 * 完結行・残余のいずれかが上限を超えたら Error を投げる（改行を含む巨大書き込みも
 * 行ごとに検証する。無制限バッファ防止）。
 */
export function drainLines(pending, chunk) {
  // 連結・分割で大きな配列を確保する前に、チャンク単体と連結後の総量を上限検証する。
  if (Buffer.byteLength(chunk) > MAX_CHUNK_BYTES) throw new Error("protocol log chunk exceeds buffer limit");
  const parts = (pending + chunk).split("\n");
  const rest = parts.pop();
  for (const l of parts) {
    if (Buffer.byteLength(l) > MAX_RAW_LINE_BYTES) throw new Error("protocol log line exceeds buffer limit");
  }
  if (Buffer.byteLength(rest) > MAX_RAW_LINE_BYTES) throw new Error("protocol log line exceeds buffer limit");
  return { lines: parts, rest };
}

/**
 * 文字列中の接続先（host:port）・ANSI を固定表現へ置換する。
 * `host` は endpoint の URL ホスト表記（`127.0.0.1` / `[::1]`）。host:port に一致し、ポート直後が
 * 数字でない箇所だけを置換するため、無関係な URL・数値（例: `example.com:12345`）は変えない。
 * host 未指定時はポート置換を行わない。
 */
export function normalizeString(s, port, host) {
  let out = s.replace(ANSI, "");
  if (port && host) {
    const esc = host.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const re = new RegExp(`(?<![0-9A-Za-z.\\-])${esc}:${port}(?![0-9])`, "g");
    out = out.replace(re, `${host}:${PLACEHOLDER_PORT}`);
  }
  return out;
}

/** JSON 値を再帰的に走査して全文字列を正規化する。 */
export function normalizeValue(v, port, host) {
  if (typeof v === "string") return normalizeString(v, port, host);
  if (Array.isArray(v)) return v.map((x) => normalizeValue(x, port, host));
  if (v !== null && typeof v === "object") {
    const o = {};
    for (const [k, x] of Object.entries(v)) {
      // `o[k] =` だと `__proto__` キーがプロトタイプ変更になり直列化で失われるため、own property として定義する。
      Object.defineProperty(o, k, { value: normalizeValue(x, port, host), enumerable: true, writable: true, configurable: true });
    }
    return o;
  }
  return v;
}

/** レコード列（meta 先頭）を seq 付き JSONL（LF 固定・末尾改行あり）へ直列化する。 */
export function toJsonl(records) {
  if (records.length > MAX_RECORDS) {
    throw new Error(`too many records: ${records.length}`);
  }
  return records.map((r, i) => JSON.stringify({ seq: i, ...r })).join("\n") + "\n";
}

/**
 * JSONL のスキーマを検証して問題の配列を返す（空なら適合）。
 * 先頭は meta、seq は 0 始まりの連番、kind は meta/http/cdp/stage、
 * cdp は dir(send|recv) と message オブジェクトを持つ。
 */
export function validateJsonl(text) {
  const problems = [];
  if (!text.endsWith("\n") || text.includes("\r")) problems.push("must be LF-terminated");
  const lines = text.split("\n").slice(0, -1);
  if (lines.length === 0 || lines.length > MAX_RECORDS) problems.push("line count out of range");
  lines.forEach((line, i) => {
    let r;
    try {
      r = JSON.parse(line);
    } catch {
      problems.push(`line ${i}: invalid JSON`);
      return;
    }
    if (r.seq !== i) problems.push(`line ${i}: seq must be ${i}`);
    if ((i === 0) !== (r.kind === "meta")) problems.push(`line ${i}: meta must be first only`);
    if (!["meta", "http", "cdp", "stage"].includes(r.kind)) problems.push(`line ${i}: bad kind`);
    if (r.kind === "cdp") {
      if (!["send", "recv"].includes(r.dir)) problems.push(`line ${i}: bad dir`);
      if (r.message === null || typeof r.message !== "object") problems.push(`line ${i}: bad message`);
    }
    if (r.kind === "stage" && (typeof r.name !== "string" || typeof r.ok !== "boolean")) {
      problems.push(`line ${i}: bad stage`);
    }
  });
  return problems;
}

/**
 * 収集結果を成果物として保存してよいか判定する（CDP-2・TASK-43.1、REPAIR-3）。
 * trace.mjs の書き込み直前から呼ばれる。CDP メッセージが 0 件のトレースは正常な収集結果ではない
 * ため理由付きで不成立を返す（fail-closed）。接続段階が失敗していても、失敗までの CDP 送受信が
 * 得られていれば調査用トレースとして有効（現行サーバーは接続ハンドシェイクで失敗する）。
 */
export function checkCollection({ cdpCount }) {
  if (cdpCount === 0) return { ok: false, reason: "no CDP messages were captured (pw:protocol output missing?)" };
  return { ok: true, reason: null };
}

/** HTTP discovery 応答本文の最大バイト数（/json/version は小さな JSON のため十分に小さく取る）。 */
export const MAX_DISCOVERY_BYTES = 64 * 1024;

/**
 * ReadableStream 由来の本文を上限付きで読み、超過時は Error を投げる。
 * trace.mjs の HTTP discovery から呼ばれる。確保前にサイズを検証するため res.json() は使わない。
 */
export async function readBodyLimited(res, maxBytes = MAX_DISCOVERY_BYTES) {
  if (!res.body) return "";
  const chunks = [];
  let total = 0;
  for await (const c of res.body) {
    total += c.byteLength;
    if (total > maxBytes) throw new Error("discovery response exceeds size limit");
    chunks.push(c);
  }
  return Buffer.concat(chunks).toString("utf8");
}

/**
 * discovery が返した WebSocket URL を検証する。loopback であることに加え、元の endpoint と
 * ホスト・ポートが一致しなければ拒否する（別ローカルサービスへの接続誘導を防ぐ。SSRF 対策）。
 */
export function validateDiscoveredWs(target, endpoint) {
  const u = validateEndpoint(target);
  if (u.hostname !== endpoint.hostname || u.port !== endpoint.port) {
    throw new Error("discovered WebSocket URL does not match the endpoint host and port");
  }
  return u;
}

/**
 * npm install 後の hidden lockfile（node_modules/.package-lock.json）を検証する。
 * 導入されたパッケージが期待する 1 つ（name・version・integrity 一致）だけであることを確認し、
 * 違反は Error を投げる（fail-closed）。verify-install.mjs（run.sh から呼ぶ）が使う。
 */
export function verifyInstalledPackages(lock, { name, version, integrity }) {
  const pkgs = lock && typeof lock === "object" ? lock.packages : null;
  if (!pkgs || typeof pkgs !== "object") throw new Error("lockfile has no packages");
  const keys = Object.keys(pkgs).filter((k) => k !== "");
  const want = `node_modules/${name}`;
  const extra = keys.filter((k) => k !== want);
  if (extra.length > 0) throw new Error(`unexpected packages installed: ${extra.join(", ")}`);
  const entry = pkgs[want];
  if (!entry) throw new Error(`${name} was not installed`);
  if (entry.version !== version) throw new Error(`version mismatch: expected ${version}, got ${entry.version}`);
  if (entry.integrity !== integrity) throw new Error("integrity mismatch");
}

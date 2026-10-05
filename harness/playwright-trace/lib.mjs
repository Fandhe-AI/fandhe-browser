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

/**
 * stderr チャンクを残余バッファへ連結し、完結した行と新しい残余を返す。
 * 完結行・残余のいずれかが上限を超えたら Error を投げる（改行を含む巨大書き込みも
 * 行ごとに検証する。無制限バッファ防止）。
 */
export function drainLines(pending, chunk) {
  const parts = (pending + chunk).split("\n");
  const rest = parts.pop();
  for (const l of parts) {
    if (Buffer.byteLength(l) > MAX_RAW_LINE_BYTES) throw new Error("protocol log line exceeds buffer limit");
  }
  if (Buffer.byteLength(rest) > MAX_RAW_LINE_BYTES) throw new Error("protocol log line exceeds buffer limit");
  return { lines: parts, rest };
}

/** 文字列中のポート・ANSI・ホームディレクトリ等を固定表現へ置換する。 */
export function normalizeString(s, port) {
  let out = s.replace(ANSI, "");
  if (port) {
    out = out.split(`:${port}`).join(`:${PLACEHOLDER_PORT}`);
  }
  return out;
}

/** JSON 値を再帰的に走査して全文字列を正規化する。 */
export function normalizeValue(v, port) {
  if (typeof v === "string") return normalizeString(v, port);
  if (Array.isArray(v)) return v.map((x) => normalizeValue(x, port));
  if (v !== null && typeof v === "object") {
    const o = {};
    for (const [k, x] of Object.entries(v)) o[k] = normalizeValue(x, port);
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

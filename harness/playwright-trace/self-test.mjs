// lib.mjs の自己テスト（TASK-43.1・#246、ビヘイビア CDP-2）。Playwright・ネットワーク不要。
// self-test.sh から呼ばれ、引数検証の拒否系・ログ解析・正規化・スキーマ検証を具体値で確認する。
// コミット済みの実トレースが現行スキーマに適合することも検証する（スキーマ破損の早期検出）。

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import assert from "node:assert/strict";
import {
  checkCollection,
  normalizeString,
  verifyInstalledPackages,
  normalizeValue,
  parseProtocolLine,
  drainLines,
  MAX_RAW_LINE_BYTES,
  MAX_CHUNK_BYTES,
  toJsonl,
  validateEndpoint,
  validateDiscoveredWs,
  readBodyLimited,
  MAX_DISCOVERY_BYTES,
  validateJsonl,
  validateOutPath,
} from "./lib.mjs";

let cases = 0;
const asyncCases = [];
const t = (name, fn) => {
  const r = fn();
  const done = () => {
    cases++;
    console.log(`ok: ${name}`);
  };
  // 非同期ケースの失敗（reject）も確実に終了コードへ反映するため、最後に await する。
  if (r && typeof r.then === "function") asyncCases.push(r.then(done));
  else done();
};

t("endpoint accepts loopback http/ws with port", () => {
  assert.equal(validateEndpoint("http://127.0.0.1:9333").port, "9333");
  assert.equal(validateEndpoint("ws://127.0.0.1:9333/devtools/browser/x").protocol, "ws:");
  assert.equal(validateEndpoint("http://[::1]:9333").hostname, "[::1]");
});

for (const bad of [
  "http://example.com:9333",
  "http://localhost:9333",
  "http://0.0.0.0:9333",
  "http://192.168.0.1:9333",
  "https://127.0.0.1:9333",
  "file:///etc/passwd",
  "http://127.0.0.1",
  "not a url",
  "",
]) {
  t(`endpoint rejects ${JSON.stringify(bad)}`, () => assert.throws(() => validateEndpoint(bad)));
}

t("out path rejects empty / option-like / NUL", () => {
  assert.equal(validateOutPath("out/x.jsonl"), "out/x.jsonl");
  for (const bad of ["", "-rf", "a\0b", undefined]) assert.throws(() => validateOutPath(bad));
});

t("parseProtocolLine parses SEND and RECV", () => {
  assert.deepEqual(
    parseProtocolLine('2026-10-05T10:09:13.132Z pw:protocol SEND ► {"id":1,"method":"Browser.getVersion"}'),
    { dir: "send", message: { id: 1, method: "Browser.getVersion" } },
  );
  assert.deepEqual(
    parseProtocolLine('2026-10-05T10:09:13.133Z pw:protocol ◀ RECV {"error":{"code":-32601},"id":1}'),
    { dir: "recv", message: { error: { code: -32601 }, id: 1 } },
  );
  assert.equal(parseProtocolLine("2026-10-05T10:09:13.126Z pw:api => started"), null);
  assert.throws(() => parseProtocolLine("pw:protocol SEND ► {broken}"));
});

t("parseProtocolLine tolerates ANSI sequences around SEND/RECV", () => {
  const r = parseProtocolLine('2026-10-05T10:09:13.132Z \u001b[0m\u001b[31mpw:protocol\u001b[0m SEND ► {"id":1}');
  assert.deepEqual(r, { dir: "send", message: { id: 1 } });
});

t("parseProtocolLine rejects pw:protocol lines in an unrecognized format", () => {
  assert.throws(() => parseProtocolLine("2026-10-05T10:09:13.132Z pw:protocol SENT >> {}"), /unrecognized/);
});

t("drainLines splits lines and keeps the unfinished remainder", () => {
  assert.deepEqual(drainLines("ab", "c\nd\ne"), { lines: ["abc", "d"], rest: "e" });
});

t("drainLines rejects oversized lines even when the chunk contains newlines", () => {
  const big = "x".repeat(MAX_RAW_LINE_BYTES + 1);
  assert.throws(() => drainLines("", `${big}\nshort\n`), /buffer limit/);
  assert.throws(() => drainLines("", `short\n${big}`), /buffer limit/);
});

t("normalize replaces port and strips ANSI", () => {
  assert.equal(normalizeString("ws://127.0.0.1:44337/x \u001b[2mdim\u001b[22m", "44337", "127.0.0.1"), "ws://127.0.0.1:<PORT>/x dim");
  assert.deepEqual(normalizeValue({ a: ["http://127.0.0.1:1234/"] }, "1234", "127.0.0.1"), { a: ["http://127.0.0.1:<PORT>/"] });
});

t("normalize only replaces the endpoint host:port", () => {
  assert.equal(normalizeString("http://example.com:12345/", "1234", "127.0.0.1"), "http://example.com:12345/");
  assert.equal(normalizeString("http://example.com:1234/", "1234", "127.0.0.1"), "http://example.com:1234/");
  assert.equal(normalizeString("at 12:1234 and 127.0.0.1:12345", "1234", "127.0.0.1"), "at 12:1234 and 127.0.0.1:12345");
  assert.equal(normalizeString("x ws://127.0.0.1:1234/a", "1234", "127.0.0.1"), "x ws://127.0.0.1:<PORT>/a");
  assert.equal(normalizeString("http://[::1]:1234/", "1234", "[::1]"), "http://[::1]:<PORT>/");
  assert.equal(normalizeString("127.0.0.1:1234", "1234", undefined), "127.0.0.1:1234");
});

t("verifyInstalledPackages accepts only the exact single package", () => {
  const want = { name: "playwright-core", version: "1.63.0", integrity: "sha512-AAAA" };
  const ok = { packages: { "node_modules/playwright-core": { version: "1.63.0", integrity: "sha512-AAAA" } } };
  assert.doesNotThrow(() => verifyInstalledPackages(ok, want));
  const extra = { packages: { ...ok.packages, "node_modules/left-pad": { version: "1.0.0" } } };
  assert.throws(() => verifyInstalledPackages(extra, want), /unexpected packages installed: node_modules\/left-pad/);
  const badInt = { packages: { "node_modules/playwright-core": { version: "1.63.0", integrity: "sha512-BBBB" } } };
  assert.throws(() => verifyInstalledPackages(badInt, want), /integrity mismatch/);
  const badVer = { packages: { "node_modules/playwright-core": { version: "1.62.0", integrity: "sha512-AAAA" } } };
  assert.throws(() => verifyInstalledPackages(badVer, want), /version mismatch: expected 1.63.0, got 1.62.0/);
  assert.throws(() => verifyInstalledPackages({ packages: {} }, want), /was not installed/);
  assert.throws(() => verifyInstalledPackages({}, want), /no packages/);
});

t("normalizeValue keeps __proto__ keys as own properties", () => {
  const msg = JSON.parse('{"a":1,"__proto__":{"x":"y"}}');
  assert.equal(JSON.stringify(normalizeValue(msg, "1", "127.0.0.1")), '{"a":1,"__proto__":{"x":"y"}}');
});

t("validateDiscoveredWs pins host and port to the endpoint", () => {
  const ep = validateEndpoint("http://127.0.0.1:9333");
  assert.equal(validateDiscoveredWs("ws://127.0.0.1:9333/devtools/browser/x", ep).port, "9333");
  assert.throws(() => validateDiscoveredWs("ws://127.0.0.1:9444/x", ep), /does not match/);
  assert.throws(() => validateDiscoveredWs("ws://[::1]:9333/x", ep), /does not match/);
});

t("readBodyLimited rejects oversized discovery bodies", async () => {
  const big = new Response("x".repeat(MAX_DISCOVERY_BYTES + 1));
  await assert.rejects(readBodyLimited(big), /size limit/);
  assert.equal(await readBodyLimited(new Response('{"a":1}')), '{"a":1}');
});

t("toJsonl adds seq and validates", () => {
  const text = toJsonl([
    { kind: "meta", schema: 1 },
    { kind: "cdp", dir: "send", message: { id: 1 } },
    { kind: "stage", name: "x", ok: false },
  ]);
  assert.equal(text.split("\n").length, 4);
  assert.deepEqual(validateJsonl(text), []);
});

t("validateJsonl reports violations", () => {
  assert.notDeepEqual(validateJsonl('{"seq":1,"kind":"meta"}\n'), []);
  assert.notDeepEqual(validateJsonl('{"seq":0,"kind":"cdp","dir":"x","message":{}}\n'), []);
  assert.notDeepEqual(validateJsonl("not json\n"), []);
  assert.notDeepEqual(validateJsonl('{"seq":0,"kind":"meta"}'), []);
});

t("checkCollection rejects empty traces but accepts failed-connection traces with CDP frames", () => {
  assert.equal(checkCollection({ cdpCount: 0 }).ok, false);
  assert.deepEqual(checkCollection({ cdpCount: 3 }), { ok: true, reason: null });
});

t("drainLines rejects an oversized chunk before splitting", () => {
  const many = "a\n".repeat(MAX_CHUNK_BYTES);
  assert.throws(() => drainLines("", many), /chunk exceeds buffer limit/);
});

t("committed trace conforms to schema", () => {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const text = readFileSync(path.join(here, "results", "newpage-trace.jsonl"), "utf8");
  assert.deepEqual(validateJsonl(text), []);
  assert.ok(!text.includes("/home/"), "no local absolute paths");
});

await Promise.all(asyncCases);
console.log(`self-test: ${cases} cases passed`);

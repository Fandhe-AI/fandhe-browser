// Rust 側パーサーとの契約テスト用サンプル出力（TASK-44.1・#476、ビヘイビア CDP-2）。
//
// 役割: tests/playwright_contract.rs の契約テストから実行され、偽 chromium を注入した
// runStages の結果を connect.mjs と同じ形式（FANDHE_SCRIPT_RESULT + JSON）で出す。
// モード: ok（全段階成功）/ fail_context（newContext で失敗）。playwright・ネットワーク不要。
import { runStages } from "./stages.mjs";

const mode = process.argv[2] ?? "ok";
const context = { newPage: async () => ({}) };
const browser = {
  newContext: async () => {
    if (mode === "fail_context") {
      const e = new Error("context boom");
      e.name = "Error";
      throw e;
    }
    return context;
  },
};
const result = await runStages({
  chromium: { connectOverCDP: async () => browser },
  endpoint: "http://127.0.0.1:1",
  stageTimeoutMs: 1000,
});
process.stdout.write(`FANDHE_SCRIPT_RESULT ${JSON.stringify(result)}\n`, () => {
  process.exit(result.ok ? 0 : 1);
});

// Rust 側パーサーとの契約テスト用サンプル出力（TASK-45.2・#481、ビヘイビア CDP-3）。
//
// 役割: tests/puppeteer_contract.rs の契約テストから実行され、偽 puppeteer を注入した
// runStages の結果を connect.mjs と同じ形式（FANDHE_SCRIPT_RESULT + JSON）で出す。
// モード: ok（全段階成功）/ fail_goto（goto で失敗）。puppeteer・ネットワーク不要。
import { runStages } from "./stages.mjs";

const mode = process.argv[2] ?? "ok";
const page = {
  goto: async () => {
    if (mode === "fail_goto") {
      const e = new Error("goto boom");
      e.name = "ProtocolError";
      throw e;
    }
  },
  $: async () => ({}),
};
const browser = { newPage: async () => page, disconnect: async () => {} };
const result = await runStages({
  puppeteer: { connect: async () => browser },
  endpoint: "ws://127.0.0.1:1/x",
  stageTimeoutMs: 1000,
});
process.stdout.write(`FANDHE_SCRIPT_RESULT ${JSON.stringify(result)}\n`, () => {
  process.exit(result.ok ? 0 : 1);
});

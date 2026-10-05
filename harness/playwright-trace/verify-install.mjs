// npm install 後の導入結果検証 CLI（TASK-43.1・#246、ビヘイビア CDP-2）。run.sh から呼ばれる。
// package.json・lockfile を置かない設計のため、一時 dir の node_modules/.package-lock.json を読み、
// playwright-core のみが期待する版・integrity で入ったことを確認する。違反は exit 1（fail-closed）。
//
// 使い方: node verify-install.mjs <module-dir> <name> <version> <integrity>

import { readFileSync } from "node:fs";
import path from "node:path";
import { verifyInstalledPackages } from "./lib.mjs";

const [dir, name, version, integrity] = process.argv.slice(2);
if (!dir || !name || !version || !integrity) {
  console.error("usage: verify-install.mjs <module-dir> <name> <version> <integrity>");
  process.exit(2);
}
try {
  const lock = JSON.parse(readFileSync(path.join(dir, "node_modules", ".package-lock.json"), "utf8"));
  verifyInstalledPackages(lock, { name, version, integrity });
} catch (e) {
  console.error(`error: install verification failed: ${e.message}`);
  process.exit(1);
}

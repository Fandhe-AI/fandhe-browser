// 一時導入の検証 CLI（TASK-46・#252）。run.sh が npm install --no-save の直後に呼ぶ。
// 一時 dir の node_modules/.package-lock.json を読み、最上位パッケージが期待する版・integrity で入ったことを確認する。
// playwright-core は単独導入を要求し、puppeteer-core は --allow-transitive で推移的依存を許す（導入された全パッケージの
// name@version を JSON 配列で stdout へ出し、run.sh が結果の meta に記録する）。違反は exit 1（fail-closed）。
//
// 使い方: node verify_install.mjs <module-dir> <name> <version> <integrity> [--allow-transitive]
import { readFileSync } from "node:fs";
import path from "node:path";
import { verifyInstall } from "./swap_lib.mjs";

const args = process.argv.slice(2);
const allowTransitive = args.includes("--allow-transitive");
const [dir, name, version, integrity] = args.filter((x) => !x.startsWith("--"));
if (!dir || !name || !version || !integrity) {
  console.error("usage: verify_install.mjs <module-dir> <name> <version> <integrity> [--allow-transitive]");
  process.exit(2);
}
try {
  const lock = JSON.parse(readFileSync(path.join(dir, "node_modules", ".package-lock.json"), "utf8"));
  process.stdout.write(JSON.stringify(verifyInstall(lock, { name, version, integrity, allowTransitive })) + "\n");
} catch (e) {
  console.error(`error: install verification failed: ${e.message}`);
  process.exit(1);
}

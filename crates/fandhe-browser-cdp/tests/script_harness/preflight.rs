//! Puppeteer / Playwright 実行の事前確認（`Profile` やサーバーに依存しない）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::ScriptCommand;

/// 未導入時に表示する導入手順（固定の英語文言。AC3）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub const INSTALL_HINT: &str = "puppeteer-core is not yet provisioned: harness/puppeteer-connect has no package.json or lockfile until its installation is approved (see harness/puppeteer-connect/README.md)";

/// 基盤の前提を満たせない場合のエラー（成功を装わず理由と導入手順を返す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    ToolUnavailable {
        tool: String,
        reason: String,
        hint: String,
    },
}

/// Puppeteer 実行の事前確認。未導入なら理由付きの [`HarnessError::ToolUnavailable`]。
///
/// `puppeteer-core` の導入確認を先に行う（判定が環境の `node` 有無に左右されないため）。
/// インストール済み版が期待版と一致するかの照合は未実装（`package.json` の完全固定版が
/// 承認・確定した後に追加する。REPAIR-3）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub fn preflight_puppeteer(harness_dir: &Path) -> Result<ScriptCommand, HarnessError> {
    let unavailable = |reason: &str| HarnessError::ToolUnavailable {
        tool: "puppeteer-core".to_string(),
        reason: reason.to_string(),
        hint: INSTALL_HINT.to_string(),
    };
    let pkg = harness_dir
        .join("node_modules")
        .join("puppeteer-core")
        .join("package.json");
    if !pkg.is_file() {
        return Err(unavailable("puppeteer-core is not installed"));
    }
    let node_ok = Command::new("node")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return Err(unavailable("node executable was not found"));
    }
    Ok(ScriptCommand {
        program: PathBuf::from("node"),
        // cwd に `harness_dir` を設定するため、引数はファイル名のみ（相対 `harness_dir` で
        // 二重に join されて解決できなくなるのを防ぐ）。
        args: vec!["connect.mjs".to_string()],
        envs: Vec::new(),
        cwd: Some(harness_dir.to_path_buf()),
    })
}

/// 未導入時に表示する導入手順（固定の英語文言。TASK-44.1・#476・AC3）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub const PLAYWRIGHT_INSTALL_HINT: &str = "playwright-core is not provisioned: run `make test-playwright-connect` (installs the pinned version into a temp dir and sets FANDHE_PLAYWRIGHT_MODULE_DIR), see harness/playwright-connect/README.md";

/// playwright-core の導入先を渡す環境変数名。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub const PLAYWRIGHT_MODULE_DIR_ENV: &str = "FANDHE_PLAYWRIGHT_MODULE_DIR";

/// `package.json` の読み取り上限（巨大ファイルによる無制限確保の防止）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
const MAX_PACKAGE_JSON_BYTES: u64 = 64 * 1024;

/// Playwright 実行の事前確認（TASK-44.1・#476、`CDP-2`）。未導入・版不一致なら skip せず
/// 理由付きの [`HarnessError::ToolUnavailable`] を返す。
///
/// 判定順は node の有無に左右されないよう、導入先未指定 → `playwright-core` 不在 → 版不一致 →
/// `node` 不在。`module_dir` は `<dir>/node_modules/playwright-core` を持つ導入先
/// （`harness/playwright-connect/run.sh` が mktemp -d へ導入して渡す）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub fn preflight_playwright(
    module_dir: Option<&Path>,
    harness_dir: &Path,
    expected_version: &str,
) -> Result<ScriptCommand, HarnessError> {
    use std::io::Read;
    let unavailable = |reason: String| HarnessError::ToolUnavailable {
        tool: "playwright-core".to_string(),
        reason,
        hint: PLAYWRIGHT_INSTALL_HINT.to_string(),
    };
    let Some(dir) = module_dir else {
        return Err(unavailable(format!(
            "{PLAYWRIGHT_MODULE_DIR_ENV} is not set"
        )));
    };
    let pkg = dir
        .join("node_modules")
        .join("playwright-core")
        .join("package.json");
    if !pkg.is_file() {
        return Err(unavailable("playwright-core is not installed".to_string()));
    }
    let mut raw = Vec::new();
    std::fs::File::open(&pkg)
        .and_then(|f| f.take(MAX_PACKAGE_JSON_BYTES).read_to_end(&mut raw))
        .map_err(|e| unavailable(format!("cannot read playwright-core package.json: {e}")))?;
    let version = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("version").and_then(|s| s.as_str().map(String::from)));
    match version {
        Some(v) if v == expected_version => {}
        Some(v) => {
            return Err(unavailable(format!(
                "playwright-core version mismatch: installed {v}, expected {expected_version}"
            )));
        }
        None => {
            return Err(unavailable(
                "playwright-core package.json has no version".to_string(),
            ));
        }
    }
    let node_ok = Command::new("node")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return Err(unavailable("node executable was not found".to_string()));
    }
    Ok(ScriptCommand {
        program: PathBuf::from("node"),
        args: vec!["connect.mjs".to_string()],
        envs: vec![(
            PLAYWRIGHT_MODULE_DIR_ENV.to_string(),
            dir.to_string_lossy().into_owned(),
        )],
        cwd: Some(harness_dir.to_path_buf()),
    })
}

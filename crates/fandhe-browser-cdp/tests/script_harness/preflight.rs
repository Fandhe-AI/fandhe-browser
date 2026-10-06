//! Puppeteer 実行の事前確認（`Profile` やサーバーに依存しない）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::ScriptCommand;

/// 未導入時に表示する導入手順（固定の英語文言。AC3）。
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

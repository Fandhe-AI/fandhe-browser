//! boa 上でページ JS 実行後 DOM の snapshot と削減率測定への影響を確認する結合テスト
//! （TASK-113・Issue #787・`JS-4`/`AISNAP-6`/`PLUG-4`）。V8 版（`page_scripts_snapshot_v8.rs`）と
//! 同一のケースを実行する。
//!
//! macOS の boa は fail-closed で無効（core の `page_actor_boa.rs` が検証済み）のため、ここでは
//! ケースを実行せず成功終了する。同じケースは V8 版が macOS を含む 3 OS で実行する。

use std::process::ExitCode;

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
#[path = "../benches/token_reduction/reduction.rs"]
mod reduction;
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
#[path = "../benches/token_reduction/snapshot_text.rs"]
mod snapshot_text;
#[cfg(not(target_os = "macos"))]
// 共有モジュールはクレートルートに置く（`reduction.rs` が `crate::tokens` を参照するため）。
#[allow(dead_code)]
#[path = "../benches/token_reduction/tokens.rs"]
mod tokens;

#[cfg(not(target_os = "macos"))]
#[path = "support/page_scripts_snapshot_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(not(target_os = "macos"))]
    {
        cases::run("boa");
        eprintln!("page_scripts_snapshot_boa: all cases passed");
    }

    #[cfg(target_os = "macos")]
    eprintln!(
        "page_scripts_snapshot_boa: skipped on macOS (boa is fail-closed there; the V8 test runs the same cases)"
    );

    ExitCode::SUCCESS
}

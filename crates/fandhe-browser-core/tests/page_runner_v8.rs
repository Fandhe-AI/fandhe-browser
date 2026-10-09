//! V8 上でページスクリプトランナーを実行する結合テスト（TASK-109・Issue #780・
//! ビヘイビア `JS-4` / `JS-6`）。ケース本体は `support/page_runner_cases.rs`（boa と共通）。
//!
//! V8 は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`（`js_shim_v8.rs` と同じ作法）。

use std::process::ExitCode;

#[path = "support/page_runner_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    cases::run("v8");
    eprintln!("page_runner_v8: all cases passed");
    ExitCode::SUCCESS
}

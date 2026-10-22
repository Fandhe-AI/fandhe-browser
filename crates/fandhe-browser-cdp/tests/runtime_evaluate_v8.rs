//! V8 での `Runtime.evaluate`・`Runtime.releaseObject` の結合テスト（TASK-44.7・#1218・ビヘイビア
//! `CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`）。ケース本体は `support/runtime_evaluate_cases.rs`
//! （boa と共通）。
//!
//! V8 は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`（`page_js_limits_v8.rs` と同じ作法）。

use std::process::ExitCode;

#[path = "support/runtime_evaluate_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    cases::run("v8");
    eprintln!("runtime_evaluate_v8: all cases passed");
    ExitCode::SUCCESS
}

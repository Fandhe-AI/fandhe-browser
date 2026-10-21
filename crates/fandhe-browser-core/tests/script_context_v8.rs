//! V8 上で評価コンテキストを動かす結合テスト（TASK-44.5・Issue #1216・ビヘイビア
//! `CDP-2` / `JS-1` / `JS-6` / `PERF-7`）。ケース本体は `support/script_context_cases.rs`（boa と共通）。
//!
//! V8 は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`（`page_actor_v8.rs` と同じ作法）。

use std::process::ExitCode;

#[path = "support/script_context_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    cases::run("v8");
    eprintln!("script_context_v8: all cases passed");
    ExitCode::SUCCESS
}

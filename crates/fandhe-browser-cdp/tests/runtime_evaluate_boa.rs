//! boa での `Runtime.evaluate`・`Runtime.releaseObject` の結合テスト（TASK-44.7・#1218・ビヘイビア
//! `CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`）。V8 版（`runtime_evaluate_v8.rs`）と同一のケースを
//! 実行する。
//!
//! macOS の boa は fail-closed で評価器が無効になるため、別エンジンへフォールバックせず
//! `Runtime.evaluate` が `EXECUTION_CONTEXTS_UNAVAILABLE` で拒否されることを確認する
//! （core の `script_context_boa.rs` と同じ扱い）。

use std::process::ExitCode;

#[path = "support/runtime_evaluate_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(not(target_os = "macos"))]
    cases::run("boa");

    #[cfg(target_os = "macos")]
    cases::run_disabled();

    eprintln!("runtime_evaluate_boa: all cases passed");
    ExitCode::SUCCESS
}

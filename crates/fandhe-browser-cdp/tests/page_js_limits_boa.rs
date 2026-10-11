//! boa でのページ JS 実行の結合テストと上限検証（TASK-113・Issue #786・ビヘイビア `JS-4`〜`JS-8`・
//! `JS-1`・`PERF-7`）。ケース本体は `support/page_js_cases.rs`（V8 と共通）。
//!
//! boa は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`。macOS の boa は fail-closed で無効のため、実行系ケースは走らせず
//! `Err(PageScriptError::Run(_))` になることだけを確認する（core の `page_actor_boa.rs` と同じ作法）。

use std::process::ExitCode;

#[path = "support/page_js_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    #[cfg(target_os = "macos")]
    cases::run_boa_fail_closed();
    #[cfg(not(target_os = "macos"))]
    cases::run("boa");
    eprintln!("page_js_limits_boa: all cases passed");
    ExitCode::SUCCESS
}

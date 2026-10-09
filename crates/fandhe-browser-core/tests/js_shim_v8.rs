//! V8 上で JS shim（`document` / Node 系）を評価する結合テスト（TASK-108・Issue #778・
//! ビヘイビア `JS-5` / `JS-6`）。ケース本体は `support/js_shim_cases.rs`（boa と共通）。
//!
//! V8 は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`（`js_stub_v8.rs` と同じ作法）。`js-v8` 有効構成でのみビルドする。

use std::process::ExitCode;

#[path = "support/js_shim_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    cases::run("v8");
    eprintln!("js_shim_v8: all cases passed");
    ExitCode::SUCCESS
}

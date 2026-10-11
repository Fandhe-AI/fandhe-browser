//! V8 でのページ JS 実行の結合テストと上限検証（TASK-113・Issue #786・ビヘイビア `JS-4`〜`JS-8`・
//! `JS-1`・`PERF-7`）。ケース本体は `support/page_js_cases.rs`（boa と共通）。
//!
//! V8 は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`。CDP 入口のケースは `Profile::open` が対応する unix のみ、アクター層の
//! ケースは 3 OS で実行する（理由と範囲は `support/page_js_cases.rs` の冒頭を参照）。

use std::process::ExitCode;

#[path = "support/page_js_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    cases::run("v8");
    eprintln!("page_js_limits_v8: all cases passed");
    ExitCode::SUCCESS
}

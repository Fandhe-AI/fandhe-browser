//! V8 上でページ JS 実行後 DOM の snapshot と削減率測定への影響を確認する結合テスト
//! （TASK-113・Issue #787・`JS-4`/`AISNAP-6`/`PLUG-4`）。ケース本体は
//! `support/page_scripts_snapshot_cases.rs`（boa と共通）。
//!
//! V8 は子プロセス版のため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`（core の `page_actor_v8.rs` と同じ作法）。macOS を含む 3 OS で実行する。

use std::process::ExitCode;

// 共有モジュールはクレートルートに置く（`reduction.rs` が `crate::tokens` を参照するため）。
#[allow(dead_code)]
#[path = "../benches/token_reduction/tokens.rs"]
mod tokens;

#[allow(dead_code)]
#[path = "../benches/token_reduction/snapshot_text.rs"]
mod snapshot_text;

#[allow(dead_code)]
#[path = "../benches/token_reduction/reduction.rs"]
mod reduction;

#[path = "support/page_scripts_snapshot_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    cases::run("v8");
    eprintln!("page_scripts_snapshot_v8: all cases passed");
    ExitCode::SUCCESS
}

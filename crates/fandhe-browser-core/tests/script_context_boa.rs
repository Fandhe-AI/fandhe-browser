//! boa 上で評価コンテキストを動かす結合テスト（TASK-44.5・Issue #1216・ビヘイビア
//! `CDP-2` / `JS-1` / `JS-6` / `PERF-7`）。V8 版（`script_context_v8.rs`）と同一のケースを実行する。
//!
//! macOS の boa は fail-closed で無効のため、別エンジンへフォールバックせずハンドルが無効になる
//! ことを確認する（`page_actor_boa.rs` と同じ）。

use std::process::ExitCode;

#[cfg(not(target_os = "macos"))]
#[path = "support/script_context_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(not(target_os = "macos"))]
    cases::run("boa");

    #[cfg(target_os = "macos")]
    macos_boa_is_fail_closed();

    eprintln!("script_context_boa: all cases passed");
    ExitCode::SUCCESS
}

/// macOS では boa のランタイム生成が失敗し、ハンドルは無効になる（成功を装わない）。
#[cfg(target_os = "macos")]
fn macos_boa_is_fail_closed() {
    use fandhe_browser_core::{Config, ScriptContextHandle};

    let cfg = Config::from_toml_str("[js]\nengine = \"boa\"\n").expect("config");
    let handle = ScriptContextHandle::new(cfg.js());
    assert!(!handle.is_enabled());
    assert_eq!(handle.stats().worker_spawns(), 0);
}

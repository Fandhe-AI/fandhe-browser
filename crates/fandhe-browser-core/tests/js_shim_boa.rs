//! boa 上で JS shim（`document` / Node 系）を評価する結合テスト（TASK-108・Issue #778・
//! ビヘイビア `JS-5` / `JS-6`）。V8 版（`js_shim_v8.rs`）と同一のケースを実行する。
//!
//! 子プロセス役を兼ねるため `harness = false`。macOS の boa は fail-closed で無効のため、
//! 別エンジンへフォールバックせず失敗することを確認する（`js_stub_boa.rs` と同じ）。

use std::process::ExitCode;

#[cfg(not(target_os = "macos"))]
#[path = "support/js_shim_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(not(target_os = "macos"))]
    cases::run("boa");

    #[cfg(target_os = "macos")]
    {
        let cfg =
            fandhe_browser_core::Config::from_toml_str("[js]\nengine = \"boa\"\n").expect("config");
        assert!(
            fandhe_browser_core::js_stub::JsRuntime::from_config(cfg.js()).is_err(),
            "boa must be disabled on macOS"
        );
    }

    eprintln!("js_shim_boa: all cases passed");
    ExitCode::SUCCESS
}

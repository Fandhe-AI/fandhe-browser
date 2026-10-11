//! boa 上でページスクリプト実行アクターを動かす結合テスト（TASK-112・Issue #784・
//! ビヘイビア `JS-1` / `JS-4` / `JS-6`）。V8 版（`page_actor_v8.rs`）と同一のケースを実行する。
//!
//! macOS の boa は fail-closed で無効のため、別エンジンへフォールバックせず `Err` になることを
//! 確認する（`page_runner_boa.rs` と同じ）。

use std::process::ExitCode;

#[cfg(not(target_os = "macos"))]
#[path = "support/page_actor_cases.rs"]
mod cases;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(not(target_os = "macos"))]
    cases::run("boa");

    #[cfg(target_os = "macos")]
    macos_boa_is_fail_closed();

    eprintln!("page_actor_boa: all cases passed");
    ExitCode::SUCCESS
}

/// macOS では boa のランタイム生成が失敗し、アクターは `Err(Run(_))` を返す（成功を装わない）。
#[cfg(target_os = "macos")]
fn macos_boa_is_fail_closed() {
    use std::sync::Arc;

    use fandhe_browser_core::{
        Config, NavigationState, PageRunRequest, PageScriptError, PageScriptExecutor,
        PageScriptHandle,
    };

    struct TokioExecutor;
    impl PageScriptExecutor for TokioExecutor {
        fn block_on<'a>(
            &self,
            future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
        ) -> std::io::Result<()> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(future);
            Ok(())
        }
    }

    let cfg = Config::from_toml_str("[js]\nengine = \"boa\"\nexecute_page_scripts = true\n")
        .expect("config");
    let handle = PageScriptHandle::new(cfg.js(), Arc::new(TokioExecutor));
    let state = NavigationState::new();
    let generation = state.begin_navigation().expect("generation");
    let request = PageRunRequest::new(
        Arc::from("<body><script>1</script></body>"),
        "https://example.test/",
        state.scope(),
        generation,
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = rt.block_on(handle.run_page_scripts(request));
    assert!(
        matches!(result, Err(PageScriptError::Run(_))),
        "boa must be disabled on macOS: {result:?}"
    );
}

//! 評価コンテキスト（`ScriptContextHandle`）の crate 外からの結合テスト（TASK-44.5・Issue #1216・
//! ビヘイビア `CDP-2`・`JS-1`・`PERF-7`）。実エンジンのケースは `script_context_v8.rs` /
//! `script_context_boa.rs`（`harness = false`）。
//!
//! 注意: エンジンなし構成のテスト（`no_engine` モジュール）は、`cargo test --workspace` では
//! `fandhe-browser-cli` の既定 feature が core の `js-v8` を有効にするため空になる。実行は
//! `cargo test -p fandhe-browser-core`（core 単独選択。CI の `rust-features-*` の既定 feature の
//! ステップ）で行う。

use std::sync::Arc;

use fandhe_browser_core::{
    ScriptContextError, ScriptContextHandle, ScriptContextKey, ScriptEvalOptions, UnavailableReason,
};

/// 評価要求を 1 つ出して結果を待つ（tokio の current_thread ランタイムで `block_on`）。
fn evaluate_once(handle: &ScriptContextHandle) -> Result<String, ScriptContextError> {
    let nav = fandhe_browser_core::NavigationState::new();
    let generation = nav.begin_navigation().expect("generation");
    let key = ScriptContextKey::new(nav.scope(), generation, 1);
    let document = Arc::new(fandhe_browser_core::NavigationResult::new(
        "https://example.test/",
        "<h1>x</h1>",
    ));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(handle.evaluate(key, document, "1", &ScriptEvalOptions::default()))
}

/// `CDP-2`・`JS-1`: 無効なハンドルは評価で `Unavailable { Disabled }` を返し、スレッドを起動しない。
#[test]
fn js_1_disabled_handle_is_unavailable_without_spawning() {
    let handle = ScriptContextHandle::disabled();
    assert!(!handle.is_enabled());
    assert_eq!(
        evaluate_once(&handle),
        Err(ScriptContextError::Unavailable {
            reason: UnavailableReason::Disabled
        })
    );
    assert_eq!(handle.stats().worker_spawns(), 0);
}

#[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
mod no_engine {
    use super::*;

    /// `JS-1`: エンジンなしビルドでは `new` のハンドルは無効（成功を装わない）。
    #[test]
    fn js_1_no_engine_build_has_disabled_handle() {
        let config = fandhe_browser_core::Config::default();
        let handle = ScriptContextHandle::new(config.js());
        assert!(!handle.is_enabled());
        assert_eq!(
            evaluate_once(&handle),
            Err(ScriptContextError::Unavailable {
                reason: UnavailableReason::Disabled
            })
        );
        assert_eq!(handle.stats().worker_spawns(), 0);
    }
}

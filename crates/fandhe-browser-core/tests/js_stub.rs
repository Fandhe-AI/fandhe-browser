//! `fandhe-browser-core` の JS 実行境界（`js_stub::execute_js_stub`）を crate 外から
//! 検証する結合テスト（TASK-30（30.3）・ビヘイビア `JS-2`・Issue #161。
//! 元は TASK-24（24.9）・`CORE-1`・Issue #43）。
//!
//! libtest ハーネスのテストから V8 子プロセスを起動しない（子の再実行にはホストの
//! `main` で `run_js_worker_if_requested` を呼ぶ必要があるため）。実評価は
//! `harness = false` の `tests/js_stub_v8.rs` が担う。

use fandhe_browser_core::Config;
use fandhe_browser_core::js_stub::{JsRuntime, execute_js_stub};

/// JS-2: エンジンなし構成では crate 外から呼んでも `JsExecutionUnavailable` で、
/// `Display` は固定文言（script を反響しない）。
#[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
#[test]
fn js_2_no_engine_build_returns_js_execution_unavailable() {
    use fandhe_browser_core::Error;
    let mut rt = JsRuntime::from_config(Config::default().js()).expect("JS 無効として構築できる");
    assert_eq!(rt.engine_kind(), None);
    let err = execute_js_stub(&mut rt, "document.title").expect_err("常に失敗する");
    assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    assert_eq!(
        err.to_string(),
        "JS execution unavailable: no JS engine is compiled into this binary; \
         JavaScript execution is disabled"
    );
    assert!(!err.to_string().contains("document.title"));
}

/// JS-2: エンジンあり構成では設定が選んだ種別がランタイムに反映される（評価はしない）。
#[cfg(any(feature = "js-v8", feature = "js-boa"))]
#[test]
fn js_2_runtime_reflects_configured_engine_kind() {
    let config = Config::default();
    // boa のみ同梱（TASK-32 まで未実装）の構成では from_config が失敗するため、成功時のみ検証する。
    if let Ok(rt) = JsRuntime::from_config(config.js()) {
        assert_eq!(rt.engine_kind(), config.js().engine());
    }
}

/// JS-2: JS 無効ランタイムは常に失敗する（全構成共通）。
#[test]
fn js_2_disabled_runtime_always_fails() {
    let mut rt = JsRuntime::disabled();
    assert!(execute_js_stub(&mut rt, "1").is_err());
}

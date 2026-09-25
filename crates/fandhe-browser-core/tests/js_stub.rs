//! `fandhe-browser-core` の JS 実行スタブ境界（`js_stub::execute_js_stub`）を
//! crate 外から検証する結合テスト（TASK-24（24.9）・ビヘイビア `CORE-1`・
//! Issue #43）。
//!
//! `dom`/`query`（#39/#41）や将来の `fandhe-browser-cdp` から見た契約
//! （常にエラーを返し、成功を装わないこと）が crate 外からも成り立つことを
//! 確認する。

use fandhe_browser_core::Error;
use fandhe_browser_core::js_stub::execute_js_stub;

/// CORE-1: crate 外から呼んでも `execute_js_stub` は常に
/// `Error::JsExecutionUnavailable` を返し、`Display` 文字列は固定である。
#[test]
fn core_1_execute_js_stub_returns_js_execution_unavailable_from_outside_crate() {
    let err = execute_js_stub("document.title").expect_err("js_stub は常に失敗する");
    assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    assert_eq!(
        err.to_string(),
        "JS execution unavailable: JS execution is not implemented yet \
         (js_stub boundary; replaced by fandhe-browser-js per JS-2/TASK-30)"
    );
}

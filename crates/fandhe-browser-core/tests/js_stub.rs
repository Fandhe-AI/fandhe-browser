//! `fandhe-browser-core` の JS 実行境界（`js_stub::execute_js_stub`）を crate 外から
//! 検証する結合テスト（TASK-30（30.3）・ビヘイビア `JS-2`・Issue #161。
//! 元は TASK-24（24.9）・`CORE-1`・Issue #43）。
//!
//! libtest ハーネスのテストから V8 子プロセスを起動しない（子の再実行にはホストの
//! `main` で `run_js_worker_if_requested` を呼ぶ必要があるため）。実評価は
//! `harness = false` の `tests/js_stub_v8.rs` が担う。エンジンなしビルドの検証は
//! `tests/js_stub_no_engine.rs`（TASK-30（30.4）・Issue #162）が担う。

use fandhe_browser_core::js_stub::{JsRuntime, execute_js_stub};

/// macOS で boa のみ同梱した構成（boa は確保時に効くメモリ上限を強制できず無効。
/// `create_engine(Boa)` は `NotYetImplemented`）か。この構成では既定選択の boa が
/// 生成できない。
#[cfg(any(feature = "js-v8", feature = "js-boa"))]
const BOA_ONLY_ON_MACOS: bool = cfg!(all(
    target_os = "macos",
    feature = "js-boa",
    not(feature = "js-v8")
));

/// JS-2: エンジンあり構成では設定が選んだ種別がランタイムに反映される（評価はしない）。
/// macOS の boa のみ構成は fail-closed で `JsExecutionUnavailable`（REPAIR-3・TASK-32.2）。
#[cfg(any(feature = "js-v8", feature = "js-boa"))]
#[test]
fn js_2_runtime_reflects_configured_engine_kind() {
    use fandhe_browser_core::{Config, Error};
    let config = Config::default();
    let result = JsRuntime::from_config(config.js());
    if BOA_ONLY_ON_MACOS {
        match result {
            Err(Error::JsExecutionUnavailable { message }) => assert_eq!(
                message,
                "js engine \"boa\" is bundled but not yet implemented"
            ),
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("boa must be disabled on macOS"),
        }
    } else {
        let rt = result.expect("同梱エンジンが選ばれる");
        assert_eq!(rt.engine_kind(), config.js().engine());
    }
}

/// JS-2: JS 無効ランタイムは常に失敗する（全構成共通）。
#[test]
fn js_2_disabled_runtime_always_fails() {
    let mut rt = JsRuntime::disabled();
    assert!(execute_js_stub(&mut rt, "1").is_err());
}

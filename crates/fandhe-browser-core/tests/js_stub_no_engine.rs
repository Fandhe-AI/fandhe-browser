#![cfg(not(any(feature = "js-v8", feature = "js-boa")))]
//! エンジンなしビルドで JS 実行を要求したときのエラー処理を crate 外から検証する
//! 結合テスト（TASK-30（30.4）・ビヘイビア `JS-2`・`js-engine.md` 決定 4・Issue #162）。
//!
//! エンジンなしビルドでは `fandhe_browser_core::js_stub::execute_js_stub` が成功を装わず、
//! 「JS エンジンが同梱されていない」ことを示す `Error::JsExecutionUnavailable` を
//! 返すこと（fail-closed）を固定する。
//!
//! 注意: `cargo test --workspace` では `fandhe-browser-cli` の `default = ["js-v8"]` が
//! feature 統合で core の `js-v8` を有効にするため、本ファイルは空になる。実行は
//! `cargo test -p fandhe-browser-core`（core 単独選択）で行い、CI でも同コマンドを
//! 実行する（`.github/workflows/ci.yml` の `rust-ci-default-features`）。

use std::sync::Arc;

use fandhe_browser_core::js_stub::{
    JsRuntime, JsStubOptions, execute_js_stub, execute_js_stub_with_options,
};
use fandhe_browser_core::{
    Config, Error, FailureKind, InMemoryRecorder, OperationKind, OperationOutcome, bundled_engines,
};

const EXPECTED_MESSAGE: &str = "JS execution unavailable: no JS engine is compiled into this \
                                binary; JavaScript execution is disabled";

fn runtime_from_default_config() -> JsRuntime {
    JsRuntime::from_config(Config::default().js()).expect("JS 無効として構築できる")
}

/// JS-2: 本テストがエンジンなし構成で動いていること（`disabled()` 直接構築との区別）。
#[test]
fn js_2_no_engine_precondition_holds() {
    assert!(bundled_engines().is_empty());
    assert_eq!(Config::default().js().engine(), None);
}

/// JS-2: エンジンなしでの実行要求は `JsExecutionUnavailable` で、`Display` は固定文言。
#[test]
fn js_2_execute_returns_js_execution_unavailable() {
    let mut rt = runtime_from_default_config();
    assert_eq!(rt.engine_kind(), None);
    let err = execute_js_stub(&mut rt, "document.title").expect_err("常に失敗する");
    assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    assert_eq!(err.to_string(), EXPECTED_MESSAGE);
}

/// JS-2: エラーが「エンジンが同梱されていない」ことを明示している。
#[test]
fn js_2_error_states_engine_is_not_compiled_in() {
    let mut rt = runtime_from_default_config();
    let err = execute_js_stub(&mut rt, "1 + 1").expect_err("常に失敗する");
    assert!(
        err.to_string()
            .contains("no JS engine is compiled into this binary")
    );
}

/// JS-2: エラー文言は script を反響せず、入力長によらず固定長（非 ASCII・長大入力）。
#[test]
fn js_2_error_does_not_echo_script() {
    let mut rt = runtime_from_default_config();
    let long = "x".repeat(10_000);
    for script in ["console.log('日本語🦀')", long.as_str()] {
        let err = execute_js_stub(&mut rt, script).expect_err("常に失敗する");
        assert_eq!(err.to_string(), EXPECTED_MESSAGE);
        assert_eq!(err.to_string().len(), EXPECTED_MESSAGE.len());
    }
}

/// JS-2: 繰り返し要求しても毎回失敗し、黙ってエンジンを有効化しない。
#[test]
fn js_2_repeated_requests_keep_failing() {
    let mut rt = runtime_from_default_config();
    for _ in 0..3 {
        let err = execute_js_stub(&mut rt, "1").expect_err("常に失敗する");
        assert_eq!(err.to_string(), EXPECTED_MESSAGE);
        assert_eq!(rt.engine_kind(), None);
    }
}

/// JS-2・REPAIR-9: 失敗は `JsStub` 操作の `JsExecutionUnavailable` として 1 件記録される。
#[test]
fn js_2_failure_is_recorded_for_observability() {
    let recorder = Arc::new(InMemoryRecorder::with_capacity(8));
    let options = JsStubOptions::new().with_recorder(recorder.clone());
    let mut rt = runtime_from_default_config();
    execute_js_stub_with_options(&mut rt, "1", &options).expect_err("常に失敗する");
    let records = recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].operation(), OperationKind::JsStub);
    assert_eq!(
        records[0].outcome(),
        OperationOutcome::Failure {
            kind: FailureKind::JsExecutionUnavailable
        }
    );
}

/// JS-2: 未同梱エンジンの明示指定は設定読み込みの時点で失敗する（詳細検証は
/// `tests/config_js_engine.rs` が担う）。
#[test]
fn js_2_explicit_engine_selection_is_rejected() {
    let err = Config::from_toml_str("[js]\nengine = \"v8\"\n").expect_err("未同梱なので失敗する");
    assert!(
        err.to_string()
            .contains("not compiled into this binary (compiled: none)")
    );
}

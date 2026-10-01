//! V8 で実際にスクリプトを評価する `js_stub` の結合テスト（TASK-30（30.3）・
//! ビヘイビア `JS-2`・Issue #161）。
//!
//! V8 は子プロセス版のため、子として再実行されたときにワーカーとして振る舞う
//! よう `main` の先頭で core の再エクスポート `run_js_worker_if_requested`
//! （#513）を呼ぶ必要がある（`fandhe-browser-js` の `tests/conformance.rs` と同じ作法）。そのため
//! `harness = false`。`js-v8` 有効構成でのみビルドする（`Cargo.toml` 参照）。

use fandhe_browser_core::js_stub::{JsRuntime, execute_js_stub};
use fandhe_browser_core::{Config, Error};
use fandhe_browser_js::{EngineKind, JsEngineError, JsValue};
use std::process::ExitCode;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    let cfg = Config::from_toml_str("[js]\nengine = \"v8\"\n").expect("config");
    let mut rt = JsRuntime::from_config(cfg.js()).expect("v8 runtime");
    assert_eq!(rt.engine_kind(), Some(EngineKind::V8));

    eprintln!("case: 1 + 2");
    let out = execute_js_stub(&mut rt, "1 + 2").expect("eval");
    assert_eq!(out.value, "3");
    assert_eq!(out.js_value, JsValue::Number(3.0));

    eprintln!("case: string concat");
    let out = execute_js_stub(&mut rt, "'a' + 'b'").expect("eval");
    assert_eq!(out.value, "ab");

    eprintln!("case: context persists");
    execute_js_stub(&mut rt, "var x = 40;").expect("eval");
    let out = execute_js_stub(&mut rt, "x + 2").expect("eval");
    assert_eq!(out.value, "42");

    eprintln!("case: undefined");
    let out = execute_js_stub(&mut rt, "undefined").expect("eval");
    assert_eq!(out.value, "undefined");
    assert_eq!(out.js_value, JsValue::Undefined);

    eprintln!("case: syntax error");
    let err = execute_js_stub(&mut rt, "1 +").expect_err("syntax error");
    assert!(
        matches!(err, Error::JsEvaluation(JsEngineError::EvaluationFailed(_))),
        "unexpected error: {err}"
    );

    eprintln!("all cases passed");
    ExitCode::SUCCESS
}

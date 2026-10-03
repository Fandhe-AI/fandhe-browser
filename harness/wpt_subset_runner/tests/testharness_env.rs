//! testharness.js 用グローバル環境と結果受け渡しの結合テスト
//! （`PLUG-10`・TASK-101.2.1・Issue #553）。
//!
//! V8 は子プロセス版のため、子として再実行されたときにワーカーとして振る舞うよう
//! `main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり `harness = false`
//! （core の `tests/js_stub_v8.rs` と同じ作法）。lib のユニットテストからはエンジンを
//! 生成しない（子プロセスの自己再実行フックが無いため）。
//!
//! エンジン構成ごとの分岐は `bundled_engines()`（実際にリンクされたエンジン）で決める
//! （skip にせず、構成ごとに具体値を検証する）:
//! - V8 同梱: V8 でケース A〜D
//! - boa 同梱（macOS 以外）: boa でケース A〜D
//! - boa 同梱（macOS）: boa は無効のため `from_config` が `JsExecutionUnavailable`
//! - どちらも非同梱: `install_testharness_globals` が inject 段階で失敗する

use std::process::ExitCode;

use fandhe_browser_core::js_stub::JsRuntime;
use fandhe_browser_core::js_stub::execute_js_stub;
use fandhe_browser_core::{Config, EngineKind, Error, JsValue, bundled_engines};
use wpt_subset_runner::{EnvironmentError, install_testharness_globals};
use wpt_subset_runner::{HarnessStatus, SubtestStatus, attach_result_reporter};

const FAKE_TESTHARNESS: &str = include_str!("fixtures/fake_testharness.js");

fn runtime_for(engine: &str) -> Result<JsRuntime, Error> {
    let cfg = Config::from_toml_str(&format!("[js]\nengine = \"{engine}\"\n")).expect("config");
    JsRuntime::from_config(cfg.js())
}

fn eval(rt: &mut JsRuntime, script: &str) -> fandhe_browser_core::js_stub::JsExecutionOutput {
    execute_js_stub(rt, script).unwrap_or_else(|e| panic!("eval failed: {e}"))
}

/// ケース A（AC1）: グローバルが注入され、Shell 環境に倒れる（document が無い）。
fn case_a_globals(mut rt: JsRuntime) {
    install_testharness_globals(&mut rt).expect("install");
    assert_eq!(
        eval(&mut rt, "typeof self === 'object' && self === globalThis").js_value,
        JsValue::Bool(true)
    );
    assert_eq!(
        eval(&mut rt, "typeof __fandheWptReportResult").js_value,
        JsValue::String("function".to_string())
    );
    assert_eq!(
        eval(&mut rt, "typeof __fandheWptReportCompletion").js_value,
        JsValue::String("function".to_string())
    );
    assert_eq!(
        eval(&mut rt, "typeof document").js_value,
        JsValue::String("undefined".to_string())
    );
}

/// ケース B（AC2）: 偽 testharness の結果が Rust 側で受け取れる。
fn case_b_results(mut rt: JsRuntime) {
    let collector = install_testharness_globals(&mut rt).expect("install");
    eval(&mut rt, FAKE_TESTHARNESS);
    attach_result_reporter(&mut rt).expect("attach");
    eval(
        &mut rt,
        "test(function () { assert_true(true); }, 'passes');\n\
         test(function () { assert_true(false, 'boom'); }, 'fails');\n\
         done();\nundefined;",
    );
    let out = collector.take().expect("collect");
    assert_eq!(out.subtests.len(), 2);
    let first = out.subtests.first().expect("first");
    assert_eq!(first.name, "passes");
    assert_eq!(first.status, SubtestStatus::Pass);
    assert_eq!(first.message, None);
    let second = out.subtests.get(1).expect("second");
    assert_eq!(second.name, "fails");
    assert_eq!(second.status, SubtestStatus::Fail);
    assert_eq!(second.message.as_deref(), Some("assert_true: boom"));
    assert!(!second.message_truncated);
    let completion = out.completion.expect("completion");
    assert_eq!(completion.status, HarnessStatus::Ok);
    assert_eq!(completion.message, None);
}

/// ケース C（fail-closed）: testharness.js 読み込み前の attach はエラーになる。
fn case_c_attach_before_load(mut rt: JsRuntime) {
    install_testharness_globals(&mut rt).expect("install");
    let err = attach_result_reporter(&mut rt).expect_err("not loaded");
    assert!(matches!(err, EnvironmentError::Reporter(_)), "{err}");
}

/// ケース D（不正入力）: 範囲外ステータスは JS 側で例外になり、take も Err になる。
fn case_d_invalid_report(mut rt: JsRuntime) {
    let collector = install_testharness_globals(&mut rt).expect("install");
    let out = eval(
        &mut rt,
        "(function () {\n\
             try { __fandheWptReportResult('x', 9, null); return 'no-throw'; }\n\
             catch (e) { return 'threw'; }\n\
         })()",
    );
    assert_eq!(out.js_value, JsValue::String("threw".to_string()));
    assert!(collector.take().is_err());
}

fn run_engine_cases(label: &str, engine: &str) {
    for (name, case) in [
        ("A", case_a_globals as fn(JsRuntime)),
        ("B", case_b_results),
        ("C", case_c_attach_before_load),
        ("D", case_d_invalid_report),
    ] {
        eprintln!("case {name} ({label})");
        // ケースごとに新しい runtime（子プロセス）を使い、状態を共有しない。
        case(runtime_for(engine).expect("runtime"));
    }
}

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    // 同梱エンジンは feature 単位ではなく実際にリンクされたもので判定する。
    // workspace 全体のビルドでは cli の既定 feature（js-v8）が feature unification で
    // 本 crate のビルドにも効くため、本 crate の feature だけでは構成を決められない。
    let bundled = bundled_engines();
    if bundled.is_empty() {
        eprintln!("case: no engine");
        let cfg = Config::from_toml_str("").expect("config");
        let mut rt = JsRuntime::from_config(cfg.js()).expect("disabled runtime");
        assert_eq!(rt.engine_kind(), None);
        match install_testharness_globals(&mut rt) {
            Err(EnvironmentError::Inject(Error::JsExecutionUnavailable { .. })) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    if bundled.contains(&EngineKind::V8) {
        run_engine_cases("v8", "v8");
    }

    if bundled.contains(&EngineKind::Boa) {
        if cfg!(target_os = "macos") {
            eprintln!("case: boa is unavailable on macOS");
            match runtime_for("boa") {
                Err(err) => assert_eq!(
                    err.to_string(),
                    "JS execution unavailable: js engine \"boa\" is bundled but not yet implemented"
                ),
                Ok(_) => panic!("boa must be disabled on macOS"),
            }
        } else {
            run_engine_cases("boa", "boa");
        }
    }

    eprintln!("all cases passed");
    ExitCode::SUCCESS
}

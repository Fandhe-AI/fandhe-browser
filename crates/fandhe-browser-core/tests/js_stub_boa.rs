//! 設定 `[js] engine = "boa"` で boa が実際に評価に使われることの結合テスト
//! （TASK-32.3・ビヘイビア `JS-1`・`JS-2`・Issue #167）。
//!
//! `js_stub_v8.rs` の対になるテストで、core の `JsRuntime::from_config` が設定を
//! エンジン種別へ解決し、`execute_js_stub` が子プロセス上の boa で評価できることを
//! 確認する。js crate は設定を知らないため、設定経由の検証は設定を扱う core に置く。
//!
//! 「boa が実際に使われた」ことの根拠は、親が子の Hello フレームの名乗るエンジン種別を
//! 要求種別と照合し、不一致なら `EngineUnavailable` で拒否する点にある
//! （js crate の `v8_worker` 結合テスト
//! `js_1_handshake_rejects_hello_naming_a_different_engine` が回帰を保証）。したがって
//! `engine_kind() == Some(Boa)` かつ評価成功は boa の子が応答した証明になる。
//!
//! 子プロセス役を兼ねるため `harness = false`（stdout はプロトコルフレーム専用。進捗は
//! stderr のみ）。macOS の boa は fail-closed で無効のため、別エンジンへフォールバック
//! せず失敗することを検証する。

use std::process::ExitCode;

use fandhe_browser_core::Config;
use fandhe_browser_core::js_stub::JsRuntime;
#[cfg(not(target_os = "macos"))]
use fandhe_browser_core::{Error, js_stub::execute_js_stub};
#[cfg(not(target_os = "macos"))]
use fandhe_browser_js::{EngineKind, JsEngineError, JsValue};

#[cfg(all(feature = "js-v8", not(target_os = "macos")))]
fn runtime_for(toml: &str, expected: EngineKind) -> JsRuntime {
    let cfg = Config::from_toml_str(toml).expect("config");
    let rt = JsRuntime::from_config(cfg.js()).expect("runtime");
    assert_eq!(rt.engine_kind(), Some(expected), "config: {toml:?}");
    rt
}

/// boa を選んだ設定で実評価できること。
#[cfg(not(target_os = "macos"))]
fn boa_evaluates_via_config() {
    let cfg = Config::from_toml_str("[js]\nengine = \"boa\"\n").expect("config");
    let mut rt = JsRuntime::from_config(cfg.js()).expect("runtime");
    assert_eq!(rt.engine_kind(), Some(EngineKind::Boa));

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

    eprintln!("case: syntax error");
    let err = execute_js_stub(&mut rt, "1 +").expect_err("syntax error");
    assert!(
        matches!(err, Error::JsEvaluation(JsEngineError::EvaluationFailed(_))),
        "unexpected error: {err}"
    );
}

/// V8 も同梱された構成で、設定どおりのエンジンが選ばれること。
#[cfg(all(feature = "js-v8", not(target_os = "macos")))]
fn selection_with_both_engines_bundled() {
    eprintln!("case: engine omitted selects V8");
    let mut rt = runtime_for("", EngineKind::V8);
    assert_eq!(execute_js_stub(&mut rt, "1 + 2").expect("eval").value, "3");

    eprintln!("case: engine = v8");
    let mut rt = runtime_for("[js]\nengine = \"v8\"\n", EngineKind::V8);
    assert_eq!(execute_js_stub(&mut rt, "1 + 2").expect("eval").value, "3");

    eprintln!("case: engine = boa wins over bundled V8");
    let mut boa = runtime_for("[js]\nengine = \"boa\"\n", EngineKind::Boa);
    assert_eq!(execute_js_stub(&mut boa, "1 + 2").expect("eval").value, "3");

    eprintln!("case: runtimes are isolated");
    execute_js_stub(&mut boa, "var only_boa = 1;").expect("eval");
    let mut v8 = runtime_for("[js]\nengine = \"v8\"\n", EngineKind::V8);
    let out = execute_js_stub(&mut v8, "typeof only_boa").expect("eval");
    assert_eq!(out.value, "undefined");
}

/// macOS では boa が別エンジンへフォールバックせず失敗すること（REPAIR-3）。
#[cfg(target_os = "macos")]
fn boa_is_unavailable_on_macos() {
    let cfg = Config::from_toml_str("[js]\nengine = \"boa\"\n").expect("config");
    match JsRuntime::from_config(cfg.js()) {
        Err(err) => assert_eq!(
            err.to_string(),
            "JS execution unavailable: js engine \"boa\" is bundled but not yet implemented"
        ),
        Ok(_) => panic!("boa must be disabled on macOS"),
    }
}

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("case: boa evaluates via config");
        boa_evaluates_via_config();
        #[cfg(feature = "js-v8")]
        selection_with_both_engines_bundled();
    }
    #[cfg(target_os = "macos")]
    {
        eprintln!("case: boa unavailable on macos");
        boa_is_unavailable_on_macos();
    }

    eprintln!("js_stub_boa: all cases passed");
    ExitCode::SUCCESS
}

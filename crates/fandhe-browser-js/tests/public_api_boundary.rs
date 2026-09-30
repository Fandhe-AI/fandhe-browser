//! 公開 API のシグネチャを固定し、`v8` crate の型が公開境界に現れないことを
//! コンパイルで検査する結合テスト（`JS-1`・TASK-29.6.1・Issue #547・REPAIR-4）。
//!
//! 仕組み: `fandhe_browser_js` と `std` の型だけで書いた関数ポインタ型注釈へ
//! 各 `pub` 項目を代入して固定する。シグネチャに `v8` の型が混入すると型
//! 不一致でコンパイルが失敗する。関数は呼ばない（子プロセスを起動しない）。
//!
//! 限界（実装済みを装わない。REPAIR-3）: 結合テストは本パッケージの
//! `[dependencies]`（`v8`）を名指しできてしまうため、保証は型注釈側にある。
//! 本ファイルで `v8` を `use` してはならない。新しい `pub` 項目を追加したら
//! 本ファイルにも追加すること。追加漏れは検出できない（stable では
//! `exported_private_dependencies` が機能しないことを実測済み）。
//! 到達不能であるべきパスは `lib.rs` の `compile_fail` doctest が検査する。

// 関数ポインタ型注釈でシグネチャを丸ごと書き下すのが本ファイルの目的のため、
// 型の複雑さの警告は抑制する。
#![allow(clippy::type_complexity)]

use fandhe_browser_js::{
    CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue,
    NativeCallContext, NativeFn, ObjectHandle, bundled_engines, create_engine,
    run_js_worker_if_requested,
};

/// エンジン種別・同梱一覧・生成関数・共通型のシグネチャが `v8` 非依存であること。
#[test]
fn js_1_public_signatures_are_pinned_without_v8_types() {
    let _: fn(EngineKind) -> &'static str = EngineKind::as_str;
    let _: fn(EngineKind) -> &'static str = EngineKind::feature_name;
    let _: fn(&str) -> Option<EngineKind> = EngineKind::from_config_name;
    let _: fn() -> &'static [EngineKind] = bundled_engines;
    let _: fn(EngineKind) -> Result<Box<dyn JsEngine>, CreateEngineError> = create_engine;
    let _: fn() -> Option<std::process::ExitCode> = run_js_worker_if_requested;
    let _: fn(u32) -> ObjectHandle = ObjectHandle::from_raw;
    let _: fn(ObjectHandle) -> u32 = ObjectHandle::raw;
    let _: fn(
        std::time::Instant,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> NativeCallContext = NativeCallContext::new;
    let _: fn(&NativeCallContext) -> bool = NativeCallContext::is_cancelled;
    let _: fn() -> EvaluateOptions = EvaluateOptions::default;
    let _: NativeFn = Box::new(|_: &[JsValue]| Ok(JsValue::Undefined));

    // トレイトメソッドの署名は、呼び出し形（コンパイルのみ。実行しない）で固定する。
    #[allow(dead_code)]
    fn trait_methods(engine: &mut dyn JsEngine, options: &EvaluateOptions, func: NativeFn) {
        let _: Result<JsValue, JsEngineError> = engine.evaluate_script("", options);
        let _: Result<(), JsEngineError> = engine.inject_global_function("", func);
        let _: Result<(), JsEngineError> =
            engine.bind_dom_like_object("", Vec::<(String, NativeFn)>::new());
    }

    // 具体値の検査（テストとして実体を持たせる）。
    assert_eq!(EngineKind::V8.as_str(), "v8");
    assert_eq!(EngineKind::from_config_name("boa"), Some(EngineKind::Boa));
}

/// エラー型・値型のペイロードが `fandhe_browser_js` と `std` の型だけであること。
#[test]
fn js_1_public_enum_payloads_are_v8_free() {
    let error = JsEngineError::Timeout("t".to_string());
    match &error {
        JsEngineError::EvaluationFailed(m)
        | JsEngineError::BindingFailed(m)
        | JsEngineError::ResourceLimitExceeded(m)
        | JsEngineError::Timeout(m)
        | JsEngineError::EngineUnavailable(m) => {
            let m: &String = m;
            assert_eq!(m, "t");
        }
        _ => unreachable!("JsEngineError is #[non_exhaustive]"),
    }

    let create_error = CreateEngineError::NotYetImplemented {
        requested: EngineKind::V8,
    };
    match &create_error {
        CreateEngineError::NotBundled { requested, bundled } => {
            let _: (&EngineKind, &&'static [EngineKind]) = (requested, bundled);
        }
        CreateEngineError::NotYetImplemented { requested } => {
            let requested: &EngineKind = requested;
            assert_eq!(*requested, EngineKind::V8);
        }
        _ => unreachable!("CreateEngineError is #[non_exhaustive]"),
    }

    let value = JsValue::String("s".to_string());
    match &value {
        JsValue::Undefined | JsValue::Null => {}
        JsValue::Bool(b) => {
            let _: &bool = b;
        }
        JsValue::Number(n) => {
            let _: &f64 = n;
        }
        JsValue::String(s) => {
            let s: &String = s;
            assert_eq!(s, "s");
        }
        JsValue::ObjectHandle(h) => {
            let _: &ObjectHandle = h;
        }
        _ => unreachable!("JsValue is #[non_exhaustive]"),
    }
}

/// `js-v8` 有効時の子プロセス版エンジン（`#[doc(hidden)] pub`）の公開
/// シグネチャが `v8` 非依存であること。
#[cfg(feature = "js-v8")]
#[test]
fn js_1_process_engine_signatures_are_v8_free() {
    use fandhe_browser_js::process_engine::{DomLikeMemberFn, ParentNativeFn, V8ProcessEngine};

    let _: fn() -> V8ProcessEngine = V8ProcessEngine::new;
    let _: fn(&mut V8ProcessEngine, &str, &EvaluateOptions) -> Result<JsValue, JsEngineError> =
        V8ProcessEngine::evaluate_script;
    let _: fn(&mut V8ProcessEngine, &str, ParentNativeFn) -> Result<(), JsEngineError> =
        V8ProcessEngine::inject_global_function;
    let _: fn(
        &mut V8ProcessEngine,
        &str,
        Vec<(String, DomLikeMemberFn)>,
    ) -> Result<ObjectHandle, JsEngineError> = V8ProcessEngine::bind_dom_like_object;
    let _: fn(ParentNativeFn) -> DomLikeMemberFn = DomLikeMemberFn::Method;
    let _: fn(ParentNativeFn) -> DomLikeMemberFn = DomLikeMemberFn::Getter;
}

/// `test-support` 有効時のテスト専用入口のシグネチャが `v8` 非依存であること。
#[cfg(feature = "test-support")]
#[test]
fn js_1_test_support_signatures_are_v8_free() {
    use fandhe_browser_js::process_engine::{V8ProcessEngine, WorkerSpawnConfigForTest};

    let _: fn(WorkerSpawnConfigForTest) -> V8ProcessEngine = V8ProcessEngine::new_for_test;
    let _: fn(&mut V8ProcessEngine, &[u8]) -> std::io::Result<()> =
        V8ProcessEngine::send_raw_frame_for_test;
    let _: fn(&V8ProcessEngine) -> Option<u32> = V8ProcessEngine::worker_pid_for_test;
    let _: fn(&mut V8ProcessEngine) -> Result<(), JsEngineError> =
        V8ProcessEngine::spawn_worker_for_test;
    let _: fn() -> usize = V8ProcessEngine::max_script_source_bytes_for_test;
    let _: fn() -> usize = V8ProcessEngine::max_raw_frame_bytes_for_test;
}

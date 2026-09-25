//! `JsEngine` 実装が満たすべき共通コンフォーマンステスト（TASK-28
//! （28.4）・Issue #150・ビヘイビア `JS-1`・MS-3）。
//!
//! `docs/spec/04-behavior/js-engine.md`「JS エンジンの切替方式」決定 5 は、
//! V8・boa が同じ API 形状（[`JsEngine`] トレイト）で差し替え可能である
//! ことを求める。本ファイルはその「差し替え可能性」を検証する側の入口で
//! あり、[`bundled_engines`] が返す各エンジン種別に対して同じ検査を流す
//! （検査内容は `docs/spec/03-poc/js-engine-comparison` の PoC-3 で実測した
//! `print`・`dom.setText`/`dom.getText`/`dom.count` の形状を下敷きにする）。
//!
//! 呼び出し文脈（将来）: 本ファイルの 3 テストは、V8 の具象実装が入る
//! TASK-29・boa の具象実装が入る TASK-32 が完了した時点で、当該エンジンに
//! 対して実際にスクリプトを評価するようになる（現時点では [`create_engine`]
//! が同梱済みの種別にも `CreateEngineError::NotYetImplemented` を返すため、
//! ここではその契約自体を検証する。実装済みを装わない。REPAIR-3）。
//! TASK-64（コンテナのスモークテスト）は同等の検査をコンテナ内で実行する
//! 想定であり、本ファイルの検査内容がその受け皿になる。
//!
//! 申し送り: TASK-29 は V8 について、TASK-32 は boa について、それぞれの
//! 実装完了後は [`run_for_each_bundled_engine`] 内の
//! `CreateEngineError::NotYetImplemented` 分岐が自分のエンジン種別に対して
//! 発生しないことを確認し、必要ならその種別を分岐対象から絞り込む（また
//! 両方完了後は分岐自体を削除する）。実装後もこの分岐を素通りさせたまま
//! にしない。

use fandhe_browser_js::{
    CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue, NativeFn,
    bundled_engines, create_engine,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// [`bundled_engines`] が返す各エンジン種別に対して `check` を実行する
/// ドライバ（TASK-28.4）。
///
/// - `Ok` の場合はチェック関数へトレイトオブジェクトを渡す（TASK-29/32
///   完了後、この分岐が実際にスクリプトを評価するようになる）
/// - `NotYetImplemented` の場合は、要求した種別と一致することだけを確認する
///   （skip ではなく、現行契約（[`create_engine`] のドキュメント参照）の
///   明示的な回帰確認。TASK-29/32 完了後にこの分岐が発生しなくなる種別から
///   順に絞り込む）
/// - それ以外の `Err`（`NotBundled` を含む。[`bundled_engines`] 由来の種別で
///   起きてはならない）は `panic!` で失敗させる（fail-closed。
///   成功を装うフォールバックを行わない。security.md「偽装・回避機能の禁止」）
fn run_for_each_bundled_engine(check: fn(EngineKind, &mut dyn JsEngine)) {
    for &kind in bundled_engines() {
        match create_engine(kind) {
            Ok(mut engine) => check(kind, engine.as_mut()),
            Err(CreateEngineError::NotYetImplemented { requested }) => {
                assert_eq!(
                    requested, kind,
                    "NotYetImplemented must report the kind it was requested for"
                );
            }
            Err(other) => {
                panic!("create_engine({kind:?}) failed unexpectedly: {other}");
            }
        }
    }
}

/// `JS-1`「スクリプト評価」のコンフォーマンス検査（エンジン非依存）。
fn check_script_evaluation(kind: EngineKind, engine: &mut dyn JsEngine) {
    let options = EvaluateOptions::default();

    let result = engine
        .evaluate_script("1 + 2 * 3", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] arithmetic evaluation failed: {err}"));
    assert_eq!(result, JsValue::Number(7.0), "[{kind:?}] arithmetic result");

    let result = engine
        .evaluate_script("'fandhe' + '-' + 'browser'", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] string concatenation failed: {err}"));
    assert_eq!(
        result,
        JsValue::String("fandhe-browser".into()),
        "[{kind:?}] string concatenation result"
    );

    let result = engine
        .evaluate_script("true", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] bool literal evaluation failed: {err}"));
    assert_eq!(
        result,
        JsValue::Bool(true),
        "[{kind:?}] bool literal result"
    );

    let result = engine
        .evaluate_script("null", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] null literal evaluation failed: {err}"));
    assert_eq!(result, JsValue::Null, "[{kind:?}] null literal result");

    let result = engine
        .evaluate_script("undefined", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] undefined literal evaluation failed: {err}"));
    assert_eq!(
        result,
        JsValue::Undefined,
        "[{kind:?}] undefined literal result"
    );

    let result = engine.evaluate_script("1 +", &options);
    assert!(
        matches!(result, Err(JsEngineError::EvaluationFailed(_))),
        "[{kind:?}] syntax error must yield EvaluationFailed, got {result:?}"
    );

    let result = engine.evaluate_script("throw new Error('boom')", &options);
    assert!(
        matches!(result, Err(JsEngineError::EvaluationFailed(_))),
        "[{kind:?}] runtime exception must yield EvaluationFailed, got {result:?}"
    );
}

/// `JS-1`「グローバル関数注入」のコンフォーマンス検査（エンジン非依存）。
fn check_global_function_injection(kind: EngineKind, engine: &mut dyn JsEngine) {
    let options = EvaluateOptions::default();

    // print: Rust 側が呼び出し引数を記録できること（PoC-3 の print 相当）。
    let printed: Rc<RefCell<Vec<JsValue>>> = Rc::new(RefCell::new(Vec::new()));
    let printed_for_closure = Rc::clone(&printed);
    let print_fn: NativeFn = Box::new(move |args: &[JsValue]| {
        printed_for_closure.borrow_mut().extend_from_slice(args);
        Ok(JsValue::Undefined)
    });
    engine
        .inject_global_function("print", print_fn)
        .unwrap_or_else(|err| panic!("[{kind:?}] injecting print failed: {err}"));
    engine
        .evaluate_script("print('hello')", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] calling print failed: {err}"));
    assert_eq!(
        *printed.borrow(),
        vec![JsValue::String("hello".into())],
        "[{kind:?}] print must record its argument on the Rust side"
    );

    // add: 戻り値が JS へ往復すること。添字アクセスは使わず get() で取り出す
    // （coding-rust.md「外部入力の経路では添字アクセスを使わない」の手本）。
    let add_fn: NativeFn = Box::new(|args: &[JsValue]| match (args.first(), args.get(1)) {
        (Some(JsValue::Number(a)), Some(JsValue::Number(b))) => Ok(JsValue::Number(a + b)),
        _ => Err(JsEngineError::BindingFailed(
            "add expects two numbers".into(),
        )),
    });
    engine
        .inject_global_function("add", add_fn)
        .unwrap_or_else(|err| panic!("[{kind:?}] injecting add failed: {err}"));
    let result = engine
        .evaluate_script("add(2, 3)", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] calling add failed: {err}"));
    assert_eq!(result, JsValue::Number(5.0), "[{kind:?}] add(2, 3) result");

    // ネイティブ関数側のエラーが評価結果へ伝播すること。具体バリアントは
    // エンジン実装依存の余地があるため、ここでは「成功値を返さない」
    // （is_err）ことのみを確認する。
    let failing_fn: NativeFn =
        Box::new(|_args: &[JsValue]| Err(JsEngineError::BindingFailed("always fails".into())));
    engine
        .inject_global_function("alwaysFails", failing_fn)
        .unwrap_or_else(|err| panic!("[{kind:?}] injecting alwaysFails failed: {err}"));
    let result = engine.evaluate_script("alwaysFails()", &options);
    assert!(
        result.is_err(),
        "[{kind:?}] calling a native fn that errors must not yield a success value, got {result:?}"
    );
}

/// `JS-1`「DOM 風オブジェクトへのバインディング」のコンフォーマンス検査
/// （エンジン非依存。PoC-3 の `dom.setText`/`dom.getText`/`dom.count` 相当）。
fn check_dom_like_binding(kind: EngineKind, engine: &mut dyn JsEngine) {
    let options = EvaluateOptions::default();
    let store: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));

    let set_text_fn: NativeFn = {
        let store = Rc::clone(&store);
        Box::new(move |args: &[JsValue]| match (args.first(), args.get(1)) {
            (Some(JsValue::String(id)), Some(JsValue::String(text))) => {
                store.borrow_mut().insert(id.clone(), text.clone());
                Ok(JsValue::Undefined)
            }
            _ => Err(JsEngineError::BindingFailed(
                "setText expects (id, text) strings".into(),
            )),
        })
    };
    let get_text_fn: NativeFn = {
        let store = Rc::clone(&store);
        Box::new(move |args: &[JsValue]| match args.first() {
            Some(JsValue::String(id)) => Ok(JsValue::String(
                store.borrow().get(id).cloned().unwrap_or_default(),
            )),
            _ => Err(JsEngineError::BindingFailed(
                "getText expects an id string".into(),
            )),
        })
    };
    let count_fn: NativeFn = {
        let store = Rc::clone(&store);
        Box::new(move |_args: &[JsValue]| Ok(JsValue::Number(store.borrow().len() as f64)))
    };

    engine
        .bind_dom_like_object(
            "dom",
            vec![
                ("setText".to_string(), set_text_fn),
                ("getText".to_string(), get_text_fn),
                ("count".to_string(), count_fn),
            ],
        )
        .unwrap_or_else(|err| panic!("[{kind:?}] binding dom object failed: {err}"));

    let result = engine
        .evaluate_script(
            "dom.setText('title', 'Fandhe'); dom.getText('title')",
            &options,
        )
        .unwrap_or_else(|err| panic!("[{kind:?}] dom.setText/getText round-trip failed: {err}"));
    assert_eq!(
        result,
        JsValue::String("Fandhe".into()),
        "[{kind:?}] dom.getText('title') result"
    );

    let result = engine
        .evaluate_script("dom.setText('body', 'x'); dom.count()", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] dom.count failed: {err}"));
    assert_eq!(
        result,
        JsValue::Number(2.0),
        "[{kind:?}] dom.count() result"
    );

    assert_eq!(
        store.borrow().get("title").map(String::as_str),
        Some("Fandhe"),
        "[{kind:?}] Rust-side store must reflect dom.setText('title', ...)"
    );
    assert_eq!(
        store.borrow().get("body").map(String::as_str),
        Some("x"),
        "[{kind:?}] Rust-side store must reflect dom.setText('body', ...)"
    );

    let result = engine
        .evaluate_script("dom.getText('missing')", &options)
        .unwrap_or_else(|err| panic!("[{kind:?}] dom.getText(missing id) failed: {err}"));
    assert_eq!(
        result,
        JsValue::String(String::new()),
        "[{kind:?}] dom.getText for an unregistered id must be an empty string"
    );
}

/// JS-1: スクリプト評価が、同梱された各エンジンで同じ形状の結果を返すこと
/// （TASK-28.4・Issue #150）。
#[test]
fn js_1_conformance_script_evaluation_for_each_bundled_engine() {
    run_for_each_bundled_engine(check_script_evaluation);
}

/// JS-1: グローバル関数注入が、同梱された各エンジンで同じ形状で動作すること
/// （TASK-28.4・Issue #150）。
#[test]
fn js_1_conformance_global_function_injection_for_each_bundled_engine() {
    run_for_each_bundled_engine(check_global_function_injection);
}

/// JS-1: DOM 風オブジェクトへのバインディングが、同梱された各エンジンで
/// 同じ形状で動作すること（TASK-28.4・Issue #150）。
#[test]
fn js_1_conformance_dom_like_binding_for_each_bundled_engine() {
    run_for_each_bundled_engine(check_dom_like_binding);
}

//! DOM ブリッジ（`dom_bridge`）を公開 API とエンジン抽象トレイト越しに検証する結合テスト
//! （TASK-108・Issue #777・ビヘイビア `JS-5` / `JS-6`）。
//!
//! 具象エンジン（V8 / boa）には依存せず、`bind_dom_like_object` の呼び出しを捕獲する
//! 偽エンジンで「`__dom.op` がトレイト越しに登録され、捕獲した関数で DOM が変わる」ことを
//! 確認する。実エンジンでの往復は #778 の共通テストの担当。

use fandhe_browser_core::{
    DOM_BRIDGE_METHOD_NAME, DOM_BRIDGE_OBJECT_NAME, DomBridge, DomBridgeLimits, JsEngineError,
    JsValue, NativeFn, ParseOptions, parse_document,
};
use fandhe_browser_js::{EvaluateOptions, JsEngine};

#[derive(Default)]
struct FakeEngine {
    bound: Vec<(String, Vec<(String, NativeFn)>)>,
    injected: usize,
}

impl JsEngine for FakeEngine {
    fn evaluate_script(
        &mut self,
        _script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        Ok(JsValue::Undefined)
    }

    fn inject_global_function(
        &mut self,
        _name: &str,
        _func: NativeFn,
    ) -> Result<(), JsEngineError> {
        self.injected += 1;
        Ok(())
    }

    fn bind_dom_like_object(
        &mut self,
        name: &str,
        methods: Vec<(String, NativeFn)>,
    ) -> Result<(), JsEngineError> {
        self.bound.push((name.to_owned(), methods));
        Ok(())
    }
}

fn s(v: &str) -> JsValue {
    JsValue::String(v.to_owned())
}

#[test]
fn js_5_register_binds_dom_op_via_engine_trait() {
    let bridge = DomBridge::new(DomBridgeLimits::default());
    let document = parse_document(
        "<html><head></head><body></body></html>",
        &ParseOptions::default(),
    )
    .expect("parse")
    .document;
    bridge.attach(document).expect("attach");

    let mut engine = FakeEngine::default();
    bridge
        .register(&mut engine as &mut dyn JsEngine)
        .expect("register");
    assert_eq!(engine.injected, 0);
    assert_eq!(engine.bound.len(), 1);
    let (name, mut methods) = engine.bound.remove(0);
    assert_eq!(name, DOM_BRIDGE_OBJECT_NAME);
    assert_eq!(name, "__dom");
    assert_eq!(methods.len(), 1);
    let (method, mut op) = methods.remove(0);
    assert_eq!(method, DOM_BRIDGE_METHOD_NAME);
    assert_eq!(method, "op");

    let body = match op(&[s("body")]).expect("body") {
        JsValue::Number(n) => n,
        other => panic!("Number を期待: {other:?}"),
    };
    let div = match op(&[s("createElement"), s("div")]).expect("create") {
        JsValue::Number(n) => n,
        other => panic!("Number を期待: {other:?}"),
    };
    op(&[
        s("appendChild"),
        JsValue::Number(body),
        JsValue::Number(div),
    ])
    .expect("append");
    let html = op(&[s("getInnerHTML"), JsValue::Number(body)]).expect("html");
    assert_eq!(html, s("<div></div>"));

    // 不正呼び出しは JS 側の例外（EvaluationFailed）になり、DOM は変わらない。
    let err = op(&[
        s("appendChild"),
        JsValue::Number(f64::NAN),
        JsValue::Number(div),
    ])
    .expect_err("NaN は拒否");
    assert!(matches!(err, JsEngineError::EvaluationFailed(_)));
    let doc = bridge.detach().expect("lock").expect("attached");
    assert_eq!(
        doc.serialize_html().expect("ser").as_str(),
        "<html><head></head><body><div></div></body></html>"
    );
}

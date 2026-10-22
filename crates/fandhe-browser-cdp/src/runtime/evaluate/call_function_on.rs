//! `Runtime.callFunctionOn`（TASK-44.8・#1219、ビヘイビア `CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・MS-4）。
//!
//! 親モジュール [`super`]（`evaluate.rs`）の評価経路（ヘルパー注入・評価・封筒の変換・`awaitPromise` の決着）を
//! 共有する子モジュール。Playwright は utility world の操作を本メソッドで行う。utility script のハンドルを
//! `objectId` にして `this` に置き、`arguments` に `{objectId: utilityScript}` と `value` 4 件
//! （isFunction・returnByValue・expression・argCount）とユーザー引数を並べ、`returnByValue: true`・
//! `awaitPromise: true`・`userGesture: true` を付ける。呼び出し元はディスパッチャ
//! （`protocol::builtin_handlers_with`）で、cli への配線と Playwright の実測は #1220。
//!
//! # 仕組み
//!
//! 引数の検証（件数・要素の形・`unserializableValue` の書式・`objectId` の書式と contextId の一致）は
//! Rust 側の純関数 [`parse_call_arguments`] が行い、検証済みの引数をペイロードの JSON にして
//! ヘルパーの `callFunctionOn` へ渡す。ソースへ可変データを連結しない（`SEC-2`。親モジュール参照）。
//! `this` と引数のハンドルはヘルパーがユーザーコードを動かす前にすべて解決し、1 つでも無ければ
//! 何も実行せず `REMOTE_OBJECT_NOT_FOUND` にする。
//!
//! # 検証順
//!
//! 親モジュールのドキュメント「検証順」の `Runtime.callFunctionOn` を参照。
//!
//! # 差異・スタブ（`REPAIR-3`）
//!
//! - `userGesture` は受け入れるが効果は無い（ユーザー活性化の概念を持たない）。`silent` も同様。型（bool）だけ検査する。
//! - `executionContextId`（`objectId` を使わない呼び出し。Puppeteer の経路）・`objectGroup`・`generatePreview`・
//!   `throwOnSideEffect`・`uniqueContextId`・`serializationOptions` 等は `UNSUPPORTED_PARAMS`（成功を装わない。`CDP-6`）。
//!   `objectId` は必須。
//! - `arguments` は 64 件まで（`CDP-7`）。`unserializableValue` は `NaN`・`±Infinity`・`-0`・bigint の書式だけを許す。
//! - `functionDeclaration` は間接 eval（グローバルスコープ）で関数に評価する。関数でなければ `NOT_A_FUNCTION`。
//!   これは `Runtime.evaluate` と同じ world・同じ評価権限の範囲で、新しい能力は足さない。
//! - `awaitPromise` の checkpoint は 1 回。native Promise の反応ジョブを流せるかはエンジンで異なる:
//!   V8 は評価の終わりに microtask が流れ、`Promise.resolve(42)`・reject・`then` 経由の resolve が
//!   決着する（実測）。boa は js crate が native Promise のジョブ（`run_jobs`）を呼ばないため、同じ 3 件が
//!   すべて `PROMISE_NOT_SETTLED` になる（実測。偽の値は返さない。js crate 側の対応は別 Issue）。結果は結合テスト
//!   （`tests/support/runtime_evaluate_cases.rs`）に具体値で記録している。
//! - 待機を伴う呼び出しは評価を最大 3 回（本体・checkpoint・決着）行うため、所要時間は `timeout` の最大 3 倍になり得る。

use serde_json::{Map, Value, json};

use super::{
    ContextLookupError, EvalLimits, EvalSite, LocateError, MAX_CONTEXT_ID, UNSERIALIZABLE_NUMBERS,
    World, bool_param, helper_call_source, is_bigint_literal, locate, parse_object_id,
};
use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, HandlerOutput};
use fandhe_browser_core::ScriptContextKey;

/// `arguments` の最大件数（`CDP-7`・`SEC-2`。要素を走査する前に件数だけで拒否する）。
const MAX_CALL_ARGUMENTS: usize = 64;

/// 検証済みの呼び出し引数 1 件。
#[derive(Debug, Clone, PartialEq)]
enum CallArg {
    /// `{"value": ...}`。
    Value(Value),
    /// `{"unserializableValue": "NaN" | "Infinity" | "-Infinity" | "-0" | "<digits>n"}`。
    Unserializable(String),
    /// `{"objectId": "<contextId>.<n>"}` のハンドル番号（contextId は `this` と一致検査済み）。
    Handle(u64),
    /// `{}`（`undefined`）。
    Undefined,
}

impl CallArg {
    /// ヘルパーへ渡すペイロード上の表現。
    fn to_payload(&self) -> Value {
        match self {
            Self::Value(v) => json!({ "k": "v", "value": v }),
            Self::Unserializable(t) => json!({ "k": "u", "text": t }),
            Self::Handle(h) => json!({ "k": "h", "handle": h }),
            Self::Undefined => json!({ "k": "undef" }),
        }
    }
}

/// `arguments` を検証して [`CallArg`] の列にする（純関数。評価器を起動しない）。
///
/// 件数が [`MAX_CALL_ARGUMENTS`] を超えれば要素を見ずに `TOO_MANY_CALL_ARGUMENTS`。各要素はオブジェクトで、
/// `value`・`unserializableValue`・`objectId` のうち多くとも 1 つを持つ（未知キー・2 つ以上は
/// `INVALID_PARAMS`）。`objectId` は書式が正しく、contextId が `this_context_id` と同じであること
/// （違えば `ARGUMENT_CONTEXT_MISMATCH`。他セッションの contextId の存在を漏らさないため評価前に Rust 側で比較する）。
fn parse_call_arguments(args: &Value, this_context_id: u64) -> Result<Vec<CallArg>, CdpError> {
    let list = args.as_array().ok_or(CdpError::INVALID_PARAMS)?;
    if list.len() > MAX_CALL_ARGUMENTS {
        return Err(CdpError::TOO_MANY_CALL_ARGUMENTS);
    }
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let obj = item.as_object().ok_or(CdpError::INVALID_PARAMS)?;
        if obj.len() > 1
            || obj
                .keys()
                .any(|k| !matches!(k.as_str(), "value" | "unserializableValue" | "objectId"))
        {
            return Err(CdpError::INVALID_PARAMS);
        }
        let arg = match obj.iter().next() {
            None => CallArg::Undefined,
            Some((k, v)) => match k.as_str() {
                "value" => CallArg::Value(v.clone()),
                "unserializableValue" => {
                    let t = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    if !UNSERIALIZABLE_NUMBERS.contains(&t) && !is_bigint_literal(t) {
                        return Err(CdpError::INVALID_PARAMS);
                    }
                    CallArg::Unserializable(t.to_owned())
                }
                _ => {
                    let id = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    let (ctx, handle) = parse_object_id(id).ok_or(CdpError::INVALID_PARAMS)?;
                    if ctx != this_context_id {
                        return Err(CdpError::ARGUMENT_CONTEXT_MISMATCH);
                    }
                    CallArg::Handle(handle)
                }
            },
        };
        out.push(arg);
    }
    Ok(out)
}

/// ヘルパーの `callFunctionOn` を呼ぶソース。可変データはすべて JSON 文字列リテラル 1 個の中に入る。
fn call_function_source(
    epoch: u64,
    function_declaration: &str,
    this_handle: u64,
    args: &[CallArg],
    return_by_value: bool,
    await_promise: bool,
) -> String {
    helper_call_source(
        "callFunctionOn",
        &json!({
            "epoch": epoch,
            "functionDeclaration": function_declaration,
            "thisHandle": this_handle,
            "args": args.iter().map(CallArg::to_payload).collect::<Vec<_>>(),
            "returnByValue": return_by_value,
            "awaitPromise": await_promise,
        }),
    )
}

/// 既知のキー。これ以外は `UNSUPPORTED_PARAMS`。
fn is_known_key(k: &str) -> bool {
    matches!(
        k,
        "functionDeclaration"
            | "objectId"
            | "arguments"
            | "returnByValue"
            | "awaitPromise"
            | "userGesture"
            | "silent"
    )
}

/// 型の検査を終えた `Runtime.callFunctionOn` のパラメータ。
struct CallParams<'a> {
    function_declaration: &'a str,
    context_id: u64,
    handle: u64,
    args: Vec<CallArg>,
    return_by_value: bool,
    await_promise: bool,
}

/// パラメータを検証順（親モジュール参照）に従って解析する。評価器・セッション状態には触れない。
fn parse_params(obj: &Map<String, Value>) -> Result<CallParams<'_>, CdpError> {
    if obj.keys().any(|k| !is_known_key(k)) {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    let function_declaration = obj
        .get("functionDeclaration")
        .and_then(Value::as_str)
        .ok_or(CdpError::INVALID_PARAMS)?;
    let object_id = obj
        .get("objectId")
        .and_then(Value::as_str)
        .ok_or(CdpError::INVALID_PARAMS)?;
    let arguments = obj.get("arguments");
    if arguments.is_some_and(|a| !a.is_array()) {
        return Err(CdpError::INVALID_PARAMS);
    }
    let return_by_value = bool_param(obj, "returnByValue")?;
    let await_promise = bool_param(obj, "awaitPromise")?;
    // `userGesture`・`silent` は受け入れるが効果は無い（`REPAIR-3`）。型だけ検査する。
    bool_param(obj, "userGesture")?;
    bool_param(obj, "silent")?;
    let (context_id, handle) = parse_object_id(object_id).ok_or(CdpError::INVALID_PARAMS)?;
    debug_assert!((1..=MAX_CONTEXT_ID).contains(&context_id));
    let args = match arguments {
        None => Vec::new(),
        Some(a) => parse_call_arguments(a, context_id)?,
    };
    Ok(CallParams {
        function_declaration,
        context_id,
        handle,
        args,
        return_by_value,
        await_promise,
    })
}

/// `Runtime.callFunctionOn`（`CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・TASK-44.8・#1219）。
///
/// `objectId` のオブジェクトを `this` にして `functionDeclaration` の関数を呼び、`{"result": RemoteObject}`
/// （JS の例外は成功応答の `exceptionDetails`）で返す。`awaitPromise` は microtask checkpoint の後に決着を読む。
pub(crate) struct RuntimeCallFunctionOn {
    limits: EvalLimits,
}

impl RuntimeCallFunctionOn {
    /// 上限を指定して作る（組込みは [`EvalLimits::default`]）。
    pub(crate) fn new(limits: EvalLimits) -> Self {
        Self { limits }
    }
}

impl CommandHandler for RuntimeCallFunctionOn {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            let p = parse_params(obj)?;
            if p.function_declaration.len() > self.limits.max_expression_bytes {
                return Err(CdpError::EXPRESSION_TOO_LARGE);
            }
            let state = ctx.state;
            let registry = state.registry();
            let tid = registry
                .session_target(sid)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if registry.is_paused(&tid) {
                return Err(CdpError::TARGET_WAITING_FOR_DEBUGGER);
            }
            if !state.evaluator_enabled() {
                return Err(CdpError::EXECUTION_CONTEXTS_UNAVAILABLE);
            }
            let located = locate(state, sid, &tid, Some(p.context_id)).map_err(|e| match e {
                LocateError::Document => CdpError::TARGET_DOCUMENT_UNAVAILABLE,
                LocateError::Lookup(ContextLookupError::UnknownContext) => {
                    CdpError::REMOTE_OBJECT_NOT_FOUND
                }
                LocateError::Lookup(l) => l.to_cdp(),
            })?;
            // main world・ヘルパー未注入は、`this` のハンドルが存在し得ないので評価せずに「無い」とする。
            let (view, epoch) = match &located.view {
                Some(v) if v.world != World::Main => match v.helper_epoch {
                    Some(e) => (v.clone(), e),
                    None => return Err(CdpError::REMOTE_OBJECT_NOT_FOUND),
                },
                _ => return Err(CdpError::REMOTE_OBJECT_NOT_FOUND),
            };
            let key = ScriptContextKey::new(located.scope, located.generation, view.id);
            let site = EvalSite {
                state,
                sid,
                view: &view,
                located: &located,
                key,
                epoch,
                limits: &self.limits,
            };
            let source = call_function_source(
                epoch,
                p.function_declaration,
                p.handle,
                &p.args,
                p.return_by_value,
                p.await_promise,
            );
            let text = site.run(&source).await?;
            site.conclude(&text, p.return_by_value).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAX_AWAITING_SLOTS;
    use super::*;

    fn args(v: Value) -> Result<Vec<CallArg>, CdpError> {
        parse_call_arguments(&v, 7)
    }

    /// `CDP-7`: 0・64 件は受理、65 件は要素を見ずに `TOO_MANY_CALL_ARGUMENTS`。
    #[test]
    fn cdp7_call_arguments_are_limited_to_64() {
        assert_eq!(args(json!([])).unwrap(), vec![]);
        let ok: Vec<Value> = (0..64).map(|i| json!({ "value": i })).collect();
        assert_eq!(args(Value::Array(ok)).unwrap().len(), 64);
        // 65 件目以降が不正な要素でも、件数の拒否が先。
        let mut over: Vec<Value> = (0..64).map(|i| json!({ "value": i })).collect();
        over.push(json!(42));
        assert_eq!(
            args(Value::Array(over)).unwrap_err(),
            CdpError::TOO_MANY_CALL_ARGUMENTS
        );
    }

    #[test]
    fn cdp6_call_argument_shapes_are_strict() {
        assert_eq!(
            args(json!([
                {"value": "x"},
                {"value": null},
                {"unserializableValue": "-0"},
                {"unserializableValue": "123n"},
                {"unserializableValue": "-5n"},
                {"objectId": "7.3"},
                {}
            ]))
            .unwrap(),
            vec![
                CallArg::Value(json!("x")),
                CallArg::Value(Value::Null),
                CallArg::Unserializable("-0".to_owned()),
                CallArg::Unserializable("123n".to_owned()),
                CallArg::Unserializable("-5n".to_owned()),
                CallArg::Handle(3),
                CallArg::Undefined,
            ]
        );
        for bad in [
            json!(["x"]),
            json!([null]),
            json!([{"value": 1, "objectId": "7.1"}]),
            json!([{"unknown": 1}]),
            json!([{"unserializableValue": "1e3"}]),
            json!([{"unserializableValue": 5}]),
            json!([{"unserializableValue": "n"}]),
            json!([{"objectId": 5}]),
            json!([{"objectId": "bad"}]),
            json!([{"objectId": "07.1"}]),
        ] {
            assert_eq!(
                args(bad.clone()).unwrap_err(),
                CdpError::INVALID_PARAMS,
                "{bad}"
            );
        }
        assert_eq!(args(json!({})).unwrap_err(), CdpError::INVALID_PARAMS);
    }

    /// 他コンテキストの `objectId` は、評価器を起動せず Rust 側の比較だけで拒否する。
    #[test]
    fn cdp2_argument_from_another_context_is_rejected() {
        assert_eq!(
            args(json!([{"objectId": "8.1"}])).unwrap_err(),
            CdpError::ARGUMENT_CONTEXT_MISMATCH
        );
    }

    /// 連結していないこと: 悪意のある `functionDeclaration`・引数値でも、ソースは固定の前置き + リテラル 1 個 +
    /// 固定の後置きで、リテラルを戻すとペイロードに一致する。
    #[test]
    fn sec2_function_declaration_is_never_concatenated_into_source() {
        let decl_list = [
            "function(){}",
            "\")",
            "'); throw 1; ('",
            "`${1}`",
            "x\u{2028}y\u{2029}z",
            "\"); throw 1; (\"",
            "日本語 😀",
        ];
        for decl in decl_list {
            let a = vec![
                CallArg::Value(json!(decl)),
                CallArg::Unserializable("NaN".to_owned()),
                CallArg::Handle(2),
                CallArg::Undefined,
            ];
            let src = call_function_source(3, decl, 1, &a, true, true);
            let payload = json!({
                "epoch": 3,
                "functionDeclaration": decl,
                "thisHandle": 1,
                "args": [
                    {"k": "v", "value": decl},
                    {"k": "u", "text": "NaN"},
                    {"k": "h", "handle": 2},
                    {"k": "undef"},
                ],
                "returnByValue": true,
                "awaitPromise": true,
            });
            let lit = super::super::js_string_literal(&payload);
            let want = format!(
                "typeof __fandheCdpHelper === \"object\" ? __fandheCdpHelper.callFunctionOn({lit}) : \"{{\\\"kind\\\":\\\"reset\\\"}}\""
            );
            assert_eq!(src, want, "{decl:?}");
            assert!(!lit.contains('\u{2028}') && !lit.contains('\u{2029}'));
            let inner: String = serde_json::from_str(&lit).expect("literal is a JSON string");
            let back: Value = serde_json::from_str(&inner).expect("inner is JSON");
            assert_eq!(back, payload);
        }
    }

    #[test]
    fn cdp6_param_validation_order() {
        let p = |v: Value| parse_params(v.as_object().unwrap()).err();
        // 未知キーは型の誤りより先。
        assert_eq!(
            p(json!({"functionDeclaration": 1, "executionContextId": 1})),
            Some(CdpError::UNSUPPORTED_PARAMS)
        );
        assert_eq!(
            p(json!({"functionDeclaration": "f", "objectId": "1.1", "objectGroup": "g"})),
            Some(CdpError::UNSUPPORTED_PARAMS)
        );
        for bad in [
            json!({}),
            json!({"functionDeclaration": 1, "objectId": "1.1"}),
            json!({"functionDeclaration": "f"}),
            json!({"functionDeclaration": "f", "objectId": 1}),
            json!({"functionDeclaration": "f", "objectId": "x"}),
            json!({"functionDeclaration": "f", "objectId": "1.1", "arguments": {}}),
            json!({"functionDeclaration": "f", "objectId": "1.1", "userGesture": 1}),
            json!({"functionDeclaration": "f", "objectId": "1.1", "silent": "y"}),
            json!({"functionDeclaration": "f", "objectId": "1.1", "awaitPromise": null}),
        ] {
            assert_eq!(p(bad.clone()), Some(CdpError::INVALID_PARAMS), "{bad}");
        }
        // 型は objectId の書式より先、書式は件数より先。
        assert_eq!(
            p(json!({"functionDeclaration": "f", "objectId": "1.1",
                      "arguments": vec![json!({"value": 1}); 65]})),
            Some(CdpError::TOO_MANY_CALL_ARGUMENTS)
        );
        assert_eq!(
            p(json!({"functionDeclaration": "f", "objectId": "x",
                      "arguments": vec![json!({"value": 1}); 65]})),
            Some(CdpError::INVALID_PARAMS)
        );
        // userGesture: true は受け入れる（効果なし。REPAIR-3）。
        assert_eq!(
            p(json!({"functionDeclaration": "f", "objectId": "1.1", "userGesture": true})),
            None
        );
    }

    #[test]
    fn cdp7_awaiting_slot_limit_is_a_concrete_value() {
        assert_eq!(MAX_AWAITING_SLOTS, 64);
        assert_eq!(MAX_CALL_ARGUMENTS, 64);
    }
}

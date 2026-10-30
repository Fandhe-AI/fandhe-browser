//! `DOM.describeNode` と `DOM.resolveNode`（#1348、ビヘイビア `CDP-2`・`CDP-3`・`CDP-6`・`CDP-7`・`SEC-2`・`JS-4`・
//! `REPAIR-3`・`REPAIR-4`、TASK-45・TASK-46・MS-4。設計 `docs/design/main-world-evaluation.md` §5・§6）。
//!
//! Puppeteer の `page.$` は、取得したハンドルを world 間で受け渡すときに
//! `DOM.describeNode {objectId}` → `DOM.resolveNode {backendNodeId, executionContextId}` を送る。DOM ドメインの
//! 採番（`backendNodeId = nodeId = base + 文書順番号`。`dom.rs`）と Runtime のハンドル（`objectId`）を 1 つの
//! ID 空間で行き来させる。親モジュール [`super`]（`evaluate.rs`）の評価経路を共有する子モジュールで、
//! 呼び出し元はディスパッチャ（`protocol::builtin_handlers_with`）。
//!
//! # 仕組み
//!
//! - ノードの同一性は core の `DomBridge` が評価コンテキストへ出す `__fandheDomIdentity`（`orderOf` / `nodeAt`。#1346）で
//!   引く。ヘルパー（`helper.js`）の `nodeOrder`（ハンドル → 初期の文書順番号。ハンドルは作らない）と
//!   `adoptNode`（番号 → ハンドル。ちょうど 1 つ払い出す）がそれを包む。
//! - 数値は検証済みの整数として JSON ペイロードに入れ、JSON 文字列リテラル 1 個にして渡す（`SEC-2`。
//!   `backendNodeId` を文字列化してソースへ連結しない）。
//! - `describeNode` は評価文書に ID 範囲が未払い出しなら払い出す（`DOM.getDocument` なしで送られるため）。
//!   `resolveNode` は払い出さない: 要求元（接続・セッション）が現在の世代の文書に払い出した範囲だけを受理し、
//!   他セッション・旧世代・未払い出し・範囲外は `NODE_NOT_FOUND` で区別せず拒否する（存在を漏らさない）。
//!
//! # 検証順
//!
//! `DOM.describeNode`: `sessionId` なし → `UNSUPPORTED_PARAMS` → 非オブジェクト → `INVALID_PARAMS` → 未知キー
//! （既知は `objectId`・`nodeId`・`backendNodeId`・`depth`・`pierce`）・`nodeId`/`backendNodeId` の指定 →
//! `UNSUPPORTED_PARAMS` → `objectId` の欠落・型・書式の誤り・`depth` の誤り → `INVALID_PARAMS` → `pierce: true` →
//! `UNSUPPORTED_PARAMS` → 未知セッション → `INVALID_PARAMS` → 止めたターゲット → `TARGET_WAITING_FOR_DEBUGGER` →
//! 評価器が無効 → `EXECUTION_CONTEXTS_UNAVAILABLE` → 確定文書なし → `TARGET_DOCUMENT_UNAVAILABLE` → contextId の解決
//! （未知・他セッション・ヘルパー未注入は `REMOTE_OBJECT_NOT_FOUND`、世代違いは `EXECUTION_CONTEXT_STALE`）→
//! ハンドルの確認（無い `REMOTE_OBJECT_NOT_FOUND`・ノードでない `OBJECT_NOT_A_NODE`・確定後に作られた／木に属さない
//! `NODE_HAS_NO_BACKEND_ID`）。
//!
//! `DOM.resolveNode`: `sessionId` なし → `UNSUPPORTED_PARAMS` → 非オブジェクト → `INVALID_PARAMS` → 未知キー
//! （既知は `backendNodeId`・`nodeId`・`executionContextId`・`objectGroup`）・`objectGroup` → `UNSUPPORTED_PARAMS` →
//! `backendNodeId` と `nodeId` の一方だけ（両方・どちらも無しは `INVALID_PARAMS`。値は `1..=2^53-1` の整数）・
//! `executionContextId`（`1..=MAX_CONTEXT_ID`）の誤り → `INVALID_PARAMS` → 未知セッション・止めたターゲット・
//! 評価器が無効（describeNode と同じ）→ 確定文書なし → `TARGET_DOCUMENT_UNAVAILABLE` → contextId の解決（`contextId` 省略時は
//! main world。未知・他セッションは `INVALID_PARAMS`、世代違いは `EXECUTION_CONTEXT_STALE`）→ ID 範囲の照合
//! （`NODE_NOT_FOUND`。評価器を起動しない）→ ハンドルの払い出し（上限超過は `TOO_MANY_REMOTE_OBJECTS`）。
//!
//! # 差異・スタブ（`REPAIR-3`）
//!
//! - **world 間で DOM の変更は共有されない**。main・isolated world はそれぞれ確定 HTML を再パースした独立の DOM を持ち
//!   （`NavigationResult` へ書き戻さない。`JS-4`）、`resolveNode` が返すのは「同じ初期の文書順番号」の別 world のノード。
//!   ある world で `setAttribute` した結果は別 world のノードに現れない。
//! - `describeNode` の `Node` は確定 HTML の初期状態から組み、world 内の変更（属性の書き換え・子の追加）は反映しない。
//!   `nodeId` は Chromium の「未 push なら 0」ではなく本実装の ID 空間の値（`DOM.querySelector` 等で使える）。
//!   `frameId`・`contentDocument`・shadow DOM は返さない。
//! - `describeNode` の `nodeId`・`backendNodeId` 入力、`pierce: true`、`resolveNode` の `objectGroup` は
//!   `UNSUPPORTED_PARAMS`（成功を装わない。`CDP-6`）。`Runtime.addBinding` は未登録（`-32601`）のまま。
//! - 確定後に作られたノード（`document.createElement` 等）は ID 空間に無く、偽の ID を返さず
//!   `NODE_HAS_NO_BACKEND_ID`。

use serde_json::{Map, Value, json};

use super::{
    ContextLookupError, Envelope, EvalLimits, EvalSite, LocateError, MAX_CONTEXT_ID,
    discard_source, ensure_helper, helper_call_source, locate, parse_envelope, parse_object_id,
    remote_object,
};
use crate::dom::{
    MAX_BACKEND_NODE_ID, check_pierce, describe_issued_node, order_in_range, parse_depth,
};
use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, HandlerOutput};
use fandhe_browser_core::ScriptContextKey;

/// ハンドルのノードの初期の文書順番号を問うヘルパー呼び出しのソース（可変データはリテラル 1 個だけ）。
fn node_order_source(epoch: u64, handle: u64) -> String {
    helper_call_source("nodeOrder", &json!({ "epoch": epoch, "handle": handle }))
}

/// 初期の文書順番号のノードのハンドルを払い出すヘルパー呼び出しのソース。
fn adopt_node_source(epoch: u64, order: u64) -> String {
    helper_call_source("adoptNode", &json!({ "epoch": epoch, "order": order }))
}

/// `DOM.describeNode` の検証済みパラメータ。
#[derive(Debug)]
struct DescribeParams {
    context_id: u64,
    handle: u64,
    depth: crate::dom::Depth,
}

/// `DOM.describeNode` のパラメータ検証（モジュール先頭の「検証順」）。
fn parse_describe_params(params: &Value) -> Result<DescribeParams, CdpError> {
    let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
    if obj.keys().any(|k| {
        !matches!(
            k.as_str(),
            "objectId" | "nodeId" | "backendNodeId" | "depth" | "pierce"
        )
    }) || obj.contains_key("nodeId")
        || obj.contains_key("backendNodeId")
    {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    let object_id = obj
        .get("objectId")
        .and_then(Value::as_str)
        .ok_or(CdpError::INVALID_PARAMS)?;
    let (context_id, handle) = parse_object_id(object_id).ok_or(CdpError::INVALID_PARAMS)?;
    let depth = parse_depth(params)?;
    check_pierce(params)?;
    Ok(DescribeParams {
        context_id,
        handle,
        depth,
    })
}

/// `DOM.resolveNode` の検証済みパラメータ。
#[derive(Debug, PartialEq, Eq)]
struct ResolveParams {
    backend_node_id: i64,
    context_id: Option<u64>,
}

/// `DOM.resolveNode` のパラメータ検証。`backendNodeId` と `nodeId` は同じ値の空間なので一方だけ受け付け、
/// 値は数値のまま `1..=MAX_BACKEND_NODE_ID` を検査する（文字列にしない。`SEC-2`）。
fn parse_resolve_params(params: &Value) -> Result<ResolveParams, CdpError> {
    let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
    if obj.keys().any(|k| {
        !matches!(
            k.as_str(),
            "backendNodeId" | "nodeId" | "executionContextId" | "objectGroup"
        )
    }) || obj.contains_key("objectGroup")
    {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    let id = |name: &str, obj: &Map<String, Value>| -> Result<Option<i64>, CdpError> {
        match obj.get(name) {
            None => Ok(None),
            Some(v) => v
                .as_i64()
                .filter(|n| (1..=MAX_BACKEND_NODE_ID).contains(n))
                .map(Some)
                .ok_or(CdpError::INVALID_PARAMS),
        }
    };
    let backend_node_id = match (id("backendNodeId", obj)?, id("nodeId", obj)?) {
        (Some(n), None) | (None, Some(n)) => n,
        _ => return Err(CdpError::INVALID_PARAMS),
    };
    let context_id = match obj.get("executionContextId") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=MAX_CONTEXT_ID).contains(n))
                .ok_or(CdpError::INVALID_PARAMS)?,
        ),
    };
    Ok(ResolveParams {
        backend_node_id,
        context_id,
    })
}

/// `DOM.describeNode`（`CDP-2`・`CDP-3`・`SEC-2`・#1348）。
///
/// `objectId`（`Runtime` のハンドル）のノードを、DOM ドメインの ID 空間の `Node`
/// （`backendNodeId`・`nodeId` を含む）にして `{"node": Node}` で返す。検証順・差異は本モジュールのドキュメント参照。
pub(crate) struct DomDescribeNode {
    limits: EvalLimits,
}

impl DomDescribeNode {
    /// 上限を指定して作る（組込みは [`EvalLimits::default`]）。
    pub(crate) fn new(limits: EvalLimits) -> Self {
        Self { limits }
    }
}

impl CommandHandler for DomDescribeNode {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let p = parse_describe_params(params)?;
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
                LocateError::Create(c) => c.to_cdp(),
            })?;
            // ヘルパー未注入ならハンドルが存在し得ないので、評価器を起動せず「無い」とする。
            let view = located.view.clone();
            let epoch = view.helper_epoch.ok_or(CdpError::REMOTE_OBJECT_NOT_FOUND)?;
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
            let text = site.run(&node_order_source(epoch, p.handle)).await?;
            let order = match parse_envelope(&text)? {
                Envelope::Order { order } => order,
                Envelope::ObjectNotFound => return Err(CdpError::REMOTE_OBJECT_NOT_FOUND),
                Envelope::NotNode => return Err(CdpError::OBJECT_NOT_A_NODE),
                Envelope::NodeUnavailable => return Err(CdpError::NODE_HAS_NO_BACKEND_ID),
                Envelope::Reset => {
                    registry.invalidate_context(sid, view.id);
                    return Err(CdpError::EXECUTION_CONTEXT_RESET);
                }
                _ => return Err(CdpError::SERVER_ERROR),
            };
            let node = describe_issued_node(&ctx, &tid, &located.doc, order, p.depth)?;
            Ok(HandlerOutput::result(json!({ "node": node })))
        })
    }
}

/// `DOM.resolveNode`（`CDP-2`・`CDP-3`・`SEC-2`・#1348）。
///
/// `backendNodeId`（`nodeId` も同値として受理）のノードを `executionContextId`（省略時は main world）の
/// world のハンドルにして `{"object": RemoteObject}` で返す。1 回の呼び出しで払い出すハンドルは 1 つだけ。
/// 対象は要求元セッションの現在の世代の文書に払い出した範囲に限る。検証順・差異は本モジュールのドキュメント参照。
pub(crate) struct DomResolveNode {
    limits: EvalLimits,
}

impl DomResolveNode {
    /// 上限を指定して作る（組込みは [`EvalLimits::default`]）。
    pub(crate) fn new(limits: EvalLimits) -> Self {
        Self { limits }
    }
}

impl CommandHandler for DomResolveNode {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let p = parse_resolve_params(params)?;
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
            let located = locate(state, sid, &tid, p.context_id).map_err(|e| match e {
                LocateError::Document => CdpError::TARGET_DOCUMENT_UNAVAILABLE,
                LocateError::Lookup(l) => l.to_cdp(),
                LocateError::Create(c) => c.to_cdp(),
            })?;
            // 要求元の接続・セッションが現在の世代の文書に払い出した範囲だけを受理する（払い出しはしない）。
            let issued = state.dom_issued();
            let base = issued
                .issued_base(ctx.conn, ctx.session_id, &located.doc)
                .ok_or(CdpError::NODE_NOT_FOUND)?;
            let order = order_in_range(base, p.backend_node_id).ok_or(CdpError::NODE_NOT_FOUND)?;
            let view = located.view.clone();
            let key = ScriptContextKey::new(located.scope, located.generation, view.id);
            let epoch = ensure_helper(state, sid, &view, &located, key, &self.limits).await?;
            let site = EvalSite {
                state,
                sid,
                view: &view,
                located: &located,
                key,
                epoch,
                limits: &self.limits,
            };
            let text = site.run(&adopt_node_source(epoch, order)).await?;
            // 解析・変換の失敗でも `?` で抜けず、コミット済みハンドルを必ず回収する。
            let envelope = match parse_envelope(&text) {
                Ok(e) => e,
                Err(e) => {
                    let _ = site.run(&discard_source(epoch)).await;
                    return Err(e);
                }
            };
            let outcome = match envelope {
                Envelope::Value { result } => remote_object(&result, view.id).and_then(|ro| {
                    let is_node = ro.get("subtype").and_then(Value::as_str) == Some("node")
                        && ro.get("objectId").is_some();
                    if is_node {
                        Ok(ro)
                    } else {
                        Err(CdpError::SERVER_ERROR)
                    }
                }),
                Envelope::NodeUnavailable => return Err(CdpError::NODE_NOT_FOUND),
                Envelope::HandleLimit => return Err(CdpError::TOO_MANY_REMOTE_OBJECTS),
                Envelope::Reset => {
                    registry.invalidate_context(sid, view.id);
                    return Err(CdpError::EXECUTION_CONTEXT_RESET);
                }
                Envelope::ResultTooLarge => return Err(CdpError::EVALUATION_RESULT_TOO_LARGE),
                _ => Err(CdpError::SERVER_ERROR),
            };
            // 評価中に遷移が確定した・払い出しが入れ替わった場合は、払い出したハンドルを回収して拒否する。
            let still_current = state
                .navigations()
                .get(&tid)
                .and_then(|n| n.latest())
                .is_some_and(|l| std::sync::Arc::ptr_eq(&l, &located.doc))
                && issued.issued_base(ctx.conn, ctx.session_id, &located.doc) == Some(base);
            match outcome {
                Ok(ro) if still_current => Ok(HandlerOutput::result(json!({ "object": ro }))),
                Ok(_) => {
                    // ベストエフォート。回収自体の失敗は元のエラーを優先する。
                    let _ = site.run(&discard_source(epoch)).await;
                    Err(CdpError::NODE_NOT_FOUND)
                }
                Err(e) => {
                    let _ = site.run(&discard_source(epoch)).await;
                    Err(e)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp2_describe_params_validation_order() {
        let ok = parse_describe_params(&json!({"objectId": "3.7"})).unwrap();
        assert_eq!((ok.context_id, ok.handle), (3, 7));
        let cases = [
            (json!([]), CdpError::INVALID_PARAMS),
            (
                json!({"objectId": "3.7", "x": 1}),
                CdpError::UNSUPPORTED_PARAMS,
            ),
            (json!({"nodeId": 5}), CdpError::UNSUPPORTED_PARAMS),
            (
                json!({"backendNodeId": 5, "objectId": "3.7"}),
                CdpError::UNSUPPORTED_PARAMS,
            ),
            (json!({}), CdpError::INVALID_PARAMS),
            (json!({"objectId": 5}), CdpError::INVALID_PARAMS),
            (json!({"objectId": "3"}), CdpError::INVALID_PARAMS),
            (json!({"objectId": "03.7"}), CdpError::INVALID_PARAMS),
            (
                json!({"objectId": "3.7", "depth": "1"}),
                CdpError::INVALID_PARAMS,
            ),
            (
                json!({"objectId": "3.7", "pierce": true}),
                CdpError::UNSUPPORTED_PARAMS,
            ),
            (
                json!({"objectId": "3.7", "pierce": 1}),
                CdpError::INVALID_PARAMS,
            ),
        ];
        for (params, want) in cases {
            assert_eq!(
                parse_describe_params(&params).unwrap_err(),
                want,
                "{params}"
            );
        }
        assert!(
            parse_describe_params(&json!({"objectId": "3.7", "depth": 0, "pierce": false})).is_ok()
        );
        assert!(parse_describe_params(&json!({"objectId": "3.7", "depth": -1})).is_ok());
    }

    #[test]
    fn cdp2_resolve_params_boundaries_are_concrete() {
        let r = |v: Value| parse_resolve_params(&v);
        assert_eq!(
            r(json!({"backendNodeId": 1})).unwrap(),
            ResolveParams {
                backend_node_id: 1,
                context_id: None
            }
        );
        assert_eq!(
            r(json!({"nodeId": 9007199254740991_i64, "executionContextId": 4})).unwrap(),
            ResolveParams {
                backend_node_id: 9_007_199_254_740_991,
                context_id: Some(4)
            }
        );
        for bad in [
            json!({"backendNodeId": 0}),
            json!({"backendNodeId": -1}),
            json!({"backendNodeId": 1.5}),
            json!({"backendNodeId": "5"}),
            json!({"backendNodeId": 9007199254740992_i64}),
            json!({"backendNodeId": 18446744073709551615_u64}),
            json!({"backendNodeId": 5, "nodeId": 5}),
            json!({}),
            json!({"executionContextId": 1}),
            json!({"backendNodeId": 5, "executionContextId": 0}),
            json!({"backendNodeId": 5, "executionContextId": "1"}),
            json!({"backendNodeId": 5, "executionContextId": -1}),
        ] {
            assert_eq!(
                r(bad.clone()).unwrap_err(),
                CdpError::INVALID_PARAMS,
                "{bad}"
            );
        }
        assert_eq!(r(json!(5)).unwrap_err(), CdpError::INVALID_PARAMS);
        for bad in [
            json!({"backendNodeId": 5, "objectGroup": "g"}),
            json!({"backendNodeId": 5, "depth": 1}),
        ] {
            assert_eq!(
                r(bad.clone()).unwrap_err(),
                CdpError::UNSUPPORTED_PARAMS,
                "{bad}"
            );
        }
    }

    /// 可変データは JSON 文字列リテラル 1 個だけで、数値を文字列化してソースへ連結しない（`SEC-2`）。
    #[test]
    fn sec2_node_sources_embed_numbers_only_inside_one_literal() {
        let order = node_order_source(5, 77);
        let adopt = adopt_node_source(5, 4_294_967_295);
        let prefix = "typeof __fandheCdpHelper === \"object\" ? __fandheCdpHelper.";
        let suffix = " : \"{\\\"kind\\\":\\\"reset\\\"}\"";
        assert_eq!(
            order,
            format!("{prefix}nodeOrder(\"{{\\\"epoch\\\":5,\\\"handle\\\":77}}\"){suffix}")
        );
        assert_eq!(
            adopt,
            format!("{prefix}adoptNode(\"{{\\\"epoch\\\":5,\\\"order\\\":4294967295}}\"){suffix}")
        );
    }

    #[test]
    fn cdp2_error_messages_are_fixed_strings() {
        assert_eq!(CdpError::OBJECT_NOT_A_NODE.message, "object is not a node");
        assert_eq!(
            CdpError::NODE_HAS_NO_BACKEND_ID.message,
            "node has no backend node id"
        );
    }
}

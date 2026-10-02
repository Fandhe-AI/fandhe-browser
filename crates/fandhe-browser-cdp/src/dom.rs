//! CDP `DOM` ドメイン（TASK-42（42.4）・#242、ビヘイビア `CDP-1`・`SEC-2`・`REPAIR-3`・MS-4）。
//!
//! `protocol::builtin_handlers` から `DOM.getDocument` として登録され、ws.rs 経由で
//! 呼ばれる。`Page.navigate`（42.2）が core の `AppState::navigation()` へ保存した
//! 最終 URL と HTML を読み、core の `parse_document` で DOM 化して CDP の `DOM.Node`
//! 形式の JSON へ変換する。読み取り専用で状態は変更しない。
//!
//! # nodeId の採番（TASK-42.5 `DOM.querySelector` が依存する契約）
//!
//! core の `NodeId` は数値を取り出せないため、cdp 側で [`number_nodes`] が決定的に採番する。
//! root を 1 とし、続いて `descendants(root)` の文書順に 1 ずつ増やす。同じ HTML なら
//! 常に同じ番号になる。`backendNodeId` は `nodeId` と同値。
//!
//! # スタブについて（実装済みを装わない。`REPAIR-3`）
//!
//! - `pierce: true` は明示エラー（`UNSUPPORTED_PARAMS`）。iframe の `contentDocument`・shadow DOM・`<template>` の `templateContent` は返さない
//! - クライアントが `depth` で要求した深さに達したノードは `children` を付けず
//!   `childNodeCount` だけ返す（CDP の契約どおり）。一方、サーバー上限
//!   （[`MAX_TREE_DEPTH`]・[`MAX_NODES_PER_RESPONSE`]）で要求どおりに展開できない場合は、
//!   続きを取る `DOM.requestChildNodes` が未実装のため切り詰めず `DOCUMENT_TOO_LARGE` を返す
//! - `sessionId` 付きの要求は、ターゲット別のナビゲート状態を読む手段が無いため
//!   `TARGET_DOCUMENT_UNAVAILABLE` で拒否する（別ターゲットの文書を返さない）。将来は
//!   ターゲット別ナビゲート状態を `CdpState` 側に持たせて 42.4 / 42.5 が読む（`CDP-1`・`CDP-2`・TASK-43）。
//!   `sessionId` なし（ブラウザレベル）は共有の直近 navigate 結果を読む
//! - `<base href>` の解決は未実装。`href` 付き `base` 要素を持つ文書では `baseURL` を省略する
//! - 採番は文書全体を走査するため、ノード数が [`MAX_NUMBERED_NODES`] を超える文書は
//!   `DOCUMENT_TOO_LARGE` エラーにする（切り詰めない）
//! - 呼び出しごとに再パースし、ノード表のセッション保持・キャッシュは行わない
//! - 直近の navigate 結果が無い場合は空ドキュメントを捏造せずエラーを返す（`SEC-2`）

use std::collections::HashMap;

use fandhe_browser_core::{Document, NodeData, NodeId, ParseOptions, parse_document};
use serde_json::{Map, Value, json};

use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, HandlerOutput};

/// 1 応答で展開する最大の深さ（root = 0）。構築は反復的だが、serde_json の直列化・drop が
/// 深さに比例して再帰するため上限を設ける。
pub(crate) const MAX_TREE_DEPTH: u32 = 256;

/// 1 応答に含めるノード数の上限（応答フレームの肥大化を防ぐ）。
pub(crate) const MAX_NODES_PER_RESPONSE: usize = 10_000;

/// 採番（文書全体の走査・表の保持）を行う文書のノード数上限。nodeId は文書全体の文書順で
/// 決まる契約のため部分採番はできず、超過した文書は黙って切り詰めずエラーにする（`SEC-2`）。
pub(crate) const MAX_NUMBERED_NODES: usize = 200_000;

/// `DOM.getDocument` の展開深さ。`limit` は [`MAX_TREE_DEPTH`] へ丸め済みで、
/// `clamped` は要求値（`-1` 含む）が上限で丸められたことを示す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Depth {
    limit: u32,
    clamped: bool,
}

impl Depth {
    const fn exact(limit: u32) -> Self {
        Self {
            limit,
            clamped: false,
        }
    }

    /// サーバー上限まで全展開（`-1` 相当）。
    #[cfg(test)]
    const fn unlimited() -> Self {
        Self {
            limit: MAX_TREE_DEPTH,
            clamped: true,
        }
    }
}

/// `params` から `depth` を検証して取り出す（省略時 1、-1 は上限まで全展開）。
pub(crate) fn parse_depth(params: &Value) -> Result<Depth, CdpError> {
    let Some(v) = params.get("depth") else {
        return Ok(Depth::exact(1));
    };
    let n = v.as_i64().ok_or(CdpError::INVALID_PARAMS)?;
    match n {
        -1 => Ok(Depth {
            limit: MAX_TREE_DEPTH,
            clamped: true,
        }),
        0.. => match u32::try_from(n) {
            Ok(d) if d <= MAX_TREE_DEPTH => Ok(Depth::exact(d)),
            _ => Ok(Depth {
                limit: MAX_TREE_DEPTH,
                clamped: true,
            }),
        },
        _ => Err(CdpError::INVALID_PARAMS),
    }
}

/// `pierce` を検証する。bool 以外は不正、`true`（shadow DOM・iframe の貫通）は未実装のため
/// 成功を装わず明示的に拒否する（`REPAIR-3`）。実装は後続で `pierce: true` を受理する。
fn check_pierce(params: &Value) -> Result<(), CdpError> {
    match params.get("pierce") {
        None | Some(Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) => Err(CdpError::UNSUPPORTED_PARAMS),
        Some(_) => Err(CdpError::INVALID_PARAMS),
    }
}

/// 文書順の採番結果。42.5 が nodeId から core の `NodeId` を解決するのに使う。
pub(crate) struct NodeNumbering {
    order: Vec<NodeId>,
    ids: HashMap<NodeId, i64>,
}

impl NodeNumbering {
    /// core の `NodeId` に対応する CDP の nodeId（1 始まり）。
    pub(crate) fn node_id(&self, id: NodeId) -> Option<i64> {
        self.ids.get(&id).copied()
    }

    /// CDP の nodeId に対応する core の `NodeId`。
    #[allow(dead_code)] // 42.5（DOM.querySelector）が使う
    pub(crate) fn resolve(&self, node_id: i64) -> Option<NodeId> {
        let idx = usize::try_from(node_id.checked_sub(1)?).ok()?;
        self.order.get(idx).copied()
    }
}

/// root から文書順に採番する（root = 1）。同じ文書なら常に同じ結果になる。
pub(crate) fn number_nodes(doc: &Document) -> NodeNumbering {
    let root = doc.root();
    let mut order = Vec::with_capacity(doc.node_count());
    order.push(root);
    order.extend(doc.descendants(root));
    let ids = order
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i64::try_from(i + 1).unwrap_or(i64::MAX)))
        .collect();
    NodeNumbering { order, ids }
}

/// 修飾名（prefix があれば `prefix:local`）。
fn qualified(prefix: Option<&str>, local: &str) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}:{local}"),
        _ => local.to_owned(),
    }
}

/// 1 ノード分の `DOM.Node`（子・parentId は含めない）。未知の `NodeData` は `None`（スキップ）。
fn node_json(
    doc: &Document,
    id: NodeId,
    node_id: i64,
    document_url: &str,
    include_base_url: bool,
) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    m.insert("nodeId".into(), json!(node_id));
    m.insert("backendNodeId".into(), json!(node_id));
    let (ty, name, local, value): (i64, String, String, String) = match doc.node_data(id)? {
        NodeData::Document => (9, "#document".into(), String::new(), String::new()),
        NodeData::Doctype {
            name,
            public_id,
            system_id,
        } => {
            m.insert("publicId".into(), json!(public_id));
            m.insert("systemId".into(), json!(system_id));
            (10, name.clone(), String::new(), String::new())
        }
        NodeData::Element { name, attrs, .. } => {
            let is_html = &*name.ns == "http://www.w3.org/1999/xhtml";
            let node_name = if is_html {
                name.local.to_string().to_ascii_uppercase()
            } else {
                qualified(name.prefix.as_deref(), &name.local)
            };
            let flat: Vec<Value> = attrs
                .iter()
                .flat_map(|a| {
                    [
                        Value::String(qualified(a.name.prefix.as_deref(), &a.name.local)),
                        Value::String(a.value.clone()),
                    ]
                })
                .collect();
            m.insert("attributes".into(), Value::Array(flat));
            (1, node_name, name.local.to_string(), String::new())
        }
        NodeData::Text { contents } => (3, "#text".into(), String::new(), contents.clone()),
        NodeData::Comment { contents } => (8, "#comment".into(), String::new(), contents.clone()),
        NodeData::ProcessingInstruction { target, data } => {
            (7, target.clone(), String::new(), data.clone())
        }
        NodeData::DocumentFragment => (
            11,
            "#document-fragment".into(),
            String::new(),
            String::new(),
        ),
        _ => return None,
    };
    m.insert("nodeType".into(), json!(ty));
    m.insert("nodeName".into(), json!(name));
    m.insert("localName".into(), json!(local));
    m.insert("nodeValue".into(), json!(value));
    if ty == 9 {
        m.insert("documentURL".into(), json!(document_url));
        if include_base_url {
            m.insert("baseURL".into(), json!(document_url));
        }
        m.insert("xmlVersion".into(), json!(""));
    }
    Some(m)
}

/// `href` 属性を持つ HTML の `base` 要素が文書内にあるか。
fn has_base_href(doc: &Document) -> bool {
    doc.descendants(doc.root()).any(|id| {
        matches!(
            doc.node_data(id),
            Some(NodeData::Element { name, attrs, .. })
                if &*name.ns == "http://www.w3.org/1999/xhtml"
                    && &*name.local == "base"
                    && attrs.iter().any(|a| &*a.name.local == "href")
        )
    })
}

/// `doc` を `DOM.Node`（root）の JSON へ変換する。明示スタックで反復的に構築し、
/// 要求深さに達したノードは `childNodeCount` のみにし、サーバー上限超過は `DOCUMENT_TOO_LARGE`。
pub(crate) fn build_document_node(
    doc: &Document,
    document_url: &str,
    depth: Depth,
) -> Result<Value, CdpError> {
    if doc.node_count() > MAX_NUMBERED_NODES {
        return Err(CdpError::DOCUMENT_TOO_LARGE);
    }
    let numbering = number_nodes(doc);
    let limit = depth.limit;
    let root = doc.root();
    // <base href> は未解決のため、存在する文書では誤った baseURL を返さず省略する（REPAIR-3）。
    let include_base_url = !has_base_href(doc);

    // 1 回目: 文書順（前順）に、含めるノードと展開有無を決める。
    let mut included: Vec<(NodeId, bool)> = Vec::new();
    let mut budget = MAX_NODES_PER_RESPONSE.saturating_sub(1);
    let mut stack: Vec<(NodeId, u32)> = vec![(root, 0)];
    while let Some((id, d)) = stack.pop() {
        let count = doc.children(id).count();
        // サーバー上限で要求どおり展開できないなら、切り詰めた成功応答にせず明示エラーにする。
        // 要求深さ（丸めなし）に達しただけなら CDP の契約どおり childNodeCount のみ返す。
        let expand = if count == 0 {
            false
        } else if d < limit {
            if count > budget {
                return Err(CdpError::DOCUMENT_TOO_LARGE);
            }
            true
        } else if depth.clamped {
            return Err(CdpError::DOCUMENT_TOO_LARGE);
        } else {
            false
        };
        if expand {
            budget -= count;
            let kids: Vec<NodeId> = doc.children(id).collect();
            stack.extend(kids.into_iter().rev().map(|c| (c, d + 1)));
        }
        included.push((id, expand));
    }

    // 2 回目: 逆順（子が先）に JSON を組み立てる。
    let mut built: HashMap<NodeId, Value> = HashMap::new();
    for (id, expanded) in included.into_iter().rev() {
        let Some(nid) = numbering.node_id(id) else {
            continue;
        };
        let Some(mut m) = node_json(doc, id, nid, document_url, include_base_url) else {
            continue;
        };
        if let Some(pid) = doc.parent(id).and_then(|p| numbering.node_id(p)) {
            m.insert("parentId".into(), json!(pid));
        }
        let count = doc.children(id).count();
        if count > 0 {
            m.insert("childNodeCount".into(), json!(count));
        }
        if expanded {
            let kids: Vec<Value> = doc.children(id).filter_map(|c| built.remove(&c)).collect();
            m.insert("children".into(), Value::Array(kids));
        }
        built.insert(id, Value::Object(m));
    }
    Ok(built.remove(&root).unwrap_or(Value::Null))
}

/// `DOM.getDocument` ハンドラ。
pub(crate) struct DomGetDocument;

impl CommandHandler for DomGetDocument {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if let Some(sid) = ctx.session_id {
                if ctx.state.registry().session_target(sid).is_none() {
                    return Err(CdpError::INVALID_PARAMS);
                }
                // ターゲット別の文書を読む手段が無く、共有状態を返すと別ページの内容を漏らし得る。
                return Err(CdpError::TARGET_DOCUMENT_UNAVAILABLE);
            }
            let depth = parse_depth(params)?;
            check_pierce(params)?;
            let latest = ctx
                .state
                .app_state()
                .navigation()
                .latest()
                .ok_or(CdpError::NO_DOCUMENT)?;
            let parsed = parse_document(latest.html(), &ParseOptions::default())
                .map_err(|_| CdpError::SERVER_ERROR)?;
            let root = build_document_node(&parsed.document, latest.url(), depth)?;
            Ok(HandlerOutput::result(json!({ "root": root })))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .unwrap()
            .document
    }

    const PAGE: &str = "<!DOCTYPE html><html><head></head><body><h1 id=\"t\">Hi</h1></body></html>";

    #[test]
    fn cdp1_get_document_default_depth_one() {
        let v = build_document_node(&doc(PAGE), "https://example.com/", Depth::exact(1)).unwrap();
        let expected = json!({
            "nodeId": 1, "backendNodeId": 1, "nodeType": 9, "nodeName": "#document",
            "localName": "", "nodeValue": "", "documentURL": "https://example.com/",
            "baseURL": "https://example.com/", "xmlVersion": "", "childNodeCount": 2,
            "children": [
                {"nodeId": 2, "backendNodeId": 2, "nodeType": 10, "nodeName": "html",
                 "localName": "", "nodeValue": "", "publicId": "", "systemId": "", "parentId": 1},
                {"nodeId": 3, "backendNodeId": 3, "nodeType": 1, "nodeName": "HTML",
                 "localName": "html", "nodeValue": "", "attributes": [], "parentId": 1,
                 "childNodeCount": 2}
            ]
        });
        assert_eq!(v, expected);
    }

    #[test]
    fn cdp1_get_document_depth_unlimited() {
        let v = build_document_node(&doc(PAGE), "u", Depth::unlimited()).unwrap();
        let body = &v["children"][1]["children"][1];
        assert_eq!(body["nodeName"], "BODY");
        let h1 = &body["children"][0];
        assert_eq!(h1["nodeId"], 6);
        assert_eq!(h1["attributes"], json!(["id", "t"]));
        let text = &h1["children"][0];
        assert_eq!(text["nodeType"], 3);
        assert_eq!(text["nodeValue"], "Hi");
        assert_eq!(text["nodeId"], 7);
        assert_eq!(text["parentId"], 6);
    }

    #[test]
    fn cdp1_get_document_depth_zero_returns_root_only() {
        let v = build_document_node(&doc(PAGE), "u", Depth::exact(0)).unwrap();
        assert_eq!(v["childNodeCount"], 2);
        assert!(v.get("children").is_none());
    }

    #[test]
    fn cdp1_get_document_comment_and_svg_namespace() {
        let d = doc("<body><!--c--><svg viewBox=\"0 0 1 1\"></svg></body>");
        let v = build_document_node(&d, "u", Depth::unlimited()).unwrap();
        let body = &v["children"][0]["children"][1];
        assert_eq!(body["children"][0]["nodeName"], "#comment");
        assert_eq!(body["children"][0]["nodeValue"], "c");
        assert_eq!(body["children"][1]["nodeName"], "svg");
        assert_eq!(body["children"][1]["localName"], "svg");
    }

    #[test]
    fn cdp1_get_document_server_depth_cap_is_error() {
        // 上限を超える段数の文書を全展開要求すると、切り詰めずエラーにする。
        let n = MAX_TREE_DEPTH as usize + 10;
        let html = format!("{}{}", "<div>".repeat(n), "</div>".repeat(n));
        let d = doc(&html);
        assert_eq!(
            build_document_node(&d, "u", Depth::unlimited()),
            Err(CdpError::DOCUMENT_TOO_LARGE)
        );
        // 要求深さが上限以内なら同じ文書でも成功する。
        let v = build_document_node(&d, "u", Depth::exact(3)).unwrap();
        assert_eq!(v["children"][0]["children"][1]["childNodeCount"], 1);
    }

    #[test]
    fn cdp1_get_document_node_budget_exceeded_is_error() {
        let n = MAX_NODES_PER_RESPONSE + 50;
        let html = format!("<body>{}</body>", "<i></i>".repeat(n));
        let d = doc(&html);
        assert_eq!(
            build_document_node(&d, "u", Depth::unlimited()),
            Err(CdpError::DOCUMENT_TOO_LARGE)
        );
        // 要求深さで body の手前までなら成功し、childNodeCount が正しい。
        let v = build_document_node(&d, "u", Depth::exact(2)).unwrap();
        let body = &v["children"][0]["children"][1];
        assert_eq!(body["childNodeCount"], n);
        assert!(body.get("children").is_none());
    }

    #[test]
    fn cdp1_get_document_base_url_omitted_with_base_href() {
        let with = doc("<head><base href=\"/x/\"></head><body></body>");
        let v = build_document_node(&with, "https://e.com/a", Depth::exact(0)).unwrap();
        assert_eq!(v["documentURL"], "https://e.com/a");
        assert!(v.get("baseURL").is_none());
        let without = doc(PAGE);
        let v = build_document_node(&without, "https://e.com/a", Depth::exact(0)).unwrap();
        assert_eq!(v["baseURL"], "https://e.com/a");
    }

    #[test]
    fn cdp1_get_document_rejects_oversized_document() {
        let n = MAX_NUMBERED_NODES + 10;
        let html = format!("<body>{}</body>", "<i></i>".repeat(n));
        let d = doc(&html);
        assert_eq!(
            build_document_node(&d, "u", Depth::exact(1)),
            Err(CdpError::DOCUMENT_TOO_LARGE)
        );
    }

    #[test]
    fn cdp1_get_document_invalid_params() {
        for p in [
            json!({"depth": "1"}),
            json!({"depth": 1.5}),
            json!({"depth": -2}),
        ] {
            assert_eq!(parse_depth(&p), Err(CdpError::INVALID_PARAMS));
        }
        assert_eq!(
            check_pierce(&json!({"pierce": 1})),
            Err(CdpError::INVALID_PARAMS)
        );
        // 未実装の pierce: true は成功を装わず明示エラー（REPAIR-3）。
        assert_eq!(
            check_pierce(&json!({"pierce": true})),
            Err(CdpError::UNSUPPORTED_PARAMS)
        );
        assert_eq!(check_pierce(&json!({"pierce": false})), Ok(()));
        assert_eq!(parse_depth(&json!({})), Ok(Depth::exact(1)));
        assert_eq!(parse_depth(&json!({"depth": -1})), Ok(Depth::unlimited()));
        assert_eq!(
            parse_depth(&json!({"depth": 99999})),
            Ok(Depth::unlimited())
        );
        assert_eq!(
            parse_depth(&json!({"depth": 256})),
            Ok(Depth::exact(MAX_TREE_DEPTH))
        );
    }

    #[test]
    fn cdp1_numbering_is_deterministic() {
        let a = number_nodes(&doc(PAGE));
        let b = number_nodes(&doc(PAGE));
        assert_eq!(a.order.len(), b.order.len());
        for i in 1..=a.order.len() as i64 {
            assert_eq!(a.resolve(i).and_then(|n| a.node_id(n)), Some(i));
            assert_eq!(b.resolve(i).and_then(|n| b.node_id(n)), Some(i));
        }
        assert_eq!(a.resolve(0), None);
        assert_eq!(a.resolve(a.order.len() as i64 + 1), None);
    }

    // CdpState は AppState（Profile::open）が必要で、非 unix では構築手段が無い
    // （protocol.rs のテストと同じ理由）ため、状態を要するテストは unix 限定。
    #[cfg(unix)]
    mod dispatch {
        use super::*;
        use crate::protocol::Dispatcher;
        use crate::server::CdpState;
        use fandhe_browser_core::{AppState, NavigationResult};
        use fandhe_browser_profile::Profile;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                Self(base.join(format!("fandhe-cdp-dom-test-{}-{n}", std::process::id())))
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn state(dir: &TempDir) -> Arc<CdpState> {
            let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
            Arc::new(CdpState::new(Arc::new(AppState::with_disabled_renderer(
                profile,
            ))))
        }

        async fn call(st: &Arc<CdpState>, text: &str) -> Value {
            let f = Dispatcher::builtin()
                .unwrap()
                .dispatch(st, text)
                .await
                .into_frames();
            serde_json::from_str(&f[0]).unwrap()
        }

        fn navigate(st: &Arc<CdpState>) {
            let nav = st.app_state().navigation();
            let g = nav.begin_navigation().unwrap();
            nav.commit_navigation(g, NavigationResult::new("https://example.com/", PAGE))
                .unwrap();
        }

        #[tokio::test]
        async fn cdp1_dispatch_get_document_after_navigation() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let v = call(&st, r#"{"id":1,"method":"DOM.getDocument"}"#).await;
            assert_eq!(v["id"], 1);
            let root = &v["result"]["root"];
            assert_eq!(root["nodeId"], 1);
            assert_eq!(root["documentURL"], "https://example.com/");
            assert_eq!(root["children"][1]["nodeName"], "HTML");
        }

        #[tokio::test]
        async fn cdp1_dispatch_get_document_about_blank() {
            let dir = TempDir::new();
            let st = state(&dir);
            let nav = st.app_state().navigation();
            let g = nav.begin_navigation().unwrap();
            nav.clear_navigation(g).unwrap();
            let v = call(
                &st,
                r#"{"id":2,"method":"DOM.getDocument","params":{"depth":-1}}"#,
            )
            .await;
            let html = &v["result"]["root"]["children"][0];
            assert_eq!(html["nodeName"], "HTML");
            assert_eq!(html["children"][0]["nodeName"], "HEAD");
            assert_eq!(html["children"][1]["nodeName"], "BODY");
        }

        #[tokio::test]
        async fn cdp1_dispatch_get_document_without_navigation_is_error() {
            let dir = TempDir::new();
            let st = state(&dir);
            let v = call(&st, r#"{"id":3,"method":"DOM.getDocument"}"#).await;
            assert_eq!(
                v,
                json!({"id": 3, "error": {"code": -32000, "message": "no document loaded"}})
            );
        }

        #[tokio::test]
        async fn cdp1_dispatch_get_document_unknown_session_rejected() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let v = call(
                &st,
                r#"{"id":4,"method":"DOM.getDocument","sessionId":"NOPE-1"}"#,
            )
            .await;
            assert_eq!(
                v["error"],
                json!({"code": -32602, "message": "invalid params"})
            );
            assert!(v.get("result").is_none());
        }

        #[tokio::test]
        async fn cdp1_dispatch_get_document_registered_session_rejected() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let reg = st.registry();
            let tid = reg
                .create_target(crate::target::TargetKind::Page, "about:blank")
                .unwrap();
            let sid = reg.attach(&tid).unwrap();
            let text = format!(
                r#"{{"id":5,"method":"DOM.getDocument","sessionId":"{}"}}"#,
                sid.as_str()
            );
            let v = call(&st, &text).await;
            assert_eq!(
                v["error"],
                json!({"code": -32000, "message": "document not available for target"})
            );
            assert!(v.get("result").is_none());
        }
    }
}

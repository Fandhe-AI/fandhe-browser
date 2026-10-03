//! CDP `DOM` ドメイン（TASK-42（42.4）・#242、ビヘイビア `CDP-1`・`SEC-2`・`REPAIR-3`・MS-4）。
//!
//! `protocol::builtin_handlers` から `DOM.getDocument` として登録され、ws.rs 経由で
//! 呼ばれる。`Page.navigate`（42.2）が core の `AppState::navigation()` へ保存した
//! 最終 URL と HTML を読み、core の `parse_document` で DOM 化して CDP の `DOM.Node`
//! 形式の JSON へ変換する。読み取り専用で状態は変更しない。
//! `DOM.querySelector`（TASK-42.5・#243、`CDP-1`・`CDP-5`）も同じ文書・採番を使う。
//!
//! # nodeId の採番（TASK-42.5 `DOM.querySelector` が依存する契約）
//!
//! core の `NodeId` は数値を取り出せないため、cdp 側で [`number_nodes`] が決定的に採番する。
//! root を `base + 1` とし、続いて `descendants(root)` の文書順に 1 ずつ増やす。`base` は
//! [`IssuedDocuments`] が払い出しごとに単調増加で割り当て、文書（払い出し）をまたいで
//! nodeId を再利用しない（旧文書の nodeId が新文書の別ノードに当たらない。`CDP-1`）。
//! 同じ払い出し済み文書への再 `DOM.getDocument` は同じ `base` を再利用する。
//! `backendNodeId` は `nodeId` と同値。
//!
//! # スタブについて（実装済みを装わない。`REPAIR-3`）
//!
//! - `pierce: true` は明示エラー（`UNSUPPORTED_PARAMS`）。iframe の `contentDocument`・shadow DOM・`<template>` の `templateContent` は返さない
//! - クライアントが `depth` で要求した深さに達したノードは `children` を付けず
//!   `childNodeCount` だけ返す（CDP の契約どおり）。一方、サーバー上限
//!   （[`MAX_TREE_DEPTH`]・[`MAX_NODES_PER_RESPONSE`]）で要求どおりに展開できない場合は、
//!   続きを取る `DOM.requestChildNodes` が未実装のため切り詰めず `DOCUMENT_TOO_LARGE` を返す
//! - `sessionId` 付きの要求は、そのセッションのターゲットが最後に確定した文書
//!   （`CdpState::navigations()`。#658）だけを返す。未遷移・遷移中・取得失敗は
//!   `TARGET_DOCUMENT_UNAVAILABLE`（空文書を捏造しない）。`frameId` の固定値・
//!   セッション／ターゲットに基づく決定は `CDP-2`・TASK-43（接続単位の nodeId 分離は実装済み）。
//!   `sessionId` なし（ブラウザレベル）は共有の直近 navigate 結果を読む
//! - `<base href>` の解決は未実装。`href` 付き `base` 要素を持つ文書では `baseURL` を省略する
//! - 採番は文書全体を走査するため、ノード数が [`MAX_NUMBERED_NODES`] を超える文書は
//!   `DOCUMENT_TOO_LARGE` エラーにする（切り詰めない）
//! - 呼び出しごとに再パースするが、nodeId の世代は [`IssuedDocuments`] で管理する。
//!   `DOM.getDocument` が払い出した文書（`Arc<NavigationResult>` の同一性）を要求元セッション別に
//!   記録し、`DOM.querySelector` は最新の確定文書がその記録と同一のときだけ nodeId を解決する。
//!   getDocument 未呼び出し・遷移後の古い nodeId は `NODE_NOT_FOUND`（別文書の別ノードへ
//!   解決しない。`CDP-1`）。ノード表そのものの保持・`DOM.requestChildNodes` は後続（`CDP-2`・TASK-43）
//! - `DOM.querySelector`: 一致なしは `{"nodeId": 0}`（`CDP-5`）。構文不正は `INVALID_PARAMS`、
//!   未対応構文（core のサブセット外）は `UNSUPPORTED_PARAMS`、解決できない nodeId は
//!   `NODE_NOT_FOUND`（0 を捏造しない。`SEC-2`）。テキスト等の非コンテナを起点にした場合は
//!   子孫要素が無いため 0 を返す（Chrome はエラー。差異）
//! - 直近の navigate 結果が無い場合は空ドキュメントを捏造せずエラーを返す（`SEC-2`）

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use fandhe_browser_core::NavigationResult;
use fandhe_browser_core::{
    Document, Error, NodeData, NodeId, ParseError, ParseOptions, ParsedDocument, parse_document,
    query_selector_str,
};
use serde_json::{Map, Value, json};

use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, ConnId, HandlerOutput};
use crate::target::{SessionId, TargetId};

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
/// nodeId は `base + 1` から始まる（`base` は払い出しごとの単調増加オフセット）。
pub(crate) struct NodeNumbering {
    base: i64,
    order: Vec<NodeId>,
    ids: HashMap<NodeId, i64>,
}

impl NodeNumbering {
    /// core の `NodeId` に対応する CDP の nodeId（`base + 1` 始まり）。
    pub(crate) fn node_id(&self, id: NodeId) -> Option<i64> {
        self.ids.get(&id).copied()
    }

    /// CDP の nodeId に対応する core の `NodeId`。
    pub(crate) fn resolve(&self, node_id: i64) -> Option<NodeId> {
        let idx = usize::try_from(node_id.checked_sub(self.base)?.checked_sub(1)?).ok()?;
        self.order.get(idx).copied()
    }
}

/// root から文書順に採番する（root = `base + 1`）。同じ文書・同じ `base` なら常に同じ結果になる。
pub(crate) fn number_nodes(doc: &Document, base: i64) -> NodeNumbering {
    let root = doc.root();
    let mut order = Vec::with_capacity(doc.node_count());
    order.push(root);
    order.extend(doc.descendants(root));
    let ids = order
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let off = i64::try_from(i + 1).unwrap_or(i64::MAX);
            (*id, base.saturating_add(off))
        })
        .collect();
    NodeNumbering { base, order, ids }
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
/// nodeId は `base + 1` から採番する（[`number_nodes`]）。
pub(crate) fn build_document_node(
    doc: &Document,
    document_url: &str,
    depth: Depth,
    base: i64,
) -> Result<Value, CdpError> {
    if doc.node_count() > MAX_NUMBERED_NODES {
        return Err(CdpError::DOCUMENT_TOO_LARGE);
    }
    let numbering = number_nodes(doc, base);
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

/// `sessionId` を解決する（なければブラウザレベル = `None`）。未登録は `INVALID_PARAMS`。
fn resolve_target(ctx: &CommandContext<'_>) -> Result<Option<TargetId>, CdpError> {
    match ctx.session_id {
        Some(sid) => Ok(Some(
            ctx.state
                .registry()
                .session_target(sid)
                .ok_or(CdpError::INVALID_PARAMS)?,
        )),
        None => Ok(None),
    }
}

/// 対象の最終確定文書を返す。`load_document` と、払い出し記録時の最新性再確認が共有する。
fn current_latest(
    ctx: &CommandContext<'_>,
    target: Option<&TargetId>,
) -> Result<Arc<NavigationResult>, CdpError> {
    match target {
        // そのターゲットが最後に確定した文書だけを読む。共有状態へはフォールバックしない
        // （別ターゲットの内容を返さない。`CDP-1`）。未確定・失敗は明示エラー（`SEC-2`）。
        Some(tid) => ctx
            .state
            .navigations()
            .get(tid)
            .and_then(|n| n.latest())
            .ok_or(CdpError::TARGET_DOCUMENT_UNAVAILABLE),
        None => ctx
            .state
            .app_state()
            .navigation()
            .latest()
            .ok_or(CdpError::NO_DOCUMENT),
    }
}

/// 対象の最終確定文書を取得して DOM 化する。`DOM.getDocument`・`DOM.querySelector` が共有し、
/// 文書の選択・上限・エラー写像を 1 か所に保つ。
fn load_document(
    ctx: &CommandContext<'_>,
    target: Option<&TargetId>,
) -> Result<(Arc<NavigationResult>, ParsedDocument), CdpError> {
    let latest = current_latest(ctx, target)?;
    // DOM 応答用の上限をパース時点で適用し、巨大文書の全ノード構築を避ける（`SEC-2`）。
    let opts = ParseOptions::default().with_max_nodes(MAX_NUMBERED_NODES);
    let parsed = parse_document(latest.html(), &opts).map_err(|e| match e {
        Error::Parse(ParseError::NodeLimitExceeded { .. }) => CdpError::DOCUMENT_TOO_LARGE,
        _ => CdpError::SERVER_ERROR,
    })?;
    Ok((latest, parsed))
}

/// `DOM.getDocument` が nodeId を払い出した文書の記録（`CDP-1`）。
///
/// キーは（WebSocket 接続 ID, 要求元の `sessionId`（ブラウザレベルは `None`））の組。
/// 別セッション・別接続が同じターゲットへ attach していても、`DOM.getDocument` を呼んだ
/// 接続・セッションの nodeId だけが有効になる（ブラウザレベルも接続ごとに分離）。接続が閉じたら
/// [`IssuedDocuments::release_connection`] でその接続の記録を解放する。
/// 接続 ID（[`ConnId`]）は `crate::ws` が `WsConnContext` の接続単位で割り当てて渡す。`None` は WebSocket を介さない
/// 直接 dispatch（テスト）専用。
/// 値は払い出し時点の確定文書と nodeId の `base`（[`number_nodes`]）で、
/// `Arc` を保持するためアドレスが再利用されず `Arc::ptr_eq` で同一文書を判定できる。
/// 遷移が確定すると新しい `Arc` に置き換わり、旧 nodeId は無効になる。保持件数は
/// 生存セッション数 + 1 以下（detach・ターゲット閉鎖で消えたセッションは記録時に破棄。`SEC-2`）。
///
/// `base` は全セッション共通の単調増加カウンタから払い出し、一度割り当てた範囲を
/// 再利用しない。これにより、遷移後に再度 `DOM.getDocument` しても旧文書の nodeId は
/// 新文書のどのノードにも解決されず、別セッションの nodeId とも重ならない（`CDP-1`）。
/// 同じ払い出し済み文書に対する再 `DOM.getDocument` は既存の `base` を再利用する。
///
/// 記録は文書（世代）単位で、ノード単位では持たない。払い出し済み文書に対しては、
/// `DOM.getDocument` が（深さ制限で）返していない範囲の nodeId も `number_nodes` の
/// 採番内なら検索起点として受理する（採番が決定的で、同じ文書をセレクタで辿れる範囲を
/// 超える情報は得られないため。Chrome の「要求済みノードのみ有効」とは差異。`CDP-1`）。
/// 一度も払い出していない範囲の nodeId は `NODE_NOT_FOUND`。
pub(crate) struct IssuedDocuments {
    inner: Mutex<IssuedState>,
}

/// 払い出し記録のキー（接続 ID, `sessionId`）。
type IssuedKey = (Option<ConnId>, Option<SessionId>);

struct IssuedState {
    docs: HashMap<IssuedKey, (Arc<NavigationResult>, i64)>,
    /// 次に割り当てる `base`（これまでに予約した nodeId 範囲の末尾）。
    next_base: i64,
    /// 生存中の接続。切断済みの接続への記録を拒否するために持つ（`SEC-2`）。
    live: HashSet<ConnId>,
}

impl IssuedDocuments {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(IssuedState {
                docs: HashMap::new(),
                next_base: 0,
                live: HashSet::new(),
            }),
        }
    }

    /// 接続 `conn` を生存中として登録する（`crate::ws` が接続の初回要求時に呼ぶ）。
    /// 登録のない接続・[`Self::release_connection`] 済みの接続への [`Self::record`] は拒否される。
    pub(crate) fn open_connection(&self, conn: ConnId) {
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        st.live.insert(conn);
    }

    /// `doc` に使う nodeId の `base` を返す。同じ文書が払い出し済みならその `base` を再利用し、
    /// そうでなければ `node_count` 個分の範囲を新規予約する（予約した範囲は記録に失敗しても
    /// 再利用しない）。カウンタがあふれる場合は `SERVER_ERROR`（`SEC-2`）。
    fn base_for(
        &self,
        conn: Option<ConnId>,
        session: Option<&SessionId>,
        doc: &Arc<NavigationResult>,
        node_count: usize,
    ) -> Result<i64, CdpError> {
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((issued, base)) = st.docs.get(&(conn, session.cloned()))
            && Arc::ptr_eq(issued, doc)
        {
            return Ok(*base);
        }
        let base = st.next_base;
        let span = i64::try_from(node_count).map_err(|_| CdpError::SERVER_ERROR)?;
        st.next_base = base.checked_add(span).ok_or(CdpError::SERVER_ERROR)?;
        Ok(base)
    }

    /// `doc`（nodeId の `base`）を払い出し済みとして記録する（旧記録は置き換え）。
    ///
    /// 並行する古い `DOM.getDocument` が遅れて完了しても新しい文書の記録を上書きしないよう、
    /// 記録用ロックを保持したまま `latest` で最新の確定文書を再取得し、`doc` と同一のときだけ
    /// 記録する（古い文書は記録しない）。同じ文書が別の `base` で記録済みの場合（並行する
    /// 同一文書の getDocument）は既存を維持して `false` を返し、呼び出し側が既存 `base` で
    /// 作り直す。戻り値は `base` が有効な記録として確定したか。
    fn record(
        &self,
        registry: &crate::target::TargetRegistry,
        conn: Option<ConnId>,
        session: Option<&SessionId>,
        doc: &Arc<NavigationResult>,
        base: i64,
        latest: impl FnOnce() -> Option<Arc<NavigationResult>>,
    ) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        st.docs.retain(|(_, k), _| {
            k.as_ref()
                .is_none_or(|s| registry.session_target(s).is_some())
        });
        if !latest().is_some_and(|l| Arc::ptr_eq(&l, doc)) {
            return false;
        }
        if !session.is_none_or(|s| registry.session_target(s).is_some()) {
            return false;
        }
        // 切断（`release_connection`）と競合した記録を拒否する。確認と挿入は同一ロック内。
        // 閉じた接続には応答の届け先がないため、呼び出し側の再試行が尽きて失敗しても害はない。
        if conn.is_some_and(|c| !st.live.contains(&c)) {
            return false;
        }
        if let Some((issued, existing)) = st.docs.get(&(conn, session.cloned()))
            && Arc::ptr_eq(issued, doc)
        {
            return *existing == base;
        }
        st.docs
            .insert((conn, session.cloned()), (Arc::clone(doc), base));
        true
    }

    /// 接続 `conn` の払い出し記録をすべて解放する（切断時に `crate::ws` の `on_close` から呼ぶ。
    /// 閉じた接続の記録を残さない。`SEC-2`・`CDP-1`）。`base` カウンタは巻き戻さない。
    pub(crate) fn release_connection(&self, conn: ConnId) {
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        st.live.remove(&conn);
        st.docs.retain(|(c, _), _| *c != Some(conn));
    }

    /// 記録の件数（テスト用）。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .docs
            .len()
    }

    /// `doc` が現在の払い出し済み文書と同一ならその nodeId の `base`。
    fn issued_base(
        &self,
        conn: Option<ConnId>,
        session: Option<&SessionId>,
        doc: &Arc<NavigationResult>,
    ) -> Option<i64> {
        let st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        st.docs
            .get(&(conn, session.cloned()))
            .filter(|(issued, _)| Arc::ptr_eq(issued, doc))
            .map(|(_, base)| *base)
    }
}

/// `params` から `(nodeId, selector)` を取り出す。欠落・型違いは `INVALID_PARAMS`。
fn parse_query_params(params: &Value) -> Result<(i64, &str), CdpError> {
    let node_id = params
        .get("nodeId")
        .and_then(Value::as_i64)
        .ok_or(CdpError::INVALID_PARAMS)?;
    let selector = params
        .get("selector")
        .and_then(Value::as_str)
        .ok_or(CdpError::INVALID_PARAMS)?;
    Ok((node_id, selector))
}

/// `nodeId` 配下でセレクタに最初に一致する要素の nodeId を返す。一致なしは `0`（`CDP-5`）。
///
/// 構文不正・未対応構文・処理量超過は 0 にせずエラーにする（一致なしを装わない。`REPAIR-3`・`SEC-2`）。
/// 採番は [`number_nodes`] で `DOM.getDocument` と同一（払い出し時の `base` を渡す）。
fn query_selector_node_id(
    doc: &Document,
    base: i64,
    node_id: i64,
    selector: &str,
) -> Result<i64, CdpError> {
    if doc.node_count() > MAX_NUMBERED_NODES {
        return Err(CdpError::DOCUMENT_TOO_LARGE);
    }
    let numbering = number_nodes(doc, base);
    let scope = numbering.resolve(node_id).ok_or(CdpError::NODE_NOT_FOUND)?;
    match query_selector_str(doc, scope, selector) {
        Ok(Some(found)) => numbering.node_id(found).ok_or(CdpError::SERVER_ERROR),
        Ok(None) => Ok(0),
        Err(Error::InvalidInput { .. }) => Err(CdpError::INVALID_PARAMS),
        Err(Error::Unsupported { .. }) => Err(CdpError::UNSUPPORTED_PARAMS),
        Err(Error::MatchCacheLimitExceeded { .. }) => Err(CdpError::DOCUMENT_TOO_LARGE),
        Err(_) => Err(CdpError::SERVER_ERROR),
    }
}

/// `DOM.querySelector` ハンドラ（TASK-42.5・#243、`CDP-1`・`CDP-5`）。
///
/// `protocol::builtin_handlers` から登録される。直近（または `sessionId` のターゲットの）
/// 確定文書を再パースし、`DOM.getDocument` と同じ採番で nodeId を返す。
pub(crate) struct DomQuerySelector;

impl CommandHandler for DomQuerySelector {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            // 検証順: セッション解決 → パラメータ検証 → 文書の取得 → 照合。
            let target = resolve_target(&ctx)?;
            let (node_id, selector) = parse_query_params(params)?;
            let (latest, parsed) = load_document(&ctx, target.as_ref())?;
            // nodeId は getDocument が払い出した文書でのみ有効。未取得・遷移後は解決しない（`CDP-1`）。
            // 払い出し記録は要求元の接続・セッション単位（別接続・別セッションの getDocument では有効にならない）。
            let Some(base) = ctx
                .state
                .dom_issued()
                .issued_base(ctx.conn, ctx.session_id, &latest)
            else {
                return Err(CdpError::NODE_NOT_FOUND);
            };
            let found = query_selector_node_id(&parsed.document, base, node_id, selector)?;
            // 検索中に遷移が確定した場合は旧文書の結果を返さない（`CDP-1`）。
            let still_latest =
                current_latest(&ctx, target.as_ref()).is_ok_and(|l| Arc::ptr_eq(&l, &latest));
            if !still_latest
                || ctx
                    .state
                    .dom_issued()
                    .issued_base(ctx.conn, ctx.session_id, &latest)
                    != Some(base)
            {
                return Err(CdpError::NODE_NOT_FOUND);
            }
            Ok(HandlerOutput::result(json!({ "nodeId": found })))
        })
    }
}

/// 文書構築中の遷移競合で記録できなかった場合の再試行上限（`SEC-2`: 無限ループ防止）。
const MAX_ISSUE_ATTEMPTS: usize = 3;

/// `build` が `(root, recorded)` を返す。記録できなかった（構築中に遷移して最新でなくなった・
/// セッションが消えた）旧文書の nodeId は後続の `DOM.querySelector` で解決できないため成功応答に
/// しない。新しい文書で上限回まで作り直し、収束しなければ明示エラーを返す（`CDP-1`・`REPAIR-3`）。
fn build_recorded_root(
    mut build: impl FnMut() -> Result<(Value, bool), CdpError>,
) -> Result<Value, CdpError> {
    for _ in 0..MAX_ISSUE_ATTEMPTS {
        let (root, recorded) = build()?;
        if recorded {
            return Ok(root);
        }
    }
    Err(CdpError::SERVER_ERROR)
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
            // 検証順: セッション解決 → パラメータ検証 → 文書の取得。
            let target = resolve_target(&ctx)?;
            let depth = parse_depth(params)?;
            check_pierce(params)?;
            let root = build_recorded_root(|| {
                let (latest, parsed) = load_document(&ctx, target.as_ref())?;
                let base = ctx.state.dom_issued().base_for(
                    ctx.conn,
                    ctx.session_id,
                    &latest,
                    parsed.document.node_count(),
                )?;
                let root = build_document_node(&parsed.document, latest.url(), depth, base)?;
                // 構築中に遷移・並行 getDocument で最新でなくなった文書は記録しない（新世代の
                // 記録を古い `Arc` で上書きしない）。記録できたかを呼び出し側へ返す。
                let recorded = ctx.state.dom_issued().record(
                    ctx.state.registry(),
                    ctx.conn,
                    ctx.session_id,
                    &latest,
                    base,
                    || current_latest(&ctx, target.as_ref()).ok(),
                );
                Ok((root, recorded))
            })?;
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
        let v =
            build_document_node(&doc(PAGE), "https://example.com/", Depth::exact(1), 0).unwrap();
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
        let v = build_document_node(&doc(PAGE), "u", Depth::unlimited(), 0).unwrap();
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
        let v = build_document_node(&doc(PAGE), "u", Depth::exact(0), 0).unwrap();
        assert_eq!(v["childNodeCount"], 2);
        assert!(v.get("children").is_none());
    }

    #[test]
    fn cdp1_get_document_comment_and_svg_namespace() {
        let d = doc("<body><!--c--><svg viewBox=\"0 0 1 1\"></svg></body>");
        let v = build_document_node(&d, "u", Depth::unlimited(), 0).unwrap();
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
            build_document_node(&d, "u", Depth::unlimited(), 0),
            Err(CdpError::DOCUMENT_TOO_LARGE)
        );
        // 要求深さが上限以内なら同じ文書でも成功する。
        let v = build_document_node(&d, "u", Depth::exact(3), 0).unwrap();
        assert_eq!(v["children"][0]["children"][1]["childNodeCount"], 1);
    }

    #[test]
    fn cdp1_get_document_node_budget_exceeded_is_error() {
        let n = MAX_NODES_PER_RESPONSE + 50;
        let html = format!("<body>{}</body>", "<i></i>".repeat(n));
        let d = doc(&html);
        assert_eq!(
            build_document_node(&d, "u", Depth::unlimited(), 0),
            Err(CdpError::DOCUMENT_TOO_LARGE)
        );
        // 要求深さで body の手前までなら成功し、childNodeCount が正しい。
        let v = build_document_node(&d, "u", Depth::exact(2), 0).unwrap();
        let body = &v["children"][0]["children"][1];
        assert_eq!(body["childNodeCount"], n);
        assert!(body.get("children").is_none());
    }

    #[test]
    fn cdp1_get_document_base_url_omitted_with_base_href() {
        let with = doc("<head><base href=\"/x/\"></head><body></body>");
        let v = build_document_node(&with, "https://e.com/a", Depth::exact(0), 0).unwrap();
        assert_eq!(v["documentURL"], "https://e.com/a");
        assert!(v.get("baseURL").is_none());
        let without = doc(PAGE);
        let v = build_document_node(&without, "https://e.com/a", Depth::exact(0), 0).unwrap();
        assert_eq!(v["baseURL"], "https://e.com/a");
    }

    #[test]
    fn cdp1_get_document_rejects_oversized_document() {
        let n = MAX_NUMBERED_NODES + 10;
        let html = format!("<body>{}</body>", "<i></i>".repeat(n));
        let d = doc(&html);
        assert_eq!(
            build_document_node(&d, "u", Depth::exact(1), 0),
            Err(CdpError::DOCUMENT_TOO_LARGE)
        );
    }

    #[test]
    fn sec2_dom_parse_limit_rejects_before_full_build() {
        // DOM 用上限でのパースは、上限超過をパース時点で NodeLimitExceeded にする（SEC-2）。
        let n = MAX_NUMBERED_NODES + 10;
        let html = format!("<body>{}</body>", "<i></i>".repeat(n));
        let opts = ParseOptions::default().with_max_nodes(MAX_NUMBERED_NODES);
        assert!(matches!(
            parse_document(&html, &opts),
            Err(Error::Parse(ParseError::NodeLimitExceeded {
                limit: MAX_NUMBERED_NODES
            }))
        ));
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

    fn qs(node_id: i64, selector: &str) -> Result<i64, CdpError> {
        query_selector_node_id(&doc(PAGE), 0, node_id, selector)
    }

    #[test]
    fn cdp1_query_selector_returns_matching_node_id() {
        for sel in ["h1", "#t", "body > h1"] {
            assert_eq!(qs(1, sel), Ok(6), "{sel}");
        }
    }

    #[test]
    fn cdp1_query_selector_node_id_matches_get_document() {
        let d = doc(PAGE);
        let tree = build_document_node(&d, "u", Depth::unlimited(), 0).unwrap();
        let h1 = &tree["children"][1]["children"][1]["children"][0];
        assert_eq!(h1["nodeName"], "H1");
        assert_eq!(
            query_selector_node_id(&d, 0, 1, "h1"),
            Ok(h1["nodeId"].as_i64().unwrap())
        );
    }

    #[test]
    fn cdp1_query_selector_scoped_to_node() {
        assert_eq!(qs(5, "h1"), Ok(6));
        assert_eq!(qs(5, "head"), Ok(0));
    }

    #[test]
    fn cdp1_query_selector_excludes_scope_itself() {
        assert_eq!(qs(6, "h1"), Ok(0));
    }

    #[test]
    fn cdp5_query_selector_missing_selector_returns_zero() {
        assert_eq!(qs(1, "#does-not-exist"), Ok(0));
    }

    #[test]
    fn cdp1_query_selector_text_node_scope_returns_zero() {
        assert_eq!(qs(7, "h1"), Ok(0));
    }

    #[test]
    fn repair3_query_selector_unsupported_selector_is_error() {
        for sel in ["h1:first-child", "*", "h1 + p"] {
            assert_eq!(qs(1, sel), Err(CdpError::UNSUPPORTED_PARAMS), "{sel}");
        }
    }

    #[test]
    fn sec2_query_selector_invalid_selector_is_error() {
        let long = "a".repeat(4097);
        for sel in ["", "#", long.as_str()] {
            assert_eq!(
                qs(1, sel),
                Err(CdpError::INVALID_PARAMS),
                "len {}",
                sel.len()
            );
        }
    }

    #[test]
    fn cdp1_query_selector_unknown_node_id_is_error() {
        for id in [0, -1, 8, i64::MAX] {
            assert_eq!(qs(id, "h1"), Err(CdpError::NODE_NOT_FOUND), "{id}");
        }
    }

    #[test]
    fn cdp1_query_selector_invalid_params() {
        for p in [
            json!({"selector": "h1"}),
            json!({"nodeId": "1", "selector": "h1"}),
            json!({"nodeId": 1.5, "selector": "h1"}),
            json!({"nodeId": 1}),
            json!({"nodeId": 1, "selector": 5}),
        ] {
            assert_eq!(parse_query_params(&p), Err(CdpError::INVALID_PARAMS), "{p}");
        }
        assert_eq!(
            parse_query_params(&json!({"nodeId": 1, "selector": "h1"})),
            Ok((1, "h1"))
        );
    }

    #[test]
    fn cdp1_numbering_is_deterministic() {
        let a = number_nodes(&doc(PAGE), 0);
        let b = number_nodes(&doc(PAGE), 0);
        assert_eq!(a.order.len(), b.order.len());
        for i in 1..=a.order.len() as i64 {
            assert_eq!(a.resolve(i).and_then(|n| a.node_id(n)), Some(i));
            assert_eq!(b.resolve(i).and_then(|n| b.node_id(n)), Some(i));
        }
        assert_eq!(a.resolve(0), None);
        assert_eq!(a.resolve(a.order.len() as i64 + 1), None);
    }

    /// 記録できなかった文書の root は成功で返さず、再試行または明示エラーにする（`CDP-1`）。
    #[test]
    fn cdp1_build_recorded_root_retries_then_errors() {
        let mut calls = 0;
        let r = build_recorded_root(|| {
            calls += 1;
            Ok((json!({ "n": calls }), calls == 2))
        });
        assert_eq!(r.unwrap(), json!({ "n": 2 }));
        let mut calls = 0;
        let r = build_recorded_root(|| {
            calls += 1;
            Ok((json!({}), false))
        });
        assert_eq!(r.unwrap_err(), CdpError::SERVER_ERROR);
        assert_eq!(calls, MAX_ISSUE_ATTEMPTS);
    }

    /// 接続 A で払い出した記録は接続 B からは見えず、接続切断で解放される（`CDP-1`・`SEC-2`）。
    #[test]
    fn cdp1_issued_is_isolated_per_connection_and_released() {
        let registry = crate::target::TargetRegistry::new();
        let d = Arc::new(NavigationResult::new("https://example.com/", "<p>a</p>"));
        let issued = IssuedDocuments::new();
        let (a, b) = (ConnId::new(1), ConnId::new(2));
        issued.open_connection(a);
        issued.open_connection(b);
        let base = issued.base_for(Some(a), None, &d, 10).unwrap();
        assert!(issued.record(&registry, Some(a), None, &d, base, || Some(Arc::clone(&d))));
        assert_eq!(issued.issued_base(Some(a), None, &d), Some(base));
        assert_eq!(issued.issued_base(Some(b), None, &d), None);
        issued.release_connection(b);
        assert_eq!(issued.len(), 1);
        issued.release_connection(a);
        assert_eq!(issued.len(), 0);
        assert_eq!(issued.issued_base(Some(a), None, &d), None);
    }

    /// 切断（release）後の記録要求は拒否され、閉じた接続の文書が残らない（`SEC-2`・`CDP-1`）。
    #[test]
    fn sec2_record_after_release_connection_is_rejected() {
        let registry = crate::target::TargetRegistry::new();
        let d = Arc::new(NavigationResult::new("https://example.com/", "<p>a</p>"));
        let issued = IssuedDocuments::new();
        let conn = ConnId::new(7);
        issued.open_connection(conn);
        let base = issued.base_for(Some(conn), None, &d, 10).unwrap();
        // getDocument の途中で切断が走った状況を再現する。
        issued.release_connection(conn);
        assert!(
            !issued.record(&registry, Some(conn), None, &d, base, || Some(Arc::clone(
                &d
            )))
        );
        assert_eq!(issued.len(), 0);
        assert_eq!(issued.issued_base(Some(conn), None, &d), None);
        // 未登録の接続も同様に拒否する。
        assert!(
            !issued.record(&registry, Some(ConnId::new(8)), None, &d, base, || Some(
                Arc::clone(&d)
            ))
        );
        assert_eq!(issued.len(), 0);
    }

    /// 古い文書の記録要求は、最新文書の払い出し記録を上書きしない（`CDP-1`）。
    #[test]
    fn cdp1_issued_record_rejects_stale_document() {
        let registry = crate::target::TargetRegistry::new();
        let old = Arc::new(NavigationResult::new("https://example.com/", "<p>old</p>"));
        let new = Arc::new(NavigationResult::new("https://example.com/", "<p>new</p>"));
        let issued = IssuedDocuments::new();
        let nb = issued.base_for(None, None, &new, 10).unwrap();
        let ob = issued.base_for(None, None, &old, 10).unwrap();
        // 最新は new。new を記録できる。
        assert!(issued.record(&registry, None, None, &new, nb, || Some(Arc::clone(&new))));
        // 遅れて完了した old は最新でないため記録されず、new が有効なまま残る。
        assert!(!issued.record(&registry, None, None, &old, ob, || Some(Arc::clone(&new))));
        assert_eq!(issued.issued_base(None, None, &new), Some(nb));
        assert_eq!(issued.issued_base(None, None, &old), None);
        // 最新を取得できない場合も記録しない。
        assert!(!issued.record(&registry, None, None, &old, ob, || None));
    }

    /// nodeId の範囲は文書をまたいで再利用せず、同一文書の再払い出しは同じ範囲（`CDP-1`）。
    #[test]
    fn cdp1_issued_base_is_monotonic_and_reused_per_document() {
        let registry = crate::target::TargetRegistry::new();
        let a = Arc::new(NavigationResult::new("https://example.com/", "<p>a</p>"));
        let b = Arc::new(NavigationResult::new("https://example.com/", "<p>a</p>"));
        let issued = IssuedDocuments::new();
        let ba = issued.base_for(None, None, &a, 10).unwrap();
        assert_eq!(ba, 0);
        assert!(issued.record(&registry, None, None, &a, ba, || Some(Arc::clone(&a))));
        // 同一文書の再払い出しは既存の base を再利用する。
        assert_eq!(issued.base_for(None, None, &a, 10).unwrap(), 0);
        // 別文書（同一 HTML でも別 Arc）は重ならない範囲を得る。
        let bb = issued.base_for(None, None, &b, 10).unwrap();
        assert_eq!(bb, 10);
        assert!(issued.record(&registry, None, None, &b, bb, || Some(Arc::clone(&b))));
        assert_eq!(issued.issued_base(None, None, &a), None);
        assert_eq!(issued.issued_base(None, None, &b), Some(10));
        // 旧範囲の nodeId は新文書の採番に含まれない。
        let n = number_nodes(&doc(PAGE), 10);
        assert_eq!(n.resolve(1), None);
        assert_eq!(n.resolve(10), None);
        assert!(n.resolve(11).is_some());
        // 同一文書が別 base で記録済みなら並行記録は確定させない。
        assert!(!issued.record(&registry, None, None, &b, 99, || Some(Arc::clone(&b))));
        // カウンタのあふれは明示エラー。
        assert_eq!(
            issued.base_for(None, None, &a, usize::MAX),
            Err(CdpError::SERVER_ERROR)
        );
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

        fn session_for_new_target(st: &Arc<CdpState>) -> (crate::target::TargetId, String) {
            let reg = st.registry();
            let tid = reg
                .create_target(crate::target::TargetKind::Page, "about:blank")
                .unwrap();
            let sid = reg.attach(&tid).unwrap();
            (tid, sid.as_str().to_owned())
        }

        fn commit_target(st: &Arc<CdpState>, tid: &crate::target::TargetId, url: &str, html: &str) {
            let n = st.navigations().get_or_create(st.registry(), tid);
            let g = n.begin_navigation().unwrap();
            n.commit_navigation(g, NavigationResult::new(url, html))
                .unwrap();
        }

        fn get_doc_text(sid: &str, params: &str) -> String {
            format!(
                r#"{{"id":5,"method":"DOM.getDocument","sessionId":"{sid}","params":{params}}}"#
            )
        }

        const UNAVAILABLE: &str =
            r#"{"code":-32000,"message":"document not available for target"}"#;

        /// 登録済みでも未遷移のターゲットは、共有状態に文書があっても返さない（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_dispatch_get_document_unnavigated_session_unavailable() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let (_tid, sid) = session_for_new_target(&st);
            let v = call(&st, &get_doc_text(&sid, "{}")).await;
            assert_eq!(
                v["error"],
                serde_json::from_str::<Value>(UNAVAILABLE).unwrap()
            );
            assert!(v.get("result").is_none());
        }

        /// 遷移中（begin のみ・未 commit）のターゲットは明示エラー（`REPAIR-3`・`SEC-2`）。
        #[tokio::test]
        async fn repair3_dispatch_get_document_in_flight_session_unavailable() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (tid, sid) = session_for_new_target(&st);
            commit_target(&st, &tid, "https://old.example/", PAGE);
            let n = st.navigations().get(&tid).unwrap();
            n.begin_navigation().unwrap();
            let v = call(&st, &get_doc_text(&sid, "{}")).await;
            assert_eq!(
                v["error"],
                serde_json::from_str::<Value>(UNAVAILABLE).unwrap()
            );
        }

        /// セッション付きはターゲットの文書、セッションなしは共有状態の文書を返す（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_dispatch_get_document_target_and_shared_are_separate() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let (tid, sid) = session_for_new_target(&st);
            commit_target(&st, &tid, "https://target.example/", PAGE);
            let t = call(&st, &get_doc_text(&sid, "{}")).await;
            assert_eq!(
                t["result"]["root"]["documentURL"],
                "https://target.example/"
            );
            let s = call(&st, r#"{"id":6,"method":"DOM.getDocument"}"#).await;
            assert_eq!(s["result"]["root"]["documentURL"], "https://example.com/");
        }

        /// セッション付きでもパラメータ検証は有効（`REPAIR-3`）。
        #[tokio::test]
        async fn repair3_dispatch_get_document_session_validates_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (tid, sid) = session_for_new_target(&st);
            commit_target(&st, &tid, "https://target.example/", PAGE);
            let bad = call(&st, &get_doc_text(&sid, r#"{"depth":-2}"#)).await;
            assert_eq!(
                bad["error"],
                json!({"code": -32602, "message": "invalid params"})
            );
            let pierce = call(&st, &get_doc_text(&sid, r#"{"pierce":true}"#)).await;
            assert_eq!(
                pierce["error"],
                json!({"code": -32602, "message": "unsupported params"})
            );
        }

        fn qs_text(id: u32, sid: Option<&str>, params: &str) -> String {
            let sess = sid.map_or(String::new(), |s| format!(r#","sessionId":"{s}""#));
            format!(r#"{{"id":{id},"method":"DOM.querySelector"{sess},"params":{params}}}"#)
        }

        #[tokio::test]
        async fn cdp1_dispatch_query_selector_after_navigation() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            call(&st, r#"{"id":90,"method":"DOM.getDocument"}"#).await;
            let v = call(&st, &qs_text(1, None, r#"{"nodeId":1,"selector":"h1"}"#)).await;
            assert_eq!(v, json!({"id": 1, "result": {"nodeId": 6}}));
        }

        #[tokio::test]
        async fn cdp5_dispatch_query_selector_missing_returns_zero() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            call(&st, r#"{"id":90,"method":"DOM.getDocument"}"#).await;
            let v = call(
                &st,
                &qs_text(2, None, r##"{"nodeId":1,"selector":"#does-not-exist"}"##),
            )
            .await;
            assert_eq!(v, json!({"id": 2, "result": {"nodeId": 0}}));
        }

        #[tokio::test]
        async fn cdp1_dispatch_query_selector_without_navigation_is_error() {
            let dir = TempDir::new();
            let st = state(&dir);
            let v = call(&st, &qs_text(3, None, r#"{"nodeId":1,"selector":"h1"}"#)).await;
            assert_eq!(
                v["error"],
                json!({"code": -32000, "message": "no document loaded"})
            );
            assert!(v.get("result").is_none());
        }

        #[tokio::test]
        async fn cdp1_dispatch_query_selector_unknown_session_rejected() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let v = call(
                &st,
                &qs_text(4, Some("NOPE-1"), r#"{"nodeId":1,"selector":"h1"}"#),
            )
            .await;
            assert_eq!(
                v["error"],
                json!({"code": -32602, "message": "invalid params"})
            );
        }

        /// セッション付きはそのターゲットの文書だけを検索する（共有状態へフォールバックしない）。
        #[tokio::test]
        async fn cdp1_dispatch_query_selector_session_uses_target_document() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let (tid, sid) = session_for_new_target(&st);
            let none = call(
                &st,
                &qs_text(5, Some(&sid), r#"{"nodeId":1,"selector":"h1"}"#),
            )
            .await;
            assert_eq!(
                none["error"],
                serde_json::from_str::<Value>(UNAVAILABLE).unwrap()
            );
            commit_target(&st, &tid, "https://target.example/", "<p id=\"x\">a</p>");
            call(&st, &get_doc_text(&sid, "{}")).await;
            let hit = call(
                &st,
                &qs_text(6, Some(&sid), r##"{"nodeId":1,"selector":"#x"}"##),
            )
            .await;
            assert_eq!(hit["result"]["nodeId"], 5);
            let miss = call(
                &st,
                &qs_text(7, Some(&sid), r#"{"nodeId":1,"selector":"h1"}"#),
            )
            .await;
            assert_eq!(miss["result"]["nodeId"], 0);
        }

        #[tokio::test]
        async fn sec2_dispatch_query_selector_error_does_not_echo_selector() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            call(&st, r#"{"id":90,"method":"DOM.getDocument"}"#).await;
            let v = call(
                &st,
                &qs_text(8, None, r#"{"nodeId":1,"selector":"h1:MARKER-xyz"}"#),
            )
            .await;
            assert_eq!(
                v["error"],
                json!({"code": -32602, "message": "unsupported params"})
            );
            assert!(!v.to_string().contains("MARKER-xyz"));
        }

        /// getDocument 前の nodeId は解決しない（`CDP-1`・`SEC-2`）。
        #[tokio::test]
        async fn cdp1_dispatch_query_selector_requires_get_document() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let v = call(&st, &qs_text(9, None, r#"{"nodeId":1,"selector":"h1"}"#)).await;
            assert_eq!(
                v["error"],
                json!({"code": -32000, "message": "node not found"})
            );
        }

        /// 遷移確定後は旧文書の nodeId を新文書の別ノードへ解決せず、再取得で復活する（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_dispatch_query_selector_stale_node_id_after_navigation() {
            let dir = TempDir::new();
            let st = state(&dir);
            navigate(&st);
            let gd = r#"{"id":90,"method":"DOM.getDocument"}"#;
            call(&st, gd).await;
            let q = qs_text(10, None, r#"{"nodeId":1,"selector":"h1"}"#);
            assert_eq!(call(&st, &q).await["result"]["nodeId"], 6);
            // 同一 HTML への再遷移でも別文書として旧 nodeId を無効化する。
            navigate(&st);
            let stale = call(&st, &q).await;
            assert_eq!(stale["error"]["message"], "node not found");
            assert!(stale.get("result").is_none());
            // 再取得後の nodeId は旧文書の範囲と重ならない（旧 nodeId 1 は新文書でも無効）。
            let root = call(&st, gd).await["result"]["root"]["nodeId"]
                .as_i64()
                .unwrap();
            assert_eq!(root, 8);
            assert_eq!(call(&st, &q).await["error"]["message"], "node not found");
            let q2 = qs_text(14, None, r#"{"nodeId":8,"selector":"h1"}"#);
            assert_eq!(call(&st, &q2).await["result"]["nodeId"], 13);
        }

        /// ターゲット別の記録は互いに独立（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_dispatch_query_selector_target_issuance_is_separate() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (tid, sid) = session_for_new_target(&st);
            commit_target(&st, &tid, "https://t.example/", PAGE);
            navigate(&st);
            call(&st, r#"{"id":90,"method":"DOM.getDocument"}"#).await;
            let v = call(
                &st,
                &qs_text(11, Some(&sid), r#"{"nodeId":1,"selector":"h1"}"#),
            )
            .await;
            assert_eq!(v["error"]["message"], "node not found");
        }

        /// 同じターゲットへ attach した別セッションの getDocument では nodeId が有効にならない（`CDP-1`）。
        #[tokio::test]
        async fn cdp1_dispatch_query_selector_issuance_is_per_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (tid, sid_a) = session_for_new_target(&st);
            let sid_b = st.registry().attach(&tid).unwrap().as_str().to_owned();
            commit_target(&st, &tid, "https://t.example/", PAGE);
            call(&st, &get_doc_text(&sid_a, "{}")).await;
            let q_a = qs_text(12, Some(&sid_a), r#"{"nodeId":1,"selector":"h1"}"#);
            assert_eq!(call(&st, &q_a).await["result"]["nodeId"], 6);
            let q_b = qs_text(13, Some(&sid_b), r#"{"nodeId":1,"selector":"h1"}"#);
            let v = call(&st, &q_b).await;
            assert_eq!(v["error"]["message"], "node not found");
            let root_b = call(&st, &get_doc_text(&sid_b, "{}")).await["result"]["root"]["nodeId"]
                .as_i64()
                .unwrap();
            // 別セッションの払い出しは重ならない範囲になり、q_a の nodeId は q_b では解決しない。
            assert_eq!(root_b, 8);
            assert_eq!(call(&st, &q_b).await["error"]["message"], "node not found");
            let q_b2 = qs_text(15, Some(&sid_b), r#"{"nodeId":8,"selector":"h1"}"#);
            assert_eq!(call(&st, &q_b2).await["result"]["nodeId"], 13);
        }
    }
}

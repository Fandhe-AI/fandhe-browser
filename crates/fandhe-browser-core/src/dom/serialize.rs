//! dom::serialize: [`Document`] の HTML シリアライズ（上限つき。TASK-107・Issue #773・
//! ビヘイビア `JS-4` / `JS-6`・MS-6）。
//!
//! ページ内 JS の実行後の DOM（`JS-4`）を、既存の `NavigationResult`（URL と HTML 文字列）
//! 経由の取得・snapshot 経路へ流せるよう、arena を HTML 文字列へ戻す。呼び出し元は
//! 後続の NavigationResult 更新（TASK-112）と `innerHTML` / `outerHTML` getter
//! （TASK-108 の `__dom.op` ブリッジ）を想定する。
//!
//! # 設計上の要点
//!
//! - WHATWG「HTML fragment serialization algorithm」に従う（void 要素は終了タグなし・
//!   `script` / `style` 等の raw text 要素の子テキストは無加工・テキストでは
//!   `& < > U+00A0`、属性値では `& " < > U+00A0` をエスケープ・`<template>` は
//!   template contents を出力・`pre` / `textarea` / `listing` は先頭 LF を補う）。
//!   属性値の `<` / `>` エスケープは 2025 年改訂（mXSS 低減）の挙動を採用する。再パース結果は
//!   旧挙動と変わらない。
//! - 明示スタックの反復実装で再帰しない。深さに上限は設けず、10,000 段の入れ子でも
//!   スタックオーバーフローしない（release は `panic = "abort"`）。走査回数は
//!   [`Document::node_count`] から導く上限で打ち切り、壊れた arena（循環）でも停止する。
//! - 出力は push の都度 `DomLimits::max_serialized_bytes`（既定 8 MiB。`JS-6`）に対して
//!   `checked_add` で検証し、超える時点で [`DomError::SerializedOutputTooLarge`] を返す。
//!   途中までの出力を成功扱いで返さない（fail-closed）。
//! - 要素名・属性名は変更 API 側で ASCII 限定の検証済み（`mutation`）。パース由来の名前は
//!   パーサーが受理した文字列で、再パースで同一になる。
//!
//! # 既知の非往復ケース（REPAIR-3）
//!
//! WHATWG どおり無加工で出す箇所は、JS が変更 API で作った内容により再パースで構造が
//! 変わりうる: raw text 要素のテキストが終了タグ列（`</script` 等）を含む場合、コメント本文が
//! `-->` を含む場合。ブラウザの `innerHTML` と同じ挙動で、内容は同一ページ由来のため
//! 信頼境界は越えない。

use std::time::Instant;

use super::{Document, HTML_NAMESPACE_URI, NodeData, NodeId, QualName};
use crate::error::{DomError, Error, Result};
use crate::observability::{FailureKind, OperationKind, OperationOutcome};

const SVG_NAMESPACE_URI: &str = "http://www.w3.org/2000/svg";
const MATHML_NAMESPACE_URI: &str = "http://www.w3.org/1998/Math/MathML";
const XML_NAMESPACE_URI: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE_URI: &str = "http://www.w3.org/2000/xmlns/";
const XLINK_NAMESPACE_URI: &str = "http://www.w3.org/1999/xlink";

/// シリアライズ範囲（[`Document::serialize_node`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SerializeScope {
    /// ノードの子のみ（`innerHTML` 相当。Document ルートなら文書全体）。
    ChildrenOnly,
    /// ノード自身を含む（`outerHTML` 相当）。
    IncludeNode,
}

/// シリアライズ結果（[`Document::serialize_node`] / [`Document::serialize_html`] の成功値。
/// `JS-4`・`REPAIR-4`）。
///
/// 文字列のみを返すと将来の付随情報（出力バイト数・打ち切り情報等）を足せないため、
/// `#[non_exhaustive]` な構造体で包む。外部 crate からは構築できず、フィールド追加は
/// 破壊的変更にならない。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SerializeResult {
    /// シリアライズした HTML 文字列。
    pub html: String,
}

impl SerializeResult {
    /// HTML 文字列を借用で返す。
    pub fn as_str(&self) -> &str {
        &self.html
    }

    /// HTML 文字列を取り出す。
    pub fn into_html(self) -> String {
        self.html
    }
}

enum Frame {
    Open(NodeId),
    Close(NodeId),
}

/// 上限つきの出力バッファ。超過時は部分出力を返さない。
struct Out {
    buf: String,
    limit: usize,
}

impl Out {
    fn push(&mut self, s: &str) -> std::result::Result<(), DomError> {
        match self.buf.len().checked_add(s.len()) {
            Some(total) if total <= self.limit => {
                self.buf.push_str(s);
                Ok(())
            }
            _ => Err(DomError::SerializedOutputTooLarge { limit: self.limit }),
        }
    }
}

fn is_void(name: &QualName) -> bool {
    &*name.ns == HTML_NAMESPACE_URI
        && matches!(
            &*name.local,
            "area"
                | "base"
                | "basefont"
                | "bgsound"
                | "br"
                | "col"
                | "embed"
                | "frame"
                | "hr"
                | "img"
                | "input"
                | "keygen"
                | "link"
                | "meta"
                | "param"
                | "source"
                | "track"
                | "wbr"
        )
}

/// 要素名の出力（HTML / SVG / MathML はローカル名、それ以外は prefix があれば修飾名）。
fn push_element_name(out: &mut Out, name: &QualName) -> std::result::Result<(), DomError> {
    let ns: &str = &name.ns;
    if !matches!(
        ns,
        HTML_NAMESPACE_URI | SVG_NAMESPACE_URI | MATHML_NAMESPACE_URI
    ) && let Some(prefix) = &name.prefix
    {
        out.push(prefix)?;
        out.push(":")?;
    }
    out.push(&name.local)
}

/// 属性名の出力（WHATWG の名前空間別規則）。
fn push_attr_name(out: &mut Out, name: &QualName) -> std::result::Result<(), DomError> {
    let ns: &str = &name.ns;
    let local: &str = &name.local;
    match ns {
        "" => out.push(local),
        XML_NAMESPACE_URI => {
            out.push("xml:")?;
            out.push(local)
        }
        XMLNS_NAMESPACE_URI => {
            if local != "xmlns" {
                out.push("xmlns:")?;
            }
            out.push(local)
        }
        XLINK_NAMESPACE_URI => {
            out.push("xlink:")?;
            out.push(local)
        }
        _ => {
            if let Some(prefix) = &name.prefix {
                out.push(prefix)?;
                out.push(":")?;
            }
            out.push(local)
        }
    }
}

/// テキスト／属性値のエスケープ出力。`attr` が真なら `"` も `&quot;` にする。
fn push_escaped(out: &mut Out, s: &str, attr: bool) -> std::result::Result<(), DomError> {
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let rep = match c {
            '&' => "&amp;",
            '\u{a0}' => "&nbsp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' if attr => "&quot;",
            _ => continue,
        };
        out.push(s.get(start..i).unwrap_or(""))?;
        out.push(rep)?;
        start = i + c.len_utf8();
    }
    out.push(s.get(start..).unwrap_or(""))
}

impl Document {
    /// 文書全体（ルートの子）を HTML 文字列へシリアライズする。
    ///
    /// `serialize_node(root, ChildrenOnly)` の薄いラッパー。出力が
    /// `DomLimits::max_serialized_bytes`（既定 [`crate::DEFAULT_MAX_SERIALIZED_BYTES`]）を
    /// 超えると [`DomError::SerializedOutputTooLarge`]（`JS-6`）。
    pub fn serialize_html(&self) -> Result<SerializeResult> {
        self.serialize_node(self.root, SerializeScope::ChildrenOnly)
    }

    /// `node` を `scope` に従って HTML 文字列へシリアライズする（TASK-107・`JS-4`/`JS-6`）。
    ///
    /// - 範囲外の `node`: [`DomError::NodeNotFound`]
    /// - 出力が上限超過: [`DomError::SerializedOutputTooLarge`]（部分出力は返さない）
    /// - 壊れた arena（循環）: [`DomError::HierarchyCycle`]
    ///
    /// `REPAIR-9` の計装対象で、recorder が有効なら 1 回につき 1 件（操作種別 `Dom`）を記録する。
    pub fn serialize_node(&self, node: NodeId, scope: SerializeScope) -> Result<SerializeResult> {
        if !self.recorder.is_enabled() {
            return self.serialize_inner(node, scope, None);
        }
        let start = Instant::now();
        let result = self.serialize_inner(node, scope, None);
        let outcome = match &result {
            Ok(_) => OperationOutcome::Success,
            Err(Error::Dom(DomError::NodeNotFound { .. })) => OperationOutcome::Failure {
                kind: FailureKind::InvalidInput,
            },
            Err(_) => OperationOutcome::Failure {
                kind: FailureKind::Dom,
            },
        };
        self.recorder
            .record_outcome(OperationKind::Dom, outcome, start.elapsed());
        result
    }

    /// `serialize_node` と同じだが、出力上限を `max_bytes` でさらに絞る（文書側上限との小さい方）。
    ///
    /// DOM ブリッジ（`JS-6`・TASK-108）が 1 応答の上限を出力バッファの確保前に効かせるために使う。
    /// 超過は [`DomError::SerializedOutputTooLarge`]（部分出力は返さない）。計装は行わない
    /// （ブリッジ 1 操作 = 公開 API 1 件の件数契約を `serialize_node` と揃えるため
    /// 呼び出し側が必要なら別途記録する）。
    pub(crate) fn serialize_node_limited(
        &self,
        node: NodeId,
        scope: SerializeScope,
        max_bytes: usize,
    ) -> Result<SerializeResult> {
        self.serialize_inner(node, scope, Some(max_bytes))
    }

    /// 子の列挙元（`<template>` は template contents の子）。
    fn serialize_children(&self, id: NodeId) -> &[NodeId] {
        let target = self.template_contents(id).unwrap_or(id);
        self.node(target).map_or(&[], |n| n.children.as_slice())
    }

    fn serialize_inner(
        &self,
        node: NodeId,
        scope: SerializeScope,
        extra_limit: Option<usize>,
    ) -> Result<SerializeResult> {
        if self.node(node).is_none() {
            return Err(DomError::NodeNotFound {
                index: node.index(),
            }
            .into());
        }
        let mut out = Out {
            buf: String::new(),
            limit: extra_limit.map_or(self.limits.max_serialized_bytes(), |m| {
                m.min(self.limits.max_serialized_bytes())
            }),
        };
        let mut stack: Vec<Frame> = Vec::new();
        match scope {
            SerializeScope::IncludeNode => stack.push(Frame::Open(node)),
            SerializeScope::ChildrenOnly => {
                stack.extend(
                    self.serialize_children(node)
                        .iter()
                        .rev()
                        .map(|&c| Frame::Open(c)),
                );
            }
        }
        // 正常な木ならノードごとに Open と Close の高々 2 フレーム。超過は arena 破損。
        let mut remaining = self.node_count().saturating_mul(2).saturating_add(2);
        while let Some(frame) = stack.pop() {
            remaining = match remaining.checked_sub(1) {
                Some(r) => r,
                None => {
                    return Err(DomError::HierarchyCycle {
                        parent: node.index(),
                        child: node.index(),
                    }
                    .into());
                }
            };
            match frame {
                Frame::Open(id) => self.serialize_open(id, &mut out, &mut stack)?,
                Frame::Close(id) => {
                    if let Some(name) = self.element_name(id) {
                        out.push("</")?;
                        push_element_name(&mut out, name)?;
                        out.push(">")?;
                    }
                }
            }
        }
        Ok(SerializeResult { html: out.buf })
    }

    fn serialize_open(
        &self,
        id: NodeId,
        out: &mut Out,
        stack: &mut Vec<Frame>,
    ) -> std::result::Result<(), DomError> {
        let Some(data) = self.node_data(id) else {
            return Ok(());
        };
        match data {
            NodeData::Document | NodeData::DocumentFragment => {
                stack.extend(
                    self.serialize_children(id)
                        .iter()
                        .rev()
                        .map(|&c| Frame::Open(c)),
                );
            }
            NodeData::Doctype { name, .. } => {
                out.push("<!DOCTYPE ")?;
                out.push(name)?;
                out.push(">")?;
            }
            NodeData::Element { name, attrs, .. } => {
                out.push("<")?;
                push_element_name(out, name)?;
                for attr in attrs {
                    out.push(" ")?;
                    push_attr_name(out, &attr.name)?;
                    out.push("=\"")?;
                    push_escaped(out, &attr.value, true)?;
                    out.push("\"")?;
                }
                out.push(">")?;
                if is_void(name) {
                    return Ok(());
                }
                let children = self.serialize_children(id);
                let is_html = &*name.ns == HTML_NAMESPACE_URI;
                if is_html
                    && matches!(&*name.local, "pre" | "textarea" | "listing")
                    && let Some(&first) = children.first()
                    && let Some(NodeData::Text { contents }) = self.node_data(first)
                    && contents.starts_with('\n')
                {
                    out.push("\n")?;
                }
                stack.push(Frame::Close(id));
                stack.extend(children.iter().rev().map(|&c| Frame::Open(c)));
            }
            NodeData::Text { contents } => {
                let raw = self
                    .parent(id)
                    .and_then(|p| self.element_name(p))
                    .is_some_and(|n| {
                        &*n.ns == HTML_NAMESPACE_URI
                            && (matches!(
                                &*n.local,
                                "style"
                                    | "script"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                                    | "plaintext"
                            ) || (self.scripting_enabled && &*n.local == "noscript"))
                    });
                if raw {
                    out.push(contents)?;
                } else {
                    push_escaped(out, contents, false)?;
                }
            }
            NodeData::Comment { contents } => {
                out.push("<!--")?;
                out.push(contents)?;
                out.push("-->")?;
            }
            NodeData::ProcessingInstruction { target, data } => {
                out.push("<?")?;
                out.push(target)?;
                out.push(" ")?;
                out.push(data)?;
                out.push(">")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::{DomLimits, Node, QuirksMode};
    use crate::observability::RecorderHandle;

    fn out_with(limit: usize) -> Out {
        Out {
            buf: String::new(),
            limit,
        }
    }

    /// JS-4: テキストは `& < > U+00A0`、属性値は加えて `"` をエスケープする。
    #[test]
    fn js_4_escape_values() {
        let mut o = out_with(100);
        push_escaped(&mut o, "a<b&c>\u{a0}\"", false).expect("上限内");
        assert_eq!(o.buf, "a&lt;b&amp;c&gt;&nbsp;\"");
        let mut o = out_with(100);
        push_escaped(&mut o, "say \"hi\" & <x>", true).expect("上限内");
        assert_eq!(o.buf, "say &quot;hi&quot; &amp; &lt;x&gt;");
    }

    /// JS-6: 上限ちょうどは成功、+1 バイトは Err。
    #[test]
    fn js_6_out_push_boundary() {
        let mut o = out_with(3);
        assert!(o.push("abc").is_ok());
        assert!(matches!(
            o.push("d"),
            Err(DomError::SerializedOutputTooLarge { limit: 3 })
        ));
    }

    /// JS-4: 範囲外 ID は `NodeNotFound`。
    #[test]
    fn js_4_out_of_range_node_is_not_found() {
        let doc = crate::parse_document("<p>x</p>", &crate::ParseOptions::default())
            .expect("成功する")
            .document;
        let bad = NodeId::new(doc.node_count() + 5);
        match doc.serialize_node(bad, SerializeScope::IncludeNode) {
            Err(Error::Dom(DomError::NodeNotFound { index })) => {
                assert_eq!(index, doc.node_count() + 5);
            }
            other => panic!("expected NodeNotFound, got {other:?}"),
        }
    }

    /// JS-6: 循環する壊れた arena でも `HierarchyCycle` の Err で停止する。
    #[test]
    fn js_6_corrupted_cyclic_arena_terminates() {
        let doc = Document {
            nodes: vec![
                Node {
                    parent: Some(NodeId::new(1)),
                    children: vec![NodeId::new(1)],
                    data: NodeData::Document,
                },
                Node {
                    parent: Some(NodeId::new(0)),
                    children: vec![NodeId::new(0)],
                    data: NodeData::DocumentFragment,
                },
            ],
            root: NodeId::new(0),
            quirks_mode: QuirksMode::NoQuirks,
            recorder: RecorderHandle::default(),
            limits: DomLimits::default(),
            retained_bytes: 0,
            scripting_enabled: false,
        };
        assert!(matches!(
            doc.serialize_html(),
            Err(Error::Dom(DomError::HierarchyCycle { .. }))
        ));
    }
}

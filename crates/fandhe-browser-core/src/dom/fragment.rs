//! dom::fragment: `innerHTML` 設定（フラグメントパースによる子の置換。TASK-107 残り・
//! TASK-108・Issue #778・ビヘイビア `JS-5` / `JS-6`・MS-6）。
//!
//! 呼び出し元は DOM ブリッジ（`dom_bridge` の `setInnerHTML`。ページ内 JS の
//! `element.innerHTML = ...` が shim 経由で届く）。JS 側にパーサーは持たず、HTML の
//! 解析は core の html5ever（`parse::parse_fragment`）が担う。
//!
//! # 設計上の要点
//!
//! - [`mutation`](super::mutation) と同じ 2 フェーズ。解析・上限検証（ノード数・
//!   テキスト長・属性・名前長・保持バイト数）をすべて終えてから木を変更し、`Err`
//!   では木を変更しない。
//! - 取り込みは明示スタックの反復で、深いネストでもスタックを溢れさせない。
//! - パーサーが出力した要素名・属性名には `validate_name` の文字種規則を再適用しない
//!   （`parse_document` の出力と同じ扱い。長さのみ検証する）。
//! - 挿入された `<script>` は実行しない（WHATWG の innerHTML と同じ。実行は
//!   ページ実行ランナーの責務）。
//! - 置換前の子は arena に残り、ノード数・保持バイト数の上限に数え続ける
//!   （[`Document::set_text_content`] と同じ）。

use super::mutation::{kind_err, not_found};
use super::{Attribute, Document, HTML_NAMESPACE_URI, Node, NodeData, NodeId, QualName};
use crate::error::{DomError, Error, NameKind, ParseError, Result};
use crate::parse::{ParseOptions, parse_fragment};
use html5ever::{LocalName, Namespace};

/// [`Document::set_inner_html`] の結果（REPAIR-4: 将来の拡張に備え構造体で返す）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InnerHtmlOutcome {
    /// 取り込んだノード数（`<template>` の contents 配下を含む）。
    pub inserted_nodes: usize,
    /// 取り外した直接の子の数。
    pub removed_children: usize,
}

/// フラグメントパースの文脈にする要素名を決める。
fn context_name(data: &NodeData) -> Option<QualName> {
    match data {
        NodeData::Element { name, .. } => Some(name.clone()),
        // DocumentFragment は `body` 文脈で解析する（WHATWG の fragment parsing と同様）。
        NodeData::DocumentFragment => Some(QualName::new(
            None,
            Namespace::from(HTML_NAMESPACE_URI),
            LocalName::from("body"),
        )),
        _ => None,
    }
}

/// 取り込み前に 1 ノード分の上限を検証し、保持バイト数を返す。
fn check_incoming(doc: &Document, node: &Node) -> Result<usize> {
    let limits = doc.limits;
    let name_check = |kind: NameKind, len: usize| -> Result<()> {
        if len > limits.max_name_bytes() {
            return Err(DomError::InvalidName {
                kind,
                len,
                reason: "name is too long",
            }
            .into());
        }
        Ok(())
    };
    match &node.data {
        NodeData::Element { name, attrs, .. } => {
            name_check(NameKind::Element, name.local.len())?;
            if attrs.len() > limits.max_attributes_per_element() {
                return Err(DomError::TooManyAttributes {
                    limit: limits.max_attributes_per_element(),
                }
                .into());
            }
            for a in attrs {
                name_check(NameKind::Attribute, a.name.local.len())?;
                if a.value.len() > limits.max_attribute_value_bytes() {
                    return Err(DomError::AttributeValueTooLarge {
                        len: a.value.len(),
                        limit: limits.max_attribute_value_bytes(),
                    }
                    .into());
                }
            }
        }
        NodeData::Text { contents } | NodeData::Comment { contents } => {
            doc.check_text_len(contents.len())?;
        }
        NodeData::ProcessingInstruction { data, .. } => doc.check_text_len(data.len())?,
        _ => {}
    }
    Ok(super::retained_bytes_of(std::slice::from_ref(node)))
}

impl Document {
    /// `node` の子を、`html` を `node` を文脈にフラグメントパースした結果で置き換える
    /// （`element.innerHTML = html` 相当。`JS-5`・TASK-108）。
    ///
    /// - 対象は Element または DocumentFragment。それ以外は [`DomError::InvalidNodeKind`]
    /// - 入力が [`DomLimits::max_text_bytes`](super::DomLimits::max_text_bytes) を超えれば
    ///   [`DomError::TextTooLarge`]
    /// - 取り込み後のノード数が上限を超えれば [`DomError::NodeLimitExceeded`]、保持
    ///   バイト数が超えれば [`DomError::TotalBytesExceeded`]。属性・名前の上限違反も `Err`
    /// - いずれの `Err` でも木は変更されない
    /// - `<template>` では子の置換先は template contents（`JS-5`）
    /// - 空文字列は子を全て外すだけ（ノードは作らない）
    ///
    /// 挿入した `<script>` の「実行済み」フラグ等は持たない（REPAIR-3）。実行制御は
    /// ページ実行ランナー（TASK-109）側の責務。
    pub fn set_inner_html(&mut self, node: NodeId, html: &str) -> Result<InnerHtmlOutcome> {
        let ctx_node = self.require_node(node)?;
        let Some(context) = context_name(&ctx_node.data) else {
            return Err(kind_err(
                "set_inner_html",
                "node kind cannot have inner HTML",
            ));
        };
        // 文脈要素の属性（annotation-xml の encoding 等）はパーサーへ引き継ぐ。
        let context_attrs: Vec<Attribute> = match &ctx_node.data {
            NodeData::Element { attrs, .. } => attrs.clone(),
            _ => Vec::new(),
        };
        // パース用補助ノード: Document・文脈要素・`<html>`。template 文脈では
        // ArenaSink が文脈用 DocumentFragment も作るため 1 個多い。
        // 判定は template_contents の有無ではなく文脈名（HTML 名前空間の `template`）で行う
        // （`create_element("template")` は template_contents を持たないが、パーサーは
        // 文脈名から template 用 Fragment を作るため）。
        let aux_nodes: usize =
            if &*context.ns == HTML_NAMESPACE_URI && &*context.local == "template" {
                4
            } else {
                3
            };
        let quirks_mode = self.quirks_mode;
        self.check_text_len(html.len())?;
        // `<template>` の子は template_contents（DocumentFragment）に保持されるため、
        // パース文脈は要素のまま、取り外し・接続先だけをその Fragment に切り替える。
        let target = self.template_contents(node).unwrap_or(node);

        if html.is_empty() {
            let removed = self.detach_all_children(target)?;
            return Ok(InnerHtmlOutcome {
                inserted_nodes: 0,
                removed_children: removed,
            });
        }

        // パース自体が残り容量を超えられないよう上限を絞る。aux_nodes は Document・文脈要素・
        // フラグメントの親 `<html>`（template では文脈用 Fragment を加えて 4）の分。
        let remaining = self.limits.max_nodes().saturating_sub(self.nodes.len());
        if remaining == 0 {
            return Err(DomError::NodeLimitExceeded {
                limit: self.limits.max_nodes(),
            }
            .into());
        }
        let options = ParseOptions::default()
            .with_max_nodes(remaining.saturating_add(aux_nodes))
            .with_scripting_enabled(self.scripting_enabled);
        let parsed = match parse_fragment(html, &context, &context_attrs, quirks_mode, &options) {
            Ok(p) => p,
            Err(Error::Parse(ParseError::NodeLimitExceeded { .. })) => {
                return Err(DomError::NodeLimitExceeded {
                    limit: self.limits.max_nodes(),
                }
                .into());
            }
            Err(e) => return Err(e),
        };
        let frag = parsed.document;

        // フラグメントの親 `<html>` を探し、その子を取り込み元にする。
        let frag_html = frag
            .children(frag.root())
            .find(|&c| frag.is_element(c))
            .ok_or_else(|| kind_err("set_inner_html", "fragment parse produced no root"))?;

        // 検証フェーズ: 取り込み順（先行順）を作りつつ上限を検証する。
        let mut order: Vec<NodeId> = Vec::new();
        let mut incoming_bytes: usize = 0;
        let mut stack: Vec<NodeId> = frag.children(frag_html).collect();
        stack.reverse();
        while let Some(src) = stack.pop() {
            let src_node = frag.node(src).ok_or_else(|| not_found(src))?;
            incoming_bytes = incoming_bytes.saturating_add(check_incoming(self, src_node)?);
            order.push(src);
            if let NodeData::Element {
                template_contents: Some(tc),
                ..
            } = &src_node.data
            {
                stack.push(*tc);
            }
            stack.extend(src_node.children.iter().rev().copied());
        }
        match self.nodes.len().checked_add(order.len()) {
            Some(total) if total <= self.limits.max_nodes() => {}
            _ => {
                return Err(DomError::NodeLimitExceeded {
                    limit: self.limits.max_nodes(),
                }
                .into());
            }
        }
        self.check_total_budget(incoming_bytes)?;

        // 変更フェーズ。
        let removed = self.detach_all_children(target)?;
        let base = self.nodes.len();
        self.nodes.reserve(order.len());
        let mut map: Vec<Option<usize>> = vec![None; frag.nodes.len()];
        for (i, &src) in order.iter().enumerate() {
            let src_node = frag.node(src).ok_or_else(|| not_found(src))?;
            let dest = NodeId::new(base + i);
            if let Some(slot) = map.get_mut(src.index()) {
                *slot = Some(dest.index());
            }
            let parent = match src_node.parent {
                Some(p) if p == frag_html => Some(target),
                Some(p) => map.get(p.index()).copied().flatten().map(NodeId::new),
                None => None,
            };
            self.nodes.push(Node {
                parent,
                children: Vec::new(),
                data: src_node.data.clone(),
            });
            if let Some(p) = parent {
                self.node_mut(p)?.children.push(dest);
            }
        }
        // template contents の参照を新しい arena の ID へ付け替える。
        for i in 0..order.len() {
            if let Some(Node {
                data:
                    NodeData::Element {
                        template_contents, ..
                    },
                ..
            }) = self.nodes.get_mut(base + i)
                && let Some(old) = *template_contents
            {
                *template_contents = map.get(old.index()).copied().flatten().map(NodeId::new);
            }
        }
        self.adjust_retained(incoming_bytes, 0);
        Ok(InnerHtmlOutcome {
            inserted_nodes: order.len(),
            removed_children: removed,
        })
    }

    /// `node` の子をすべて外す（arena のノードは残す）。外した個数を返す。
    fn detach_all_children(&mut self, node: NodeId) -> Result<usize> {
        let old_children = std::mem::take(&mut self.node_mut(node)?.children);
        let count = old_children.len();
        for c in old_children {
            if let Some(n) = self.nodes.get_mut(c.index()) {
                n.parent = None;
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests;

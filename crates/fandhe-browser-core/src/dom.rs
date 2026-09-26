//! dom: `parse` が構築した木構造の DOM 表現（arena 方式）を担うモジュール。
//!
//! `parse` モジュール（TASK-24.4・#38）が構築する arena の型定義に加え、
//! 走査 API（親子・兄弟・祖先・子孫を辿るイテレータ）・要素/属性アクセサ・
//! `text_content` を提供する（TASK-24.5・#39・ビヘイビア `CORE-1`）。
//! `query` モジュール（TASK-24.10・#418）や上位 crate（`fandhe-browser-ai` の
//! 簡約 DOM 生成・`fandhe-browser-cdp` 等）は本モジュールが公開する走査 API・
//! アクセサ経由で arena を参照する想定で、html5ever を直接依存に持つ必要が
//! ないよう [`QualName`] をここで再エクスポートする。
//!
//! セレクタ照合（`query_selector(_all)` 相当）は `query` モジュール
//! （TASK-24.10・#418）が本モジュールの走査 API を使って実装する。PoC 由来の
//! 高レベル helper（`get_text(selector)` 等）は本モジュール・`query` いずれの
//! スコープにも含めない。
//!
//! # 設計上の要点
//!
//! - ノードは `Vec<Node>`（arena）に平坦に格納し、親子関係は [`NodeId`]
//!   （`Vec` へのインデックス）で表現する。参照カウント（`Rc`）や内部可変性
//!   （`RefCell`）を持たないため [`Document`] は `Send + Sync` になり、
//!   マルチスレッド環境（将来の並列スクレイピング）へ持ち出せる
//!   （本モジュール本体の `const` 静的アサーションと
//!   `core_1_dom_types_are_send_sync` テストでコンパイル時に検証する）。
//! - `Vec` が平坦に確保されるため、`Drop` 時の解放は再帰しない
//!   （数万段のネストでもスタックオーバーフローしない。security.md
//!   「無制限リソース確保」対策の一部）。
//! - 属性値・テキストは html5ever の `StrTendril`（`Send` でない）ではなく
//!   `String` に変換して保持する（Issue #35 決定・PoC-5 で判明した
//!   `scraper::Html` の `!Send` 問題の再発防止）。
//! - 走査系のイテレータ（[`Ancestors`]・[`Descendants`]）は再帰せず明示スタック
//!   ／歩数カウントで実装し、`parent`/`children` リンクが（万一）壊れていても
//!   `node_count` を超えて走査を続けない（security.md「不安全な設計」対策。
//!   壊れた arena を手組みして確認するユニットテストを本モジュールに置く）。

/// arena（[`Document::nodes`]）内のノードを指すインデックス。
///
/// 添字アクセス（`nodes[id]`）ではなく [`Document::node`] 等の `get` 系
/// メソッド経由でのみ参照する（coding-rust.md「外部入力の経路では添字アクセスを
/// 使わない」。HTML は外部入力であり、`parse` モジュールが構築するノード数は
/// `ParseOptions::max_nodes` で上限管理されるが、念のため境界チェックを徹底する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) usize);

impl NodeId {
    /// arena のインデックス `index` を指す `NodeId` を構築する。
    ///
    /// 構築は crate 内（`parse` の `ArenaSink`・本モジュールのテスト）に
    /// 限定する。CDP の `nodeId`（数値マッピング）等、crate 外向けの
    /// 公開手段は cdp 実装時（TASK-24 系）に別途検討する。
    pub(crate) const fn new(index: usize) -> Self {
        NodeId(index)
    }

    /// arena 内のインデックスを返す。
    pub(crate) const fn index(self) -> usize {
        self.0
    }
}

/// HTML 要素の属性 1 つ分。
///
/// html5ever の `Attribute`（値が `StrTendril`）とは異なり、値を `String` に
/// 変換済みで保持する（本モジュールの設計上の要点を参照）。
#[derive(Debug, Clone)]
pub struct Attribute {
    /// 属性名（名前空間込み）。
    pub name: html5ever::QualName,
    /// 属性値。
    pub value: String,
}

/// ノード 1 つが持つ種別ごとのデータ。
///
/// `parse`（#38）の `ArenaSink`（`TreeSink` 実装）が html5ever のトークン列から
/// 構築する。`#[non_exhaustive]` とすることで、後続タスクでのバリアント追加を
/// 非破壊にする（REPAIR-4・coding-rust.md「公開 API」）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NodeData {
    /// ドキュメントのルートノード（[`Document::root`] が指す）。
    Document,
    /// `<!DOCTYPE ...>` 宣言。
    Doctype {
        /// DOCTYPE 名（通常は `"html"`）。
        name: String,
        /// 公開識別子（多くの場合空文字列）。
        public_id: String,
        /// システム識別子（多くの場合空文字列）。
        system_id: String,
    },
    /// 要素ノード（`<div>` 等）。
    Element {
        /// 要素名（名前空間込み）。[`Document::element_name`]・
        /// [`Document::local_name`] 経由でアクセスする。
        name: html5ever::QualName,
        /// 属性一覧。html5ever の契約（`add_attrs_if_missing` は
        /// 既存属性名を優先する）に従い、重複する属性名は最初の 1 つのみを残す。
        attrs: Vec<Attribute>,
        /// `<template>` 要素の template contents（別ドキュメントフラグメントの
        /// ノード）。`<template>` 以外では常に `None`。
        template_contents: Option<NodeId>,
        /// MathML の `annotation-xml` 要素が HTML integration point かどうか
        /// （html5ever の `TreeSink::is_mathml_annotation_xml_integration_point`
        /// が参照する）。
        mathml_annotation_xml_integration_point: bool,
    },
    /// テキストノード。html5ever の契約により、隣接するテキストは 1 ノードへ
    /// 連結済み（`parse` の `ArenaSink::append` 等が担う）。
    Text {
        /// テキスト内容。
        contents: String,
    },
    /// コメントノード（`<!-- ... -->`）。
    Comment {
        /// コメント内容。
        contents: String,
    },
    /// 処理命令ノード（XML 由来。HTML では稀）。
    ProcessingInstruction {
        /// ターゲット名。
        target: String,
        /// データ本体。
        data: String,
    },
    /// `<template>` の template contents 用に確保される、どの親も持たない
    /// ドキュメントフラグメントのルート。
    DocumentFragment,
}

/// arena 内のノード 1 つ。親子関係は [`NodeId`] で表現する。
#[derive(Debug, Clone)]
pub struct Node {
    /// 親ノードの ID。ルートノード・detach 済みノードは `None`。
    pub parent: Option<NodeId>,
    /// 子ノードの ID（出現順）。
    pub children: Vec<NodeId>,
    /// ノード種別ごとのデータ。
    pub data: NodeData,
}

/// ドキュメントの quirks mode。
///
/// html5ever の `QuirksMode`（tendril 等の `!Send` な内部可変性を持たない
/// 素の enum）をそのまま再エクスポートする。独自の enum を再定義して
/// 変換コードを増やすより、html5ever（本 crate が唯一許可された HTML パーサー
/// 依存。Issue #35 決定）の型をそのまま公開 API に使う方が保守コストが低い。
pub type QuirksMode = html5ever::interface::QuirksMode;

/// 要素名・属性名の型。ai/cdp 等の上位 crate が html5ever を直接依存に
/// 持たずに済むよう、[`Attribute::name`]・`NodeData::Element::name` と同じ
/// 型をここで再エクスポートする。
pub use html5ever::QualName;

/// HTML 名前空間の URI（`html5ever::ns!(html)` が展開する定数と同じ値）。
///
/// [`Document::attribute`]・[`Document::local_name`] の大文字小文字照合規則
/// （HTML 要素のみ ASCII 大文字小文字を無視する）の判定に使う。`query`
/// モジュール（TASK-24.10・#418）が要素の名前空間判定に同じ値を再利用できる
/// よう `pub(crate)` にしてある（単一の真実源を保つ。crate 外には公開しない）。
pub(crate) const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// `parse::parse_document`（#38）が構築する DOM ドキュメント全体。
///
/// `Rc`・`RefCell`・`StrTendril` を含まないため `Send + Sync` になる
/// （本モジュール本体の静的アサーションで検証する）。
#[derive(Debug)]
pub struct Document {
    /// 全ノードを格納する arena。インデックス 0 は常に [`NodeData::Document`]
    /// （[`Document::root`] と一致）。
    pub(crate) nodes: Vec<Node>,
    /// ルートノードの ID（常に `NodeId(0)`）。
    pub(crate) root: NodeId,
    /// パース時に決定した quirks mode。
    pub(crate) quirks_mode: QuirksMode,
}

impl Document {
    /// ルートノード（[`NodeData::Document`]）の ID を返す。
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// quirks mode を返す。
    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks_mode
    }

    /// 総ノード数（`ParseOptions::max_nodes` の検証・[`Ancestors`]/
    /// [`Descendants`] の打ち切り上限に使う）。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// `id` が指すノードへの参照を返す。範囲外なら `None`
    /// （添字アクセスではなく `get` 経由。coding-rust.md）。
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0)
    }

    /// `id` が指すノードのデータへの参照を返す。範囲外なら `None`。
    pub fn node_data(&self, id: NodeId) -> Option<&NodeData> {
        self.node(id).map(|node| &node.data)
    }

    /// `id` が指すノードの親 ID を返す。ルートノード・範囲外なら `None`。
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).and_then(|node| node.parent)
    }

    /// `id` が指すノードの子を出現順に辿るイテレータを返す。
    ///
    /// `id` が範囲外なら空イテレータを返す（panic しない）。
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            children: self.node(id).map(|node| node.children.as_slice()),
            front: 0,
            back_exclusive: self.node(id).map_or(0, |node| node.children.len()),
        }
    }

    /// `id` が指すノードの最初の子を返す。子がない・範囲外なら `None`。
    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.node(id)
            .and_then(|node| node.children.first().copied())
    }

    /// `id` が指すノードの最後の子を返す。子がない・範囲外なら `None`。
    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.node(id).and_then(|node| node.children.last().copied())
    }

    /// `id` が指すノードの次の兄弟を返す。
    ///
    /// 親の `children` 内で `id` の位置を線形探索するため O(親の子数)
    /// （呼び出し側が広い兄弟集合を繰り返し辿る場合は [`Document::children`]
    /// の利用を検討する）。親がない・末子・範囲外なら `None`。
    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent_id = self.parent(id)?;
        let siblings = &self.node(parent_id)?.children;
        let position = siblings.iter().position(|&sibling| sibling == id)?;
        siblings.get(position + 1).copied()
    }

    /// `id` が指すノードの前の兄弟を返す。
    ///
    /// [`Document::next_sibling`] と同じく O(親の子数)。親がない・先頭子・
    /// 範囲外なら `None`。
    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        let parent_id = self.parent(id)?;
        let siblings = &self.node(parent_id)?.children;
        let position = siblings.iter().position(|&sibling| sibling == id)?;
        position
            .checked_sub(1)
            .and_then(|prev| siblings.get(prev).copied())
    }

    /// `id` 自身を含まず、親→ルート方向に祖先を辿るイテレータを返す。
    ///
    /// `parent` リンクが壊れて循環している場合でも、最大 [`Document::node_count`]
    /// 歩で打ち切る（security.md「不安全な設計」対策）。
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            document: self,
            next: self.parent(id),
            remaining_steps: self.node_count(),
        }
    }

    /// `id` 自身を含まず、文書順（前順）で子孫を辿るイテレータを返す。
    ///
    /// 再帰せず明示スタック（子を逆順に push）で走査するため、深いネストでも
    /// スタックオーバーフローしない。産出数を [`Document::node_count`] で
    /// 上限し、`children` リンクが壊れていても必ず停止する。
    /// `<template>` の template contents は子ではないため含まれない
    /// （[`Document::template_contents`] 参照）。
    pub fn descendants(&self, id: NodeId) -> Descendants<'_> {
        let mut stack = Vec::new();
        if let Some(node) = self.node(id) {
            stack.extend(node.children.iter().rev().copied());
        }
        Descendants {
            document: self,
            stack,
            remaining_steps: self.node_count(),
        }
    }

    /// `id` が `<template>` 要素なら、その template contents（別ドキュメント
    /// フラグメントのルート）を返す。`<template>` 以外・範囲外なら `None`。
    pub fn template_contents(&self, id: NodeId) -> Option<NodeId> {
        match self.node_data(id)? {
            NodeData::Element {
                template_contents, ..
            } => *template_contents,
            _ => None,
        }
    }

    /// `id` が要素ノードかどうかを返す。範囲外なら `false`。
    pub fn is_element(&self, id: NodeId) -> bool {
        matches!(self.node_data(id), Some(NodeData::Element { .. }))
    }

    /// `id` が要素ノードなら要素名（名前空間込み）を返す。
    /// 要素以外・範囲外なら `None`。
    pub fn element_name(&self, id: NodeId) -> Option<&QualName> {
        match self.node_data(id)? {
            NodeData::Element { name, .. } => Some(name),
            _ => None,
        }
    }

    /// `id` が要素ノードなら local name（名前空間・prefix を除いた表記）を
    /// 返す。元の大文字小文字表記のまま返す（SVG の `foreignObject` 等を
    /// 壊さない）。要素以外・範囲外なら `None`。
    pub fn local_name(&self, id: NodeId) -> Option<&str> {
        self.element_name(id).map(|name| &*name.local)
    }

    /// `id` が要素ノードなら名前空間 URI を返す。要素以外・範囲外なら `None`。
    pub fn namespace_url(&self, id: NodeId) -> Option<&str> {
        self.element_name(id).map(|name| &*name.ns)
    }

    /// `id` が要素ノードなら属性一覧を返す。要素以外・範囲外なら空スライス
    /// （`Option` にせず、呼び出し側が `for` でそのまま回せるようにする）。
    pub fn attributes(&self, id: NodeId) -> &[Attribute] {
        match self.node_data(id) {
            Some(NodeData::Element { attrs, .. }) => attrs,
            _ => &[],
        }
    }

    /// `id` が要素ノードなら、属性 `name` の値を返す。
    ///
    /// 名前空間を持たない属性（`name.ns` が空）の local name とのみ照合する。
    /// 要素が HTML 名前空間なら ASCII 大文字小文字を区別せず
    /// （`eq_ignore_ascii_case`）、それ以外（SVG・MathML）は完全一致とする
    /// （`selector.rs` の属性セレクタ照合契約と揃える）。要素以外・範囲外・
    /// 該当属性なしなら `None`。
    pub fn attribute(&self, id: NodeId, name: &str) -> Option<&str> {
        let is_html = self
            .element_name(id)
            .is_some_and(|n| &*n.ns == HTML_NAMESPACE_URI);
        self.attributes(id)
            .iter()
            .find(|attr| {
                if !attr.name.ns.is_empty() {
                    return false;
                }
                if is_html {
                    (*attr.name.local).eq_ignore_ascii_case(name)
                } else {
                    &*attr.name.local == name
                }
            })
            .map(|attr| attr.value.as_str())
    }

    /// `id` が要素ノードなら、`class` 属性を ASCII 空白で分割したクラス名を
    /// 返す。空文字列トークンは除外する。要素以外・範囲外・`class` 属性なしは
    /// 空イテレータ。
    pub fn class_names(&self, id: NodeId) -> impl Iterator<Item = &str> {
        self.attribute(id, "class")
            .into_iter()
            .flat_map(|value| value.split_ascii_whitespace())
    }

    /// DOM Standard の `textContent` に相当するテキストを返す。
    ///
    /// - Text / Comment / ProcessingInstruction: 自身のデータをそのまま返す
    /// - Element / DocumentFragment: 子孫の Text ノードを文書順に連結する
    ///   （[`Document::descendants`] を利用し再帰しない。template contents は
    ///   子孫に含まれないため、テキストにも含まれない）
    /// - Document / Doctype・範囲外 ID: `None`
    pub fn text_content(&self, id: NodeId) -> Option<String> {
        match self.node_data(id)? {
            NodeData::Text { contents } | NodeData::Comment { contents } => Some(contents.clone()),
            NodeData::ProcessingInstruction { data, .. } => Some(data.clone()),
            NodeData::Element { .. } | NodeData::DocumentFragment => {
                let mut text = String::new();
                for descendant in self.descendants(id) {
                    if let Some(NodeData::Text { contents }) = self.node_data(descendant) {
                        text.push_str(contents);
                    }
                }
                Some(text)
            }
            NodeData::Document | NodeData::Doctype { .. } => None,
        }
    }
}

/// [`Document::children`] が返すイテレータ。子ノードの ID を出現順に返す。
///
/// `id` が範囲外だった場合は空イテレータになる（`children` フィールドが
/// `None`）。
#[derive(Debug, Clone)]
#[must_use]
pub struct Children<'a> {
    children: Option<&'a [NodeId]>,
    front: usize,
    back_exclusive: usize,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let children = self.children?;
        if self.front >= self.back_exclusive {
            return None;
        }
        let id = *children.get(self.front)?;
        self.front += 1;
        Some(id)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for Children<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let children = self.children?;
        if self.front >= self.back_exclusive {
            return None;
        }
        self.back_exclusive -= 1;
        children.get(self.back_exclusive).copied()
    }
}

impl ExactSizeIterator for Children<'_> {
    fn len(&self) -> usize {
        self.back_exclusive.saturating_sub(self.front)
    }
}

/// [`Document::ancestors`] が返すイテレータ。自身を含まず、親→ルート方向に
/// 祖先の ID を返す。
///
/// `remaining_steps` は構築時の [`Document::node_count`] から始まり、
/// `parent` リンクが循環していても打ち切れるようにする（コードのコメントは
/// 本モジュール冒頭「設計上の要点」参照）。
#[derive(Debug, Clone)]
#[must_use]
pub struct Ancestors<'a> {
    document: &'a Document,
    next: Option<NodeId>,
    remaining_steps: usize,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_steps == 0 {
            return None;
        }
        let current = self.next?;
        self.remaining_steps -= 1;
        self.next = self.document.parent(current);
        Some(current)
    }
}

/// [`Document::descendants`] が返すイテレータ。自身を含まず、文書順（前順）で
/// 子孫の ID を返す。
///
/// 明示スタック（フィールド `stack`）で走査するため再帰しない。
/// `remaining_steps` は構築時の [`Document::node_count`] から始まり、
/// `children` リンクが壊れていても打ち切れるようにする。
#[derive(Debug, Clone)]
#[must_use]
pub struct Descendants<'a> {
    document: &'a Document,
    stack: Vec<NodeId>,
    remaining_steps: usize,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_steps == 0 {
            return None;
        }
        let current = self.stack.pop()?;
        self.remaining_steps -= 1;
        if let Some(node) = self.document.node(current) {
            self.stack.extend(node.children.iter().rev().copied());
        }
        Some(current)
    }
}

// `Document` および走査 API が扱う型がすべて `Send + Sync` であることを
// ビルド時に保証する（TASK-24.5・#39 の受入基準）。`Rc`/`RefCell`/`StrTendril`
// を持ち込むとここでコンパイルエラーになる。
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Document>();
    assert_send_sync::<Node>();
    assert_send_sync::<NodeData>();
    assert_send_sync::<Attribute>();
    assert_send_sync::<NodeId>();
    assert_send_sync::<QuirksMode>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_document;
    use crate::parse::{ParseOptions, ParsedDocument};

    fn parse(html: &str) -> ParsedDocument {
        parse_document(html, &ParseOptions::default()).expect("テスト入力は必ず成功する")
    }

    fn find_descendant_element(doc: &Document, root: NodeId, local_name: &str) -> NodeId {
        doc.descendants(root)
            .find(|&id| doc.local_name(id) == Some(local_name))
            .unwrap_or_else(|| panic!("要素 {local_name} が見つからない"))
    }

    /// CORE-1: `Document`・走査 API が扱う各型は `Send + Sync` を満たす。
    #[test]
    fn core_1_dom_types_are_send_sync() {
        fn assert_bounds<T: Send + Sync>() {}
        assert_bounds::<Document>();
        assert_bounds::<Node>();
        assert_bounds::<NodeData>();
        assert_bounds::<Attribute>();
        assert_bounds::<NodeId>();
    }

    /// CORE-1: `children`・`first_child`/`last_child`・`next_sibling`/
    /// `prev_sibling` が文書順を保ち、端では `None` を返す。
    #[test]
    fn core_1_children_and_siblings_in_source_order() {
        let parsed = parse("<ul><li>a</li><li>b</li><li>c</li></ul>");
        let doc = &parsed.document;
        let ul = find_descendant_element(doc, doc.root(), "ul");

        let children: Vec<NodeId> = doc.children(ul).collect();
        assert_eq!(children.len(), 3);
        assert_eq!(doc.first_child(ul), Some(children[0]));
        assert_eq!(doc.last_child(ul), Some(children[2]));

        assert_eq!(doc.prev_sibling(children[0]), None);
        assert_eq!(doc.next_sibling(children[0]), Some(children[1]));
        assert_eq!(doc.prev_sibling(children[1]), Some(children[0]));
        assert_eq!(doc.next_sibling(children[1]), Some(children[2]));
        assert_eq!(doc.next_sibling(children[2]), None);
        assert_eq!(doc.prev_sibling(children[2]), Some(children[1]));

        let texts: Vec<String> = children
            .iter()
            .map(|&id| doc.text_content(id).unwrap_or_default())
            .collect();
        assert_eq!(
            texts,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    /// CORE-1: `ancestors` は `li` → `ul` → `body` → `html` → ルートの順で
    /// 自身を含まずに祖先を返す。
    #[test]
    fn core_1_parent_and_ancestors_chain() {
        let parsed = parse("<ul><li>a</li></ul>");
        let doc = &parsed.document;
        let li = find_descendant_element(doc, doc.root(), "li");
        let ul = doc.parent(li).expect("li の親が存在する");
        assert_eq!(doc.local_name(ul), Some("ul"));

        let ancestor_names: Vec<&str> = doc
            .ancestors(li)
            .map(|id| doc.local_name(id).unwrap_or("#document"))
            .collect();
        assert_eq!(ancestor_names, vec!["ul", "body", "html", "#document"]);
        assert_eq!(doc.parent(doc.root()), None);
    }

    /// CORE-1: `descendants` は入れ子構造を文書順（前順）で辿る。
    #[test]
    fn core_1_descendants_in_document_order() {
        let parsed = parse("<div><p>a<b>b</b></p><span>c</span></div>");
        let doc = &parsed.document;
        let div = find_descendant_element(doc, doc.root(), "div");
        let names: Vec<&str> = doc
            .descendants(div)
            .filter_map(|id| doc.local_name(id))
            .collect();
        assert_eq!(names, vec!["p", "b", "span"]);
    }

    /// CORE-1: 属性照合は HTML 要素なら大文字小文字を無視し、SVG 要素では
    /// 完全一致のみを許す。
    #[test]
    fn core_1_attribute_lookup_case_rules() {
        let parsed = parse(r#"<a HREF="/x">link</a>"#);
        let doc = &parsed.document;
        let a = find_descendant_element(doc, doc.root(), "a");
        assert_eq!(doc.attribute(a, "href"), Some("/x"));
        assert_eq!(doc.attribute(a, "HREF"), Some("/x"));

        let parsed = parse(r#"<svg><rect viewBox="0 0 1 1"></rect></svg>"#);
        let doc = &parsed.document;
        let rect = find_descendant_element(doc, doc.root(), "rect");
        assert_eq!(doc.attribute(rect, "viewBox"), Some("0 0 1 1"));
        assert_eq!(doc.attribute(rect, "viewbox"), None);
    }

    /// CORE-1: `class_names` は ASCII 空白で分割し、空トークンを含めない。
    #[test]
    fn core_1_class_names_split_on_ascii_whitespace() {
        let parsed = parse(r#"<div class=" a  b	c "></div>"#);
        let doc = &parsed.document;
        let div = find_descendant_element(doc, doc.root(), "div");
        let classes: Vec<&str> = doc.class_names(div).collect();
        assert_eq!(classes, vec!["a", "b", "c"]);
    }

    /// CORE-1: `text_content` は子孫の Text ノードを文書順に連結し、
    /// Comment ノード自身のデータは自身の `text_content` として返す。
    /// Document ノードは `None`。
    #[test]
    fn core_1_text_content_concatenates_descendant_text() {
        let parsed = parse("<p>a<b>b</b><!--x-->c</p>");
        let doc = &parsed.document;
        let p = find_descendant_element(doc, doc.root(), "p");
        assert_eq!(doc.text_content(p).as_deref(), Some("abc"));

        let comment = doc
            .descendants(p)
            .find(|&id| matches!(doc.node_data(id), Some(NodeData::Comment { .. })))
            .expect("コメントノードが見つかる");
        assert_eq!(doc.text_content(comment).as_deref(), Some("x"));

        assert_eq!(doc.text_content(doc.root()), None);
    }

    /// CORE-1: `<template>` の template contents は `children`/`descendants`/
    /// `text_content` に含まれない。
    #[test]
    fn core_1_template_contents_excluded_from_children_and_text() {
        let parsed = parse("<template>inside</template>after");
        let doc = &parsed.document;
        let template = find_descendant_element(doc, doc.root(), "template");

        assert_eq!(doc.children(template).len(), 0);
        assert_eq!(doc.text_content(template).as_deref(), Some(""));

        let contents = doc
            .template_contents(template)
            .expect("template contents が存在する");
        assert!(
            doc.descendants(contents)
                .any(|id| doc.text_content(id).as_deref() == Some("inside"))
        );
    }

    /// CORE-1: arena 外の `NodeId` を渡しても各 API は `None`／空を返し
    /// panic しない。
    #[test]
    fn core_1_out_of_range_node_id_returns_none_or_empty() {
        let parsed = parse("<p>a</p>");
        let doc = &parsed.document;
        let out_of_range = NodeId::new(usize::MAX);

        assert!(doc.node(out_of_range).is_none());
        assert!(doc.node_data(out_of_range).is_none());
        assert_eq!(doc.parent(out_of_range), None);
        assert_eq!(doc.children(out_of_range).len(), 0);
        assert_eq!(doc.first_child(out_of_range), None);
        assert_eq!(doc.last_child(out_of_range), None);
        assert_eq!(doc.next_sibling(out_of_range), None);
        assert_eq!(doc.prev_sibling(out_of_range), None);
        assert_eq!(doc.ancestors(out_of_range).count(), 0);
        assert_eq!(doc.descendants(out_of_range).count(), 0);
        assert_eq!(doc.template_contents(out_of_range), None);
        assert!(!doc.is_element(out_of_range));
        assert_eq!(doc.element_name(out_of_range), None);
        assert_eq!(doc.local_name(out_of_range), None);
        assert_eq!(doc.namespace_url(out_of_range), None);
        assert_eq!(doc.attributes(out_of_range).len(), 0);
        assert_eq!(doc.attribute(out_of_range, "href"), None);
        assert_eq!(doc.class_names(out_of_range).count(), 0);
        assert_eq!(doc.text_content(out_of_range), None);
    }

    /// CORE-1: 数千段の入れ子でも `descendants`・`ancestors`・`text_content`
    /// がスタックオーバーフローせずに完走する（件数を具体値で確認する）。
    #[test]
    fn core_1_deeply_nested_traversal_does_not_overflow_stack() {
        const DEPTH: usize = 20_000;
        let mut input = String::with_capacity(DEPTH * 5 + 6);
        for _ in 0..DEPTH {
            input.push_str("<div>");
        }
        input.push('x');
        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let parsed = parse_document(&input, &options).expect("深いネストでも成功する");
        let doc = &parsed.document;
        let outer_div = find_descendant_element(doc, doc.root(), "div");

        let descendant_count = doc.descendants(outer_div).count();
        assert_eq!(descendant_count, DEPTH - 1 + 1); // 内側の div (DEPTH - 1) + テキストノード 1

        let innermost_text = doc
            .descendants(outer_div)
            .find(|&id| matches!(doc.node_data(id), Some(NodeData::Text { .. })))
            .expect("最深部のテキストノードが見つかる");
        let ancestor_count = doc.ancestors(innermost_text).count();
        // DEPTH 個の div + body + html + ルート。
        assert_eq!(ancestor_count, DEPTH + 3);

        assert_eq!(doc.text_content(outer_div).as_deref(), Some("x"));
    }

    /// CORE-1: `parent`/`children` が循環するよう手組みした arena でも、
    /// `ancestors`/`descendants` は `node_count` 以内で必ず停止する
    /// （security.md「不安全な設計」対策の検証）。
    #[test]
    fn core_1_corrupted_cyclic_arena_traversal_terminates() {
        // 0 と 1 が互いを親子として指す壊れた arena を手組みする
        // （`pub(crate)` フィールドを直接操作できるのは同一 crate のみ）。
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
        };

        let ancestor_count = doc.ancestors(NodeId::new(0)).count();
        assert!(ancestor_count <= doc.node_count());

        let descendant_count = doc.descendants(NodeId::new(0)).count();
        assert!(descendant_count <= doc.node_count());
    }
}

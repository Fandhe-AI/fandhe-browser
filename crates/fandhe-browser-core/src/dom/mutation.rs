//! dom::mutation: [`Document`] の変更 API とノード数・サイズ上限（TASK-107・Issue #772・
//! ビヘイビア `JS-5` / `JS-6`・MS-6）。
//!
//! ページ内 JS の DOM 操作（`JS-5`）を受けるため、`parse` が組み立てた arena を
//! 変更する。呼び出し元は後続の `__dom.op` ブリッジ（TASK-108）を想定し、そこで
//! 外部入力（JS 由来の引数）が [`NodeId`] や文字列としてここへ渡る。
//! ノードの作りすぎを防ぐため、ノード数・属性値サイズ・名前長の上限
//! （[`DomLimits`]。`JS-6`）を持ち、超過は panic ではなく `Err` で返す。
//!
//! # 設計上の要点
//!
//! - どのメソッドも「検証フェーズ」で前提をすべて確かめてから「変更フェーズ」に
//!   入る。`Err` を返す経路では arena に触れない（木は変更されない）。
//! - arena の参照は `get` / `get_mut` / `position` のみで行い、添字アクセスや
//!   `unwrap` / `expect` を使わない（coding-rust.md）。
//! - 戻り値は真偽値にせず `Result` と将来拡張しやすい値（ノード ID・置換前の値）で
//!   返す（REPAIR-4）。
//! - 文書全体の保持バイト数（テキスト・名前・属性値の合計）も
//!   [`DomLimits::max_total_bytes`] で制限する。arena から外したノードの内容も
//!   保持分に数え、コピー前に `checked_add` で増分を検証する。
//! - arena は縮まない。[`Document::remove_child`] で外したノードも
//!   [`DomLimits::max_nodes`] に数えるため、作成と削除の繰り返しで上限をすり抜け
//!   られない。
//! - 要素名・属性名の検証は WHATWG DOM より意図的に厳しい ASCII 限定の規則
//!   （`serialize` での HTML 出力時のマークアップ注入を防ぐ）。緩めるかは TASK-108
//!   の実測後に判断する。
//!
//! # 未実装（実装済みを装わない。REPAIR-3）
//!
//! - `innerHTML` 設定（フラグメントパース。TASK-107 の残り）。HTML シリアライズ自体は
//!   `serialize` サブモジュール（Issue #773）に実装済み。
//! - Document 直下に置ける要素を 1 個に限る制約、DocumentFragment の展開挿入、
//!   名前空間付きの要素作成、`create_element("template")` 時の template contents
//!   生成（`template_contents` は `None`）。

use html5ever::{LocalName, Namespace};

use super::{Attribute, Document, HTML_NAMESPACE_URI, Node, NodeData, NodeId, QualName};
use crate::error::{DomError, Error, NameKind, Result};

/// 変更 API の既定ノード数上限（`JS-6`）。パース側の既定（1,000,000）より小さい。
pub const DEFAULT_DOM_MAX_NODES: usize = 100_000;
/// 属性値 1 個あたりの既定上限（64 KiB。`JS-6`）。
pub const DEFAULT_MAX_ATTRIBUTE_VALUE_BYTES: usize = 64 * 1024;
/// 要素名・属性名の既定の最大バイト長。
pub const DEFAULT_MAX_NAME_BYTES: usize = 1_024;
/// テキストノード（コメント・処理命令の内容を含む）1 個あたりの既定上限（1 MiB。`JS-6`）。
pub const DEFAULT_MAX_TEXT_BYTES: usize = 1024 * 1024;
/// 要素 1 個あたりの既定の属性個数上限（`JS-6`）。
pub const DEFAULT_MAX_ATTRIBUTES_PER_ELEMENT: usize = 256;
/// HTML シリアライズ出力の既定上限（8 MiB。`JS-6`・TASK-107）。
pub const DEFAULT_MAX_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;
/// 文書全体で保持するテキスト・名前・属性値の既定の合計上限（64 MiB。`JS-6`）。
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 変更 API の上限設定（`JS-6`）。`ParseOptions` と同じビルダー形式。
///
/// [`Document::set_limits`] で文書に設定する。`parse` した文書は
/// [`DomLimits::default`] で初期化される。パースで既定の 100,000 ノードを超えた
/// 文書は、上限を引き上げるまで作成系がすべて [`DomError::NodeLimitExceeded`] になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DomLimits {
    max_nodes: usize,
    max_attribute_value_bytes: usize,
    max_name_bytes: usize,
    max_text_bytes: usize,
    max_attributes_per_element: usize,
    max_total_bytes: usize,
    max_serialized_bytes: usize,
}

impl Default for DomLimits {
    fn default() -> Self {
        DomLimits {
            max_nodes: DEFAULT_DOM_MAX_NODES,
            max_attribute_value_bytes: DEFAULT_MAX_ATTRIBUTE_VALUE_BYTES,
            max_name_bytes: DEFAULT_MAX_NAME_BYTES,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
            max_attributes_per_element: DEFAULT_MAX_ATTRIBUTES_PER_ELEMENT,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_serialized_bytes: DEFAULT_MAX_SERIALIZED_BYTES,
        }
    }
}

impl DomLimits {
    /// ノード数の上限を設定する（0 の場合、作成系は常に `Err`）。
    pub fn with_max_nodes(mut self, max_nodes: usize) -> Self {
        self.max_nodes = max_nodes;
        self
    }

    /// 属性値 1 個あたりの最大バイト長を設定する。
    pub fn with_max_attribute_value_bytes(mut self, bytes: usize) -> Self {
        self.max_attribute_value_bytes = bytes;
        self
    }

    /// 要素名・属性名の最大バイト長を設定する。
    pub fn with_max_name_bytes(mut self, bytes: usize) -> Self {
        self.max_name_bytes = bytes;
        self
    }

    /// テキスト内容 1 個あたりの最大バイト長を設定する。
    pub fn with_max_text_bytes(mut self, bytes: usize) -> Self {
        self.max_text_bytes = bytes;
        self
    }

    /// 要素 1 個あたりの属性個数の上限を設定する（既存属性の置換は上限到達後も可能）。
    pub fn with_max_attributes_per_element(mut self, count: usize) -> Self {
        self.max_attributes_per_element = count;
        self
    }

    /// 文書全体の保持バイト数（テキスト・名前・属性値の合計。パース済みの内容と
    /// 外したノードの内容を含む）の上限を設定する。
    pub fn with_max_total_bytes(mut self, bytes: usize) -> Self {
        self.max_total_bytes = bytes;
        self
    }

    /// HTML シリアライズ出力の最大バイト数を設定する（超過は `Err`。`JS-6`）。
    pub fn with_max_serialized_bytes(mut self, bytes: usize) -> Self {
        self.max_serialized_bytes = bytes;
        self
    }

    /// HTML シリアライズ出力の最大バイト数。
    pub fn max_serialized_bytes(&self) -> usize {
        self.max_serialized_bytes
    }

    /// 文書全体の保持バイト数の上限。
    pub fn max_total_bytes(&self) -> usize {
        self.max_total_bytes
    }

    /// テキスト内容 1 個あたりの最大バイト長。
    pub fn max_text_bytes(&self) -> usize {
        self.max_text_bytes
    }

    /// 要素 1 個あたりの属性個数の上限。
    pub fn max_attributes_per_element(&self) -> usize {
        self.max_attributes_per_element
    }

    /// ノード数の上限。
    pub fn max_nodes(&self) -> usize {
        self.max_nodes
    }

    /// 属性値 1 個あたりの最大バイト長。
    pub fn max_attribute_value_bytes(&self) -> usize {
        self.max_attribute_value_bytes
    }

    /// 要素名・属性名の最大バイト長。
    pub fn max_name_bytes(&self) -> usize {
        self.max_name_bytes
    }
}

fn is_name_rest(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-')
}

/// 名前の長さと文字種を検証する（規則はモジュール doc を参照）。
fn validate_name(
    kind: NameKind,
    name: &str,
    max_bytes: usize,
) -> std::result::Result<(), DomError> {
    let len = name.len();
    let invalid = |reason: &'static str| DomError::InvalidName { kind, len, reason };
    if len == 0 {
        return Err(invalid("name is empty"));
    }
    if len > max_bytes {
        return Err(invalid("name is too long"));
    }
    let mut bytes = name.bytes();
    let first_ok = match bytes.next() {
        Some(b) => match kind {
            NameKind::Element => b.is_ascii_alphabetic(),
            NameKind::Attribute => b.is_ascii_alphabetic() || matches!(b, b'_' | b':'),
        },
        None => false,
    };
    if !first_ok {
        return Err(invalid("invalid leading character"));
    }
    if !bytes.all(is_name_rest) {
        return Err(invalid("name contains a disallowed character"));
    }
    Ok(())
}

/// arena 全体が保持する可変長データの合計バイト数を数える（`parse` が初期値に使う）。
pub(crate) fn retained_bytes_of(nodes: &[Node]) -> usize {
    nodes.iter().fold(0usize, |acc, n| {
        let own = match &n.data {
            NodeData::Document | NodeData::DocumentFragment => 0,
            NodeData::Doctype {
                name,
                public_id,
                system_id,
            } => name.len() + public_id.len() + system_id.len(),
            NodeData::Element { name, attrs, .. } => {
                name.local.len()
                    + attrs
                        .iter()
                        .map(|a| a.name.local.len() + a.value.len())
                        .fold(0usize, usize::saturating_add)
            }
            NodeData::Text { contents } | NodeData::Comment { contents } => contents.len(),
            NodeData::ProcessingInstruction { target, data } => target.len() + data.len(),
        };
        acc.saturating_add(own)
    })
}

fn not_found(id: NodeId) -> Error {
    DomError::NodeNotFound { index: id.index() }.into()
}

fn kind_err(operation: &'static str, reason: &'static str) -> Error {
    DomError::InvalidNodeKind { operation, reason }.into()
}

impl Document {
    fn require_node(&self, id: NodeId) -> Result<&Node> {
        self.node(id).ok_or_else(|| not_found(id))
    }

    fn node_mut(&mut self, id: NodeId) -> Result<&mut Node> {
        self.nodes.get_mut(id.index()).ok_or_else(|| not_found(id))
    }

    /// ノード数上限を判定する。arena の長さで数えるため、外したノードも含む。
    fn check_node_capacity(&self) -> Result<()> {
        if self.nodes.len() >= self.limits.max_nodes {
            return Err(DomError::NodeLimitExceeded {
                limit: self.limits.max_nodes,
            }
            .into());
        }
        Ok(())
    }

    /// テキスト内容の長さを上限と照合する（コピー前に呼ぶ）。
    fn check_text_len(&self, len: usize) -> Result<()> {
        if len > self.limits.max_text_bytes {
            return Err(DomError::TextTooLarge {
                len,
                limit: self.limits.max_text_bytes,
            }
            .into());
        }
        Ok(())
    }

    /// 保持バイト数が `add` バイト増えても上限内かを判定する（コピー前に呼ぶ）。
    ///
    /// `add == 0`（保持量が増えない操作）は、`set_limits` で上限を下げた後や
    /// パース結果が上限超の場合でも許可する。縮小・回復の書き込みを拒否しないため。
    fn check_total_budget(&self, add: usize) -> Result<()> {
        if add == 0 {
            return Ok(());
        }
        let limit = self.limits.max_total_bytes;
        match self.retained_bytes.checked_add(add) {
            Some(total) if total <= limit => Ok(()),
            _ => Err(DomError::TotalBytesExceeded {
                retained: self.retained_bytes,
                requested: add,
                limit,
            }
            .into()),
        }
    }

    /// 検証済みの増減を保持バイト数へ反映する。
    fn adjust_retained(&mut self, add: usize, sub: usize) {
        self.retained_bytes = self.retained_bytes.saturating_add(add).saturating_sub(sub);
    }

    /// 未接続の新規ノードを arena に追加する。作成系はすべてここを通る。
    fn push_node(&mut self, data: NodeData) -> Result<NodeId> {
        self.check_node_capacity()?;
        let id = NodeId::new(self.nodes.len());
        self.nodes.push(Node {
            parent: None,
            children: Vec::new(),
            data,
        });
        Ok(id)
    }

    /// 未接続の要素を作成する（`JS-5`）。名前は ASCII 小文字化して HTML 名前空間で作る。
    ///
    /// 名前の規則違反は [`DomError::InvalidName`]、ノード数上限超過は
    /// [`DomError::NodeLimitExceeded`]。
    pub fn create_element(&mut self, local_name: &str) -> Result<NodeId> {
        validate_name(NameKind::Element, local_name, self.limits.max_name_bytes)?;
        self.check_total_budget(local_name.len())?;
        let lowered = local_name.to_ascii_lowercase();
        let name = QualName::new(
            None,
            Namespace::from(HTML_NAMESPACE_URI),
            LocalName::from(lowered.as_str()),
        );
        let id = self.push_node(NodeData::Element {
            name,
            attrs: Vec::new(),
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
        })?;
        self.adjust_retained(local_name.len(), 0);
        Ok(id)
    }

    /// 未接続のテキストノードを作成する（`JS-5` / `JS-6`）。
    ///
    /// 長さが [`DomLimits::max_text_bytes`] を超えれば [`DomError::TextTooLarge`]、
    /// ノード数上限超過は [`DomError::NodeLimitExceeded`]、文書全体の保持バイト数が
    /// 上限を超えれば [`DomError::TotalBytesExceeded`]。いずれもコピー前に判定する。
    pub fn create_text_node(&mut self, data: &str) -> Result<NodeId> {
        self.check_text_len(data.len())?;
        self.check_node_capacity()?;
        self.check_total_budget(data.len())?;
        let id = self.push_node(NodeData::Text {
            contents: data.to_string(),
        })?;
        self.adjust_retained(data.len(), 0);
        Ok(id)
    }

    /// `child` を `parent` の末尾に追加し、`child` を返す（`JS-5`）。
    ///
    /// 検証内容は [`Document::insert_before`] と同じ。
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<NodeId> {
        self.insert_before(parent, child, None)
    }

    /// `child` を `parent` の `reference` の直前（`None` なら末尾）に挿入し、`child` を返す
    /// （`JS-5`）。`child` に親がいれば移動として扱う。
    ///
    /// 次の場合は `Err` で木を変更しない。存在しない `NodeId`
    /// （[`DomError::NodeNotFound`]）、`parent` が要素・Document・DocumentFragment
    /// 以外、`child` が要素・テキスト・コメント・処理命令以外
    /// （[`DomError::InvalidNodeKind`]）、自分自身または祖先を子にする挿入
    /// （[`DomError::HierarchyCycle`]）、`reference` が `parent` の子でない
    /// （[`DomError::NotAChild`]）。Document 直下の要素 1 個制約は検証しない。
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        child: NodeId,
        reference: Option<NodeId>,
    ) -> Result<NodeId> {
        let parent_node = self.require_node(parent)?;
        let child_node = self.require_node(child)?;
        if let Some(r) = reference {
            self.require_node(r)?;
        }
        if !matches!(
            parent_node.data,
            NodeData::Element { .. } | NodeData::Document | NodeData::DocumentFragment
        ) {
            return Err(kind_err("insert_before", "parent cannot have children"));
        }
        if !matches!(
            child_node.data,
            NodeData::Element { .. }
                | NodeData::Text { .. }
                | NodeData::Comment { .. }
                | NodeData::ProcessingInstruction { .. }
        ) {
            return Err(kind_err("insert_before", "node kind cannot be inserted"));
        }
        if child == parent || self.ancestors(parent).any(|a| a == child) {
            return Err(DomError::HierarchyCycle {
                parent: parent.index(),
                child: child.index(),
            }
            .into());
        }
        if let Some(r) = reference {
            if self.parent(r) != Some(parent) {
                return Err(DomError::NotAChild {
                    parent: parent.index(),
                    child: r.index(),
                }
                .into());
            }
            if r == child {
                // 位置が変わらないため何もしない（DOM 仕様の読み替え結果と同じ）。
                return Ok(child);
            }
        }

        // 変更フェーズ。ここまでで存在・親子関係は検証済み。
        if let Some(old_parent) = child_node.parent
            && let Some(old) = self.nodes.get_mut(old_parent.index())
        {
            old.children.retain(|c| *c != child);
            // 確保容量が残ると、移動を繰り返すだけで保持メモリがノード数の二乗規模に
            // 膨らむため、外した分の容量は即座に解放する。
            old.children.shrink_to_fit();
        }
        let parent_mut = self.node_mut(parent)?;
        let pos = reference.and_then(|r| parent_mut.children.iter().position(|c| *c == r));
        match pos {
            Some(i) => parent_mut.children.insert(i, child),
            None => parent_mut.children.push(child),
        }
        self.node_mut(child)?.parent = Some(parent);
        Ok(child)
    }

    /// `child` を `parent` から外し、`child` を返す（`JS-5`）。
    ///
    /// ノード自体は arena に残る（ノード数上限には数えたまま）。`child` が `parent` の
    /// 子でなければ [`DomError::NotAChild`]。
    pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<NodeId> {
        let parent_node = self.require_node(parent)?;
        let child_node = self.require_node(child)?;
        if child_node.parent != Some(parent) || !parent_node.children.contains(&child) {
            return Err(DomError::NotAChild {
                parent: parent.index(),
                child: child.index(),
            }
            .into());
        }
        let parent_mut = self.node_mut(parent)?;
        parent_mut.children.retain(|c| *c != child);
        // 外した分の確保容量を解放する（移動時と同じ理由）。
        parent_mut.children.shrink_to_fit();
        self.node_mut(child)?.parent = None;
        Ok(child)
    }

    /// 要素 `element` の属性 `name` を `value` に設定し、置き換え前の値を返す
    /// （新規なら `None`。`JS-5` / `JS-6`）。
    ///
    /// 要素以外は [`DomError::InvalidNodeKind`]、名前の規則違反は
    /// [`DomError::InvalidName`]、値が上限超過なら [`DomError::AttributeValueTooLarge`]、新規属性で個数上限に
    /// 達していれば [`DomError::TooManyAttributes`]（既存属性の置換は可能）、文書全体の
    /// 保持バイト数が上限を超えれば [`DomError::TotalBytesExceeded`]。
    /// HTML 要素の属性名は ASCII 小文字化し、照合は [`Document::attribute`] と同じ。
    pub fn set_attribute(
        &mut self,
        element: NodeId,
        name: &str,
        value: &str,
    ) -> Result<Option<String>> {
        if !matches!(self.require_node(element)?.data, NodeData::Element { .. }) {
            return Err(kind_err("set_attribute", "node is not an element"));
        }
        validate_name(NameKind::Attribute, name, self.limits.max_name_bytes)?;
        if value.len() > self.limits.max_attribute_value_bytes {
            return Err(DomError::AttributeValueTooLarge {
                len: value.len(),
                limit: self.limits.max_attribute_value_bytes,
            }
            .into());
        }
        let is_html = self
            .element_name(element)
            .is_some_and(|n| &*n.ns == HTML_NAMESPACE_URI);
        let existing = self.attribute_index(element, name);
        let old_len = existing
            .and_then(|i| self.attributes(element).get(i))
            .map(|a| a.value.len());
        let added = match old_len {
            Some(old) => value.len().saturating_sub(old),
            None => name.len().saturating_add(value.len()),
        };
        self.check_total_budget(added)?;
        if existing.is_none() {
            let count = self.attributes(element).len();
            if count >= self.limits.max_attributes_per_element {
                return Err(DomError::TooManyAttributes {
                    limit: self.limits.max_attributes_per_element,
                }
                .into());
            }
        }
        let local = if is_html {
            name.to_ascii_lowercase()
        } else {
            name.to_string()
        };
        let result = match self.node_mut(element)?.data {
            NodeData::Element { ref mut attrs, .. } => {
                if let Some(attr) = existing.and_then(|i| attrs.get_mut(i)) {
                    Some(std::mem::replace(&mut attr.value, value.to_string()))
                } else {
                    attrs.push(Attribute {
                        name: QualName::new(
                            None,
                            Namespace::from(""),
                            LocalName::from(local.as_str()),
                        ),
                        value: value.to_string(),
                    });
                    None
                }
            }
            _ => return Err(kind_err("set_attribute", "node is not an element")),
        };
        match &result {
            Some(old) => self.adjust_retained(value.len(), old.len()),
            None => self.adjust_retained(name.len() + value.len(), 0),
        }
        Ok(result)
    }

    /// 要素 `element` の属性 `name` を削除し、削除した値を返す（`JS-5`）。
    ///
    /// もともと無ければ `Ok(None)`（DOM と同じ）。要素以外は
    /// [`DomError::InvalidNodeKind`]、名前の規則違反は [`DomError::InvalidName`]。
    pub fn remove_attribute(&mut self, element: NodeId, name: &str) -> Result<Option<String>> {
        if !matches!(self.require_node(element)?.data, NodeData::Element { .. }) {
            return Err(kind_err("remove_attribute", "node is not an element"));
        }
        validate_name(NameKind::Attribute, name, self.limits.max_name_bytes)?;
        let existing = self.attribute_index(element, name);
        let removed = match self.node_mut(element)?.data {
            NodeData::Element { ref mut attrs, .. } => match existing {
                Some(i) if i < attrs.len() => {
                    let a = attrs.remove(i);
                    // 確保容量は保持バイト数の予算に数えないため、削除のたびに解放する
                    // （追加と全削除の繰り返しで容量が蓄積するのを防ぐ。AGENTS.md リソース上限）。
                    attrs.shrink_to_fit();
                    Some(a)
                }
                _ => None,
            },
            _ => return Err(kind_err("remove_attribute", "node is not an element")),
        };
        Ok(removed.map(|a| {
            self.adjust_retained(0, a.name.local.len() + a.value.len());
            a.value
        }))
    }

    /// `node` のテキスト内容を `text` に置き換える（`JS-5`）。
    ///
    /// 要素・DocumentFragment は全ての子を外し、`text` が空でなければテキストノードを
    /// 1 個作って追加し、その ID を返す（空なら `None`）。テキスト・コメント・処理命令は
    /// 内容を書き換えて `None`。Document・Doctype は [`DomError::InvalidNodeKind`]
    /// （DOM 上は何もしない操作だが fail-closed を優先する。呼び出し側で無視扱いに変換できる）。
    /// ノード数上限に達していれば、子を外す前に [`DomError::NodeLimitExceeded`]。
    /// `text` が [`DomLimits::max_text_bytes`] を超えれば、何も変更せず
    /// [`DomError::TextTooLarge`]。文書全体の保持バイト数が上限を超える場合も
    /// 何も変更せず [`DomError::TotalBytesExceeded`]（外した子の内容は保持分に残る）。
    pub fn set_text_content(&mut self, node: NodeId, text: &str) -> Result<Option<NodeId>> {
        self.require_node(node)?;
        self.check_text_len(text.len())?;
        match &self.require_node(node)?.data {
            NodeData::Element { .. } | NodeData::DocumentFragment => {
                if !text.is_empty() {
                    self.check_node_capacity()?;
                    // 外した子の内容も arena に残って保持分に数え続けるため、増分は全量。
                    self.check_total_budget(text.len())?;
                }
                let old_children = std::mem::take(&mut self.node_mut(node)?.children);
                for c in old_children {
                    if let Some(n) = self.nodes.get_mut(c.index()) {
                        n.parent = None;
                    }
                }
                if text.is_empty() {
                    return Ok(None);
                }
                let id = self.push_node(NodeData::Text {
                    contents: text.to_string(),
                })?;
                self.adjust_retained(text.len(), 0);
                self.node_mut(node)?.children.push(id);
                self.node_mut(id)?.parent = Some(node);
                Ok(Some(id))
            }
            NodeData::Text { contents } | NodeData::Comment { contents } => {
                let old_len = contents.len();
                self.check_total_budget(text.len().saturating_sub(old_len))?;
                if let NodeData::Text { contents } | NodeData::Comment { contents } =
                    &mut self.node_mut(node)?.data
                {
                    *contents = text.to_string();
                }
                self.adjust_retained(text.len(), old_len);
                Ok(None)
            }
            NodeData::ProcessingInstruction { data, .. } => {
                let old_len = data.len();
                self.check_total_budget(text.len().saturating_sub(old_len))?;
                if let NodeData::ProcessingInstruction { data, .. } = &mut self.node_mut(node)?.data
                {
                    *data = text.to_string();
                }
                self.adjust_retained(text.len(), old_len);
                Ok(None)
            }
            _ => Err(kind_err(
                "set_text_content",
                "node kind has no text content",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{ParseOptions, parse_document};
    use crate::query::query_selector_str;

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    type Snap = Vec<(Option<NodeId>, Vec<NodeId>, Vec<(String, String)>)>;

    /// 木が変更されていないことの比較用に、親・子・属性の全体を取る。
    fn snapshot(doc: &Document) -> Snap {
        (0..doc.node_count())
            .map(|i| {
                let id = NodeId::new(i);
                (
                    doc.parent(id),
                    doc.children(id).collect(),
                    doc.attributes(id)
                        .iter()
                        .map(|a| (a.name.local.to_string(), a.value.clone()))
                        .collect(),
                )
            })
            .collect()
    }

    fn dom_err(r: Result<impl std::fmt::Debug>) -> DomError {
        match r {
            Err(Error::Dom(e)) => e,
            other => panic!("expected Error::Dom, got {other:?}"),
        }
    }

    fn body(doc: &Document) -> NodeId {
        query_selector_str(doc, doc.root(), "body")
            .expect("selector ok")
            .expect("body exists")
    }

    /// 本文に div を 3 個 (a, b, c) 並べた文書を返す。
    fn with_three(doc: &mut Document) -> (NodeId, [NodeId; 3]) {
        let b = body(doc);
        let mut ids = [b; 3];
        for slot in ids.iter_mut() {
            let e = doc.create_element("div").expect("create");
            doc.append_child(b, e).expect("append");
            *slot = e;
        }
        (b, ids)
    }

    /// JS-5 / JS-6 (TASK-107): `Error::Dom` の Display と source 連鎖。
    #[test]
    fn js_6_error_dom_display_and_source() {
        use std::error::Error as _;
        let err: Error = DomError::NodeLimitExceeded { limit: 100_000 }.into();
        assert_eq!(err.to_string(), "DOM error: node limit of 100000 exceeded");
        assert_eq!(
            err.source().map(ToString::to_string).as_deref(),
            Some("node limit of 100000 exceeded")
        );
    }

    /// JS-5 (TASK-107): append_child は末尾に追加し parent を設定する。
    #[test]
    fn js_5_append_child_appends_and_sets_parent() {
        let mut doc = parse("<body></body>");
        let b = body(&doc);
        let x = doc.create_element("p").expect("create");
        let y = doc.create_text_node("hi").expect("create");
        assert_eq!(doc.append_child(b, x).expect("append"), x);
        doc.append_child(b, y).expect("append");
        assert_eq!(doc.children(b).collect::<Vec<_>>(), vec![x, y]);
        assert_eq!(doc.parent(x), Some(b));
        assert_eq!(doc.parent(y), Some(b));
    }

    /// JS-5 (TASK-107): insert_before は reference の直前、None は末尾。
    #[test]
    fn js_5_insert_before_inserts_at_reference_and_none_appends() {
        let mut doc = parse("<body></body>");
        let (b, [a, bb, _]) = with_three(&mut doc);
        let n = doc.create_element("span").expect("create");
        doc.insert_before(b, n, Some(bb)).expect("insert");
        let kids: Vec<_> = doc.children(b).collect();
        assert_eq!(kids.get(1), Some(&n));
        assert_eq!(kids.first(), Some(&a));
        let m = doc.create_element("em").expect("create");
        doc.insert_before(b, m, None).expect("insert");
        assert_eq!(doc.last_child(b), Some(m));
    }

    /// JS-5 (TASK-107): 同じ親内の移動で位置がずれない。
    #[test]
    fn js_5_insert_before_moves_within_same_parent() {
        let mut doc = parse("<body></body>");
        let (b, [a, bb, c]) = with_three(&mut doc);
        doc.insert_before(b, c, Some(a)).expect("move");
        assert_eq!(doc.children(b).collect::<Vec<_>>(), vec![c, a, bb]);
        doc.insert_before(b, a, Some(bb)).expect("move");
        assert_eq!(doc.children(b).collect::<Vec<_>>(), vec![c, a, bb]);
        doc.insert_before(b, c, Some(bb)).expect("move");
        assert_eq!(doc.children(b).collect::<Vec<_>>(), vec![a, c, bb]);
        let before = snapshot(&doc);
        doc.insert_before(b, c, Some(c)).expect("noop");
        assert_eq!(snapshot(&doc), before);
    }

    /// JS-5 (TASK-107): 別の親からの移動で元の親の children から消える。
    #[test]
    fn js_5_append_child_moves_from_other_parent() {
        let mut doc = parse("<body><div id=a><p id=p></p></div><div id=b></div></body>");
        let a = query_selector_str(&doc, doc.root(), "#a").unwrap().unwrap();
        let b = query_selector_str(&doc, doc.root(), "#b").unwrap().unwrap();
        let p = query_selector_str(&doc, doc.root(), "#p").unwrap().unwrap();
        doc.append_child(b, p).expect("move");
        assert_eq!(doc.children(a).count(), 0);
        assert_eq!(doc.children(b).collect::<Vec<_>>(), vec![p]);
        assert_eq!(doc.parent(p), Some(b));
    }

    /// JS-5 (TASK-107): 自分自身・祖先の挿入は循環として拒否し木を変更しない。
    #[test]
    fn js_5_insert_rejects_self_and_ancestor_cycle_and_leaves_tree_unchanged() {
        let mut doc = parse("<body><div id=a><p id=p></p></div></body>");
        let a = query_selector_str(&doc, doc.root(), "#a").unwrap().unwrap();
        let p = query_selector_str(&doc, doc.root(), "#p").unwrap().unwrap();
        let before = snapshot(&doc);
        assert!(matches!(
            dom_err(doc.append_child(a, a)),
            DomError::HierarchyCycle { .. }
        ));
        assert!(matches!(
            dom_err(doc.append_child(p, a)),
            DomError::HierarchyCycle { .. }
        ));
        assert_eq!(snapshot(&doc), before);
    }

    /// JS-5 (TASK-107): 範囲外 NodeId は全メソッドで NodeNotFound、木は不変。
    #[test]
    fn js_5_out_of_range_node_id_returns_node_not_found_for_every_mutation() {
        let mut doc = parse("<body></body>");
        let b = body(&doc);
        let bad = NodeId::new(usize::MAX);
        let before = snapshot(&doc);
        let is_nf =
            |e: DomError| matches!(e, DomError::NodeNotFound { index } if index == usize::MAX);
        assert!(is_nf(dom_err(doc.append_child(bad, b))));
        assert!(is_nf(dom_err(doc.append_child(b, bad))));
        assert!(is_nf(dom_err(doc.insert_before(b, b, Some(bad)))));
        assert!(is_nf(dom_err(doc.remove_child(bad, b))));
        assert!(is_nf(dom_err(doc.remove_child(b, bad))));
        assert!(is_nf(dom_err(doc.set_attribute(bad, "a", "b"))));
        assert!(is_nf(dom_err(doc.remove_attribute(bad, "a"))));
        assert!(is_nf(dom_err(doc.set_text_content(bad, "x"))));
        assert_eq!(snapshot(&doc), before);
    }

    /// JS-5 (TASK-107): remove_child は子でないノードを拒否し、正常系では arena に残す。
    #[test]
    fn js_5_remove_child_rejects_non_child_and_leaves_tree_unchanged() {
        let mut doc = parse("<body></body>");
        let (b, [a, _, _]) = with_three(&mut doc);
        let stray = doc.create_element("i").expect("create");
        let before = snapshot(&doc);
        assert!(matches!(
            dom_err(doc.remove_child(b, stray)),
            DomError::NotAChild { .. }
        ));
        assert!(matches!(
            dom_err(doc.insert_before(b, stray, Some(stray))),
            DomError::NotAChild { .. }
        ));
        assert_eq!(snapshot(&doc), before);
        let count = doc.node_count();
        assert_eq!(doc.remove_child(b, a).expect("remove"), a);
        assert_eq!(doc.parent(a), None);
        assert_eq!(doc.children(b).count(), 2);
        assert_eq!(doc.node_count(), count);
    }

    /// JS-5 (TASK-107): 種別が不正な操作は InvalidNodeKind。
    #[test]
    fn js_5_invalid_node_kind() {
        let mut doc = parse("<body></body>");
        let b = body(&doc);
        let t = doc.create_text_node("t").expect("create");
        let e = doc.create_element("p").expect("create");
        assert!(matches!(
            dom_err(doc.append_child(t, e)),
            DomError::InvalidNodeKind { .. }
        ));
        let root = doc.root();
        assert!(matches!(
            dom_err(doc.append_child(b, root)),
            DomError::InvalidNodeKind { .. }
        ));
        assert!(matches!(
            dom_err(doc.set_attribute(t, "a", "b")),
            DomError::InvalidNodeKind { .. }
        ));
        assert!(matches!(
            dom_err(doc.set_text_content(root, "x")),
            DomError::InvalidNodeKind { .. }
        ));
    }

    /// JS-5 (TASK-107): set_attribute は大文字小文字を無視して置換し旧値を返す。
    #[test]
    fn js_5_set_attribute_replaces_case_insensitively_on_html_and_returns_previous() {
        let mut doc = parse("<body></body>");
        let e = doc.create_element("div").expect("create");
        assert_eq!(doc.set_attribute(e, "Data-X", "1").expect("set"), None);
        assert_eq!(doc.attribute(e, "data-x"), Some("1"));
        assert_eq!(
            doc.set_attribute(e, "DATA-x", "2").expect("set"),
            Some("1".to_string())
        );
        assert_eq!(doc.attributes(e).len(), 1);
        assert_eq!(doc.attribute(e, "data-x"), Some("2"));
        assert_eq!(
            doc.remove_attribute(e, "data-X").expect("remove"),
            Some("2".to_string())
        );
        assert_eq!(doc.remove_attribute(e, "data-x").expect("remove"), None);
        assert_eq!(doc.attributes(e).len(), 0);
    }

    /// JS-5 (TASK-107): create_element は小文字化し、追加後に query で取れる。
    #[test]
    fn js_5_create_element_lowercases_and_is_queryable() {
        let mut doc = parse("<body></body>");
        let b = body(&doc);
        let e = doc.create_element("My-Widget").expect("create");
        assert_eq!(doc.local_name(e), Some("my-widget"));
        doc.append_child(b, e).expect("append");
        assert_eq!(
            query_selector_str(&doc, doc.root(), "my-widget").unwrap(),
            Some(e)
        );
    }

    /// JS-5 (TASK-107): set_text_content は子を置き換える。
    #[test]
    fn js_5_set_text_content_replaces_children_and_updates_text_node() {
        let mut doc = parse("<body><div id=d><b>x</b>y</div></body>");
        let d = query_selector_str(&doc, doc.root(), "#d").unwrap().unwrap();
        let old = doc.first_child(d).expect("child");
        let t = doc.set_text_content(d, "hello").expect("set").expect("id");
        assert_eq!(doc.children(d).collect::<Vec<_>>(), vec![t]);
        assert_eq!(doc.parent(old), None);
        assert_eq!(doc.text_content(d).as_deref(), Some("hello"));
        assert_eq!(doc.set_text_content(t, "bye").expect("set"), None);
        assert_eq!(doc.text_content(d).as_deref(), Some("bye"));
        assert_eq!(doc.set_text_content(d, "").expect("set"), None);
        assert_eq!(doc.children(d).count(), 0);
    }

    /// JS-6 (TASK-107): テキスト長上限。超過は Err、ちょうどは成功、Err 時は木が不変。
    #[test]
    fn js_6_text_size_limit() {
        let mut doc = parse("<body><div id=d><b>x</b></div></body>");
        doc.set_limits(doc.limits().with_max_text_bytes(4));
        let d = query_selector_str(&doc, doc.root(), "#d").unwrap().unwrap();
        let before = snapshot(&doc);
        let n = doc.node_count();
        assert!(matches!(
            dom_err(doc.create_text_node("12345")),
            DomError::TextTooLarge { len: 5, limit: 4 }
        ));
        assert!(matches!(
            dom_err(doc.set_text_content(d, "12345")),
            DomError::TextTooLarge { len: 5, limit: 4 }
        ));
        assert_eq!(doc.node_count(), n);
        assert_eq!(snapshot(&doc), before);
        doc.create_text_node("1234").expect("ちょうど上限は成功");
        let t = doc.set_text_content(d, "1234").expect("set").expect("id");
        assert_eq!(doc.text_content(d).as_deref(), Some("1234"));
        assert!(matches!(
            dom_err(doc.set_text_content(t, "12345")),
            DomError::TextTooLarge { .. }
        ));
    }

    /// JS-6 (TASK-107): ノード数上限到達時の create_text_node はコピー前に Err。
    #[test]
    fn js_6_create_text_node_node_limit() {
        let mut doc = parse("<body></body>");
        doc.set_limits(doc.limits().with_max_nodes(doc.node_count()));
        assert!(matches!(
            dom_err(doc.create_text_node("x")),
            DomError::NodeLimitExceeded { .. }
        ));
    }

    /// JS-6 (TASK-107): 属性個数上限。新規追加は Err、既存の置換は成功。
    #[test]
    fn js_6_attribute_count_limit() {
        let mut doc = parse("<body></body>");
        doc.set_limits(doc.limits().with_max_attributes_per_element(2));
        let e = doc.create_element("div").expect("create");
        doc.set_attribute(e, "a", "1").expect("set");
        doc.set_attribute(e, "b", "2").expect("set");
        assert!(matches!(
            dom_err(doc.set_attribute(e, "c", "3")),
            DomError::TooManyAttributes { limit: 2 }
        ));
        assert_eq!(doc.attributes(e).len(), 2);
        assert_eq!(
            doc.set_attribute(e, "A", "9").expect("置換は可能"),
            Some("1".to_string())
        );
        assert_eq!(doc.attributes(e).len(), 2);
    }

    /// JS-5 / JS-6 (TASK-107): 名前の検証。
    #[test]
    fn js_5_name_validation() {
        let mut doc = parse("<body></body>");
        let e = doc.create_element("div").expect("create");
        let is_invalid = |r: Result<_>| matches!(dom_err(r), DomError::InvalidName { .. });
        assert!(is_invalid(doc.create_element("").map(|_| ())));
        assert!(is_invalid(doc.create_element("1div").map(|_| ())));
        assert!(is_invalid(doc.create_element("a b").map(|_| ())));
        assert!(is_invalid(doc.create_element("a<b").map(|_| ())));
        assert!(is_invalid(doc.create_element("é").map(|_| ())));
        assert!(is_invalid(doc.set_attribute(e, "a=b", "v").map(|_| ())));
        assert!(is_invalid(doc.set_attribute(e, "a\"", "v").map(|_| ())));
        assert!(is_invalid(doc.set_attribute(e, "1a", "v").map(|_| ())));
        assert!(is_invalid(doc.set_attribute(e, "a\0", "v").map(|_| ())));
        assert!(doc.set_attribute(e, "aria-label", "v").is_ok());
        assert!(doc.set_attribute(e, "xml:lang", "v").is_ok());
        let max = doc.limits().max_name_bytes();
        assert!(doc.create_element(&"a".repeat(max)).is_ok());
        assert!(is_invalid(
            doc.create_element(&"a".repeat(max + 1)).map(|_| ())
        ));
    }

    /// JS-6 (TASK-107): 属性値上限の境界 (65,536 は OK / 65,537 は Err で元の値が残る)。
    #[test]
    fn js_6_attribute_value_limit_boundary() {
        let mut doc = parse("<body></body>");
        let e = doc.create_element("div").expect("create");
        assert!(doc.set_attribute(e, "a", &"x".repeat(65_536)).is_ok());
        let err = dom_err(doc.set_attribute(e, "a", &"y".repeat(65_537)));
        assert!(matches!(
            err,
            DomError::AttributeValueTooLarge {
                len: 65_537,
                limit: 65_536
            }
        ));
        assert_eq!(doc.attribute(e, "a").map(str::len), Some(65_536));
        assert_eq!(doc.attribute(e, "a").map(|v| &v[..1]), Some("x"));
    }

    /// JS-6 (TASK-107): ノード数上限の境界 (99,999 → 100,000 は成功、100,001 は Err)。
    #[test]
    fn js_6_node_limit_boundary_at_default_100000() {
        let mut doc = parse("");
        assert_eq!(doc.limits().max_nodes(), 100_000);
        while doc.node_count() < 99_999 {
            doc.create_text_node("").expect("under the limit");
        }
        assert_eq!(doc.node_count(), 99_999);
        doc.create_text_node("").expect("reaches exactly 100000");
        assert_eq!(doc.node_count(), 100_000);
        assert!(matches!(
            dom_err(doc.create_text_node("")),
            DomError::NodeLimitExceeded { limit: 100_000 }
        ));
        assert!(matches!(
            dom_err(doc.create_element("div")),
            DomError::NodeLimitExceeded { limit: 100_000 }
        ));
        assert_eq!(doc.node_count(), 100_000);
    }

    /// JS-6 (TASK-107): 上限は set_limits で変更でき、set_text_content は Err 時に子を外さない。
    #[test]
    fn js_6_node_limit_configurable_via_set_limits() {
        let mut doc = parse("<body><div id=d>x</div></body>");
        let d = query_selector_str(&doc, doc.root(), "#d").unwrap().unwrap();
        let limit = doc.node_count();
        doc.set_limits(doc.limits().with_max_nodes(limit));
        let before = snapshot(&doc);
        assert!(matches!(
            dom_err(doc.create_element("p")),
            DomError::NodeLimitExceeded { .. }
        ));
        assert!(matches!(
            dom_err(doc.set_text_content(d, "new")),
            DomError::NodeLimitExceeded { .. }
        ));
        assert_eq!(snapshot(&doc), before);
        // 空文字は新規ノード不要なので上限でも成功する。
        assert_eq!(doc.set_text_content(d, "").expect("clear"), None);
        doc.set_limits(doc.limits().with_max_nodes(limit + 1));
        assert!(doc.create_element("p").is_ok());
    }
    /// JS-6 (TASK-107): 総量上限を超えた状態でも、保持量が増えない縮小・同量書き込みは成功する。
    #[test]
    fn js_6_shrinking_writes_allowed_when_over_total_limit() {
        let mut doc = parse("<body><div id=d>x</div></body>");
        let d = query_selector_str(&doc, doc.root(), "#d").unwrap().unwrap();
        let t = doc
            .create_text_node("abcdefghijklmnopqrst")
            .expect("20 bytes");
        doc.set_attribute(d, "title", "0123456789").expect("attr");
        // 現在の保持量より小さい上限へ下げる。
        let over = doc.retained_bytes - 10;
        doc.set_limits(doc.limits().with_max_total_bytes(over));
        // 増加は拒否される。
        assert!(matches!(
            dom_err(doc.set_text_content(t, "abcdefghijklmnopqrstu")),
            DomError::TotalBytesExceeded { .. }
        ));
        let before = doc.retained_bytes;
        // 縮小は成功し、保持量が 20 バイト減る。
        assert_eq!(doc.set_text_content(t, "").expect("shrink"), None);
        assert_eq!(doc.retained_bytes, before - 20);
        // 同じ長さの属性値の置換も成功する。
        assert_eq!(
            doc.set_attribute(d, "title", "9876543210")
                .expect("same len"),
            Some("0123456789".to_string())
        );
        assert_eq!(doc.retained_bytes, before - 20);
    }

    /// JS-6 (TASK-107): 子を外した親の children は確保容量を残さない（容量の二乗蓄積防止）。
    #[test]
    fn js_6_children_capacity_released_on_move_and_remove() {
        let mut doc = parse("<body></body>");
        let b = body(&doc);
        let a = doc.create_element("div").unwrap();
        let c = doc.create_element("div").unwrap();
        doc.append_child(b, a).unwrap();
        doc.append_child(b, c).unwrap();
        let mut kids = Vec::new();
        for _ in 0..64 {
            let k = doc.create_element("i").unwrap();
            doc.append_child(a, k).unwrap();
            kids.push(k);
        }
        for k in &kids {
            doc.append_child(c, *k).unwrap();
        }
        let a_node = doc.nodes.get(a.index()).unwrap();
        assert_eq!(a_node.children.len(), 0);
        assert_eq!(a_node.children.capacity(), 0);
        for k in &kids[..63] {
            doc.remove_child(c, *k).unwrap();
        }
        let c_node = doc.nodes.get(c.index()).unwrap();
        assert_eq!(c_node.children.len(), 1);
        assert_eq!(c_node.children.capacity(), 1);
    }
}

//! `snapshot::Node::state` フィールドの算出ロジック（`AISNAP-1`・`TASK-11.5`・
//! `MS-2`・Issue #74）。
//!
//! 呼び出し文脈: 現時点では呼び出し元がない。DOM から `Snapshot`/`Node` を
//! 構築する TASK-11.7（Issue #76）が、ツリー構築時に要素ごとへ
//! [`compute_state`] を呼ぶ想定である（実装済みを装わない。REPAIR-3）。
//!
//! # スコープ
//!
//! 本実装が算出するのは次の 2 つのみである。
//!
//! - `disabled`: HTML の無効化可能な要素（`button`・`input`・`select`・
//!   `textarea`・`optgroup`・`option`・`fieldset`）が `disabled` content
//!   attribute を持つかどうか
//! - `checked`: `input[type=checkbox]`・`input[type=radio]` の `checked`
//!   content attribute の有無
//!
//! 以下は本 Issue のスコープ外とし、将来のタスクへ引き継ぐ（実装済みを
//! 装わない。REPAIR-3）。
//!
//! - `aria-disabled`・`aria-checked`（`mixed` を含む）の反映
//! - 祖先の `<fieldset disabled>` からの無効化の継承（最初の `<legend>` は
//!   例外）・`<optgroup disabled>` 配下の `<option>` の無効化
//! - `selected`・`expanded`・`pressed`・`required`・`readonly` 等の他の状態
//! - JS による動的な状態（IDL の `checked`/`indeterminate`）。本実装は
//!   静的 DOM の content attribute だけを見る

use fandhe_browser_core::dom::{Document, NodeId};

/// HTML 名前空間の URI。`core::dom::HTML_NAMESPACE_URI` は `pub(crate)` で
/// crate 外から使えないため、本モジュール用にローカルへ定義する
/// （`core` の公開 API は変更しない）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// チェック状態を持つ要素（`input[type=checkbox]`・`input[type=radio]`）の
/// チェック有無（`AISNAP-1`・`TASK-11.5`）。
///
/// `bool` ではなく専用の enum にする理由: 「チェックできない要素」
/// （[`State::checked`] が `None`）と「未チェックのチェックボックス」
/// （`Some(CheckedState::Unchecked)`）を区別できないと、AI エージェントに
/// とって重要な情報が失われるため。`#[non_exhaustive]` により、将来
/// `aria-checked="mixed"` 用の `Mixed` 等を追加しても非破壊で済む（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckedState {
    /// `checked` content attribute を持つ。
    Checked,
    /// `checked` content attribute を持たない。
    Unchecked,
}

/// 要素の状態（`AISNAP-1`・`TASK-11.5`）。[`super::Node::state`] の型。
///
/// `#[non_exhaustive]` により、将来 `expanded`・`selected`・`pressed` 等の
/// フィールド追加が非破壊になる（REPAIR-4）。crate 外（将来の cli/cdp の
/// テスト等）から値を組み立てられるよう、フィールドごとにビルダーを用意する。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct State {
    /// 無効化されているかどうか。§ 本モジュール doc の判定規則を参照。
    pub disabled: bool,
    /// チェック状態。チェックできない要素（対象外の `input` を含む）では
    /// `None`。
    pub checked: Option<CheckedState>,
}

impl State {
    /// `disabled` を設定した `State` を返す（ビルダー）。
    #[must_use]
    pub fn with_disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// `checked` を設定した `State` を返す（ビルダー）。
    #[must_use]
    pub fn with_checked(mut self, checked: Option<CheckedState>) -> Self {
        self.checked = checked;
        self
    }
}

/// `id` が HTML 名前空間の要素で local name が `name`（ASCII 大文字小文字を
/// 区別しない）と一致するかどうかを返す。
fn is_html_element_named(doc: &Document, id: NodeId, name: &str) -> bool {
    doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && doc
            .local_name(id)
            .is_some_and(|local| local.eq_ignore_ascii_case(name))
}

/// `doc` の要素 `id` から、`disabled` content attribute の有無を算出する。
///
/// HTML 仕様上 `disabled` 属性を持てる要素（`button`・`input`・`select`・
/// `textarea`・`optgroup`・`option`・`fieldset`）に限り、属性の**値ではなく
/// 有無**で判定する（boolean attribute のため `disabled="false"` も無効を
/// 表す）。それ以外の要素（`<a disabled>`・`<div disabled>` 等）は、HTML
/// 仕様上無効化されず操作できるままなので、AI エージェントに誤って
/// 「無効」と伝えないよう常に `false` を返す。
fn compute_disabled(doc: &Document, id: NodeId) -> bool {
    const DISABLEABLE: [&str; 7] = [
        "button", "input", "select", "textarea", "optgroup", "option", "fieldset",
    ];
    DISABLEABLE
        .iter()
        .any(|name| is_html_element_named(doc, id, name))
        && doc.attribute(id, "disabled").is_some()
}

/// `doc` の要素 `id` から、チェック状態を算出する。
///
/// `input` 要素で、`type` 属性の値（前後の ASCII 空白を除き、ASCII の
/// 大文字小文字を区別しない）が `checkbox` または `radio` のときのみ
/// `Some` を返す。`type` を省略した場合（既定値は text）を含め、それ以外は
/// `None`（チェック状態を持たない要素）とする。
fn compute_checked(doc: &Document, id: NodeId) -> Option<CheckedState> {
    if !is_html_element_named(doc, id, "input") {
        return None;
    }
    let input_type = doc
        .attribute(id, "type")?
        .trim_matches(|c: char| c.is_ascii_whitespace());
    if !input_type.eq_ignore_ascii_case("checkbox") && !input_type.eq_ignore_ascii_case("radio") {
        return None;
    }
    Some(if doc.attribute(id, "checked").is_some() {
        CheckedState::Checked
    } else {
        CheckedState::Unchecked
    })
}

/// `doc` の要素 `id` から状態を算出する（`AISNAP-1`・`TASK-11.5`）。
///
/// 要素以外（テキストノード等）・範囲外の `id` では `State::default()` を
/// 返す（`Result` にはしない。`core::dom` のアクセサ群が採る「範囲外・
/// 対象外は `None`/既定値」の契約に合わせる）。
pub fn compute_state(doc: &Document, id: NodeId) -> State {
    if !doc.is_element(id) {
        return State::default();
    }
    State {
        disabled: compute_disabled(doc, id),
        checked: compute_checked(doc, id),
    }
}

#[cfg(test)]
mod tests {
    use super::{CheckedState, State, compute_state};
    use fandhe_browser_core::dom::{Document, NodeId};
    use fandhe_browser_core::parse::{ParseOptions, parse_document};
    use fandhe_browser_core::query::query_selector_str;

    /// テスト入力の HTML をパースし、CSS セレクタで対象要素を 1 つ特定する。
    fn parse_and_select(html: &str, selector: &str) -> (Document, NodeId) {
        let parsed =
            parse_document(html, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        let doc = parsed.document;
        let root = doc.root();
        let target = query_selector_str(&doc, root, selector)
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる");
        (doc, target)
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: `disabled` 属性を持つ `button` は
    /// `disabled: true`・`checked: None` になる。
    #[test]
    fn aisnap_1_button_disabled() {
        let (doc, id) = parse_and_select("<button disabled>送信</button>", "button");
        assert_eq!(
            compute_state(&doc, id),
            State {
                disabled: true,
                checked: None,
            }
        );
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: `disabled` は値に関係なく属性の
    /// 有無で判定する（boolean attribute）。
    #[test]
    fn aisnap_1_disabled_value_ignored() {
        let (doc, id) = parse_and_select(r#"<input disabled="false">"#, "input");
        assert!(compute_state(&doc, id).disabled);
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: チェック済みチェックボックスは
    /// `checked: Some(Checked)` になる。
    #[test]
    fn aisnap_1_checkbox_checked() {
        let (doc, id) = parse_and_select(r#"<input type="checkbox" checked>"#, "input");
        assert_eq!(
            compute_state(&doc, id),
            State {
                disabled: false,
                checked: Some(CheckedState::Checked),
            }
        );
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: 未チェックのチェックボックスは
    /// `checked: Some(Unchecked)`（`None` とは区別する）になる。
    #[test]
    fn aisnap_1_checkbox_unchecked() {
        let (doc, id) = parse_and_select(r#"<input type="checkbox">"#, "input");
        assert_eq!(
            compute_state(&doc, id).checked,
            Some(CheckedState::Unchecked)
        );
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: チェック済みラジオボタンは
    /// `checked: Some(Checked)` になる。
    #[test]
    fn aisnap_1_radio_checked() {
        let (doc, id) = parse_and_select(r#"<input type="radio" checked>"#, "input");
        assert_eq!(compute_state(&doc, id).checked, Some(CheckedState::Checked));
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: `type` の大文字小文字を区別せず、
    /// `disabled`・`checked` を同時に算出できる（複合ケース）。
    #[test]
    fn aisnap_1_type_case_insensitive_and_combined() {
        let (doc, id) = parse_and_select(r#"<input type="CheckBox" disabled checked>"#, "input");
        assert_eq!(
            compute_state(&doc, id),
            State {
                disabled: true,
                checked: Some(CheckedState::Checked),
            }
        );
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: `disabled` を持てない要素
    /// （`<a disabled>`）は `disabled` 属性があっても無視され既定値になる。
    #[test]
    fn aisnap_1_anchor_disabled_attribute_ignored() {
        let (doc, id) = parse_and_select(r##"<a href="#" disabled>link</a>"##, "a");
        assert_eq!(compute_state(&doc, id), State::default());
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: `disabled` を持てない要素
    /// （`<div disabled>`）も既定値になる。
    #[test]
    fn aisnap_1_div_disabled_attribute_ignored() {
        let (doc, id) = parse_and_select("<div disabled>text</div>", "div");
        assert_eq!(compute_state(&doc, id), State::default());
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: `type` を省略した `input`（既定値は
    /// text）に `checked` があってもチェック状態は持たない（`None`）。
    #[test]
    fn aisnap_1_input_without_type_has_no_checked_state() {
        let (doc, id) = parse_and_select("<input checked>", "input");
        assert_eq!(compute_state(&doc, id).checked, None);
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: 属性を持たない要素は既定値になる。
    #[test]
    fn aisnap_1_element_without_attributes_is_default() {
        let (doc, id) = parse_and_select("<p>text</p>", "p");
        assert_eq!(compute_state(&doc, id), State::default());
    }

    /// AISNAP-1（TASK-11.5・Issue #74）: テキストノードの `NodeId` は
    /// `State::default()` になる（要素以外は対象外）。
    #[test]
    fn aisnap_1_text_node_is_default() {
        let parsed = parse_document("<p>text</p>", &ParseOptions::default())
            .expect("テスト入力は必ず成功する");
        let doc = parsed.document;
        let root = doc.root();
        let p = query_selector_str(&doc, root, "p")
            .expect("セレクタは解釈できる")
            .expect("p 要素が見つかる");
        let text_node = doc.first_child(p).expect("p の子にテキストノードがある");
        assert!(!doc.is_element(text_node));
        assert_eq!(compute_state(&doc, text_node), State::default());
    }
}

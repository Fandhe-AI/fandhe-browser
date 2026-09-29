//! `snapshot::Node::role` フィールドの算出ロジックの骨格と代表要素
//! （`AISNAP-1`・`TASK-11.3.1`・`MS-2`・Issue #541。親は TASK-11.3・Issue #72）。
//!
//! 呼び出し文脈: 現時点では呼び出し元がない。DOM から `Snapshot`/`Node` を
//! 構築する TASK-11.7（Issue #76）が、ツリー構築時に要素ごとへ
//! [`compute_role`] を呼ぶ想定である（実装済みを装わない。REPAIR-3）。
//!
//! # 設計上の制約（文書を走査しない）
//!
//! 本モジュールが暗黙 role を割り当てるのは、「要素自身の local name と
//! 自身の属性だけ」で決まる要素に限る。祖先・子孫・ID 参照（`aria-labelledby`
//! 等）・テキスト内容は一切参照しない。1 要素あたりの計算量は O(自身の属性数)
//! で、外部 HTML 由来の巨大 DOM でも文書全体の走査（O(N²) 化）を起こさない
//! （coding-rust.md「長さ・件数の上限」・security.md「不安全な設計」）。
//!
//! # 算出する role
//!
//! 1. 明示 role: `role` 属性のトークンのうち、WAI-ARIA 1.2 の具象 role
//!    （abstract role を除く）の許可リストに一致する最初の 1 つ。ただし
//!    `none`/`presentation` は ARIA 1.2 の競合解決に従い、フォーカス可能な
//!    要素・グローバル ARIA 属性を持つ要素では無視する
//! 2. 暗黙 role（HTML-AAM の一部）:
//!    - `button` → `button`
//!    - `a`・`area`: `href` があれば `link`、無ければ `generic`
//!    - `h1`〜`h6` → `heading`
//!    - `table`・`thead`/`tbody`/`tfoot`・`tr`・`td` →
//!      `table`・`rowgroup`・`row`・`cell`
//!    - `th`: `scope="row"`（大文字小文字を区別しない）なら `rowheader`、
//!      それ以外は `columnheader`
//!    - `ul`/`ol` → `list`、`li` → `listitem`
//!    - ドキュメントルート → `document`
//! 3. 上記以外は `generic`（Fallback）
//!
//! # スコープ外（後続タスクへ引き継ぐ。実装済みを装わない。REPAIR-3）
//!
//! - `input[type]` の対応表: TASK-11.3.2・Issue #542。現状 `input` は
//!   `generic`（Fallback）を返す暫定分岐である
//! - `select`（`multiple`/`size` による `listbox`/`combobox`）・
//!   `header`/`footer`（祖先による `banner`/`contentinfo`）など文脈で変わる
//!   role: TASK-11.3.3・Issue #543。現状はいずれも `generic`（Fallback）
//! - `th` の表文脈による `cell` への降格: TASK-11.3.3・Issue #543
//! - `form`・`section` の名前依存の昇格（`form`/`region`）と、`img` の
//!   `alt=""` による `none`: accessible name の算出（TASK-11.4・Issue #73）
//!   に依存するため未実装で、現状は `generic`（`img` も `generic`）。
//!   これらの担当 sub-issue は未割り当て
//! - role の必須コンテキスト（required context role）の検証
//! - DPub/Graphics ARIA モジュールの role
//! - JS による動的な role の変更（本実装は content attribute のみを見る）
//!
//! `none`/`presentation` の競合解決は次の 2 点を意図的に保守的に簡略化する
//! （どちらも操作対象を意味上消さない方向）。`disabled` によるフォーカス
//! 不可は考慮しない。`tabindex` は値の妥当性を問わず、属性があればフォーカス
//! 可能として扱う。

use fandhe_browser_core::dom::{Document, NodeData, NodeId};

/// HTML 名前空間の URI。`core::dom::HTML_NAMESPACE_URI` は `pub(crate)` で
/// crate 外から使えないため、本モジュール用にローカルへ定義する
/// （[`state`](super::state) と同じ理由。`core` の公開 API は変更しない）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// WAI-ARIA 1.2 の具象 role（concrete role）の許可リスト。
///
/// abstract role（`command`・`composite`・`input`・`landmark`・`range`・
/// `roletype`・`section`・`sectionhead`・`select`・`structure`・`widget`・
/// `window`）と DPub/Graphics モジュールの role は含めない。`role` 属性の値を
/// この許可リストと照合し、一致する最初のトークンだけを採用する
/// （[`explicit_role`]。任意文字列を role として下流へ通さないための境界）。
const KNOWN_ROLES: &[&str] = &[
    "alert",
    "alertdialog",
    "application",
    "article",
    "banner",
    "blockquote",
    "button",
    "caption",
    "cell",
    "checkbox",
    "code",
    "columnheader",
    "combobox",
    "complementary",
    "contentinfo",
    "definition",
    "deletion",
    "dialog",
    "directory",
    "document",
    "emphasis",
    "feed",
    "figure",
    "form",
    "generic",
    "grid",
    "gridcell",
    "group",
    "heading",
    "img",
    "insertion",
    "link",
    "list",
    "listbox",
    "listitem",
    "log",
    "main",
    "marquee",
    "math",
    "meter",
    "menu",
    "menubar",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "navigation",
    "none",
    "note",
    "option",
    "paragraph",
    "presentation",
    "progressbar",
    "radio",
    "radiogroup",
    "region",
    "row",
    "rowgroup",
    "rowheader",
    "scrollbar",
    "search",
    "searchbox",
    "separator",
    "slider",
    "spinbutton",
    "status",
    "strong",
    "subscript",
    "superscript",
    "switch",
    "tab",
    "table",
    "tablist",
    "tabpanel",
    "term",
    "textbox",
    "time",
    "timer",
    "toolbar",
    "tooltip",
    "tree",
    "treegrid",
    "treeitem",
];

/// グローバル ARIA 属性（WAI-ARIA 1.2「Global States and Properties」。
/// deprecated のものを含む）。`none`/`presentation` の競合解決で使う。
/// deprecated を含めて偽陽性側に倒しても、暗黙 role が残るだけで安全。
const GLOBAL_ARIA_ATTRIBUTES: &[&str] = &[
    "aria-atomic",
    "aria-busy",
    "aria-controls",
    "aria-current",
    "aria-describedby",
    "aria-description",
    "aria-details",
    "aria-disabled",
    "aria-dropeffect",
    "aria-errormessage",
    "aria-flowto",
    "aria-grabbed",
    "aria-haspopup",
    "aria-hidden",
    "aria-invalid",
    "aria-keyshortcuts",
    "aria-label",
    "aria-labelledby",
    "aria-live",
    "aria-owns",
    "aria-relevant",
    "aria-roledescription",
];

/// role の算出根拠（`AISNAP-1`・`TASK-11.3.1`）。
///
/// AI エージェントが「作者の明示指定」と「ブラウザの既定解釈」を区別できる
/// よう、算出元を残す。`#[non_exhaustive]` により、将来の根拠追加が
/// 非破壊で済む（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoleSource {
    /// `role` 属性の有効なトークンを採用した。
    Explicit,
    /// HTML-AAM に基づく要素・属性からの暗黙 role。
    Implicit,
    /// 対応表に該当せず `generic` にフォールバックした。
    Fallback,
}

/// 算出済みの role（`AISNAP-1`・`TASK-11.3.1`）。[`compute_role`] の戻り値。
///
/// フィールドを private にしアクセサ経由で読ませることで、将来 role の内部
/// 表現を変えても呼び出し側を壊さずに済む（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ComputedRole {
    role: &'static str,
    source: RoleSource,
}

impl ComputedRole {
    /// role トークンを返す（例: `"button"`・`"heading"`）。
    ///
    /// `snapshot::Node::role` へ詰める際は `as_str().to_string()` とする
    /// 想定（配線は TASK-11.7・Issue #76 が担う）。
    pub fn as_str(&self) -> &'static str {
        self.role
    }

    /// role の算出根拠を返す。
    pub fn source(&self) -> RoleSource {
        self.source
    }

    fn explicit(role: &'static str) -> Self {
        Self {
            role,
            source: RoleSource::Explicit,
        }
    }

    fn implicit(role: &'static str) -> Self {
        Self {
            role,
            source: RoleSource::Implicit,
        }
    }

    fn fallback() -> Self {
        Self {
            role: "generic",
            source: RoleSource::Fallback,
        }
    }
}

/// `id` が HTML 名前空間の要素で local name が `name`（ASCII 大文字小文字を
/// 区別しない）と一致するかどうかを返す。[`super::state`] の同名 private
/// helper と同じ判定規則（共通化は TASK-11.7 以降のフォローアップ。
/// 本 Issue では `state.rs` を変更しない）。
fn is_html_element_named(doc: &Document, id: NodeId, name: &str) -> bool {
    doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && doc
            .local_name(id)
            .is_some_and(|local| local.eq_ignore_ascii_case(name))
}

/// 要素 `id` が「ネイティブにフォーカス可能」または `tabindex` を持つか
/// （`none`/`presentation` の競合解決 (a)）。自身の属性しか見ない。
fn is_focusable(doc: &Document, id: NodeId) -> bool {
    let named = |name: &str| is_html_element_named(doc, id, name);
    if doc.attribute(id, "tabindex").is_some() {
        return true;
    }
    // 編集可能要素（`contenteditable` が `false` 以外）はフォーカス可能扱い。
    // 継承による編集可否は自身の属性しか見ない方針のためここでは扱わない。
    if doc.attribute(id, "contenteditable").is_some_and(|value| {
        !value
            .trim_matches(|c: char| c.is_ascii_whitespace())
            .eq_ignore_ascii_case("false")
    }) {
        return true;
    }
    if named("a") || named("area") {
        return doc.attribute(id, "href").is_some();
    }
    if named("input") {
        let hidden = doc.attribute(id, "type").is_some_and(|t| {
            t.trim_matches(|c: char| c.is_ascii_whitespace())
                .eq_ignore_ascii_case("hidden")
        });
        return !hidden;
    }
    named("button") || named("select") || named("textarea") || named("summary")
}

/// 要素 `id` がグローバル ARIA 属性を 1 つ以上持つか（競合解決 (b)）。
/// 属性一覧を 1 回走査するだけで、属性ごとの再検索はしない。
fn has_global_aria_attribute(doc: &Document, id: NodeId) -> bool {
    doc.attributes(id).iter().any(|attr| {
        attr.name.ns.is_empty()
            && GLOBAL_ARIA_ATTRIBUTES
                .iter()
                .any(|known| (*attr.name.local).eq_ignore_ascii_case(known))
    })
}

/// `none`/`presentation` の明示 role を無視すべきか（ARIA 1.2
/// 「Presentational Roles Conflict Resolution」）。フォーカス可能、または
/// グローバル ARIA 属性を持つ要素では、操作対象を消さないため無視する。
fn ignores_presentational_role(doc: &Document, id: NodeId) -> bool {
    is_focusable(doc, id) || has_global_aria_attribute(doc, id)
}

/// `a`/`area` 要素の暗黙 role を返す。`href` があれば `link`、無ければ
/// （リンクとして機能しないため）`generic`。
fn implicit_role_for_hyperlink(doc: &Document, id: NodeId) -> ComputedRole {
    if doc.attribute(id, "href").is_some() {
        ComputedRole::implicit("link")
    } else {
        ComputedRole::fallback()
    }
}

/// `th` 要素の暗黙 role を返す（`scope="row"`/`"rowgroup"` なら `rowheader`、それ以外は
/// `columnheader`）。`scope` の照合は大文字小文字を区別しない。表の文脈
/// による `cell` への降格は TASK-11.3.3（Issue #543）で扱う。
fn implicit_role_for_th(doc: &Document, id: NodeId) -> ComputedRole {
    let is_row_scope = doc.attribute(id, "scope").is_some_and(|value| {
        let value = value.trim_matches(|c: char| c.is_ascii_whitespace());
        value.eq_ignore_ascii_case("row") || value.eq_ignore_ascii_case("rowgroup")
    });
    if is_row_scope {
        ComputedRole::implicit("rowheader")
    } else {
        ComputedRole::implicit("columnheader")
    }
}

/// `input` 要素の暗黙 role。
///
/// スタブ: `input[type]` の対応表は TASK-11.3.2（Issue #542・`AISNAP-1`）で
/// 実装する。それまでは `generic`（Fallback）を返す。#542 はこの関数の
/// 中身だけを差し替える。
fn implicit_role_for_input(_doc: &Document, _id: NodeId) -> ComputedRole {
    ComputedRole::fallback()
}

/// 文脈（祖先・属性値）で role が変わる要素（`select`・`header`・`footer`）
/// の暗黙 role。
///
/// スタブ: TASK-11.3.3（Issue #543・`AISNAP-1`）で実装する。`select` は
/// `multiple`/`size` による `listbox`/`combobox`、`header`/`footer` は
/// sectioning 祖先の有無による `banner`/`contentinfo`/`generic`。それまでは
/// `generic`（Fallback）を返す。#543 はこの関数の中身だけを差し替える。
fn implicit_role_for_contextual(_doc: &Document, _id: NodeId) -> ComputedRole {
    ComputedRole::fallback()
}

/// 要素 `id` の暗黙 role を local name で振り分ける単一のディスパッチャ。
///
/// 非 HTML 名前空間の要素（SVG・MathML）と対応表に無い要素は `generic`
/// （Fallback）。後続 sub-issue（#542・#543）は分岐の中身を足すだけでよい。
fn implicit_role(doc: &Document, id: NodeId) -> ComputedRole {
    if doc.namespace_url(id) != Some(HTML_NAMESPACE_URI) {
        return ComputedRole::fallback();
    }
    let Some(local) = doc.local_name(id) else {
        return ComputedRole::fallback();
    };
    let local = local.to_ascii_lowercase();
    match local.as_str() {
        "button" => ComputedRole::implicit("button"),
        "a" | "area" => implicit_role_for_hyperlink(doc, id),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => ComputedRole::implicit("heading"),
        "table" => ComputedRole::implicit("table"),
        "thead" | "tbody" | "tfoot" => ComputedRole::implicit("rowgroup"),
        "tr" => ComputedRole::implicit("row"),
        "td" => ComputedRole::implicit("cell"),
        "th" => implicit_role_for_th(doc, id),
        "ul" | "ol" => ComputedRole::implicit("list"),
        "li" => ComputedRole::implicit("listitem"),
        "input" => implicit_role_for_input(doc, id),
        "select" | "header" | "footer" => implicit_role_for_contextual(doc, id),
        // form・section・img を含む対応表外は generic（module doc 参照）。
        _ => ComputedRole::fallback(),
    }
}

/// `role` 属性の値から、有効な明示 role を算出する。
///
/// 空白区切りのトークン列を先頭から走査し、[`KNOWN_ROLES`] に大文字小文字を
/// 区別せず一致する最初のトークンを採用する。採用トークンが
/// `none`/`presentation` で競合解決により無視される場合は `None`
/// （暗黙 role へ）を返し、次のトークンは試さない。`role` 属性は名前空間を
/// 問わず見る。
fn explicit_role(doc: &Document, id: NodeId) -> Option<ComputedRole> {
    let value = doc.attribute(id, "role")?;
    let known = value.split_ascii_whitespace().find_map(|token| {
        KNOWN_ROLES
            .iter()
            .find(|known| token.eq_ignore_ascii_case(known))
    })?;
    if (*known == "none" || *known == "presentation") && ignores_presentational_role(doc, id) {
        return None;
    }
    Some(ComputedRole::explicit(known))
}

/// `doc` のノード `id` から role を算出する（`AISNAP-1`・`TASK-11.3.1`）。
///
/// - ドキュメントルートは `Some("document", Implicit)`
/// - 要素は必ず `Some`（明示 role → 暗黙 role → `generic` の順）
/// - テキスト等の非要素・範囲外の `id` は `None`（role には非要素向けの
///   既定トークンが無いため、`compute_state` の既定値返却とは契約が異なる）
pub fn compute_role(doc: &Document, id: NodeId) -> Option<ComputedRole> {
    match doc.node_data(id)? {
        NodeData::Document => Some(ComputedRole::implicit("document")),
        NodeData::Element { .. } => {
            Some(explicit_role(doc, id).unwrap_or_else(|| implicit_role(doc, id)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{ComputedRole, RoleSource, compute_role};
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

    fn assert_role(role: Option<ComputedRole>, expected: &str, source: RoleSource) {
        let role = role.expect("role が算出される");
        assert_eq!(role.as_str(), expected);
        assert_eq!(role.source(), source);
    }

    /// 単一要素の HTML から role を算出して検証する。
    fn check(html: &str, selector: &str, expected: &str, source: RoleSource) {
        let (doc, id) = parse_and_select(html, selector);
        assert_role(compute_role(&doc, id), expected, source);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: ドキュメントルートは document。
    #[test]
    fn aisnap_1_document_root() {
        let parsed =
            parse_document("<p>x</p>", &ParseOptions::default()).expect("パースは成功する");
        let doc = parsed.document;
        assert_role(
            compute_role(&doc, doc.root()),
            "document",
            RoleSource::Implicit,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: button・link の暗黙 role。
    #[test]
    fn aisnap_1_button_and_link() {
        check(
            "<button>b</button>",
            "button",
            "button",
            RoleSource::Implicit,
        );
        check(r#"<a href="/x">l</a>"#, "a", "link", RoleSource::Implicit);
        check("<a>l</a>", "a", "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: heading の暗黙 role。
    #[test]
    fn aisnap_1_headings() {
        check("<h1>t</h1>", "h1", "heading", RoleSource::Implicit);
        check("<h6>t</h6>", "h6", "heading", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: table 系の暗黙 role。
    #[test]
    fn aisnap_1_table_family() {
        let html = "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>c</td></tr></tbody><tfoot><tr><td>f</td></tr></tfoot></table>";
        let cases = [
            ("table", "table"),
            ("thead", "rowgroup"),
            ("tbody", "rowgroup"),
            ("tfoot", "rowgroup"),
            ("tr", "row"),
            ("td", "cell"),
            ("th", "columnheader"),
        ];
        for (selector, expected) in cases {
            check(html, selector, expected, RoleSource::Implicit);
        }
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `th scope=row` は大文字小文字を
    /// 区別せず rowheader。
    #[test]
    fn aisnap_1_th_scope_row() {
        for scope in ["row", "ROW", "rowgroup", "RowGroup"] {
            let html = format!(r#"<table><tr><th scope="{scope}">h</th></tr></table>"#);
            check(&html, "th", "rowheader", RoleSource::Implicit);
        }
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `contenteditable` 要素は
    /// フォーカス可能扱いで `role="none"` を無視する。`false` は対象外。
    #[test]
    fn aisnap_1_contenteditable_ignores_none() {
        check(
            r#"<div contenteditable role="none">x</div>"#,
            "div",
            "generic",
            RoleSource::Fallback,
        );
        check(
            r#"<div contenteditable="false" role="none">x</div>"#,
            "div",
            "none",
            RoleSource::Explicit,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: list 系の暗黙 role。
    #[test]
    fn aisnap_1_list_family() {
        let html = "<ul><li>a</li></ul><ol><li>b</li></ol>";
        check(html, "ul", "list", RoleSource::Implicit);
        check(html, "ol", "list", RoleSource::Implicit);
        check(html, "li", "listitem", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 明示 role は暗黙 role より優先。
    #[test]
    fn aisnap_1_explicit_role_wins() {
        check(
            r#"<div role="button">x</div>"#,
            "div",
            "button",
            RoleSource::Explicit,
        );
        check(
            r#"<a href="/x" role="tab">x</a>"#,
            "a",
            "tab",
            RoleSource::Explicit,
        );
        check(
            r#"<div role="none">x</div>"#,
            "div",
            "none",
            RoleSource::Explicit,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 未知トークンは飛ばして最初の
    /// 有効トークンを採用し、大文字小文字は区別しない。
    #[test]
    fn aisnap_1_explicit_role_token_selection() {
        check(
            r#"<div role="foo button">x</div>"#,
            "div",
            "button",
            RoleSource::Explicit,
        );
        check(
            r#"<div role="Button">x</div>"#,
            "div",
            "button",
            RoleSource::Explicit,
        );
        check(
            r#"<div role="foo">x</div>"#,
            "div",
            "generic",
            RoleSource::Fallback,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: abstract role は拒否し暗黙 role。
    #[test]
    fn aisnap_1_abstract_role_rejected() {
        check(
            r#"<button role="landmark">x</button>"#,
            "button",
            "button",
            RoleSource::Implicit,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: none/presentation の競合解決。
    #[test]
    fn aisnap_1_presentational_conflict_resolution() {
        check(
            r#"<button role="presentation">x</button>"#,
            "button",
            "button",
            RoleSource::Implicit,
        );
        check(
            r#"<a href="/x" role="none">x</a>"#,
            "a",
            "link",
            RoleSource::Implicit,
        );
        check(
            r#"<div role="presentation" aria-label="x">x</div>"#,
            "div",
            "generic",
            RoleSource::Fallback,
        );
        check(
            r#"<span role="none" tabindex="0">x</span>"#,
            "span",
            "generic",
            RoleSource::Fallback,
        );
        check(
            r#"<div role="presentation">x</div>"#,
            "div",
            "presentation",
            RoleSource::Explicit,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `input` は #542 で差し替えるまで
    /// generic（Fallback）。継ぎ目を固定する回帰テスト（#542 で更新する）。
    #[test]
    fn aisnap_1_input_is_placeholder_until_task_11_3_2() {
        check(
            r#"<input type="text">"#,
            "input",
            "generic",
            RoleSource::Fallback,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: select・header・footer は #543 で
    /// 差し替えるまで generic（Fallback）。
    #[test]
    fn aisnap_1_contextual_elements_are_placeholder_until_task_11_3_3() {
        check(
            "<select><option>a</option></select>",
            "select",
            "generic",
            RoleSource::Fallback,
        );
        check(
            "<header>h</header>",
            "header",
            "generic",
            RoleSource::Fallback,
        );
        check(
            "<footer>f</footer>",
            "footer",
            "generic",
            RoleSource::Fallback,
        );
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: テキストノード・範囲外 ID は None。
    #[test]
    fn aisnap_1_non_element_is_none() {
        let (doc, id) = parse_and_select("<p>text</p>", "p");
        let text = doc.children(id).next().expect("テキスト子がある");
        assert!(compute_role(&doc, text).is_none());
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 非 HTML 名前空間（SVG）は generic。
    #[test]
    fn aisnap_1_svg_is_generic() {
        check(
            "<svg><a href=\"/x\"></a></svg>",
            "svg",
            "generic",
            RoleSource::Fallback,
        );
    }
}

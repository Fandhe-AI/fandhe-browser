//! `snapshot::Node::role` フィールドの算出ロジック（`AISNAP-1`・`TASK-11.3`・
//! `MS-2`・Issue #72）。本ファイルは TASK-11.3.1（Issue #541）が担う骨格と
//! 代表要素のみを実装する。
//!
//! 呼び出し文脈: 現時点では呼び出し元がない。DOM から `Snapshot`/`Node` を
//! 構築する TASK-11.7（Issue #76）が、ツリー構築時に要素ごとへ
//! [`compute_role`] を呼ぶ想定である（実装済みを装わない。REPAIR-3）。
//!
//! # スコープ（TASK-11.3.1・Issue #541）
//!
//! 本実装が算出するのは次の 2 つのみである。
//!
//! - 明示 role: `role` 属性のトークンのうち、WAI-ARIA 1.2 の具象 role
//!   （abstract role を除く）の許可リストに一致する最初の 1 つ
//! - 暗黙 role: HTML-AAM（ARIA in HTML）の代表要素（button・link・heading・
//!   table 系・list 系・`th`・`img` 等。下記対応表）からの role
//!
//! 算出は content attribute だけを見る静的算出であり、JS による動的な
//! role の変更（IDL 属性・`Element.setAttribute` 呼び出し）は扱わない。
//!
//! 以下は本 Issue（TASK-11.3.1）のスコープ外とし、後続タスクへ引き継ぐ
//! （実装済みを装わない。REPAIR-3）。
//!
//! - `input[type]` ごとの role の対応表（password を含む全 type）
//!   → TASK-11.3.2（Issue #542）
//! - `select`（`multiple`・`size`）の role → TASK-11.3.3（Issue #543）
//! - `header`/`footer`/`aside` の、sectioning 祖先（`role` 属性で指定した
//!   祖先も含む）による role の切り替え → TASK-11.3.3（Issue #543）
//! - `none`/`presentation` の競合解決の完全な判定（[`explicit_role`] は
//!   フォーカス可能性・グローバル ARIA 属性の簡易判定（`tabindex` 属性の
//!   有無・代表的な対話要素・`aria-` 接頭辞の属性有無）による近似のみを
//!   行う。`disabled`/`hidden` 等によるフォーカス可能性の除外、WAI-ARIA
//!   1.2 §6.4 のグローバル状態・プロパティの正確な一覧との照合、
//!   presentation の子孫への継承は扱わない。PR #566 レビュー指摘）
//! - role の必須コンテキスト（required context role）の検証
//! - DPub/Graphics ARIA モジュールの role
//! - `section` の `region` 昇格（accessible name を持つときのみ `region`
//!   になる規則。accessible name の算出自体が TASK-11.4・Issue #73 の範囲）
//! - `form`・`img` の完全な accessible name 算出。`aria-labelledby` が
//!   指す要素の実テキストの連結・空白正規化・ネイティブラベリング機構
//!   （`label` 要素等）・`title` 属性まで含めた優先順位付き判定は
//!   TASK-11.4・Issue #73 の範囲であり、本実装は `aria-label` の非空値の
//!   有無、または `aria-labelledby` が指す ID が文書内に実在するかどうか
//!   だけを見る簡易 hint（[`has_name_hint`]）に留める（PR #566 レビュー
//!   指摘を受け、参照先未解決の `aria-labelledby` は名前ヒントに含めない
//!   よう修正済み）
//! - `th`/`td` の表文脈による完全な判定（`scope` が無い `th` を位置で
//!   行見出し・列見出し・セルに振り分ける処理、`td` が `gridcell` になる
//!   文脈）。本実装は `th` を `scope` 属性のみで判定する簡易実装である
//! - `isDataLeaf`（表・一覧類型の葉ノード判定。`AISNAP-3`・TASK-13）
//!
//! # PoC との違い
//!
//! `docs/spec/03-poc/ai-interface-token-reduction/proto/reduce.mjs` の
//! `roleOf`（PoC-4）は `label` 要素を一律 `"text"` としているが、本実装は
//! HTML-AAM に従い、`label` は対応表に無いため `generic`（Fallback）になる。

use fandhe_browser_core::dom::{Document, NodeData, NodeId};

/// HTML 名前空間の URI。`core::dom::HTML_NAMESPACE_URI` は `pub(crate)` で
/// crate 外から使えないため、本モジュール用にローカルへ定義する
/// （[`super::state`] と同じ理由。`core` の公開 API は変更しない。
/// 共通化は TASK-11.7 以降のフォローアップとする）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// WAI-ARIA 1.2 の具象 role（concrete role）の許可リスト（出典: WAI-ARIA 1.2
/// §5.4 Definition of Roles の role 一覧）。
///
/// abstract role（`command`・`composite`・`input`・`landmark`・`range`・
/// `roletype`・`section`・`sectionhead`・`select`・`structure`・`widget`・
/// `window`）は含めない。`role` 属性の値をこの許可リストと照合し、一致する
/// 最初のトークンだけを採用する（[`explicit_role`] の設計。WAI-ARIA 1.2 の
/// role フォールバック規則）。
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

/// role の算出根拠（`AISNAP-1`・`TASK-11.3`）。
///
/// AI エージェントが「作者の明示指定」と「ブラウザの既定解釈」を区別できる
/// よう、算出元を残す。`#[non_exhaustive]` により、将来の根拠追加
/// （例: CSS `appearance` からの推定）が非破壊で済む（REPAIR-4）。
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

/// 算出済みの role（`AISNAP-1`・`TASK-11.3`）。[`compute_role`] の戻り値。
///
/// フィールドを private にしアクセサ（[`ComputedRole::as_str`]・
/// [`ComputedRole::source`]）経由で読ませることで、将来 role の内部表現
/// （例: 複数トークンの保持）を変えても呼び出し側を壊さずに済む（REPAIR-4）。
/// `#[non_exhaustive]` も同じ理由で付ける。
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

/// `th` 要素の暗黙 role を返す（`scope` の値が `row`/`rowgroup` なら
/// `rowheader`、それ以外は `columnheader`）。`scope` の照合は大文字小文字を
/// 区別しない。
///
/// 簡易実装: HTML-AAM の表文脈による完全な判定（`scope` が無い `th` を
/// 位置で行見出し・列見出しに振り分ける処理）は TASK-11.3.3（Issue #543）
/// の範囲であり、本実装は `scope` 属性の値のみで判定する。
fn implicit_role_for_th(doc: &Document, id: NodeId) -> ComputedRole {
    let is_row_scope = doc.attribute(id, "scope").is_some_and(|value| {
        value.eq_ignore_ascii_case("row") || value.eq_ignore_ascii_case("rowgroup")
    });
    if is_row_scope {
        ComputedRole::implicit("rowheader")
    } else {
        ComputedRole::implicit("columnheader")
    }
}

/// `doc` 内に `id` content attribute の値が `target` と一致する要素が
/// 存在するかどうかを返す（`aria-labelledby` の IDREF 解決用）。
///
/// [`Document`] は id 索引を持たないため、`doc.root()` から
/// [`Document::descendants`] で全要素を走査する（`node_count` で上限打ち切り
/// 済みのため無制限走査にはならない。security.md「不安全な設計」対策）。
/// 複数要素が同じ `id` を持つ不正な HTML では最初に見つかった要素を採用する
/// （最初の一致で resolve する、というブラウザの一般的な `getElementById`
/// 挙動に合わせる）。
fn element_with_id_exists(doc: &Document, target: &str) -> bool {
    doc.descendants(doc.root())
        .any(|node| doc.attribute(node, "id") == Some(target))
}

/// `aria-labelledby` の値（空白区切りの ID 列）のうち、`doc` 内に実在する
/// 要素を指すトークンが 1 つでもあるかどうかを返す。
///
/// 参照先が存在しない IDREF（例: `aria-labelledby="missing"`）は、それが
/// 指す要素のテキストを持ちえないため名前のヒントとして扱わない
/// （PR #566 レビュー指摘。`has_name_hint` 参照）。
fn labelledby_references_existing_element(doc: &Document, id: NodeId) -> bool {
    doc.attribute(id, "aria-labelledby").is_some_and(|value| {
        value
            .split_ascii_whitespace()
            .any(|token| element_with_id_exists(doc, token))
    })
}

/// 属性値のみを見る簡易な「名前ヒント」判定。次のいずれかが成り立てば
/// 名前を持つとみなす。
///
/// - `aria-label` が空でない値を持つ
/// - `aria-labelledby` が、`doc` 内に実在する要素を指す ID を 1 つ以上含む
///   （[`labelledby_references_existing_element`]。参照先未解決の IDREF は
///   名前のヒントとして扱わない）
///
/// 簡易実装: WAI-ARIA の accessible name 算出アルゴリズム（`aria-labelledby`
/// が指す要素の実テキスト・ネイティブラベリング機構（`label` 要素等）・
/// `title` 属性まで含めた優先順位付き算出）は TASK-11.4（Issue #73）の
/// 範囲であり、本実装は参照先の「存在」までしか見ない（実テキストの連結・
/// 空白正規化・`aria-label` との優先順位づけは行わない。
/// [`implicit_role_for_img`]・`form` の暗黙 role 判定から利用）。
fn has_name_hint(doc: &Document, id: NodeId) -> bool {
    let non_empty = |attr: &str| {
        doc.attribute(id, attr)
            .is_some_and(|value| !value.trim().is_empty())
    };
    non_empty("aria-label") || labelledby_references_existing_element(doc, id)
}

/// `img` 要素の暗黙 role を返す。`alt=""`（空文字列。属性自体が無い場合は
/// 含まない）は装飾画像として `none` になるが、[`has_name_hint`] が示す
/// ARIA 名（`aria-label`/`aria-labelledby`）を持つ場合はその意図を優先し
/// `img` のまま扱う（HTML-AAM。ARIA 名がある画像を装飾画像として消さない
/// ため）。それ以外（`alt` 省略を含む）は `img`。
fn implicit_role_for_img(doc: &Document, id: NodeId) -> ComputedRole {
    if doc.attribute(id, "alt") == Some("") && !has_name_hint(doc, id) {
        ComputedRole::implicit("none")
    } else {
        ComputedRole::implicit("img")
    }
}

/// `a`/`area` 要素の暗黙 role を返す。`href` があれば `link`、無ければ
/// （リンクとして機能しないため）`generic`（Fallback）。
fn implicit_role_for_hyperlink(doc: &Document, id: NodeId) -> ComputedRole {
    if doc.attribute(id, "href").is_some() {
        ComputedRole::implicit("link")
    } else {
        ComputedRole::fallback()
    }
}

/// HTML-AAM（ARIA in HTML）の対応表に基づき、要素 `id` の暗黙 role を
/// 算出する（本モジュール doc「スコープ」節の対応表。TASK-11.3.1 が扱う
/// 代表要素のみ）。
///
/// `id` が HTML 名前空間の要素でない場合（SVG/MathML 等）、または対応表に
/// 該当しない場合は `generic`（Fallback）を返す。
///
/// 拡張点: 分岐は 1 つの `match` 相当の連鎖に集約してあり、後続 Issue が
/// 分岐を追加する箇所を以下にコメントで示す。
fn implicit_role(doc: &Document, id: NodeId) -> ComputedRole {
    let named = |name: &str| is_html_element_named(doc, id, name);

    if named("a") || named("area") {
        return implicit_role_for_hyperlink(doc, id);
    }
    if named("button") {
        return ComputedRole::implicit("button");
    }
    // `input` → TASK-11.3.2（Issue #542）が `implicit_role_for_input` を
    // 追加してここに分岐を足す。それまでは対応表に無い要素として generic
    // （Fallback）になる。
    // `select` → TASK-11.3.3（Issue #543）が `implicit_role_for_select` を
    // 追加してここに分岐を足す。それまでは generic（Fallback）になる。
    if named("option") {
        return ComputedRole::implicit("option");
    }
    if named("textarea") {
        return ComputedRole::implicit("textbox");
    }
    const HEADINGS: [&str; 6] = ["h1", "h2", "h3", "h4", "h5", "h6"];
    if HEADINGS.iter().any(|tag| named(tag)) {
        return ComputedRole::implicit("heading");
    }
    if named("table") {
        return ComputedRole::implicit("table");
    }
    if named("thead") || named("tbody") || named("tfoot") {
        return ComputedRole::implicit("rowgroup");
    }
    if named("tr") {
        return ComputedRole::implicit("row");
    }
    if named("td") {
        return ComputedRole::implicit("cell");
    }
    if named("th") {
        return implicit_role_for_th(doc, id);
    }
    if named("ul") || named("ol") || named("menu") {
        return ComputedRole::implicit("list");
    }
    if named("li") {
        return ComputedRole::implicit("listitem");
    }
    if named("nav") {
        return ComputedRole::implicit("navigation");
    }
    if named("main") {
        return ComputedRole::implicit("main");
    }
    // `aside` → TASK-11.3.3（Issue #543）が sectioning 祖先による判定
    // （`header`/`footer` と同じ判定規則）を追加してここに分岐を足す。
    // それまでは generic（Fallback）になる。
    // `form` は HTML-AAM で accessible name を持つ場合に限り `form` になる
    // （`section` の `region` 昇格と同じ規則）。[`has_name_hint`] は
    // `aria-label`/`aria-labelledby` の非空値だけを見る簡易判定であり、
    // `aria-labelledby` が指す要素のテキストやネイティブラベリング機構
    // までを含めた完全な accessible name 算出は TASK-11.4（Issue #73）の
    // 範囲。名前が無い/判定できない場合はここで return せず下の
    // フォールバックへ流し `generic` にする。
    if named("form") && has_name_hint(doc, id) {
        return ComputedRole::implicit("form");
    }
    if named("article") {
        return ComputedRole::implicit("article");
    }
    // `header`/`footer` → TASK-11.3.3（Issue #543）が sectioning 祖先
    // （`role` 属性で指定した祖先も含む）による `banner`/`contentinfo` と
    // `generic` の切り替えを追加してここに分岐を足す。それまでは対応表に
    // 無い要素として generic（Fallback）になる。
    if named("img") {
        return implicit_role_for_img(doc, id);
    }
    if named("p") {
        return ComputedRole::implicit("paragraph");
    }
    if named("hr") {
        return ComputedRole::implicit("separator");
    }
    if named("fieldset") {
        return ComputedRole::implicit("group");
    }
    if named("dialog") {
        return ComputedRole::implicit("dialog");
    }
    // `section` は accessible name を持つときだけ `region` になる
    // （HTML-AAM）。accessible name の算出は TASK-11.4・Issue #73 の範囲の
    // ため、本 Issue では常に generic を返す（module doc「スコープ外」）。
    ComputedRole::fallback()
}

/// ネイティブに（`tabindex` 属性なしで）フォーカス可能な HTML 要素かどうかを
/// 判定する。[`is_presentation_conflict`] からのみ使う簡易判定であり、
/// disabled・`hidden` 等による除外は見ない（フォーカス可能性の完全な判定は
/// 本 Issue のスコープ外。下記コメント参照）。
fn is_natively_focusable_element(doc: &Document, id: NodeId) -> bool {
    let named = |name: &str| is_html_element_named(doc, id, name);
    if (named("a") || named("area")) && doc.attribute(id, "href").is_some() {
        return true;
    }
    named("button") || named("input") || named("select") || named("textarea")
}

/// `id` が「フォーカス可能」または「グローバル ARIA 状態・プロパティ」を
/// 持つかどうかを返す（WAI-ARIA 1.2 の presentation/none 競合解決規則の
/// 判定に使う）。
///
/// 簡易実装: フォーカス可能性は `tabindex` 属性の有無（値の妥当性は見ない）
/// と、代表的なネイティブ対話要素（[`is_natively_focusable_element`]。
/// `disabled`・`hidden` 等による除外は考慮しない）だけで判定する。
/// グローバル ARIA 属性は `aria-` 接頭辞を持つ属性の有無で近似する
/// （WAI-ARIA 1.2 §6.4 の正確な一覧との照合はしない。`aria-hidden` も
/// 含める。完全な判定は TASK-11.3.3・Issue #543 以降のフォローアップとする
/// ）。
fn has_focus_or_global_aria(doc: &Document, id: NodeId) -> bool {
    if doc.attribute(id, "tabindex").is_some() {
        return true;
    }
    if is_natively_focusable_element(doc, id) {
        return true;
    }
    doc.attributes(id)
        .iter()
        .any(|attr| attr.name.ns.is_empty() && attr.name.local.starts_with("aria-"))
}

/// `role` 属性の値から、有効な明示 role を算出する。
///
/// WAI-ARIA のフォールバック規則に従い、空白区切りのトークン列を先頭から
/// 走査し、[`KNOWN_ROLES`]（abstract role を除く具象 role の許可リスト）に
/// 一致する最初のトークンを採用する。大文字小文字は区別しない
/// （`role="Button"` も `button` として受け付ける）。有効なトークンが
/// 無ければ `None`（暗黙 role へフォールバックさせる）を返す。
///
/// 採用したトークンが `none`/`presentation` で、かつ要素がフォーカス可能
/// または グローバル ARIA 状態・プロパティを持つ場合（[`has_focus_or_global_aria`]）
/// は、WAI-ARIA の presentation/none 競合解決規則に従い `role` 属性の指定を
/// 無視し `None`（暗黙 role へフォールバック）を返す（PR #566 レビュー指摘。
/// 操作対象の要素から role を消してしまわないため）。
///
/// 返すのは許可リスト側の `&'static str` である。属性値の文字列（外部の
/// HTML に由来する untrusted 入力）はそのまま出力へ流さない。
///
/// `role` 属性は名前空間を問わず見る（SVG 要素でも有効）。
fn explicit_role(doc: &Document, id: NodeId) -> Option<ComputedRole> {
    let value = doc.attribute(id, "role")?;
    let token = value.split_ascii_whitespace().find_map(|token| {
        KNOWN_ROLES
            .iter()
            .find(|known| token.eq_ignore_ascii_case(known))
    })?;
    if (*token == "none" || *token == "presentation") && has_focus_or_global_aria(doc, id) {
        return None;
    }
    Some(ComputedRole::explicit(token))
}

/// `doc` のノード `id` から role を算出する（`AISNAP-1`・`TASK-11.3`・
/// `TASK-11.3.1`・Issue #541）。
///
/// - ドキュメントルート（[`Document::root`]）は `Some("document", Implicit)`。
///   spec の想定 JSON でルートが `role: "document"` になっているため
///   （`AISNAP-1`）
/// - 要素は必ず `Some`（明示 role → 暗黙 role → `generic` フォールバックの
///   順。本 Issue の時点では暗黙 role は代表要素のみに対応し、それ以外は
///   generic になる。TASK-11.3.2・11.3.3 が分岐を追加する）
/// - テキスト・コメント・doctype 等の非要素、範囲外の `id` は `None`。
///   これらは role を持たず [`super::Node`] にもならないので「無い」を
///   そのまま返す（[`super::state::compute_state`] が範囲外・対象外に
///   既定値を返すのとは契約が異なる。role には「要素以外の既定値」に
///   ふさわしいトークンが無いため）。
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
    /// [`super::super::state`] のテストヘルパと同じ形。
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

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `document` ルートは
    /// `"document"`/Implicit になる。
    #[test]
    fn aisnap_1_document_root_is_document() {
        let parsed =
            parse_document("<p>text</p>", &ParseOptions::default()).expect("パースは成功する");
        let doc = parsed.document;
        let root = doc.root();
        assert_role(compute_role(&doc, root), "document", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `button` 要素は `"button"`/Implicit。
    #[test]
    fn aisnap_1_button_is_button() {
        let (doc, id) = parse_and_select("<button>送信</button>", "button");
        assert_role(compute_role(&doc, id), "button", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `href` を持つ `a` は `"link"`。
    #[test]
    fn aisnap_1_anchor_with_href_is_link() {
        let (doc, id) = parse_and_select(r##"<a href="#">more</a>"##, "a");
        assert_role(compute_role(&doc, id), "link", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `href` の無い `a` はリンクとして
    /// 機能しないため `"generic"`/Fallback。
    #[test]
    fn aisnap_1_anchor_without_href_is_generic() {
        let (doc, id) = parse_and_select("<a>more</a>", "a");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `href` を持つ `area` は `"link"`。
    #[test]
    fn aisnap_1_area_with_href_is_link() {
        let (doc, id) = parse_and_select(
            r##"<map><area href="#" shape="rect" coords="0,0,1,1"></map>"##,
            "area",
        );
        assert_role(compute_role(&doc, id), "link", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `h1`〜`h6` はすべて `"heading"`。
    #[test]
    fn aisnap_1_headings_are_heading() {
        let (doc, id) = parse_and_select("<h1>title</h1>", "h1");
        assert_role(compute_role(&doc, id), "heading", RoleSource::Implicit);
        let (doc, id) = parse_and_select("<h6>title</h6>", "h6");
        assert_role(compute_role(&doc, id), "heading", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: table 系要素が対応表どおりの role
    /// になる（`table`/`rowgroup`/`row`/`columnheader`/`cell`）。
    #[test]
    fn aisnap_1_table_elements() {
        let html = r#"<table><thead><tr><th>H</th></tr></thead>
            <tbody><tr><td>D</td></tr></tbody></table>"#;
        let (doc, table) = parse_and_select(html, "table");
        assert_role(compute_role(&doc, table), "table", RoleSource::Implicit);
        let (doc, thead) = parse_and_select(html, "thead");
        assert_role(compute_role(&doc, thead), "rowgroup", RoleSource::Implicit);
        let (doc, tr) = parse_and_select(html, "thead tr");
        assert_role(compute_role(&doc, tr), "row", RoleSource::Implicit);
        let (doc, th) = parse_and_select(html, "th");
        assert_role(compute_role(&doc, th), "columnheader", RoleSource::Implicit);
        let (doc, td) = parse_and_select(html, "td");
        assert_role(compute_role(&doc, td), "cell", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `<th scope="row">` は
    /// `"rowheader"`。
    #[test]
    fn aisnap_1_th_scope_row_is_rowheader() {
        let (doc, id) = parse_and_select(
            "<table><tbody><tr><th scope=\"row\">H</th></tr></tbody></table>",
            "th",
        );
        assert_role(compute_role(&doc, id), "rowheader", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `<th scope="rowgroup">` も
    /// `"rowheader"`（大文字小文字は区別しない）。
    #[test]
    fn aisnap_1_th_scope_rowgroup_is_rowheader() {
        let (doc, id) = parse_and_select(
            "<table><tbody><tr><th scope=\"rowgroup\">H</th></tr></tbody></table>",
            "th",
        );
        assert_role(compute_role(&doc, id), "rowheader", RoleSource::Implicit);

        let (doc, id) = parse_and_select(
            "<table><tbody><tr><th scope=\"ROW\">H</th></tr></tbody></table>",
            "th",
        );
        assert_role(compute_role(&doc, id), "rowheader", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `ul`/`ol`/`menu` は `"list"`、
    /// `li` は `"listitem"`。
    #[test]
    fn aisnap_1_list_elements() {
        let (doc, ul) = parse_and_select("<ul><li>a</li></ul>", "ul");
        assert_role(compute_role(&doc, ul), "list", RoleSource::Implicit);
        let (doc, ol) = parse_and_select("<ol><li>a</li></ol>", "ol");
        assert_role(compute_role(&doc, ol), "list", RoleSource::Implicit);
        let (doc, menu) = parse_and_select("<menu><li>a</li></menu>", "menu");
        assert_role(compute_role(&doc, menu), "list", RoleSource::Implicit);
        let (doc, li) = parse_and_select("<ul><li>a</li></ul>", "li");
        assert_role(compute_role(&doc, li), "listitem", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `nav`/`main`/`article`/
    /// `p`/`hr`/`fieldset`/`dialog`/`option`/`textarea` の暗黙 role。
    /// `form` は別途 [`aisnap_1_form_role_requires_name`] で扱う。
    #[test]
    fn aisnap_1_other_representative_elements() {
        let (doc, id) = parse_and_select("<nav>menu</nav>", "nav");
        assert_role(compute_role(&doc, id), "navigation", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<main>content</main>", "main");
        assert_role(compute_role(&doc, id), "main", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<article>post</article>", "article");
        assert_role(compute_role(&doc, id), "article", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<p>text</p>", "p");
        assert_role(compute_role(&doc, id), "paragraph", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<hr>", "hr");
        assert_role(compute_role(&doc, id), "separator", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<fieldset></fieldset>", "fieldset");
        assert_role(compute_role(&doc, id), "group", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<dialog></dialog>", "dialog");
        assert_role(compute_role(&doc, id), "dialog", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<select><option>a</option></select>", "option");
        assert_role(compute_role(&doc, id), "option", RoleSource::Implicit);

        let (doc, id) = parse_and_select("<textarea></textarea>", "textarea");
        assert_role(compute_role(&doc, id), "textbox", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `img` は `"img"`、`alt=""` は
    /// `"none"`、`alt` 省略は `"img"`。
    #[test]
    fn aisnap_1_img_roles() {
        let (doc, id) = parse_and_select(r#"<img src="a.png" alt="猫">"#, "img");
        assert_role(compute_role(&doc, id), "img", RoleSource::Implicit);

        let (doc, id) = parse_and_select(r#"<img src="a.png" alt="">"#, "img");
        assert_role(compute_role(&doc, id), "none", RoleSource::Implicit);

        let (doc, id) = parse_and_select(r#"<img src="a.png">"#, "img");
        assert_role(compute_role(&doc, id), "img", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）: `alt=""`
    /// でも `aria-label` の非空値、または `aria-labelledby` が文書内に
    /// 実在する要素を指す場合（ARIA 名のヒント）は装飾画像として `none` に
    /// せず `img` のまま扱う。
    #[test]
    fn aisnap_1_img_empty_alt_with_aria_name_stays_img() {
        let (doc, id) = parse_and_select(r#"<img src="a.png" alt="" aria-label="説明">"#, "img");
        assert_role(compute_role(&doc, id), "img", RoleSource::Implicit);

        let (doc, id) = parse_and_select(
            r#"<span id="caption">説明</span><img src="a.png" alt="" aria-labelledby="caption">"#,
            "img",
        );
        assert_role(compute_role(&doc, id), "img", RoleSource::Implicit);

        // 空白のみの ARIA 名はヒントとして扱わない（`none` のまま）。
        let (doc, id) = parse_and_select(r#"<img src="a.png" alt="" aria-label="  ">"#, "img");
        assert_role(compute_role(&doc, id), "none", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）: 参照先が
    /// 文書内に存在しない `aria-labelledby`（dangling IDREF）は名前の
    /// ヒントとして扱わず、`alt=""` の `img` は `none` のままになる
    /// （`has_name_hint` が参照先未解決でも非空値だけで名前ありと誤判定
    /// していた不具合の修正）。
    #[test]
    fn aisnap_1_img_empty_alt_with_dangling_labelledby_stays_none() {
        let (doc, id) = parse_and_select(
            r#"<img src="a.png" alt="" aria-labelledby="missing">"#,
            "img",
        );
        assert_role(compute_role(&doc, id), "none", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）: 名前の
    /// ない `form` は `form` にせず `generic`（Fallback）とする
    /// （HTML-AAM。`section` の `region` 昇格と同じ規則）。名前が
    /// `aria-label` の非空値、または `aria-labelledby` の参照先が文書内に
    /// 実在する場合のみ `form` になる。
    #[test]
    fn aisnap_1_form_role_requires_name() {
        let (doc, id) = parse_and_select("<form></form>", "form");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);

        let (doc, id) = parse_and_select(r#"<form aria-label="検索"></form>"#, "form");
        assert_role(compute_role(&doc, id), "form", RoleSource::Implicit);

        let (doc, id) = parse_and_select(
            r#"<h1 id="h1">検索</h1><form aria-labelledby="h1"></form>"#,
            "form",
        );
        assert_role(compute_role(&doc, id), "form", RoleSource::Implicit);

        // 空白のみの aria-label はヒントとして扱わない（`generic` のまま）。
        let (doc, id) = parse_and_select(r#"<form aria-label="  "></form>"#, "form");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）: 参照先が
    /// 文書内に存在しない `aria-labelledby`（dangling IDREF）を持つ `form`
    /// は名前を持たないとみなし `generic`（Fallback）のままになる。
    #[test]
    fn aisnap_1_form_dangling_labelledby_stays_generic() {
        let (doc, id) = parse_and_select(r#"<form aria-labelledby="missing"></form>"#, "form");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 明示 role（`role` 属性）が
    /// 暗黙 role より優先される。
    #[test]
    fn aisnap_1_explicit_role_wins() {
        let (doc, id) = parse_and_select(r#"<div role="button">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "button", RoleSource::Explicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 複数トークンのうち、許可リストに
    /// 最初に一致するものを採用する。
    #[test]
    fn aisnap_1_explicit_role_first_valid_token() {
        let (doc, id) = parse_and_select(r#"<div role="foo button">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "button", RoleSource::Explicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `role` 属性値の大文字小文字は
    /// 区別しない。
    #[test]
    fn aisnap_1_explicit_role_case_insensitive() {
        let (doc, id) = parse_and_select(r#"<div role="Button">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "button", RoleSource::Explicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 前後に空白を含む `role` 属性値
    /// からもトークンを抽出できる。
    #[test]
    fn aisnap_1_explicit_role_trims_whitespace() {
        let (doc, id) = parse_and_select(r#"<div role="  button  ">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "button", RoleSource::Explicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 有効なトークンが無ければ暗黙
    /// role へフォールバックする。
    #[test]
    fn aisnap_1_explicit_role_unknown_falls_back_to_implicit() {
        let (doc, id) = parse_and_select(r#"<div role="foo">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 空の `role=""` は有効なトークンが
    /// 無いため暗黙 role へ進む。
    #[test]
    fn aisnap_1_explicit_role_empty_falls_back_to_implicit() {
        let (doc, id) = parse_and_select(r#"<button role="">x</button>"#, "button");
        assert_role(compute_role(&doc, id), "button", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: abstract role（例: `landmark`）は
    /// 許可リストに含まれないため、有効な `role` 指定として扱わない。
    #[test]
    fn aisnap_1_abstract_role_is_not_accepted() {
        let (doc, id) = parse_and_select(r#"<button role="landmark">x</button>"#, "button");
        assert_role(compute_role(&doc, id), "button", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: `role` 属性は要素の暗黙 role を
    /// 上書きできる。
    #[test]
    fn aisnap_1_explicit_role_overrides_element_role() {
        let (doc, id) = parse_and_select(r##"<a href="#" role="tab">x</a>"##, "a");
        assert_role(compute_role(&doc, id), "tab", RoleSource::Explicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）:
    /// フォーカス可能な要素（ネイティブに対話的な `button`）には
    /// `role="presentation"`/`role="none"` を適用できず、暗黙 role
    /// （`button`/Implicit）へフォールバックする（presentation/none
    /// 競合解決規則。[`has_focus_or_global_aria`]）。
    #[test]
    fn aisnap_1_presentation_ignored_on_focusable_button() {
        let (doc, id) = parse_and_select(r#"<button role="presentation">x</button>"#, "button");
        assert_role(compute_role(&doc, id), "button", RoleSource::Implicit);

        let (doc, id) = parse_and_select(r#"<button role="none">x</button>"#, "button");
        assert_role(compute_role(&doc, id), "button", RoleSource::Implicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）:
    /// `tabindex` 属性を持つ要素もフォーカス可能とみなし、
    /// `role="presentation"` を無視して暗黙 role（`div` は対応表に無いため
    /// `generic`/Fallback）へフォールバックする。
    #[test]
    fn aisnap_1_presentation_ignored_on_tabindex_element() {
        let (doc, id) = parse_and_select(r#"<div tabindex="0" role="presentation">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）: グローバル
    /// ARIA 属性（`aria-label`）を持つ要素も `role="none"` を無視して
    /// 暗黙 role へフォールバックする。
    #[test]
    fn aisnap_1_none_ignored_on_element_with_global_aria_attribute() {
        let (doc, id) = parse_and_select(r#"<div role="none" aria-label="x">y</div>"#, "div");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541・PR #566 レビュー指摘）: フォーカス
    /// 可能でもグローバル ARIA 属性も持たない要素は、従来どおり
    /// `presentation`/`none` を明示 role として採用する。
    #[test]
    fn aisnap_1_presentation_accepted_without_focus_or_global_aria() {
        let (doc, id) = parse_and_select(r#"<div role="presentation">x</div>"#, "div");
        assert_role(compute_role(&doc, id), "presentation", RoleSource::Explicit);

        let (doc, id) = parse_and_select(r#"<img src="a.png" alt="" role="none">"#, "img");
        assert_role(compute_role(&doc, id), "none", RoleSource::Explicit);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: テキストノードは role を持たず
    /// `None`。
    #[test]
    fn aisnap_1_text_node_has_no_role() {
        let parsed =
            parse_document("<p>text</p>", &ParseOptions::default()).expect("パースは成功する");
        let doc = parsed.document;
        let root = doc.root();
        let p = query_selector_str(&doc, root, "p")
            .expect("セレクタは解釈できる")
            .expect("p 要素が見つかる");
        let text_node = doc.first_child(p).expect("p の子にテキストノードがある");
        assert_eq!(compute_role(&doc, text_node), None);
    }

    /// AISNAP-1（TASK-11.3.1・Issue #541）: 対応表に無い要素（SVG）は
    /// `"generic"`/Fallback。
    #[test]
    fn aisnap_1_svg_is_generic_fallback() {
        let (doc, id) = parse_and_select("<svg><circle/></svg>", "svg");
        assert_role(compute_role(&doc, id), "generic", RoleSource::Fallback);
    }
}

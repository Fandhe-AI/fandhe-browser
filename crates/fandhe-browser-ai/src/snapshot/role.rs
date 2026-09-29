//! `snapshot::Node::role` フィールドの算出ロジックの骨格と代表要素
//! （`AISNAP-1`・`TASK-11.3.1`・`MS-2`・Issue #541。親は TASK-11.3・Issue #72）。
//!
//! 呼び出し文脈: `snapshot::build::build_snapshot`（TASK-11.7・Issue #76）が、
//! ツリー構築時に要素ごとへ [`compute_role`] を呼ぶ。
//!
//! # 設計上の制約（文書を走査しない）
//!
//! 本モジュールが暗黙 role を割り当てるのは、「要素自身の local name と
//! 自身の属性だけ」で決まる要素が中心である。唯一の例外は `header`/`footer`
//! （TASK-11.3.3・Issue #543）で、祖先だけを最大
//! [`MAX_SECTIONING_ANCESTOR_STEPS`] 段まで辿る（上限に届いた場合は
//! `generic`（Fallback））。祖先・子孫・ID 参照（`aria-labelledby`
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
//!    - `input`: `type` ごとの対応表（`TASK-11.3.2`・Issue #542。下記
//!      「HTML-AAM からの逸脱方針」参照）
//!    - `a`・`area`: `href` があれば `link`、無ければ `generic`
//!    - `h1`〜`h6` → `heading`
//!    - `table`・`thead`/`tbody`/`tfoot`・`tr`・`td` →
//!      `table`・`rowgroup`・`row`・`cell`
//!    - `th`: `scope="row"`（大文字小文字を区別しない）なら `rowheader`、
//!      それ以外は `columnheader`
//!    - `select`: `multiple` または `size` > 1 なら `listbox`、それ以外は
//!      `combobox`（TASK-11.3.3・Issue #543）
//!    - `header`/`footer`: sectioning 祖先が無ければ `banner`/`contentinfo`、
//!      あれば `generic`（TASK-11.3.3・Issue #543）
//!    - `ul`/`ol` → `list`、`li` → `listitem`
//!    - ドキュメントルート → `document`
//! 3. 上記以外は `generic`（Fallback）
//!
//! # HTML-AAM からの逸脱方針（`input[type]`・`TASK-11.3.2`・Issue #542）
//!
//! `input[type]` は HTML-AAM を基準とし、逸脱は次の 1 件だけである。
//!
//! - `type=password` は HTML-AAM では「対応 role なし」だが、`textbox`
//!   （Implicit）を返す。主要ブラウザと Playwright が textbox を返し、AI
//!   エージェントが入力欄を特定するには widget role が要るため。role の
//!   算出は `value` 属性を一切読まず、値の開示とは無関係である
//!
//! HTML-AAM で「対応 role なし」の `color`・`date`・`datetime-local`・
//! `month`・`time`・`week`・`file` と、非公開の `hidden` は `generic`
//! （Fallback）とする。`hidden` を要素として `Some` で返す契約を保つためで、
//! スナップショットからの除外はツリー構築側（`build_snapshot`・TASK-11.7・Issue #76。実装済み）の責務。
//! 欠落・空・未知の `type` は HTML Standard どおり Text 状態（`textbox`）で、
//! `type` の照合は ASCII 大文字小文字を区別せず前後の空白を除去しない
//! （空白を含む値はどのキーワードにも一致しない）。
//!
//! # スコープ外（後続タスクへ引き継ぐ。実装済みを装わない。REPAIR-3）
//!
//! - `list` 属性（suggestions source element）による `combobox` 化
//!   （text・email・tel・url・search が対象）: `datalist` への ID 参照の
//!   解決が要り「自身の属性だけを見る」制約に反するため未実装で、
//!   `textbox`・`searchbox` のまま返す。担当 sub-issue は未割り当て
//! - `th` の表文脈による `cell` への降格、`aside` の `complementary`:
//!   担当 sub-issue は未割り当て
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
    "mark",
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
    "aria-braillelabel",
    "aria-brailleroledescription",
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
    /// 想定（配線は `build_snapshot`・TASK-11.7・Issue #76 で実装済み）。
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
        return !input_type_is(doc, id, "hidden");
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
/// による `cell` への降格は未実装（担当 sub-issue は未割り当て）。
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

/// `input` 要素の `type` 属性が `keyword` と一致するか。ASCII 大文字小文字は
/// 区別せず、前後の空白は除去しない（HTML Standard では空白を含む値は
/// どのキーワードにも一致せず Text 状態になる。[`super::name`] の
/// `normalized_input_type` と同じ規則。共通化は TASK-11.7 以降）。
fn input_type_is(doc: &Document, id: NodeId, keyword: &str) -> bool {
    doc.attribute(id, "type")
        .is_some_and(|value| value.eq_ignore_ascii_case(keyword))
}

/// `input` 要素の暗黙 role（`AISNAP-1`・`TASK-11.3.2`・`MS-2`・Issue #542）。
///
/// `type` ごとの対応表（HTML-AAM）を引く。`button`/`submit`/`reset`/`image`
/// は `button`、`checkbox`/`radio` は同名、`range` は `slider`、`number` は
/// `spinbutton`、`search` は `searchbox`、`text`/`email`/`tel`/`url` および
/// 欠落・未知の値は `textbox`。逸脱として `password` も `textbox` を返す
/// （module doc 参照。`value` は読まない）。対応 role のない `color`・日時系・
/// `file` と `hidden` は `generic`（Fallback）。`list` 属性による `combobox`
/// 化は未対応（module doc の「スコープ外」）。
fn implicit_role_for_input(doc: &Document, id: NodeId) -> ComputedRole {
    const TABLE: &[(&str, Option<&str>)] = &[
        ("button", Some("button")),
        ("submit", Some("button")),
        ("reset", Some("button")),
        ("image", Some("button")),
        ("checkbox", Some("checkbox")),
        ("radio", Some("radio")),
        ("range", Some("slider")),
        ("number", Some("spinbutton")),
        ("search", Some("searchbox")),
        ("password", Some("textbox")),
        ("color", None),
        ("date", None),
        ("datetime-local", None),
        ("month", None),
        ("time", None),
        ("week", None),
        ("file", None),
        ("hidden", None),
    ];
    for (keyword, role) in TABLE {
        if input_type_is(doc, id, keyword) {
            return match role {
                Some(role) => ComputedRole::implicit(role),
                None => ComputedRole::fallback(),
            };
        }
    }
    // text・email・tel・url・欠落・空・未知の値は Text 状態。
    ComputedRole::implicit("textbox")
}

/// 祖先走査の最大段数（`header`/`footer` の sectioning 祖先判定用）。
///
/// 実ページの DOM の深さが 100 を超えることはまれだが、外部 HTML は極端に
/// 深くネストできる（core の既定 `max_nodes` は 1,000,000）。上限が無いと
/// ツリー全体の構築（TASK-11.7・Issue #76）で要素数 × 深さの O(N²) になる。
/// 定数で打ち切ることで 1 要素あたり O(1)・全体 O(N) に収める
/// （security.md「不安全な設計」）。
const MAX_SECTIONING_ANCESTOR_STEPS: usize = 512;

/// sectioning 祖先の判定結果（[`sectioning_scope`] の戻り値）。
enum SectioningScope {
    /// sectioning 要素または対応 role を持つ祖先が見つかった。
    Scoped,
    /// 祖先を最後まで確認し、該当する祖先が無かった。
    TopLevel,
    /// 上限 [`MAX_SECTIONING_ANCESTOR_STEPS`] までに判定できなかった。
    Undetermined,
}

/// `role` 属性の最初の有効トークン（[`KNOWN_ROLES`] に大文字小文字を区別して
/// 完全一致するもの）を返す。[`explicit_role`] と祖先の role 判定
/// （[`sectioning_scope`]）が同じトークン規則を共有するための helper。
fn first_known_role_token(doc: &Document, id: NodeId) -> Option<&'static str> {
    let value = doc.attribute(id, "role")?;
    value
        .split_ascii_whitespace()
        .find_map(|token| KNOWN_ROLES.iter().find(|known| token == **known))
        .copied()
}

/// HTML の「非負整数のパース規則」（rules for parsing non-negative integers）
/// に従い `value` を解釈する。`select` の `size` 判定
/// （[`implicit_role_for_select`]）から呼ばれる。
///
/// 先頭の ASCII 空白と `+` を読み飛ばし、続く ASCII 数字を読む（後続の
/// 非数字は無視）。桁あふれは飽和させ、panic しない。`-0` は 0、それ以外の
/// 負数と数字で始まらない値は `None`。
fn parse_non_negative_integer(value: &str) -> Option<u64> {
    let mut chars = value
        .trim_start_matches(|c: char| c.is_ascii_whitespace())
        .chars()
        .peekable();
    let negative = match chars.peek() {
        Some('-') => {
            chars.next();
            true
        }
        Some('+') => {
            chars.next();
            false
        }
        _ => false,
    };
    let mut result: Option<u64> = None;
    for c in chars {
        if !c.is_ascii_digit() {
            break;
        }
        let digit = u64::from(c.to_digit(10).unwrap_or(0));
        result = Some(result.unwrap_or(0).saturating_mul(10).saturating_add(digit));
    }
    match result {
        Some(0) => Some(0),
        Some(_) if negative => None,
        other => other,
    }
}

/// `select` の暗黙 role（`AISNAP-1`・TASK-11.3.3・Issue #543）。`multiple`
/// があるか、`size` が 1 より大きければ `listbox`、それ以外は `combobox`。
/// 自身の属性しか読まない。
fn implicit_role_for_select(doc: &Document, id: NodeId) -> ComputedRole {
    let multiple = doc.attribute(id, "multiple").is_some();
    let large_size = doc
        .attribute(id, "size")
        .and_then(parse_non_negative_integer)
        .is_some_and(|size| size > 1);
    if multiple || large_size {
        ComputedRole::implicit("listbox")
    } else {
        ComputedRole::implicit("combobox")
    }
}

/// `id` の祖先に sectioning 要素（`article`/`aside`/`main`/`nav`/`section`）
/// または対応 role（`article`/`complementary`/`main`/`navigation`/`region`）
/// を持つ要素があるかを、最大 [`MAX_SECTIONING_ANCESTOR_STEPS`] 段まで調べる。
fn sectioning_scope(doc: &Document, id: NodeId) -> SectioningScope {
    const SECTIONING: &[&str] = &["article", "aside", "main", "nav", "section"];
    const SECTIONING_ROLES: &[&str] = &["article", "complementary", "main", "navigation", "region"];
    let mut ancestors = doc.ancestors(id);
    for ancestor in ancestors.by_ref().take(MAX_SECTIONING_ANCESTOR_STEPS) {
        if !doc.is_element(ancestor) {
            continue;
        }
        if SECTIONING
            .iter()
            .any(|name| is_html_element_named(doc, ancestor, name))
            || first_known_role_token(doc, ancestor)
                .is_some_and(|role| SECTIONING_ROLES.contains(&role))
        {
            return SectioningScope::Scoped;
        }
    }
    if ancestors.next().is_some() {
        SectioningScope::Undetermined
    } else {
        SectioningScope::TopLevel
    }
}

/// `header`/`footer` の暗黙 role（`AISNAP-1`・TASK-11.3.3・Issue #543）。
/// sectioning 祖先が無ければ `landmark`（`banner`/`contentinfo`）、あれば
/// `generic`（Implicit）。上限までに判定できなければ、ランドマークを誤って
/// 名乗らないよう `generic`（Fallback）を返す。
fn implicit_role_for_header_or_footer(
    doc: &Document,
    id: NodeId,
    landmark: &'static str,
) -> ComputedRole {
    match sectioning_scope(doc, id) {
        SectioningScope::TopLevel => ComputedRole::implicit(landmark),
        SectioningScope::Scoped => ComputedRole::implicit("generic"),
        SectioningScope::Undetermined => ComputedRole::fallback(),
    }
}

/// 文脈（祖先・属性値）で role が変わる要素（`select`・`header`・`footer`）
/// の暗黙 role（`AISNAP-1`・TASK-11.3.3・Issue #543）。
///
/// [`implicit_role`] から呼ばれ、local name で `select`（combobox/listbox）・
/// `header`（banner）・`footer`（contentinfo）へ振り分ける。
fn implicit_role_for_contextual(doc: &Document, id: NodeId) -> ComputedRole {
    let Some(local) = doc.local_name(id) else {
        return ComputedRole::fallback();
    };
    match local.to_ascii_lowercase().as_str() {
        "select" => implicit_role_for_select(doc, id),
        "header" => implicit_role_for_header_or_footer(doc, id, "banner"),
        "footer" => implicit_role_for_header_or_footer(doc, id, "contentinfo"),
        _ => ComputedRole::fallback(),
    }
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
/// 区別して完全一致する最初のトークンを採用する（WAI-ARIA の role 値は
/// 大文字小文字を区別するため `Button` は無効トークンとして読み飛ばす）。採用トークンが
/// `none`/`presentation` で競合解決により無視される場合は `None`
/// （暗黙 role へ）を返し、次のトークンは試さない。`role` 属性は名前空間を
/// 問わず見る。
fn explicit_role(doc: &Document, id: NodeId) -> Option<ComputedRole> {
    let known = first_known_role_token(doc, id)?;
    if (known == "none" || known == "presentation") && ignores_presentational_role(doc, id) {
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
        // role 値は大文字小文字を区別する。`Button` は無効で次トークンへ委ねる。
        check(
            r#"<div role="Button link">x</div>"#,
            "div",
            "link",
            RoleSource::Explicit,
        );
        check(
            r#"<div role="Button">x</div>"#,
            "div",
            "generic",
            RoleSource::Fallback,
        );
        check(
            r#"<div role="mark">x</div>"#,
            "div",
            "mark",
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
        // ARIA 1.2 の点字系グローバル属性も競合解決の対象になる
        check(
            r#"<div role="presentation" aria-braillelabel="x">x</div>"#,
            "div",
            "generic",
            RoleSource::Fallback,
        );
        check(
            r#"<div role="none" aria-brailleroledescription="x">x</div>"#,
            "div",
            "generic",
            RoleSource::Fallback,
        );
    }

    /// AISNAP-1（TASK-11.3.2・Issue #542）: 全 type キーワードの対応表。
    #[test]
    fn aisnap_1_input_type_table() {
        use RoleSource::{Fallback, Implicit};
        let cases = [
            ("hidden", "generic", Fallback),
            ("text", "textbox", Implicit),
            ("search", "searchbox", Implicit),
            ("tel", "textbox", Implicit),
            ("url", "textbox", Implicit),
            ("email", "textbox", Implicit),
            ("password", "textbox", Implicit),
            ("date", "generic", Fallback),
            ("month", "generic", Fallback),
            ("week", "generic", Fallback),
            ("time", "generic", Fallback),
            ("datetime-local", "generic", Fallback),
            ("number", "spinbutton", Implicit),
            ("range", "slider", Implicit),
            ("color", "generic", Fallback),
            ("checkbox", "checkbox", Implicit),
            ("radio", "radio", Implicit),
            ("file", "generic", Fallback),
            ("submit", "button", Implicit),
            ("image", "button", Implicit),
            ("reset", "button", Implicit),
            ("button", "button", Implicit),
        ];
        for (ty, expected, source) in cases {
            check(
                &format!(r#"<input type="{ty}">"#),
                "input",
                expected,
                source,
            );
        }
    }

    /// AISNAP-1（TASK-11.3.2・Issue #542）: password は HTML-AAM からの逸脱で
    /// textbox（Implicit）。
    #[test]
    fn aisnap_1_input_password_is_textbox() {
        check(
            r#"<input type="password">"#,
            "input",
            "textbox",
            RoleSource::Implicit,
        );
    }

    /// AISNAP-1（TASK-11.3.2・Issue #542）: type の正規化（欠落・未知・大文字
    /// 小文字・空白は除去しない）。
    #[test]
    fn aisnap_1_input_type_normalization() {
        let cases = [
            ("<input>", "textbox"),
            (r#"<input type="">"#, "textbox"),
            (r#"<input type="unknown-type">"#, "textbox"),
            (r#"<input type="datetime">"#, "textbox"),
            (r#"<input type="CheckBox">"#, "checkbox"),
            (r#"<input type="PASSWORD">"#, "textbox"),
            (r#"<input type=" checkbox ">"#, "textbox"),
            (r#"<input type=" submit ">"#, "textbox"),
            (r#"<input type=" hidden ">"#, "textbox"),
        ];
        for (html, expected) in cases {
            check(html, "input", expected, RoleSource::Implicit);
        }
    }

    /// AISNAP-1（TASK-11.3.2・Issue #542）: `list` 属性は解決しない。
    /// combobox 化は後続タスクで差し替える（現状は textbox・searchbox）。
    #[test]
    fn aisnap_1_input_list_attribute_not_resolved() {
        let dl = r#"<datalist id="d"></datalist>"#;
        check(
            &format!(r#"<input type="text" list="d">{dl}"#),
            "input",
            "textbox",
            RoleSource::Implicit,
        );
        check(
            &format!(r#"<input type="search" list="d">{dl}"#),
            "input",
            "searchbox",
            RoleSource::Implicit,
        );
    }

    /// AISNAP-1（TASK-11.3.2・Issue #542）: 明示 role は input でも優先。
    #[test]
    fn aisnap_1_input_explicit_role_wins() {
        check(
            r#"<input type="checkbox" role="switch">"#,
            "input",
            "switch",
            RoleSource::Explicit,
        );
    }

    /// AISNAP-1（TASK-11.3.2・Issue #542）: input の none/presentation 競合解決。
    /// `type=" hidden "` は Text 状態でフォーカス可能なので none を無視する。
    #[test]
    fn aisnap_1_input_presentational_conflict() {
        check(
            r#"<input type="text" role="none">"#,
            "input",
            "textbox",
            RoleSource::Implicit,
        );
        check(
            r#"<input type="hidden" role="none">"#,
            "input",
            "none",
            RoleSource::Explicit,
        );
        check(
            r#"<input type=" hidden " role="none">"#,
            "input",
            "textbox",
            RoleSource::Implicit,
        );
    }

    /// AISNAP-1（TASK-11.3.3・Issue #543）: 非負整数パースの具体値。
    #[test]
    fn aisnap_1_parse_non_negative_integer_values() {
        let huge = "9".repeat(30);
        let cases: [(&str, Option<u64>); 12] = [
            ("0", Some(0)),
            ("2", Some(2)),
            ("-0", Some(0)),
            ("-1", None),
            ("", None),
            ("abc", None),
            ("+2", Some(2)),
            (" \t\n2", Some(2)),
            ("2px", Some(2)),
            ("\u{000B}2", None),
            ("\u{3000}2", None),
            (huge.as_str(), Some(u64::MAX)),
        ];
        for (input, expected) in cases {
            assert_eq!(
                super::parse_non_negative_integer(input),
                expected,
                "{input:?}"
            );
        }
    }

    /// AISNAP-1（TASK-11.3.3・Issue #543）: select の multiple/size。
    #[test]
    fn aisnap_1_select_role() {
        let implicit = RoleSource::Implicit;
        check(
            "<select><option>a</option></select>",
            "select",
            "combobox",
            implicit,
        );
        check("<select multiple></select>", "select", "listbox", implicit);
        check(
            r#"<select multiple="false"></select>"#,
            "select",
            "listbox",
            implicit,
        );
        check(
            r#"<select multiple size="1"></select>"#,
            "select",
            "listbox",
            implicit,
        );
        let huge = "9".repeat(30);
        let combobox = [
            "",
            "abc",
            "-1",
            "-0",
            "0",
            "1",
            "\u{3000}2",
            "\u{0662}",
            "\u{000B}2",
        ];
        for size in combobox {
            let html = format!(r#"<select size="{size}"></select>"#);
            check(&html, "select", "combobox", implicit);
        }
        let listbox = ["2", " 2", "\t\n2", "+2", "0002", "2px", huge.as_str()];
        for size in listbox {
            let html = format!(r#"<select size="{size}"></select>"#);
            check(&html, "select", "listbox", implicit);
        }
        check(
            r#"<select role="none"></select>"#,
            "select",
            "combobox",
            implicit,
        );
        check(
            r#"<select multiple role="none"></select>"#,
            "select",
            "listbox",
            implicit,
        );
        check(
            r#"<select role="menu"></select>"#,
            "select",
            "menu",
            RoleSource::Explicit,
        );
    }

    /// AISNAP-1（TASK-11.3.3・Issue #543）: header/footer は sectioning 祖先
    /// が無ければ banner/contentinfo。
    #[test]
    fn aisnap_1_header_footer_top_level() {
        let implicit = RoleSource::Implicit;
        check("<header>h</header>", "header", "banner", implicit);
        check("<footer>f</footer>", "footer", "contentinfo", implicit);
        for wrapper in [
            "<div>{}</div>",
            "<form>{}</form>",
            r#"<div role="banner">{}</div>"#,
            r#"<div role="Region">{}</div>"#,
            "<header>{}</header>",
        ] {
            let inner = wrapper.replace("{}", "<footer>f</footer>");
            check(&inner, "footer", "contentinfo", implicit);
        }
        check(
            r#"<div role="Region"><header>h</header></div>"#,
            "header",
            "banner",
            implicit,
        );
    }

    /// AISNAP-1（TASK-11.3.3・Issue #543）: sectioning 祖先（要素・role）の
    /// 下では generic（Implicit）。
    #[test]
    fn aisnap_1_header_footer_scoped() {
        let implicit = RoleSource::Implicit;
        let wrappers = [
            "<article>{}</article>",
            "<aside>{}</aside>",
            "<main>{}</main>",
            "<nav>{}</nav>",
            "<section>{}</section>",
            r#"<div role="article">{}</div>"#,
            r#"<div role="complementary">{}</div>"#,
            r#"<div role="main">{}</div>"#,
            r#"<div role="navigation">{}</div>"#,
            r#"<div role="region">{}</div>"#,
            r#"<div role="foo region">{}</div>"#,
            "<section><div><div>{}</div></div></section>",
        ];
        for wrapper in wrappers {
            check(
                &wrapper.replace("{}", "<header>h</header>"),
                "header",
                "generic",
                implicit,
            );
            check(
                &wrapper.replace("{}", "<footer>f</footer>"),
                "footer",
                "generic",
                implicit,
            );
        }
        check(
            r#"<header role="navigation">h</header>"#,
            "header",
            "navigation",
            RoleSource::Explicit,
        );
    }

    /// AISNAP-1（TASK-11.3.3・Issue #543）: 祖先走査の上限。段数は
    /// html・body を含む祖先の数（div の数 + 2、section を含めるなら +1）。
    #[test]
    fn aisnap_1_header_ancestor_limit() {
        let nest = |divs: usize, section: bool| {
            let open = "<div>".repeat(divs);
            let close = "</div>".repeat(divs);
            let (so, sc) = if section {
                ("<section>", "</section>")
            } else {
                ("", "")
            };
            format!("{so}{open}<header>h</header>{close}{sc}")
        };
        // 祖先は div + body + html + Document ルートで、上限内に収まる。
        let within = super::MAX_SECTIONING_ANCESTOR_STEPS - 4;
        check(
            &nest(within, false),
            "header",
            "banner",
            RoleSource::Implicit,
        );
        check(
            &nest(within - 1, true),
            "header",
            "generic",
            RoleSource::Implicit,
        );
        // 上限を超える深さの外側に section があっても判定できない。
        let beyond = super::MAX_SECTIONING_ANCESTOR_STEPS + 10;
        check(
            &nest(beyond, true),
            "header",
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

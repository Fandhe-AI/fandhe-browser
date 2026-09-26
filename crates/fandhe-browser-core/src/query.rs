//! query: `dom` が保持する木構造に対する CSS セレクタ問い合わせを担うモジュール。
//!
//! `dom` モジュール（TASK-24.5・#39）の走査 API と `selector` モジュール
//! （TASK-24.7・#41）が解析する `SelectorList` を組み合わせ、
//! `Element.matches()` / `querySelector` / `querySelectorAll` 相当の API を
//! 提供する（TASK-24（24.10）・ビヘイビア `CORE-1`・Issue #418）。`cdp`
//! （`DOM.querySelector` 系ハンドラ）・`ai`（簡約 DOM 抽出）から呼ばれる想定で、
//! 戻り値は要素の要約（tag・text・attrs 等）ではなく [`crate::dom::NodeId`] に
//! 留める。呼び出し側が `dom` のアクセサ（`local_name`・`attributes`・
//! `text_content`）で必要な要約を組み立てる（REPAIR-4: 戻り値は将来拡張できる
//! 構造にする。PoC-2 の `ElementInfo` 相当の高レベル helper は本モジュールの
//! スコープ外。dom.rs モジュール doc の方針と揃える）。
//!
//! # scope の意味論
//!
//! 候補は [`Document::descendants`]`(scope)` のうち [`Document::is_element`]
//! を満たすノードのみとし、`scope` 自身は候補に含めない（DOM の
//! `querySelectorAll` と同じ）。`descendants` はもともと `<template>` の
//! template contents を辿らないため、template contents 配下を検索したい
//! 場合は呼び出し側が [`Document::template_contents`] を `scope` に渡す
//! （`DocumentFragment` も `scope` にできる）。
//!
//! 結合子（子孫・子）の祖先照合は `scope` より上の祖先まで辿る。これは DOM
//! 仕様どおりの挙動で `:scope` 相対ではない（例: `scope` が `<span>` の親
//! `<p>` でも、`div span` は `<p>` の外側にある `<div>` によって一致し得る）。
//!
//! `scope` が範囲外の [`crate::dom::NodeId`] やテキストノードの場合は、
//! 候補となる子孫要素が存在しないため空の結果になる（panic しない）。
//! 文書全体を対象にする場合は [`Document::root`] を `scope` に渡す。
//!
//! # 照合規則
//!
//! - 要素以外のノードは常に不一致（[`Document::is_element`] で判定）。
//! - 型名（[`crate::selector::CompoundSelector::type_name`]）: HTML 名前空間
//!   の要素なら [`crate::selector::html_local_name_eq`]（ASCII 大文字小文字
//!   を無視）、それ以外（SVG・MathML 等）は完全一致（`==`）で比較する
//!   （`selector` モジュール doc の照合契約と `dom::Document::attribute` の
//!   規則に揃える）。HTML 名前空間かどうかは `dom::Document::namespace_url`
//!   が返す名前空間 URI で判定する。
//! - ID・クラス: [`Document::attribute`]・[`Document::class_names`] を使う。
//!   `dom::Document::quirks_mode` が `Quirks` の場合、Selectors 仕様に従い
//!   ASCII 大文字小文字を無視して比較する。`NoQuirks`/`LimitedQuirks` では
//!   完全一致にする。
//! - 属性（[`crate::selector::AttributeSelector`]）: [`Document::attribute`]
//!   を使う（名前空間なしの属性に限定。HTML 名前空間の要素なら属性名は
//!   大文字小文字を無視して照合する。`Document::attribute` の契約どおり）。
//!   `Exists` は値の有無、`Equals` は値の完全一致（大文字小文字を区別する）
//!   で判定する。HTML 仕様の「値を大文字小文字無視で比較する属性」
//!   （`type`・`lang` 等）への対応は本モジュールのスコープ外（REPAIR-3:
//!   実装済みを装わない。必要になった段階で別 Issue で拡張する）。
//!
//! # 文書順・重複排除
//!
//! [`Document::descendants`] を 1 回だけ走査し、各要素がリスト中いずれかの
//! `ComplexSelector` に一致するかを判定する。この方式により文書順の保持と
//! 重複排除が同時に満たされる（arena のインデックスは foster parenting・
//! adoption agency により文書順と一致しないため、セレクタごとに結果を集めて
//! マージ・ソートする方式は採らない）。
//!
//! # 計算量・DoS 対策
//!
//! 複雑セレクタの照合は右から左へバックトラックしながら行う（右端の複合
//! セレクタから照合し、子結合子は親を辿り、子孫結合子は祖先を順に試す）。
//! 再帰は複合セレクタのインデックス方向のみに限り（最大
//! [`crate::selector::MAX_COMPOUNDS_PER_COMPLEX`] 段）、祖先・子孫方向は
//! [`Document::ancestors`]・[`Document::descendants`] のイテレータで走査する
//! （`node_count` で打ち切られるため、壊れた arena・深いネストでもスタック
//! オーバーフローせず必ず停止する。security.md「不安全な設計」対策）。
//! 最悪計算量は O(候補要素数 × 深さ × 複合セレクタ数) で、候補要素数は
//! `ParseOptions::max_nodes` に、複合セレクタ数は `selector` モジュールの
//! 上限（[`crate::selector::MAX_SELECTORS_PER_LIST`] 等）に、それぞれ縛られる。
//! bloom filter・top-down DP 等による高速化は本モジュールのスコープ外
//! （REPAIR-3: 将来課題として明記する）。
//!
//! 対応 ID: `CORE-1`・TASK-24（24.10）・MS-1。

use crate::dom::{Document, NodeId, QuirksMode};
use crate::error::Result;
use crate::selector::{
    AttributeMatcher, Combinator, ComplexSelector, CompoundSelector, SelectorList, SimpleSelector,
    html_local_name_eq, parse_selector_list,
};

/// `dom::Document::HTML_NAMESPACE_URI` 相当の値（`dom` モジュールに
/// `pub(crate)` として定義済みのものを再利用する）。
///
/// crate 内の単一の真実源を保つため、ここで独自に定数を再定義せず
/// `dom` の関数（[`Document::namespace_url`]）が返す値をそのまま比較する。
fn is_html_element(document: &Document, element: NodeId) -> bool {
    document.namespace_url(element) == Some(crate::dom::HTML_NAMESPACE_URI)
}

/// `element` が `compound` に一致するかを判定する（型名・ID・クラス・属性）。
///
/// 呼び出し前提: `element` は [`Document::is_element`] を満たすノードである
/// こと（呼び出し元の [`complex_matches`] がすでに検証済み）。
fn compound_matches(document: &Document, element: NodeId, compound: &CompoundSelector) -> bool {
    let is_html = is_html_element(document, element);

    if let Some(type_name) = compound.type_name() {
        let matches_type = match document.local_name(element) {
            Some(local_name) if is_html => html_local_name_eq(local_name, type_name),
            Some(local_name) => local_name == type_name,
            None => false,
        };
        if !matches_type {
            return false;
        }
    }

    compound
        .simple_selectors()
        .iter()
        .all(|simple| simple_matches(document, element, simple))
}

/// `element` が単一の [`SimpleSelector`] に一致するかを判定する。
fn simple_matches(document: &Document, element: NodeId, simple: &SimpleSelector) -> bool {
    match simple {
        SimpleSelector::Id(value) => document
            .attribute(element, "id")
            .is_some_and(|actual| id_or_class_eq(document, actual, value)),
        SimpleSelector::Class(value) => document
            .class_names(element)
            .any(|actual| id_or_class_eq(document, actual, value)),
        SimpleSelector::Attribute(attr) => match_attribute(document, element, attr),
        // `SimpleSelector` は `#[non_exhaustive]`（selector.rs）だが、本
        // モジュールは crate 内でその全 variant を網羅する義務を負う
        // （coding-rust.md: crate 内の match はワイルドカード腕なしで書き、
        // 将来 variant 追加時に更新漏れをコンパイルエラーで検出させる）。
        // `non_exhaustive` は crate 外にしか効かないため、ここではワイルド
        // カード腕を書かない。
    }
}

/// ID・クラスの値比較。`quirks_mode` が `Quirks` の場合のみ ASCII
/// 大文字小文字を無視する（Selectors 仕様の quirks mode 挙動）。
fn id_or_class_eq(document: &Document, actual: &str, expected: &str) -> bool {
    if document.quirks_mode() == QuirksMode::Quirks {
        actual.eq_ignore_ascii_case(expected)
    } else {
        actual == expected
    }
}

/// 属性セレクタの照合。`Document::attribute` が要素自身の名前空間から
/// HTML 名前空間かどうかを判定し、大文字小文字の扱いを決める契約になって
/// いるため、本関数側で `is_html` を別途受け取る必要はない。
fn match_attribute(
    document: &Document,
    element: NodeId,
    attr: &crate::selector::AttributeSelector,
) -> bool {
    match &attr.matcher {
        AttributeMatcher::Exists => document.attribute(element, &attr.name).is_some(),
        AttributeMatcher::Equals(expected) => {
            document.attribute(element, &attr.name) == Some(expected.as_str())
        }
    }
}

/// `element` が `complex` に一致するかを、右端の複合セレクタから左へ
/// バックトラックしながら判定する。
///
/// 呼び出し前提: `element` は [`Document::is_element`] を満たすノードである
/// こと。
fn complex_matches(document: &Document, element: NodeId, complex: &ComplexSelector) -> bool {
    // `rest` の最後の添字を「次に照合する複合セレクタ」として
    // `matches_compound_chain` に渡す。`rest` が空（`checked_sub` が `None`）
    // なら `first` 自身が右端になる。
    let up_to = complex.rest().len().checked_sub(1);
    matches_compound_chain(document, element, complex, up_to)
}

/// `node` が、`up_to`（`rest` の添字。`None` なら `first`）が指す複合セレクタ
/// から左側（`first` 方向）にかけての連鎖全体に一致するかを判定する。
///
/// 1. `up_to` が指す複合セレクタを `node` 自身に照合する。
/// 2. 一致すれば、その複合セレクタに紐づく結合子（`rest[up_to].0`。`up_to`
///    が `None`＝`first` の場合はこれ以上左がないため即座に一致で終える）に
///    従って左隣（子結合子）または祖先（子孫結合子）へ進み、1 つ手前の
///    複合セレクタ（`up_to - 1`。`0` なら `first`）との照合を再帰する。
///
/// 再帰は `up_to` を単調に減らすだけなので、深さは高々
/// `complex.rest().len()`（≤ [`crate::selector::MAX_COMPOUNDS_PER_COMPLEX`]）
/// に収まり、文書の深さには比例しない（祖先方向は
/// [`Document::ancestors`] のイテレータで走査するため、こちらも文書の深さ分
/// の再帰を積まない）。
fn matches_compound_chain(
    document: &Document,
    node: NodeId,
    complex: &ComplexSelector,
    up_to: Option<usize>,
) -> bool {
    let Some(index) = up_to else {
        return compound_matches(document, node, complex.first());
    };

    // 外部入力（セレクタ文字列）から構築した `rest` への添字アクセスを
    // 避け、`get` で範囲外を検出する（`index` は常に呼び出し元が
    // `rest.len()` 未満の値として渡すため通常は到達しないが、添字アクセス
    // `[]` を使わない方針を徹底する）。
    let Some((combinator, compound)) = complex.rest().get(index) else {
        return false;
    };
    if !compound_matches(document, node, compound) {
        return false;
    }

    let next_up_to = index.checked_sub(1);
    match combinator {
        Combinator::Child => {
            let Some(parent) = document.parent(node) else {
                return false;
            };
            document.is_element(parent)
                && matches_compound_chain(document, parent, complex, next_up_to)
        }
        Combinator::Descendant => document
            .ancestors(node)
            .filter(|&ancestor| document.is_element(ancestor))
            .any(|ancestor| matches_compound_chain(document, ancestor, complex, next_up_to)),
    }
}

/// `element` がセレクタリストの少なくとも 1 個の [`ComplexSelector`] に
/// 一致するかを判定する。
fn list_matches(document: &Document, element: NodeId, selectors: &SelectorList) -> bool {
    selectors
        .selectors()
        .iter()
        .any(|complex| complex_matches(document, element, complex))
}

/// `element` が `selectors` に一致するかどうかを判定する（`Element.matches()`
/// 相当）。`element` が要素でない場合や範囲外の場合は `false` を返す
/// （panic しない）。
pub fn element_matches(document: &Document, element: NodeId, selectors: &SelectorList) -> bool {
    document.is_element(element) && list_matches(document, element, selectors)
}

/// `scope` の子孫要素のうち `selectors` に一致するものを、文書順・重複なしで
/// 全件返す（`querySelectorAll` 相当）。`scope` 自身は対象に含めない。
///
/// scope の意味論・照合規則はモジュール doc を参照。一致がなければ空の
/// `Vec` を返す（panic しない）。
pub fn query_selector_all(
    document: &Document,
    scope: NodeId,
    selectors: &SelectorList,
) -> Vec<NodeId> {
    document
        .descendants(scope)
        .filter(|&candidate| {
            document.is_element(candidate) && list_matches(document, candidate, selectors)
        })
        .collect()
}

/// `scope` の子孫要素のうち `selectors` に一致する最初のもの（文書順）を
/// 返す（`querySelector` 相当）。最初の一致で走査を打ち切る。
///
/// scope の意味論・照合規則はモジュール doc を参照。一致がなければ `None`
/// を返す（panic しない）。
pub fn query_selector(
    document: &Document,
    scope: NodeId,
    selectors: &SelectorList,
) -> Option<NodeId> {
    document.descendants(scope).find(|&candidate| {
        document.is_element(candidate) && list_matches(document, candidate, selectors)
    })
}

/// `selector` 文字列を [`parse_selector_list`] で解析してから
/// [`query_selector_all`] を呼ぶ薄いラッパー。
///
/// # エラー
///
/// `parse_selector_list` が返すエラー（[`crate::error::Error::InvalidInput`]・
/// [`crate::error::Error::Unsupported`]）をそのまま返す。
pub fn query_selector_all_str(
    document: &Document,
    scope: NodeId,
    selector: &str,
) -> Result<Vec<NodeId>> {
    let selectors = parse_selector_list(selector)?;
    Ok(query_selector_all(document, scope, &selectors))
}

/// `selector` 文字列を [`parse_selector_list`] で解析してから
/// [`query_selector`] を呼ぶ薄いラッパー。
///
/// # エラー
///
/// `parse_selector_list` が返すエラー（[`crate::error::Error::InvalidInput`]・
/// [`crate::error::Error::Unsupported`]）をそのまま返す。
pub fn query_selector_str(
    document: &Document,
    scope: NodeId,
    selector: &str,
) -> Result<Option<NodeId>> {
    let selectors = parse_selector_list(selector)?;
    Ok(query_selector(document, scope, &selectors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::NodeData;
    use crate::error::Error;
    use crate::parse::{ParseOptions, parse_document};

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn find_by_local_name(doc: &Document, root: NodeId, local_name: &str) -> NodeId {
        doc.descendants(root)
            .find(|&id| doc.local_name(id) == Some(local_name))
            .unwrap_or_else(|| panic!("要素 {local_name} が見つからない"))
    }

    fn selectors(input: &str) -> SelectorList {
        parse_selector_list(input).unwrap_or_else(|e| panic!("{input:?} は解析できるはず: {e}"))
    }

    fn text(doc: &Document, id: NodeId) -> String {
        doc.text_content(id).unwrap_or_default()
    }

    /// CORE-1: 型セレクタが文書順で全件一致する。
    #[test]
    fn core_1_type_selector_matches_in_document_order() {
        let doc = parse("<ul><li>a</li><li>b</li><li>c</li></ul>");
        let root = doc.root();
        let results = query_selector_all(&doc, root, &selectors("li"));
        let texts: Vec<String> = results.iter().map(|&id| text(&doc, id)).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
    }

    /// CORE-1: ID・複数クラス・属性存在・属性完全一致・複合セレクタが
    /// それぞれ期待する要素だけに一致する。
    #[test]
    fn core_1_id_class_attribute_and_compound_selectors() {
        let doc = parse(
            r#"<div id="main">
                <a class="link primary" href="/x">a</a>
                <input type="checkbox" data-x="1">
                <span class="link">not a link tag</span>
            </div>"#,
        );
        let root = doc.root();

        let by_id = query_selector_all(&doc, root, &selectors("#main"));
        assert_eq!(by_id.len(), 1);
        assert_eq!(doc.local_name(by_id[0]), Some("div"));

        let by_class = query_selector_all(&doc, root, &selectors(".link"));
        assert_eq!(by_class.len(), 2);

        let by_href = query_selector_all(&doc, root, &selectors("[href]"));
        assert_eq!(by_href.len(), 1);
        assert_eq!(doc.local_name(by_href[0]), Some("a"));

        let by_checkbox = query_selector_all(&doc, root, &selectors("[type=checkbox]"));
        assert_eq!(by_checkbox.len(), 1);
        assert_eq!(doc.local_name(by_checkbox[0]), Some("input"));

        let by_data = query_selector_all(&doc, root, &selectors("[data-x=\"1\"]"));
        assert_eq!(by_data.len(), 1);
        assert_eq!(doc.local_name(by_data[0]), Some("input"));

        let by_compound = query_selector_all(&doc, root, &selectors("a.link[href]"));
        assert_eq!(by_compound.len(), 1);
        assert_eq!(doc.local_name(by_compound[0]), Some("a"));
    }

    /// CORE-1: `div > p` は直接の子のみ、`div p` は子孫すべてに一致する
    /// （子結合子・子孫結合子の差）。
    #[test]
    fn core_1_child_vs_descendant_combinator() {
        // `<p>` の中に `<p>` を書くと、HTML5 のパース規則（新しい `<p>` 開始
        // タグは開いている `<p>` を暗黙に閉じる）により意図した入れ子構造に
        // ならないため、`<section>` で挟んで検証する。
        let doc = parse(
            "<div><p id=\"direct\"><span>a</span></p><section><p id=\"nested\">b</p></section></div>",
        );
        let root = doc.root();

        let child_matches = query_selector_all(&doc, root, &selectors("div > p"));
        assert_eq!(child_matches.len(), 1);
        assert_eq!(doc.attribute(child_matches[0], "id"), Some("direct"));

        let descendant_matches = query_selector_all(&doc, root, &selectors("div p"));
        assert_eq!(descendant_matches.len(), 2);
        let ids: Vec<Option<&str>> = descendant_matches
            .iter()
            .map(|&id| doc.attribute(id, "id"))
            .collect();
        assert_eq!(ids, vec![Some("direct"), Some("nested")]);
    }

    /// CORE-1: `.a > .b .c` は直近の `.b` では親が `.a` にならず失敗し、
    /// より外側の `.b` まで遡って一致するバックトラックを要求する。
    #[test]
    fn core_1_backtracking_across_ancestors() {
        let doc = parse(
            r#"<div class="a">
                <div class="b">
                    <div class="b">
                        <p class="c">x</p>
                    </div>
                </div>
            </div>"#,
        );
        let root = doc.root();
        let results = query_selector_all(&doc, root, &selectors(".a > .b .c"));
        assert_eq!(results.len(), 1);
        assert_eq!(doc.local_name(results[0]), Some("p"));
    }

    /// CORE-1: カンマ区切りのセレクタリストは記述順ではなく文書順で返り、
    /// 両方に一致する要素は 1 回だけ現れる（重複排除）。
    #[test]
    fn core_1_selector_list_document_order_without_duplicates() {
        let doc = parse("<h1 class=\"x\">t</h1><p class=\"x\">a</p><p>b</p>");
        let root = doc.root();
        let results = query_selector_all(&doc, root, &selectors("p, h1"));
        let names: Vec<&str> = results
            .iter()
            .filter_map(|&id| doc.local_name(id))
            .collect();
        assert_eq!(names, vec!["h1", "p", "p"]);

        let dedup = query_selector_all(&doc, root, &selectors("p, .x"));
        // <h1 class="x">・<p class="x">・<p>（class なし）が候補。
        // <p class="x"> は両方の枝に一致するが結果には 1 回のみ現れる。
        assert_eq!(dedup.len(), 3);
    }

    /// CORE-1: scope 自身は結果に含まれず、scope より上の祖先を使う結合子
    /// （`div span`）も一致する。
    #[test]
    fn core_1_scope_excludes_self_and_ancestor_combinator_reaches_outside_scope() {
        let doc = parse("<div><p><span>x</span></p></div>");
        let root = doc.root();
        let p = find_by_local_name(&doc, root, "p");

        // scope = p。`p` 自身は `p` セレクタの候補にならない。
        let self_matches = query_selector_all(&doc, p, &selectors("p"));
        assert!(self_matches.is_empty());

        // `div span` は scope（p）の外側にある `div` を祖先として辿って一致する。
        let outside_ancestor = query_selector_all(&doc, p, &selectors("div span"));
        assert_eq!(outside_ancestor.len(), 1);
        assert_eq!(doc.local_name(outside_ancestor[0]), Some("span"));
    }

    /// CORE-1: `<template>` の中身は root からの検索では返らないが、
    /// `template_contents` を scope に渡すと返る。
    #[test]
    fn core_1_template_contents_require_explicit_scope() {
        let doc = parse("<template><p class=\"in-template\">x</p></template>");
        let root = doc.root();

        let from_root = query_selector_all(&doc, root, &selectors(".in-template"));
        assert!(from_root.is_empty());

        let template = find_by_local_name(&doc, root, "template");
        let contents = doc
            .template_contents(template)
            .expect("template contents が存在する");
        let from_contents = query_selector_all(&doc, contents, &selectors(".in-template"));
        assert_eq!(from_contents.len(), 1);
    }

    /// CORE-1: HTML 名前空間の要素・属性は大文字小文字を無視して照合する。
    #[test]
    fn core_1_html_namespace_is_case_insensitive() {
        let doc = parse(r#"<DIV HREF="/x"></DIV>"#);
        let root = doc.root();

        let by_type = query_selector_all(&doc, root, &selectors("div"));
        assert_eq!(by_type.len(), 1);

        let by_attr = query_selector_all(&doc, root, &selectors("[href]"));
        assert_eq!(by_attr.len(), 1);
    }

    /// CORE-1: SVG 名前空間の要素・属性は完全一致でのみ照合する
    /// （大文字小文字混在の名前を大文字小文字無視にしない）。
    #[test]
    fn core_1_svg_namespace_is_case_sensitive() {
        let doc = parse(r#"<svg><foreignObject viewBox="0 0 1 1"></foreignObject></svg>"#);
        let root = doc.root();

        assert_eq!(
            query_selector_all(&doc, root, &selectors("foreignObject")).len(),
            1
        );
        assert!(query_selector_all(&doc, root, &selectors("foreignobject")).is_empty());

        assert_eq!(
            query_selector_all(&doc, root, &selectors("[viewBox]")).len(),
            1
        );
        assert!(query_selector_all(&doc, root, &selectors("[viewbox]")).is_empty());
    }

    /// CORE-1: ID・クラスの大文字小文字は quirks mode でのみ無視される
    /// （NoQuirks では区別、Quirks では無視）。
    #[test]
    fn core_1_id_and_class_case_sensitivity_depends_on_quirks_mode() {
        let no_quirks = parse("<!DOCTYPE html><div id=\"main\" class=\"Item\"></div>");
        assert_eq!(no_quirks.quirks_mode(), QuirksMode::NoQuirks);
        let root = no_quirks.root();
        assert!(query_selector_all(&no_quirks, root, &selectors("#Main")).is_empty());
        assert!(query_selector_all(&no_quirks, root, &selectors(".item")).is_empty());
        assert_eq!(
            query_selector_all(&no_quirks, root, &selectors("#main")).len(),
            1
        );

        let quirks = parse("<div id=\"main\" class=\"Item\"></div>");
        assert_eq!(quirks.quirks_mode(), QuirksMode::Quirks);
        let root = quirks.root();
        assert_eq!(
            query_selector_all(&quirks, root, &selectors("#Main")).len(),
            1
        );
        assert_eq!(
            query_selector_all(&quirks, root, &selectors(".item")).len(),
            1
        );
    }

    /// CORE-1: 一致がなければ `query_selector_all` は空、`query_selector` は
    /// `None` を返す。
    #[test]
    fn core_1_no_match_returns_empty_or_none() {
        let doc = parse("<div></div>");
        let root = doc.root();
        assert!(query_selector_all(&doc, root, &selectors("span")).is_empty());
        assert_eq!(query_selector(&doc, root, &selectors("span")), None);
    }

    /// CORE-1: 範囲外の `NodeId` を scope に渡しても空になる（panic しない）。
    #[test]
    fn core_1_out_of_range_scope_returns_empty() {
        let doc = parse("<div></div>");
        let out_of_range = NodeId::new(usize::MAX);
        assert!(query_selector_all(&doc, out_of_range, &selectors("div")).is_empty());
        assert_eq!(query_selector(&doc, out_of_range, &selectors("div")), None);
    }

    /// CORE-1: テキストノードを scope に渡しても空になる（子孫の要素がない）。
    #[test]
    fn core_1_text_node_scope_returns_empty() {
        let doc = parse("<p>hello</p>");
        let root = doc.root();
        let text_node = doc
            .descendants(root)
            .find(|&id| matches!(doc.node_data(id), Some(NodeData::Text { .. })))
            .expect("テキストノードが見つかる");
        assert!(query_selector_all(&doc, text_node, &selectors("p")).is_empty());
    }

    /// CORE-1: `element_matches` は要素以外に対して `false` を返す。
    #[test]
    fn core_1_element_matches_returns_false_for_non_element() {
        let doc = parse("<p>hello</p>");
        let root = doc.root();
        let text_node = doc
            .descendants(root)
            .find(|&id| matches!(doc.node_data(id), Some(NodeData::Text { .. })))
            .expect("テキストノードが見つかる");
        assert!(!element_matches(&doc, text_node, &selectors("p")));
        assert!(!element_matches(
            &doc,
            NodeId::new(usize::MAX),
            &selectors("p")
        ));

        let p = find_by_local_name(&doc, root, "p");
        assert!(element_matches(&doc, p, &selectors("p")));
    }

    /// CORE-1: 結果は要素ノードのみで、テキスト・コメントは含まれない。
    #[test]
    fn core_1_results_contain_only_element_nodes() {
        let doc = parse("<div>text<!--comment--><p>x</p></div>");
        let root = doc.root();
        let all = query_selector_all(&doc, root, &selectors("div, p"));
        assert!(all.iter().all(|&id| doc.is_element(id)));
    }

    /// CORE-1: 深いネスト（2 万段）でも `div span` の照合が完了し、
    /// スタックオーバーフローしない（件数を具体値で確認する）。
    #[test]
    fn core_1_deeply_nested_document_does_not_overflow_stack() {
        const DEPTH: usize = 20_000;
        let mut input = String::with_capacity(DEPTH * 5 + 20);
        for _ in 0..DEPTH {
            input.push_str("<div>");
        }
        input.push_str("<span>x</span>");
        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let doc = parse_document(&input, &options)
            .expect("深いネストでも成功する")
            .document;
        let root = doc.root();

        let results = query_selector_all(&doc, root, &selectors("div span"));
        assert_eq!(results.len(), 1);
        assert_eq!(doc.local_name(results[0]), Some("span"));
    }

    /// CORE-1: `&str` 版は `parse_selector_list` のエラーをそのまま返す。
    #[test]
    fn core_1_str_variants_propagate_parse_errors() {
        let doc = parse("<div></div>");
        let root = doc.root();

        let err = query_selector_all_str(&doc, root, "a:hover")
            .expect_err("疑似クラスは Unsupported のはず");
        assert!(matches!(err, Error::Unsupported { .. }));

        let err = query_selector_str(&doc, root, "").expect_err("空文字列は InvalidInput のはず");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-1: `&str` 版は正常系で `query_selector_all`/`query_selector` と
    /// 同じ結果を返す。
    #[test]
    fn core_1_str_variants_match_typed_variants_on_success() {
        let doc = parse("<ul><li>a</li><li>b</li></ul>");
        let root = doc.root();

        let typed = query_selector_all(&doc, root, &selectors("li"));
        let from_str = query_selector_all_str(&doc, root, "li").expect("li は解析できるはず");
        assert_eq!(typed, from_str);

        let typed_first = query_selector(&doc, root, &selectors("li"));
        let from_str_first = query_selector_str(&doc, root, "li").expect("li は解析できるはず");
        assert_eq!(typed_first, from_str_first);
    }
}

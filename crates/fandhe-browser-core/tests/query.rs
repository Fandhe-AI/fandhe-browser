//! `fandhe-browser-core` の公開 query API（`query` モジュール）を crate 外から
//! 検証する結合テスト（TASK-24（24.10）・ビヘイビア `CORE-1`・Issue #418）。
//!
//! `cdp`（`DOM.querySelector` 系ハンドラ）・`ai`（簡約 DOM 抽出）はここで検証する
//! `fandhe_browser_core::{query_selector, query_selector_all, element_matches}`
//! を crate 境界を越えて呼ぶ想定であり、公開 API のみで代表的な HTML 文書
//! （記事・表・ナビゲーション・フォーム）を検索できることを確認する。

use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{
    Document, ParseOptions, element_matches, parse_document, query_selector, query_selector_all,
    query_selector_all_str, query_selector_str,
};

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

/// CORE-1: crate 外から記事構造（見出し・著者）を検索できることを確認する。
#[test]
fn core_1_query_selector_all_from_outside_crate_article() {
    let doc = parse(
        r#"<article>
            <h1 class="title">見出し</h1>
            <p class="byline">著者: Alice</p>
            <p>本文</p>
        </article>"#,
    );
    let root = doc.root();

    let title = parse_selector_list(".title").expect(".title は解析できるはず");
    let titles = query_selector_all(&doc, root, &title).expect("キャッシュ上限に達しない");
    assert_eq!(titles.len(), 1);
    assert_eq!(doc.text_content(titles[0]).as_deref(), Some("見出し"));

    let byline = parse_selector_list("article > p.byline").expect("解析できるはず");
    let first = query_selector(&doc, root, &byline)
        .expect("キャッシュ上限に達しない")
        .expect("著者行が見つかるはず");
    assert_eq!(doc.text_content(first).as_deref(), Some("著者: Alice"));
}

/// CORE-1: crate 外から表の行・セルを検索できることを確認する。
#[test]
fn core_1_query_selector_all_from_outside_crate_table() {
    let doc = parse(
        r#"<table>
            <tr><td>1</td><td>2</td></tr>
            <tr><td>3</td><td>4</td></tr>
        </table>"#,
    );
    let root = doc.root();

    let rows = parse_selector_list("tr").expect("解析できるはず");
    let row_ids = query_selector_all(&doc, root, &rows).expect("キャッシュ上限に達しない");
    assert_eq!(row_ids.len(), 2);

    let cells = parse_selector_list("td").expect("解析できるはず");
    let cell_ids = query_selector_all(&doc, root, &cells).expect("キャッシュ上限に達しない");
    let values: Vec<String> = cell_ids
        .iter()
        .map(|&id| doc.text_content(id).unwrap_or_default())
        .collect();
    assert_eq!(values, vec!["1", "2", "3", "4"]);
}

/// CORE-1: crate 外からナビゲーションリンクを検索できることを確認する
/// （`nav a[href]` の複合セレクタ + 子孫結合子）。
#[test]
fn core_1_query_selector_all_from_outside_crate_navigation() {
    let doc = parse(
        r#"<nav>
            <a href="/">Home</a>
            <a>No href</a>
            <a href="/about">About</a>
        </nav>"#,
    );
    let root = doc.root();

    let links = parse_selector_list("nav a[href]").expect("解析できるはず");
    let link_ids = query_selector_all(&doc, root, &links).expect("キャッシュ上限に達しない");
    assert_eq!(link_ids.len(), 2);
    let hrefs: Vec<Option<&str>> = link_ids
        .iter()
        .map(|&id| doc.attribute(id, "href"))
        .collect();
    assert_eq!(hrefs, vec![Some("/"), Some("/about")]);
}

/// CORE-1: crate 外からフォーム input の属性で検索できることと、
/// `element_matches`（`Element.matches()` 相当）を確認する。
#[test]
fn core_1_query_selector_and_element_matches_from_outside_crate_form() {
    let doc = parse(
        r#"<form>
            <input type="text" name="q">
            <input type="checkbox" name="agree" checked>
        </form>"#,
    );
    let root = doc.root();

    let checkbox = parse_selector_list("[type=checkbox]").expect("解析できるはず");
    let checkbox_id = query_selector(&doc, root, &checkbox)
        .expect("キャッシュ上限に達しない")
        .expect("チェックボックスが見つかる");
    assert_eq!(doc.attribute(checkbox_id, "name"), Some("agree"));

    let text_selector = parse_selector_list("input[type=text]").expect("解析できるはず");
    assert!(!element_matches(&doc, checkbox_id, &text_selector).expect("キャッシュ上限に達しない"));

    let checkbox_selector = parse_selector_list("input[type=checkbox]").expect("解析できるはず");
    assert!(
        element_matches(&doc, checkbox_id, &checkbox_selector).expect("キャッシュ上限に達しない")
    );
}

/// CORE-1: 一致がなければ `query_selector_all` は空、`query_selector` は
/// `None` を返す（crate 外からの確認）。
#[test]
fn core_1_no_match_from_outside_crate() {
    let doc = parse("<div></div>");
    let root = doc.root();
    let selectors = parse_selector_list("span").expect("解析できるはず");
    assert!(
        query_selector_all(&doc, root, &selectors)
            .expect("キャッシュ上限に達しない")
            .is_empty()
    );
    assert_eq!(
        query_selector(&doc, root, &selectors).expect("キャッシュ上限に達しない"),
        None
    );
}

/// CORE-1: 文字列版の re-export（`query_selector_str`・`query_selector_all_str`）を
/// crate 外から呼び、具体的な件数・要素・テキストが得られる
/// （両者が同じ先頭要素を指すことも確認する）。
#[test]
fn core_1_str_query_api_is_usable_from_outside_crate() {
    let doc = parse(r#"<ul><li id="a">1</li><li id="b">2</li><li id="c">3</li></ul>"#);
    let root = doc.root();

    let first = query_selector_str(&doc, root, "li")
        .expect("li は解析できる")
        .expect("li が存在する");
    assert_eq!(doc.attribute(first, "id"), Some("a"));
    assert_eq!(doc.text_content(first).as_deref(), Some("1"));

    let all = query_selector_all_str(&doc, root, "li").expect("li は解析できる");
    assert_eq!(all.len(), 3);
    assert_eq!(all.first().copied(), Some(first));
    assert_eq!(doc.attribute(all[2], "id"), Some("c"));

    assert_eq!(
        query_selector_str(&doc, root, "table").expect("table は解析できる"),
        None
    );
    assert!(
        query_selector_all_str(&doc, root, "table")
            .expect("table は解析できる")
            .is_empty()
    );
}

/// CORE-1: `:not()` の照合（emotion の `style[data-emotion]:not([data-s])`・Issue #1184）。
#[test]
fn core_1_not_pseudo_class_matching() {
    let doc = parse(
        r#"<style data-emotion="css" data-s=""></style><style data-emotion="css"></style><style></style><div class="a"></div><div class="b"></div><div></div>"#,
    );
    let root = doc.root();
    let count = |sel: &str| {
        let list = parse_selector_list(sel).expect("解析できるはず");
        query_selector_all(&doc, root, &list)
            .expect("キャッシュ上限に達しない")
            .len()
    };
    assert_eq!(count("style[data-emotion]:not([data-s])"), 1);
    assert_eq!(count("div:not(.a)"), 2);
    assert_eq!(
        count(":not(DIV):not(style):not(html):not(head):not(body)"),
        0
    );
    assert_eq!(count("div:not(.a, .b)"), 1);
    assert_eq!(count("div:not(:not(.a))"), 1);
}

/// CORE-1: 全称セレクタ `*` は要素だけに文書順・重複なしで一致する（Issue #1237・TASK-44）。
#[test]
fn core_1_universal_selector_matches_every_element_in_document_order() {
    let doc =
        parse(r#"<div id="d" class="c"><p id="p">t</p><svg id="s"><a id="sa"></a></svg></div>"#);
    let root = doc.root();
    let ids = |sel: &str| -> Vec<String> {
        query_selector_all_str(&doc, root, sel)
            .expect("解析できる")
            .into_iter()
            .map(|n| doc.attribute(n, "id").unwrap_or("-").to_string())
            .collect()
    };
    // html・head・body は id なし。テキストノードは含まれない。
    assert_eq!(ids("*"), vec!["-", "-", "-", "d", "p", "s", "sa"]);
    assert_eq!(ids("div > *"), vec!["p", "s"]);
    assert_eq!(ids("*.c"), vec!["d"]);
    assert_eq!(ids("#d *"), vec!["p", "s", "sa"]);
    assert!(ids(":not(*)").is_empty());

    let first = query_selector_str(&doc, root, "*")
        .expect("解析できる")
        .expect("要素が存在する");
    assert_eq!(doc.attribute(first, "id"), None);
    let d = query_selector_str(&doc, root, "body > *")
        .expect("解析できる")
        .expect("存在する");
    assert_eq!(doc.attribute(d, "id"), Some("d"));
}

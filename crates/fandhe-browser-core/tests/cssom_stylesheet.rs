//! `cssom::split_rule_blocks` を crate 外から検証する結合テスト（TASK-105.4.1・#551・
//! ビヘイビア `CORE-5`）。分割 → セレクタ解析 → 宣言解析 → `StyleRule` の流れ（#552 が
//! 担う組み立て）を公開 API だけで通せることを確認する。

use fandhe_browser_core::cssom::{parse_declarations, split_rule_blocks};
use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{Importance, StyleRule};

/// CORE-5: 壊れたルール・at-rule・コメントが混ざっても有効なルールが StyleRule になる。
#[test]
fn core_5_split_then_build_style_rules() {
    let css = "/* head */ @charset \"utf-8\"; } div.a { color: red; margin: 0 !important }\n\
               @media print { p { x: y } }\n{ orphan: 1 }\n#id, span { /* c */ top: 1px }";
    let blocks = split_rule_blocks(css).expect("must split");
    assert_eq!(blocks.skipped_at_rules(), 2);
    assert_eq!(blocks.errors().len(), 2);

    let rules: Vec<StyleRule> = blocks
        .blocks()
        .iter()
        .map(|b| {
            StyleRule::new(
                parse_selector_list(b.prelude()).expect("selector"),
                parse_declarations(b.body()).expect("declarations"),
            )
        })
        .collect();
    assert_eq!(rules.len(), 2);

    let first: Vec<_> = rules[0]
        .declarations()
        .iter()
        .map(|d| (d.property(), d.value(), d.importance()))
        .collect();
    assert_eq!(
        first,
        vec![
            ("color", "red", Importance::Normal),
            ("margin", "0", Importance::Important),
        ]
    );
    assert_eq!(rules[1].declarations().len(), 1);
    assert_eq!(rules[1].declarations()[0].property(), "top");
    assert_eq!(rules[1].declarations()[0].value(), "1px");
}

/// CORE-5: 未対応セレクタは分割段階ではエラーにならず、セレクタ解析が Err を返す
/// （この失敗の記録は `parse_stylesheet` が行う。#552）。
#[test]
fn core_5_unsupported_selector_fails_at_selector_parse() {
    let blocks = split_rule_blocks("a:hover { color: red }").expect("must split");
    assert!(blocks.errors().is_empty());
    assert_eq!(blocks.blocks().len(), 1);
    assert!(parse_selector_list(blocks.blocks()[0].prelude()).is_err());
}
// ---- #552: DOM からのスタイル源収集 ----

use fandhe_browser_core::cssom::{
    RuleBlockErrorKind, collect_document_styles, parse_style_attribute, parse_stylesheet,
};
use fandhe_browser_core::{Document, ParseOptions, parse_document};

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("test input must parse")
        .document
}

/// CORE-5: `<style>` 1 個からルールを具体値で構築し、ノードが `<style>` を指す。
#[test]
fn core_5_collects_style_element() {
    let doc = parse(
        "<html><head><style>p { color: red } .x { margin: 0 !important }</style></head><body></body></html>",
    );
    let styles = collect_document_styles(&doc).expect("collect");
    assert_eq!(styles.style_sheets().len(), 1);
    let sheet = &styles.style_sheets()[0];
    assert_eq!(doc.local_name(sheet.node()), Some("style"));
    let rules = sheet.parsed().stylesheet().rules();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].declarations()[0].property(), "color");
    assert_eq!(rules[0].declarations()[0].value(), "red");
    assert_eq!(
        rules[1].declarations()[0].importance(),
        Importance::Important
    );
    assert!(styles.inline_styles().is_empty());
}

/// CORE-5: 複数 `<style>` は文書順。
#[test]
fn core_5_style_elements_in_document_order() {
    let doc = parse("<head><style>a{x:1}</style></head><body><style>b{x:2}b2{x:3}</style></body>");
    let styles = collect_document_styles(&doc).expect("collect");
    let lens: Vec<_> = styles
        .style_sheets()
        .iter()
        .map(|s| s.parsed().stylesheet().len())
        .collect();
    assert_eq!(lens, vec![1, 2]);
}

/// CORE-5: `type` が text/css 以外は除外、TEXT/CSS と空は採用。
#[test]
fn core_5_style_type_attribute() {
    let doc = parse(
        "<style type=\"text/plain\">a{x:1}</style><style type=\"TEXT/CSS\">b{x:1}</style><style type=\"\">c{x:1}</style>",
    );
    let styles = collect_document_styles(&doc).expect("collect");
    assert_eq!(styles.style_sheets().len(), 2);
}

/// CORE-5: `<template>` 内・SVG 内の `<style>` は対象外。
#[test]
fn core_5_ignores_template_and_svg_style() {
    let doc = parse(
        "<template><style>a{x:1}</style></template><svg><style>b{x:1}</style></svg><style>c{x:1}</style>",
    );
    let styles = collect_document_styles(&doc).expect("collect");
    assert_eq!(styles.style_sheets().len(), 1);
}

/// CORE-5: style 属性は要素ごとの宣言列になる。空は空列、大文字属性名も拾う。
#[test]
fn core_5_collects_inline_styles() {
    let doc = parse(
        "<div style=\"color: red; margin: 0 !important\"></div><p style=\"\"></p><span STYLE=\"top:1px\"></span><i></i>",
    );
    let styles = collect_document_styles(&doc).expect("collect");
    let inl = styles.inline_styles();
    assert_eq!(inl.len(), 3);
    assert_eq!(doc.local_name(inl[0].node()), Some("div"));
    assert_eq!(inl[0].declarations().len(), 2);
    assert_eq!(inl[0].declarations()[1].importance(), Importance::Important);
    assert!(inl[1].declarations().is_empty());
    assert_eq!(inl[2].declarations()[0].value(), "1px");
}

/// CORE-5: 単一要素の style 属性取得。属性なし・要素以外は None。
#[test]
fn core_5_parse_style_attribute() {
    let doc = parse("<div style=\"a:b\"></div><p></p>");
    let div = doc
        .descendants(doc.root())
        .find(|&i| doc.local_name(i) == Some("div"))
        .expect("div");
    let p = doc
        .descendants(doc.root())
        .find(|&i| doc.local_name(i) == Some("p"))
        .expect("p");
    let got = parse_style_attribute(&doc, div).expect("ok").expect("some");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].property(), "a");
    assert!(parse_style_attribute(&doc, p).expect("ok").is_none());
    assert!(
        parse_style_attribute(&doc, doc.root())
            .expect("ok")
            .is_none()
    );
}

/// CORE-5: スタイル源が無い文書は両方空。
#[test]
fn core_5_no_style_sources() {
    let doc = parse("<p>hi</p>");
    let styles = collect_document_styles(&doc).expect("collect");
    assert!(styles.style_sheets().is_empty());
    assert!(styles.inline_styles().is_empty());
}

/// CORE-5: 壊れたルールを含む `<style>` でも有効ルールは構築され、エラーが記録される。
#[test]
fn core_5_broken_rule_in_style_element() {
    let doc = parse("<style>a:hover{x:1} p{y:2}</style>");
    let styles = collect_document_styles(&doc).expect("collect");
    let parsed = styles.style_sheets()[0].parsed();
    assert_eq!(parsed.stylesheet().len(), 1);
    assert_eq!(parsed.errors().len(), 1);
    assert_eq!(
        parsed.errors()[0].kind(),
        RuleBlockErrorKind::UnsupportedSelector
    );
}

/// CORE-5: 外部スタイルシート文字列の入口でも未対応セレクタが記録される。
#[test]
fn core_5_parse_stylesheet_records_unsupported() {
    let p = parse_stylesheet("a:hover { color: red }").expect("parse");
    assert!(p.stylesheet().is_empty());
    assert_eq!(
        p.errors()[0].kind(),
        RuleBlockErrorKind::UnsupportedSelector
    );
    assert_eq!(p.errors()[0].offset(), 0);
}

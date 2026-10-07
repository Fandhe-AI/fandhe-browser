//! CSSOM の外部入力安全性の結合テスト（TASK-105.7・#261・ビヘイビア `CORE-5`）。公開 API のみを使う。
//!
//! 不正な CSS 構文は panic せず、壊れた単位だけを捨てて `Ok` で記録する（エラー回復契約）。
//! `Err` は入力長・件数の上限違反（`Error::InvalidInput`）に限る。fixture はインラインで持ち、
//! `harness/` は実行時に読まない。

use fandhe_browser_core::cssom::{
    MAX_DECLARATION_INPUT_BYTES, MAX_DECLARATIONS_PER_BLOCK, MAX_STYLESHEET_INPUT_BYTES,
    RuleBlockErrorKind, collect_document_styles, computed_style, parse_declarations,
    parse_stylesheet, split_rule_blocks,
};
use fandhe_browser_core::{Error, ParseOptions, parse_document};

const MALFORMED: &[&str] = &[
    "",
    "a{",
    "a{b:c",
    "a{b:\"unterminated",
    "a{b:c /* unterminated",
    "/* only comment",
    "}}}",
    ")]}",
    "a{b:url(x}",
    "a{b:c))]]}}",
    "@",
    "@media",
    "@import url(x",
    "a\\",
    "a{b:\\",
    "a{b:c !",
    "a{b:!important",
    "a{;;;}",
    "{a:b}",
    "<!-- a{b:c} -->",
    "a\u{0}b{c:d}",
    "a{b:\u{1}\u{7f}}",
    "é{ü:ö}\u{1F600}{a:b}",
    "a,,b{c:d}",
    "a>{b:c}",
    "a:hover{b:c}",
];

/// CORE-5: 不正入力の網羅表と全文字境界の接頭辞が、3 つの入口で panic せず `Ok` になる。
#[test]
fn core_5_malformed_inputs_never_panic() {
    for input in MALFORMED {
        assert!(split_rule_blocks(input).is_ok(), "split {input:?}");
        assert!(parse_stylesheet(input).is_ok(), "stylesheet {input:?}");
        assert!(parse_declarations(input).is_ok(), "declarations {input:?}");
    }
    let base = "@media (a){b{c:d}} é{a:\"x\\\"y\" !important; b:url(a}b)} /* c */ }} p>q{r:s";
    for (i, _) in base
        .char_indices()
        .chain(std::iter::once((base.len(), ' ')))
    {
        let prefix = base.get(..i).expect("char boundary");
        assert!(split_rule_blocks(prefix).is_ok(), "split prefix {prefix:?}");
        assert!(parse_stylesheet(prefix).is_ok(), "sheet prefix {prefix:?}");
        assert!(parse_declarations(prefix).is_ok(), "decl prefix {prefix:?}");
    }
}

/// CORE-5: 壊れたルールは捨てて種別を記録し、有効なルールは残る（具体値）。
#[test]
fn core_5_invalid_rules_are_recorded_and_valid_rules_kept() {
    let css = "} {a:b} ok{color:red} a:hover{x:y} <!-- z{q:r} p{s:t";
    let parsed = parse_stylesheet(css).expect("must parse");
    assert_eq!(parsed.stylesheet().rules().len(), 2);
    let kinds: Vec<RuleBlockErrorKind> = parsed.errors().iter().map(|e| e.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            RuleBlockErrorKind::UnexpectedCloseBrace,
            RuleBlockErrorKind::EmptyPrelude,
            RuleBlockErrorKind::UnsupportedSelector,
            RuleBlockErrorKind::InvalidSelector,
        ]
    );
    assert_eq!(parsed.dropped_errors(), 0);
}

/// CORE-5: 巨大入力・大量宣言は確保前に `InvalidInput`（入力断片をメッセージに含めない）。
#[test]
fn core_5_oversize_inputs_are_rejected_before_allocation() {
    let css = format!("/*{}*/", "x".repeat(MAX_STYLESHEET_INPUT_BYTES));
    for r in [
        parse_stylesheet(&css).map(|_| ()),
        split_rule_blocks(&css).map(|_| ()),
    ] {
        match r {
            Err(Error::InvalidInput { message }) => assert!(!message.contains("xxx")),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
    let decls = format!("a:{}", "y".repeat(MAX_DECLARATION_INPUT_BYTES));
    assert!(matches!(
        parse_declarations(&decls),
        Err(Error::InvalidInput { .. })
    ));
    let many = "a:b;".repeat(MAX_DECLARATIONS_PER_BLOCK + 1);
    assert!(matches!(
        parse_declarations(&many),
        Err(Error::InvalidInput { .. })
    ));
    let at_limit = "a:b;".repeat(MAX_DECLARATIONS_PER_BLOCK);
    assert_eq!(
        parse_declarations(&at_limit).expect("at limit").len(),
        MAX_DECLARATIONS_PER_BLOCK
    );
}

/// CORE-5: 壊れた HTML + 壊れた CSS の文書でも、収集から全要素の computed style まで完走する。
#[test]
fn core_5_broken_document_runs_end_to_end() {
    let html = r#"<ul><li style="color: red; ;; : x; top:">a<li style="margin: 0 !">b
        <style>li { color: blue } } p{ <!-- x{y:z} li.k{top:1px}</style>
        <style type="text/css">ul{padding:0</style>"#;
    let doc = parse_document(html, &ParseOptions::default())
        .expect("must parse")
        .document;
    let styles = collect_document_styles(&doc).expect("must collect");
    assert!(styles.skipped_sources().is_empty());
    let mut lis = 0;
    for id in doc.descendants(doc.root()) {
        if !doc.is_element(id) {
            continue;
        }
        let got = computed_style(
            &doc,
            id,
            styles
                .style_sheets()
                .iter()
                .map(|s| s.parsed().stylesheet()),
        )
        .expect("must compute");
        if doc.local_name(id) == Some("li") {
            lis += 1;
            assert!(got.get("color").is_some());
        }
    }
    assert_eq!(lis, 2);
}

/// CORE-5 / #261: 上限超過の style 属性は collect と computed_style の両入口で源単位 skip になる。
#[test]
fn core_5_oversize_inline_is_skipped_consistently_in_computed_style() {
    let big = "a".repeat(MAX_DECLARATION_INPUT_BYTES + 1);
    let html = format!(r#"<p style="color:{big}">x</p>"#);
    let doc = parse_document(&html, &ParseOptions::default())
        .expect("must parse")
        .document;
    let styles = collect_document_styles(&doc).expect("must collect");
    assert_eq!(styles.skipped_sources().len(), 1);
    let p = doc
        .descendants(doc.root())
        .find(|&i| doc.local_name(i) == Some("p"))
        .expect("p");
    let got = computed_style(
        &doc,
        p,
        styles
            .style_sheets()
            .iter()
            .map(|s| s.parsed().stylesheet()),
    )
    .expect("must not fail");
    assert!(got.inline_skipped());
    assert_eq!(got.len(), 0);
}

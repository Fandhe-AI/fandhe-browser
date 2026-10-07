//! CSSOM 結合テストスイート（TASK-105.8・#262・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! 複数スタイルシート × 複数セレクタ × カスケード優先順位を `tests/fixtures/cssom/` の
//! 自作 HTML 文書で通しに検証し、computed style API の契約（値・由来）を公開 API のみで固定する。
//! 呼び出し元の想定は TASK-100 の CSSOM プロファイル。`docs/spec`・`harness/` は読まない。
//! 安全性（上限検証）は `cssom_safety.rs` が担保済みのため重複しない。

use fandhe_browser_core::cssom::{
    ComputedStyle, DocumentStyles, collect_document_styles, computed_style,
    computed_style_in_document,
};
use fandhe_browser_core::{
    DeclarationOrigin, Document, Importance, NodeId, ParseOptions, Specificity, parse_document,
};

const MULTI_SHEET: &str = include_str!("fixtures/cssom/01-multi-sheet-cascade.html");
const IMPORTANT_INLINE: &str = include_str!("fixtures/cssom/02-important-and-inline.html");
const SELECTORS_RECOVERY: &str = include_str!("fixtures/cssom/03-selectors-and-recovery.html");

fn load(html: &str) -> (Document, DocumentStyles) {
    let doc = parse_document(html, &ParseOptions::default())
        .expect("fixture must parse")
        .document;
    let styles = collect_document_styles(&doc).expect("fixture must collect");
    (doc, styles)
}

fn by_id(doc: &Document, id: &str) -> NodeId {
    doc.descendants(doc.root())
        .find(|&n| doc.attribute(n, "id") == Some(id))
        .unwrap_or_else(|| panic!("element #{id} must exist"))
}

/// 両入口（シート列 / 収集結果）の結果が一致することを確認してから返す。
fn styles_of(doc: &Document, styles: &DocumentStyles, id: &str) -> ComputedStyle {
    let node = by_id(doc, id);
    let a = computed_style(
        doc,
        node,
        styles
            .style_sheets()
            .iter()
            .map(|s| s.parsed().stylesheet()),
    )
    .expect("must compute");
    let b = computed_style_in_document(doc, node, styles).expect("must compute");
    assert_eq!(a, b, "both entry points must agree for #{id}");
    a
}

fn flat(s: &ComputedStyle) -> Vec<(&str, &str)> {
    s.declarations()
        .iter()
        .map(|d| (d.property(), d.value()))
        .collect()
}

fn rule(sheet: usize, rule: usize, ids: u32, classes: u32, types: u32) -> DeclarationOrigin {
    DeclarationOrigin::Rule {
        sheet_index: sheet,
        rule_index: rule,
        specificity: Specificity::new(ids, classes, types),
    }
}

fn origin(s: &ComputedStyle, prop: &str) -> DeclarationOrigin {
    s.get(prop)
        .unwrap_or_else(|| panic!("{prop} must exist"))
        .origin()
}

/// CORE-5: 3 シートの競合で、同詳細度は後のシートが勝ち、全体が property 昇順で一致する。
#[test]
fn core_5_later_sheet_wins_at_equal_specificity() {
    let (doc, styles) = load(MULTI_SHEET);
    assert_eq!(styles.style_sheets().len(), 3);
    let plain = styles_of(&doc, &styles, "plain");
    assert_eq!(
        flat(&plain),
        vec![("color", "green"), ("line-height", "2"), ("margin", "0")]
    );
    assert_eq!(origin(&plain, "color"), rule(1, 0, 0, 0, 1));
    assert_eq!(origin(&plain, "line-height"), rule(1, 2, 0, 0, 1));
    assert_eq!(origin(&plain, "margin"), rule(0, 0, 0, 0, 1));
}

/// CORE-5: 先のシートの高詳細度（#id）が後のシートの低詳細度に勝ち、同詳細度では後勝ち。
#[test]
fn core_5_earlier_high_specificity_beats_later_low() {
    let (doc, styles) = load(MULTI_SHEET);
    let lead = styles_of(&doc, &styles, "lead");
    assert_eq!(
        flat(&lead),
        vec![
            ("color", "blue"),
            ("font-size", "14px"),
            ("line-height", "1"),
            ("margin", "0"),
            ("padding", "2px"),
        ]
    );
    assert_eq!(origin(&lead, "line-height"), rule(0, 2, 1, 0, 0));
    assert_eq!(origin(&lead, "color"), rule(2, 0, 0, 1, 0));
    assert_eq!(origin(&lead, "font-size"), rule(2, 0, 0, 1, 0));
    assert_eq!(origin(&lead, "padding"), rule(1, 1, 0, 1, 0));
    assert_eq!(origin(&lead, "margin"), rule(0, 0, 0, 0, 1));
}

/// CORE-5: 要素ごとに結果が異なり、どのルールにも当たらない要素は空になる。
#[test]
fn core_5_elements_resolve_independently_and_unmatched_is_empty() {
    let (doc, styles) = load(MULTI_SHEET);
    let plain = styles_of(&doc, &styles, "plain");
    let lead = styles_of(&doc, &styles, "lead");
    assert_ne!(flat(&plain), flat(&lead));
    let other = styles_of(&doc, &styles, "other");
    assert_eq!(other, ComputedStyle::default());
    assert_eq!(other.len(), 0);
}

/// CORE-5: シートを跨ぐ !important が後のシートの高詳細度な通常宣言・inline 通常宣言に勝つ。
#[test]
fn core_5_important_beats_later_higher_specificity() {
    let (doc, styles) = load(IMPORTANT_INLINE);
    let b = styles_of(&doc, &styles, "b");
    let color = b.get("color").expect("color");
    assert_eq!(color.value(), "red");
    assert_eq!(color.importance(), Importance::Important);
    assert_eq!(color.origin(), rule(0, 0, 0, 0, 1));

    let a = styles_of(&doc, &styles, "a");
    assert_eq!(a.get("color").expect("color").value(), "red");
}

/// CORE-5: inline は通常宣言の #id ルールに勝ち、!important 同士では inline が勝つ。
#[test]
fn core_5_inline_versus_rules() {
    let (doc, styles) = load(IMPORTANT_INLINE);
    let a = styles_of(&doc, &styles, "a");
    let margin = a.get("margin").expect("margin");
    assert_eq!(margin.value(), "9px");
    assert_eq!(margin.origin(), DeclarationOrigin::Inline);
    assert_eq!(margin.importance(), Importance::Normal);
    assert_eq!(a.get("top").expect("top").value(), "9px");
    assert_eq!(origin(&a, "top"), DeclarationOrigin::Inline);
    let left = a.get("left").expect("left");
    assert_eq!(left.value(), "99px");
    assert_eq!(left.importance(), Importance::Important);
    assert_eq!(left.origin(), DeclarationOrigin::Inline);
    assert_eq!(a.get("width").expect("width").value(), "99px");
    assert_eq!(origin(&a, "width"), DeclarationOrigin::Inline);
}

/// CORE-5: 競合しない property が複数シート・inline からマージされ、昇順・重複なしで返る。
#[test]
fn core_5_merge_from_sheets_and_inline_sorted() {
    let (doc, styles) = load(IMPORTANT_INLINE);
    let b = styles_of(&doc, &styles, "b");
    assert_eq!(
        flat(&b),
        vec![("color", "red"), ("margin", "3px"), ("top", "5px")]
    );
    assert_eq!(origin(&b, "margin"), rule(1, 1, 0, 1, 1));
    assert_eq!(origin(&b, "top"), rule(1, 1, 0, 1, 1));
    let a = styles_of(&doc, &styles, "a");
    assert_eq!(
        flat(&a),
        vec![
            ("color", "red"),
            ("left", "99px"),
            ("margin", "9px"),
            ("top", "9px"),
            ("width", "99px"),
        ]
    );
}

/// CORE-5: セレクタリストは一致分の最大詳細度を採用し、結合子・属性セレクタの当たり外れが反映される。
#[test]
fn core_5_selector_list_max_specificity_and_combinators() {
    let (doc, styles) = load(SELECTORS_RECOVERY);
    // #deep: `span`(0,0,1) と `#x span`(1,0,1) が一致し最大の (1,0,1) を採用。
    let deep = styles_of(&doc, &styles, "deep");
    assert_eq!(origin(&deep, "color"), rule(0, 0, 1, 0, 1));
    assert_eq!(deep.get("color").expect("color").value(), "red");
    assert_eq!(deep.get("margin").expect("margin").value(), "1px");
    // `div > span.c` は p が間にあるため不一致。
    assert!(deep.get("padding").is_none());

    // #direct: `div > span`(0,0,2) より `#x span`(1,0,1) が最大。
    let direct = styles_of(&doc, &styles, "direct");
    assert_eq!(origin(&direct, "color"), rule(0, 0, 1, 0, 1));
    assert_eq!(direct.get("margin").expect("margin").value(), "1px");

    // #outside: `span` のみ一致。
    let outside = styles_of(&doc, &styles, "outside");
    assert_eq!(flat(&outside), vec![("color", "red")]);
    assert_eq!(origin(&outside, "color"), rule(0, 0, 0, 0, 1));

    // 属性セレクタ。
    let link = styles_of(&doc, &styles, "link");
    assert_eq!(flat(&link), vec![("left", "2px"), ("top", "1px")]);
    assert_eq!(origin(&link, "top"), rule(0, 3, 0, 1, 1));
    assert_eq!(origin(&link, "left"), rule(0, 4, 0, 1, 1));

    assert_eq!(styles_of(&doc, &styles, "bare"), ComputedStyle::default());
}

/// CORE-5: 壊れたルール・未対応セレクタ・@media は捨てられ、残りだけでカスケードされる。
#[test]
fn core_5_discarded_rules_do_not_affect_cascade() {
    let (doc, styles) = load(SELECTORS_RECOVERY);
    let second = styles.style_sheets().get(1).expect("second sheet").parsed();
    assert_eq!(second.errors().len(), 3);
    assert_eq!(second.skipped_at_rules(), 1);
    assert_eq!(second.stylesheet().rules().len(), 1);

    // 有効な `span.c { color: teal }` はシート 1 の rule 0（捨てたルールを詰めた添字）。
    // #deep では `#x span`(1,0,1) が span.c(0,1,1) より高詳細度なので red のまま。
    let deep = styles_of(&doc, &styles, "deep");
    assert_eq!(deep.get("color").expect("color").value(), "red");
    assert!(deep.get("width").is_none());
    assert!(flat(&deep).iter().all(|&(_, v)| v != "pink"));
    let outside = styles_of(&doc, &styles, "outside");
    assert!(flat(&outside).iter().all(|&(_, v)| v != "pink"));
}

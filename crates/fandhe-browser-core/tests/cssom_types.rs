//! `fandhe-browser-core` の公開 CSSOM 型（`cssom` モジュール）を crate 外から検証する
//! 結合テスト（TASK-105.1・#255・ビヘイビア `CORE-5`）。
//!
//! 後続の #551・#552 が行う組み立てと同じ手順（`selector::parse_selector_list` の戻り値から
//! `StyleRule`・`Stylesheet` を作る）を、公開 API だけで通せることを確認する。

use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{Declaration, Importance, Specificity, StyleRule, Stylesheet};

fn rule(selector: &str, decls: Vec<Declaration>) -> StyleRule {
    let selectors = parse_selector_list(selector).expect("selector must parse");
    StyleRule::new(selectors, decls)
}

/// CORE-5: セレクタ解析結果からルール・スタイルシートを組み立て、順序と値を保つ。
#[test]
fn core_5_assemble_stylesheet_from_parsed_selectors() {
    let sheet = Stylesheet::new(vec![
        rule(
            "div.a, #b",
            vec![
                Declaration::new("COLOR", "Red", Importance::Normal),
                Declaration::new("margin", "0", Importance::Important),
            ],
        ),
        rule(
            "p",
            vec![Declaration::new("--Main-Color", "#FFF", Importance::Normal)],
        ),
    ]);

    assert_eq!(sheet.len(), 2);
    assert!(!sheet.is_empty());

    let first = &sheet.rules()[0];
    assert_eq!(first.selectors().selectors().len(), 2);
    let decls = first.declarations();
    assert_eq!(decls.len(), 2);
    assert_eq!(decls[0].property(), "color");
    assert_eq!(decls[0].value(), "Red");
    assert_eq!(decls[0].importance(), Importance::Normal);
    assert_eq!(decls[1].property(), "margin");
    assert_eq!(decls[1].value(), "0");
    assert_eq!(decls[1].importance(), Importance::Important);

    let second = &sheet.rules()[1];
    assert_eq!(second.selectors().selectors().len(), 1);
    assert_eq!(second.declarations()[0].property(), "--Main-Color");
    assert_eq!(second.declarations()[0].value(), "#FFF");
}

/// CORE-5: 空のスタイルシートと既定値。
#[test]
fn core_5_empty_stylesheet_and_defaults() {
    let sheet = Stylesheet::default();
    assert_eq!(sheet.len(), 0);
    assert!(sheet.is_empty());
    assert!(sheet.rules().is_empty());
    assert_eq!(Importance::default(), Importance::Normal);
    assert_eq!(Specificity::default(), Specificity::ZERO);
}

/// CORE-5: 詳細度は ids → classes → types の辞書式順で並ぶ。
#[test]
fn core_5_specificity_sorts_lexicographically_from_outside() {
    let mut v = vec![
        Specificity::new(1, 0, 0),
        Specificity::new(0, 0, 1),
        Specificity::new(0, 1, 0),
    ];
    v.sort();
    assert_eq!(
        v,
        vec![
            Specificity::new(0, 0, 1),
            Specificity::new(0, 1, 0),
            Specificity::new(1, 0, 0),
        ]
    );
}

/// CORE-5: ルール同士の等価比較はセレクタと宣言の両方が一致したときのみ成り立つ。
#[test]
fn core_5_style_rule_equality_depends_on_selectors_and_declarations() {
    let d = || vec![Declaration::new("color", "red", Importance::Normal)];
    assert_eq!(rule("p", d()), rule("p", d()));
    assert_ne!(rule("p", d()), rule("a", d()));
    assert_ne!(rule("p", d()), rule("p", vec![]));
}

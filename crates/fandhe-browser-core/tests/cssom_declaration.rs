//! `cssom::parse_declarations` を crate 外から検証する結合テスト（TASK-105.2・#256・
//! ビヘイビア `CORE-5`）。宣言列の解析結果を `StyleRule` へ渡す #551 と同じ流れを、
//! 公開 API だけで通せることを確認する。

use fandhe_browser_core::cssom::parse_declarations;
use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{Importance, StyleRule};

/// CORE-5: 宣言列を解析して StyleRule に渡せ、値・順序・重要度が保たれる。
#[test]
fn core_5_parse_declarations_into_style_rule() {
    let decls = parse_declarations("COLOR: Red; margin: 0 auto !important; bogus; ")
        .expect("declarations must parse");
    let rule = StyleRule::new(parse_selector_list("div.a").expect("selector"), decls);

    let got: Vec<_> = rule
        .declarations()
        .iter()
        .map(|d| (d.property(), d.value(), d.importance()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("color", "Red", Importance::Normal),
            ("margin", "0 auto", Importance::Important),
        ]
    );
}

/// CORE-5: 空の style 属性は失敗せず空の列になる。
#[test]
fn core_5_empty_style_attribute_is_ok() {
    assert!(parse_declarations("").expect("empty").is_empty());
}

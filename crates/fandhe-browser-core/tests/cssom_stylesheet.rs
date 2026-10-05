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
/// （この失敗を記録するのは #552 の責務）。
#[test]
fn core_5_unsupported_selector_fails_at_selector_parse() {
    let blocks = split_rule_blocks("a:hover { color: red }").expect("must split");
    assert!(blocks.errors().is_empty());
    assert_eq!(blocks.blocks().len(), 1);
    assert!(parse_selector_list(blocks.blocks()[0].prelude()).is_err());
}

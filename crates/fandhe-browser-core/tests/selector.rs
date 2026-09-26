//! `fandhe-browser-core` の公開セレクタパーサー API（`selector` モジュール）を
//! crate 外から検証する結合テスト（TASK-24（24.7）・ビヘイビア `CORE-1`・
//! Issue #41）。
//!
//! `query`（TASK-24.10・Issue #418）はここで検証する
//! `fandhe_browser_core::selector::parse_selector_list` の戻り値
//! （`SelectorList`）を DOM 照合の入力として使う想定であり、crate 境界を
//! 越えて呼び出しても契約通りに動くことを確認する。

use fandhe_browser_core::Error;
use fandhe_browser_core::selector::{SimpleSelector, parse_selector_list};

/// CORE-1: crate 外から正常系（型セレクタ）を解析できることを確認する。
#[test]
fn core_1_parse_selector_list_from_outside_crate() {
    let list = parse_selector_list("div").expect("div は解析できるはず");
    assert_eq!(list.selectors().len(), 1);
    assert_eq!(list.selectors()[0].first().type_name(), Some("div"));
    assert!(list.selectors()[0].first().simple_selectors().is_empty());
    assert!(list.selectors()[0].rest().is_empty());
}

/// CORE-1: crate 外から `Unsupported` を確認できることを確認する
/// （`SimpleSelector` 等は `#[non_exhaustive]` のため `matches!` で扱う）。
#[test]
fn core_1_parse_selector_list_unsupported_from_outside_crate() {
    let err = parse_selector_list("a:hover").expect_err("疑似クラスは Unsupported のはず");
    assert!(matches!(err, Error::Unsupported { .. }));
}

/// CORE-1: crate 外から `InvalidInput` を確認できることを確認する。
#[test]
fn core_1_parse_selector_list_invalid_input_from_outside_crate() {
    let err = parse_selector_list("").expect_err("空文字列は InvalidInput のはず");
    assert!(matches!(err, Error::InvalidInput { .. }));
}

/// CORE-1: `FromStr` 経由でも crate 外から解析できることを確認する。
/// あわせて `SimpleSelector` を `matches!` で判定できることも確認する
/// （`#[non_exhaustive]` のため非網羅 `match` は書けない）。
#[test]
fn core_1_from_str_from_outside_crate() {
    use fandhe_browser_core::selector::SelectorList;
    use std::str::FromStr;

    let list = SelectorList::from_str("#main.item").expect("#main.item は解析できるはず");
    let simple_selectors = list.selectors()[0].first().simple_selectors();
    assert_eq!(simple_selectors.len(), 2);
    assert!(matches!(simple_selectors[0], SimpleSelector::Id(ref v) if v == "main"));
    assert!(matches!(simple_selectors[1], SimpleSelector::Class(ref v) if v == "item"));
}

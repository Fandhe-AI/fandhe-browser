//! `cssom::specificity` / `specificities` を公開 API 経由で検証する結合テスト
//! （TASK-105.3・#257・ビヘイビア `CORE-5`）。
//!
//! #259・#260 と同じ利用形（`parse_selector_list` → `StyleRule` → 詳細度）を通し、
//! 対応外構文が詳細度計算へ到達せず `Err` になる契約（panic しない）も固定する。

use fandhe_browser_core::cssom::{specificities, specificity};
use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{Error, Specificity, StyleRule};

/// CORE-5: 解析結果から type / class / id / 複合の詳細度が具体値で得られる。
#[test]
fn core_5_specificity_via_public_api() {
    let cases: [(&str, Specificity); 4] = [
        ("p", Specificity::new(0, 0, 1)),
        (".k", Specificity::new(0, 1, 0)),
        ("#i", Specificity::new(1, 0, 0)),
        ("div#i.k > a[href]", Specificity::new(1, 2, 2)),
    ];
    for (input, expected) in cases {
        let list = parse_selector_list(input).expect("must parse");
        assert_eq!(
            specificity(&list.selectors()[0]),
            expected,
            "input: {input}"
        );
    }
}

/// CORE-5: `StyleRule::selectors()` 経由でもリスト順に詳細度が得られる。
#[test]
fn core_5_specificities_from_style_rule() {
    let rule = StyleRule::new(parse_selector_list("#a, .b, div p").expect("parse"), vec![]);
    assert_eq!(
        specificities(rule.selectors()),
        vec![
            Specificity::new(1, 0, 0),
            Specificity::new(0, 1, 0),
            Specificity::new(0, 0, 2),
        ]
    );
}

/// CORE-5: 対応外構文は `Error::Unsupported` になり詳細度計算へ到達しない。
#[test]
fn core_5_unsupported_syntax_is_err() {
    for input in [
        "*",
        "a:hover",
        "a::before",
        "a + b",
        "a ~ b",
        "[a$=v]",
        ".a\\:b",
    ] {
        match parse_selector_list(input) {
            Err(Error::Unsupported { .. }) => {}
            other => panic!("input {input:?}: expected Unsupported, got {other:?}"),
        }
    }
}

/// CORE-5: 不正構文は `Error::InvalidInput` になる。
#[test]
fn core_5_invalid_syntax_is_err() {
    for input in ["", "#", ".", "> a"] {
        match parse_selector_list(input) {
            Err(Error::InvalidInput { .. }) => {}
            other => panic!("input {input:?}: expected InvalidInput, got {other:?}"),
        }
    }
}

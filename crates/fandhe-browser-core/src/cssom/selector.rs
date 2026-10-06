//! セレクタの詳細度（specificity）計算（TASK-105.3・#257・MS-8・ビヘイビア `CORE-5`。
//! `PLUG-8` の前提条件）。
//!
//! 役割: [`crate::selector`] が解析した AST（[`SelectorList`] / [`ComplexSelector`]）を
//! 走査し、[`Specificity`] を返すだけのモジュール。解析そのものは [`crate::selector`] の
//! 責務で、本モジュールは第 2 のパーサーを持たない（#263 の決定: `cssparser` /
//! `selectors` は MPL-2.0 のため core では使わず、`crate::selector` を共有する）。
//!
//! 呼び出し文脈: #551 が解析した `SelectorList` を持つ `StyleRule` に対し、マッチング（#259）と
//! カスケード解決（#260）が比較用の詳細度を取得するために呼ぶ。
//!
//! 数え方（CSS Selectors の詳細度）:
//!
//! | 要素 | 加算先 |
//! | ---- | ------ |
//! | `#id` | a（[`Specificity::ids`]） |
//! | `.class`・`[attr]`・`[attr=v]` | b（[`Specificity::classes`]） |
//! | 型セレクタ | c（[`Specificity::types`]） |
//! | 結合子（空白・`>`） | 数えない |
//!
//! 未実装（REPAIR-3: 実装済みを装わない）: `*`・疑似クラス・疑似要素・`:is()` / `:not()` /
//! `:where()` は [`crate::selector`] が `Error::Unsupported` にするため、AST に現れず
//! ここへ到達しない。構文対応を足す際は本モジュールの数え方も同時に拡張する（`match` は
//! 網羅で書いてあり、`SimpleSelector` への variant 追加はコンパイルエラーで気づける）。

use super::Specificity;
use crate::selector::{ComplexSelector, CompoundSelector, SelectorList, SimpleSelector};

/// 複雑セレクタ 1 個の詳細度を返す。
///
/// 解析済み AST に対する全域関数で失敗しない。加算は飽和演算で、AST の上限
/// （`MAX_COMPOUNDS_PER_COMPLEX` 等）からオーバーフローは起きないが、panic しない契約を
/// 型に頼らず守る。
pub fn specificity(selector: &ComplexSelector) -> Specificity {
    let mut counts = (0u32, 0u32, 0u32);
    add_compound(&mut counts, selector.first());
    for (_, compound) in selector.rest() {
        add_compound(&mut counts, compound);
    }
    Specificity::new(counts.0, counts.1, counts.2)
}

/// セレクタリストの各複雑セレクタの詳細度を返す。
///
/// 順序・件数は [`SelectorList::selectors`] と一致する。1 ルールは複雑セレクタごとに別の
/// 詳細度を持つため、リスト全体を 1 値へ畳まない（#259 がマッチした要素の値を選ぶ）。
pub fn specificities(list: &SelectorList) -> Vec<Specificity> {
    // 件数は `MAX_SELECTORS_PER_LIST` で上限検証済みのため、確保しても安全。
    let mut out = Vec::with_capacity(list.selectors().len());
    out.extend(list.selectors().iter().map(specificity));
    out
}

/// 複合セレクタ 1 個分を (a, b, c) へ加算する。
fn add_compound(counts: &mut (u32, u32, u32), compound: &CompoundSelector) {
    if compound.type_name().is_some() {
        counts.2 = counts.2.saturating_add(1);
    }
    for simple in compound.simple_selectors() {
        match simple {
            SimpleSelector::Id(_) => counts.0 = counts.0.saturating_add(1),
            SimpleSelector::Class(_) | SimpleSelector::Attribute(_) => {
                counts.1 = counts.1.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::{
        MAX_COMPOUNDS_PER_COMPLEX, MAX_SELECTOR_INPUT_BYTES, MAX_SIMPLE_SELECTORS_PER_COMPOUND,
        parse_selector_list,
    };

    fn spec(input: &str) -> Specificity {
        let list = parse_selector_list(input).expect("selector must parse");
        let all = specificities(&list);
        assert_eq!(all.len(), 1);
        all[0]
    }

    /// CORE-5: 代表的なセレクタの詳細度が仕様どおりの具体値になる。
    #[test]
    fn core_5_specificity_counts_components() {
        let cases: [(&str, (u32, u32, u32)); 10] = [
            ("div", (0, 0, 1)),
            (".a", (0, 1, 0)),
            ("#a", (1, 0, 0)),
            (".a.b", (0, 2, 0)),
            ("[href]", (0, 1, 0)),
            ("[type=\"text\"]", (0, 1, 0)),
            ("div#id.cls[href]", (1, 2, 1)),
            ("a > b c", (0, 0, 3)),
            ("ul li.x", (0, 1, 2)),
            ("#a #b", (2, 0, 0)),
        ];
        for (input, (a, b, c)) in cases {
            assert_eq!(spec(input), Specificity::new(a, b, c), "input: {input}");
        }
    }

    /// CORE-5: リストは複雑セレクタごとに順序・件数を保って返す。
    #[test]
    fn core_5_specificities_keep_order_and_count() {
        let list = parse_selector_list("#a, .b, div").expect("must parse");
        assert_eq!(list.selectors().len(), 3);
        assert_eq!(
            specificities(&list),
            vec![
                Specificity::new(1, 0, 0),
                Specificity::new(0, 1, 0),
                Specificity::new(0, 0, 1),
            ]
        );
    }

    /// CORE-5: 詳細度の大小は a > b > c の辞書式になる。
    #[test]
    fn core_5_specificity_ordering() {
        let many_classes = ".c".repeat(MAX_SIMPLE_SELECTORS_PER_COMPOUND);
        assert!(spec("#a") > spec(&many_classes));
        assert!(spec(".a") > spec("div div div"));
        assert_eq!(spec("#a .b"), spec(".b#a"));
    }

    /// CORE-5: 上限構成（複合 32 個 × 単純セレクタ 32 個）でも panic せず具体値になる。
    #[test]
    fn core_5_specificity_at_limits() {
        // 各複合セレクタ: 型 1 個 + クラス (上限 - 1) 個。
        let classes = MAX_SIMPLE_SELECTORS_PER_COMPOUND - 1;
        let compound = format!("a{}", ".c".repeat(classes));
        let input = vec![compound; MAX_COMPOUNDS_PER_COMPLEX].join(" ");
        assert!(
            input.len() <= MAX_SELECTOR_INPUT_BYTES,
            "len {}",
            input.len()
        );
        let expect_b = (MAX_COMPOUNDS_PER_COMPLEX * classes) as u32;
        assert_eq!(
            spec(&input),
            Specificity::new(0, expect_b, MAX_COMPOUNDS_PER_COMPLEX as u32)
        );
    }
}

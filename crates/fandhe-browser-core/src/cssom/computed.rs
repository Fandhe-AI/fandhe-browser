//! カスケード解決と computed style API（TASK-105.6・#260・MS-8・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! [`match_rules`]（セレクタマッチング。#259）の結果と要素の `style` 属性の宣言を入力に、
//! プロパティごとに勝者の宣言を 1 つ決めて要素単位の「有効宣言一覧」を返す。
//! 呼び出し元は TASK-100（CSSOM プロファイル feature gating。`PLUG-8`）と結合テスト（#262）を想定し、
//! 呼び出し先は core 内の [`match_rules`] と [`parse_style_attribute`] のみ（新規依存なし）。
//!
//! # 優先順位
//!
//! 1. inline（`style` 属性）は詳細度に関係なく author ルールより常に優先する。
//! 2. ルール同士は詳細度が高い方が勝つ。
//! 3. 同詳細度ならソース順（シート順 → ルール順 → 宣言順）で後の方が勝つ。
//!
//! # PLUG-8 との関係
//!
//! TASK-100 は `--profile chrome|safari` 指定時に非対応 property の宣言を除去する。本 API は
//! gating 用のフックを持たない。TASK-100 は (a) 呼び出し前に [`Stylesheet`] や inline 宣言を絞るか、
//! (b) 戻り値の [`ComputedStyle::declarations`] を property 名で絞る。property 名は正規化済みで、
//! 列挙順は決定的、各宣言には由来（[`DeclarationOrigin`]）が付く。
//! UA 文字列・フィンガープリント等の識別面には関与しない（`SEC-1`・`SEC-2`）。
//!
//! # 未実装（REPAIR-3: 実装済みを装わない）
//!
//! 戻り値は厳密な CSS の "computed value" ではなく「カスケード後の有効宣言一覧」である。
//!
//! - `!important` の順位付け（[`Importance`] は勝者へ素通しで保持するだけ。後続で扱いを決める）
//! - 継承・初期値・shorthand 展開・値の型付き解釈・`var()` 解決・レイアウト依存値
//! - 起源（UA / user / author）・`@layer`・`@media` 等の条件付きルール
//! - 上限の見直し（#261）。上限は上流（`parse_declarations`・`parse_stylesheet`・`match_rules`）で検証済み

use std::collections::BTreeMap;

use crate::dom::{Document, NodeId};
use crate::error::Result;

use super::{
    Declaration, Importance, MatchedRule, Specificity, Stylesheet, match_rules,
    parse_style_attribute,
};

/// 有効宣言の由来。将来の起源・レイヤ追加に備え `#[non_exhaustive]`（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeclarationOrigin {
    /// スタイルシートのルール由来。
    Rule {
        /// `match_rules` に渡したシート列内の添字。
        sheet_index: usize,
        /// シート内のルール添字。
        rule_index: usize,
        /// 勝ったルールの詳細度。
        specificity: Specificity,
    },
    /// 要素の `style` 属性由来。
    Inline,
}

/// カスケードに勝った 1 件の宣言と由来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedDeclaration {
    declaration: Declaration,
    origin: DeclarationOrigin,
}

impl ComputedDeclaration {
    /// 勝者の宣言本体。
    pub fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// 正規化済みの property 名（[`Declaration::property`] への委譲）。
    pub fn property(&self) -> &str {
        self.declaration.property()
    }

    /// 値（文字列のまま。[`Declaration::value`] への委譲）。
    pub fn value(&self) -> &str {
        self.declaration.value()
    }

    /// `!important` 指定の有無。素通しで保持するだけで順位付けには使わない。
    pub fn importance(&self) -> Importance {
        self.declaration.importance()
    }

    /// 宣言の由来。
    pub fn origin(&self) -> DeclarationOrigin {
        self.origin
    }
}

/// 要素 1 つ分のカスケード後の有効宣言一覧。property 名の昇順（バイト順）で決定的。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComputedStyle {
    declarations: Vec<ComputedDeclaration>,
}

impl ComputedStyle {
    /// property 名の昇順（バイト順）の有効宣言。同名 property は 1 件だけ。
    pub fn declarations(&self) -> &[ComputedDeclaration] {
        &self.declarations
    }

    /// property の有効宣言を引く。名前は正規化済みを期待し、ここでは小文字化しない
    /// （標準 property は ASCII 小文字、`--` カスタムプロパティは大文字小文字区別）。
    pub fn get(&self, property: &str) -> Option<&ComputedDeclaration> {
        self.declarations
            .binary_search_by(|d| d.property().cmp(property))
            .ok()
            .and_then(|i| self.declarations.get(i))
    }

    /// 有効宣言の件数。
    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    /// 有効宣言が 0 件か。
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }
}

/// 純粋なカスケード解決（DOM 非依存）。
///
/// `matched` は [`match_rules`] が返したソース順のままであること（並べ替えた列は結果未規定）。
/// 詳細度が `>=` の宣言が置き換えるため、同詳細度・同一ルール内の重複は後勝ち。
/// `inline` は詳細度に関係なく無条件で上書きする。戻り値は勝者の宣言を所有する（clone）。
/// 失敗しないので `Result` を返さない。件数は入力宣言数以下（入力は上流で上限検証済み）。
pub fn cascade(matched: &[MatchedRule<'_>], inline: &[Declaration]) -> ComputedStyle {
    let mut winners: BTreeMap<&str, (&Declaration, DeclarationOrigin)> = BTreeMap::new();
    for m in matched {
        for decl in m.rule().declarations() {
            let beats = match winners.get(decl.property()) {
                Some((_, DeclarationOrigin::Rule { specificity, .. })) => {
                    m.specificity() >= *specificity
                }
                Some(_) => false,
                None => true,
            };
            if beats {
                let origin = DeclarationOrigin::Rule {
                    sheet_index: m.sheet_index(),
                    rule_index: m.rule_index(),
                    specificity: m.specificity(),
                };
                winners.insert(decl.property(), (decl, origin));
            }
        }
    }
    for decl in inline {
        winners.insert(decl.property(), (decl, DeclarationOrigin::Inline));
    }
    ComputedStyle {
        declarations: winners
            .into_values()
            .map(|(decl, origin)| ComputedDeclaration {
                declaration: decl.clone(),
                origin,
            })
            .collect(),
    }
}

/// 要素の computed style（カスケード後の有効宣言一覧）を返す入口。
///
/// `sheets` の渡し方は [`match_rules`] と同じ。`element` が要素でなければ `Ok` の空を返す。
/// TASK-100（`PLUG-8`）はこの関数を呼ぶ想定。
///
/// # エラー
///
/// [`match_rules`] と [`parse_style_attribute`] のエラー（走査ルール総数・照合キャッシュ・
/// 宣言入力長・宣言数の上限超過）をそのまま伝播する。部分結果は返さない。
pub fn computed_style<'a>(
    document: &Document,
    element: NodeId,
    sheets: impl IntoIterator<Item = &'a Stylesheet>,
) -> Result<ComputedStyle> {
    if !document.is_element(element) {
        return Ok(ComputedStyle::default());
    }
    let matched = match_rules(document, element, sheets)?;
    let inline = parse_style_attribute(document, element)?.unwrap_or_default();
    Ok(cascade(&matched, &inline))
}

#[cfg(test)]
mod tests {
    use super::super::matcher::MAX_SCANNED_RULES;
    use super::super::parse_stylesheet;
    use super::*;
    use crate::error::Error;
    use crate::parse::{ParseOptions, parse_document};

    fn doc(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は成功する")
            .document
    }

    fn sheet(css: &str) -> Stylesheet {
        parse_stylesheet(css)
            .expect("テスト入力は成功する")
            .stylesheet()
            .clone()
    }

    fn find(doc: &Document, name: &str) -> NodeId {
        doc.descendants(doc.root())
            .find(|&id| doc.local_name(id) == Some(name))
            .unwrap_or_else(|| panic!("要素 {name} が見つからない"))
    }

    fn run(html: &str, css: &[&str]) -> ComputedStyle {
        let d = doc(html);
        let sheets: Vec<Stylesheet> = css.iter().map(|c| sheet(c)).collect();
        computed_style(&d, find(&d, "p"), sheets.iter()).expect("上限に達しない")
    }

    fn rule(
        sheet_index: usize,
        rule_index: usize,
        ids: u32,
        classes: u32,
        types: u32,
    ) -> DeclarationOrigin {
        DeclarationOrigin::Rule {
            sheet_index,
            rule_index,
            specificity: Specificity::new(ids, classes, types),
        }
    }

    /// CORE-5: 先に書かれた高詳細度が後の低詳細度に勝つ。
    #[test]
    fn core_5_higher_specificity_wins_over_later() {
        let s = run(r#"<p id="x">a</p>"#, &["#x{color:red} p{color:blue}"]);
        let c = s.get("color").expect("color がある");
        assert_eq!(c.value(), "red");
        assert_eq!(c.origin(), rule(0, 0, 1, 0, 0));
    }

    /// CORE-5: 同詳細度は同一シート内で後のルールが勝つ。
    #[test]
    fn core_5_same_specificity_later_rule_wins() {
        let s = run("<p>a</p>", &["p{color:red} p{color:blue}"]);
        let c = s.get("color").expect("color がある");
        assert_eq!(c.value(), "blue");
        assert_eq!(c.origin(), rule(0, 1, 0, 0, 1));
    }

    /// CORE-5: 同詳細度は後のシートが勝つ。
    #[test]
    fn core_5_same_specificity_later_sheet_wins() {
        let s = run("<p>a</p>", &["p{color:red}", "p{color:blue}"]);
        let c = s.get("color").expect("color がある");
        assert_eq!(c.value(), "blue");
        assert_eq!(c.origin(), rule(1, 0, 0, 0, 1));
    }

    /// CORE-5: 同一ルール内の重複宣言は後勝ち。
    #[test]
    fn core_5_duplicate_in_rule_last_wins() {
        let s = run("<p>a</p>", &["p{color:red;color:green}"]);
        assert_eq!(s.get("color").expect("color がある").value(), "green");
        assert_eq!(s.len(), 1);
    }

    /// CORE-5: inline は `#id` ルールにも勝つ。
    #[test]
    fn core_5_inline_beats_id_rule() {
        let s = run(r#"<p id="x" style="color:green">a</p>"#, &["#x{color:red}"]);
        let c = s.get("color").expect("color がある");
        assert_eq!(c.value(), "green");
        assert_eq!(c.origin(), DeclarationOrigin::Inline);
    }

    /// CORE-5: inline 内の重複は後勝ち。
    #[test]
    fn core_5_inline_duplicate_last_wins() {
        let s = run(r#"<p style="color:red;color:blue">a</p>"#, &[]);
        assert_eq!(s.get("color").expect("color がある").value(), "blue");
    }

    /// CORE-5: 競合しない property は全て残り、property 昇順。
    #[test]
    fn core_5_non_conflicting_kept_in_property_order() {
        let s = run(r#"<p style="top:1px">a</p>"#, &["p{margin:0} p{color:red}"]);
        let names: Vec<&str> = s.declarations().iter().map(|d| d.property()).collect();
        assert_eq!(names, vec!["color", "margin", "top"]);
        assert_eq!(s.len(), 3);
        assert!(!s.is_empty());
    }

    /// CORE-5: property 名の大文字小文字は標準のみ統合、カスタムは区別。
    #[test]
    fn core_5_property_name_normalization() {
        let s = run("<p>a</p>", &["p{COLOR:red;color:blue;--X:1;--x:2}"]);
        assert_eq!(s.get("color").expect("color がある").value(), "blue");
        assert_eq!(s.get("--X").expect("--X がある").value(), "1");
        assert_eq!(s.get("--x").expect("--x がある").value(), "2");
        assert_eq!(s.len(), 3);
        assert!(s.get("missing").is_none());
    }

    /// CORE-5: 空入力・空 style 属性・importance 素通し。
    #[test]
    fn core_5_empty_and_passthrough() {
        assert_eq!(run("<p>a</p>", &[]), ComputedStyle::default());
        assert_eq!(cascade(&[], &[]), ComputedStyle::default());
        let s = run(r#"<p style="">a</p>"#, &["p{color:red}"]);
        assert_eq!(s.get("color").expect("color がある").value(), "red");
        let s = run("<p>a</p>", &["p{color:red !important}"]);
        assert_eq!(
            s.get("color").expect("color がある").importance(),
            Importance::Important
        );
    }

    /// CORE-5: 非要素は `Ok` の空。
    #[test]
    fn core_5_non_element_is_empty() {
        let d = doc(r#"<p style="color:red">a</p>"#);
        let sheets = [sheet("p{color:red}")];
        let p = find(&d, "p");
        let text = d.children(p).next().expect("テキストノード");
        for id in [text, d.root(), NodeId::new(usize::MAX)] {
            let got = computed_style(&d, id, sheets.iter()).expect("Ok");
            assert_eq!(got, ComputedStyle::default());
        }
    }

    /// CORE-5: 走査ルール総数の上限超過は Err で伝播する。
    #[test]
    fn core_5_scan_limit_propagates() {
        let d = doc("<p>a</p>");
        let big = sheet(&"p{color:red}".repeat(16_384));
        let sheets: Vec<&Stylesheet> = vec![&big; MAX_SCANNED_RULES / 16_384 + 1];
        let err = computed_style(&d, find(&d, "p"), sheets).expect_err("上限超過");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }
}

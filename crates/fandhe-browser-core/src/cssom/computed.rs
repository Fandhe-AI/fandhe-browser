//! カスケード解決と computed style API（TASK-105.6・#260・MS-8・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! [`match_rules`]（セレクタマッチング。#259）の結果と要素の `style` 属性の宣言を入力に、
//! プロパティごとに勝者の宣言を 1 つ決めて要素単位の「有効宣言一覧」を返す。
//! 呼び出し元は TASK-100（CSSOM プロファイル feature gating。`PLUG-8`）と結合テスト（#262）を想定し、
//! 呼び出し先は core 内の [`match_rules`] と [`parse_style_attribute`] のみ（新規依存なし）。
//!
//! # 優先順位
//!
//! 1. `!important` 宣言は通常宣言より常に優先する（inline の通常宣言にも勝つ）。
//! 2. 同じ importance なら inline（`style` 属性）が詳細度に関係なく author ルールより優先する。
//! 3. ルール同士は詳細度が高い方が勝つ。
//! 4. 同順位ならソース順（シート順 → ルール順 → 宣言順）で後の方が勝つ。
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
//! - 起源（UA / user / author）ごとの `!important` 逆転（author のみを扱う）
//! - 継承・初期値・shorthand 展開・値の型付き解釈・`var()` 解決・レイアウト依存値
//! - 起源（UA / user / author）・`@layer`・`@media` 等の条件付きルール
//! - 上限検証は確定済み（#261）。上限は上流（`parse_declarations`・`parse_stylesheet`・`match_rules`）で検証済み

use std::collections::BTreeMap;

use crate::dom::{Document, NodeId};
use crate::error::{Error, Result};

use super::{
    Declaration, DocumentStyles, Importance, MatchedRule, Specificity, Stylesheet, match_rules,
    parse_style_attribute,
};

/// 1 回の [`computed_style`] が処理するカスケード対象宣言（マッチしたルールの宣言と inline 宣言の合計）の上限。
///
/// 上流の上限（1 ブロック 4,096 宣言・走査ルール 65,536）の積は巨大になりうるため、
/// カスケードの総量をここで別途制限する（AGENTS.md「リソース上限」。`CORE-5`）。
pub const MAX_CASCADE_DECLARATIONS: usize = 65_536;

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
    inline_skipped: bool,
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

    /// 上限違反の `style` 属性を捨てて計算したか。
    ///
    /// [`collect_document_styles`](super::collect_document_styles) が源単位で skip する契約と
    /// 揃え、`computed_style` も上限違反の inline 宣言だけを捨てて継続する（`CORE-5`・#261）。
    pub fn inline_skipped(&self) -> bool {
        self.inline_skipped
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

/// 宣言の優先キー（`!important` の有無 → inline か → 詳細度の順に大きい方が勝つ）。
type PriorityKey = (u8, u8, Specificity);

fn importance_rank(importance: Importance) -> u8 {
    match importance {
        Importance::Important => 1,
        _ => 0,
    }
}

/// 純粋なカスケード解決（DOM 非依存）。
///
/// `matched` は [`match_rules`] が返したソース順のままであること（並べ替えた列は結果未規定）。
/// 勝者は「`!important` の有無 → inline か → 詳細度」の順で大きい方で、同順位ならソース順で後勝ち。
/// したがって `!important` は inline の通常宣言にも勝ち、`!important` 同士は inline が優先する。
/// 戻り値は勝者の宣言を所有する（clone）。
///
/// # エラー
///
/// 入力宣言の総数（`matched` の各ルールの宣言と `inline` の合計）が
/// [`MAX_CASCADE_DECLARATIONS`] を超える場合は走査前に [`Error::InvalidInput`] を返す。
/// 部分結果は返さない（`CORE-5`）。
pub fn cascade(matched: &[MatchedRule<'_>], inline: &[Declaration]) -> Result<ComputedStyle> {
    let mut total = inline.len();
    for m in matched {
        total = total.saturating_add(m.rule().declarations().len());
        if total > MAX_CASCADE_DECLARATIONS {
            break;
        }
    }
    if total > MAX_CASCADE_DECLARATIONS {
        return Err(Error::InvalidInput {
            message: format!("cascade declaration count exceeds {MAX_CASCADE_DECLARATIONS}"),
        });
    }
    let mut winners: BTreeMap<&str, (&Declaration, DeclarationOrigin, PriorityKey)> =
        BTreeMap::new();
    for m in matched {
        for decl in m.rule().declarations() {
            let key: PriorityKey = (importance_rank(decl.importance()), 0, m.specificity());
            let beats = match winners.get(decl.property()) {
                Some((_, _, cur)) => key >= *cur,
                None => true,
            };
            if beats {
                let origin = DeclarationOrigin::Rule {
                    sheet_index: m.sheet_index(),
                    rule_index: m.rule_index(),
                    specificity: m.specificity(),
                };
                winners.insert(decl.property(), (decl, origin, key));
            }
        }
    }
    for decl in inline {
        let key: PriorityKey = (
            importance_rank(decl.importance()),
            1,
            Specificity::default(),
        );
        let beats = match winners.get(decl.property()) {
            Some((_, _, cur)) => key >= *cur,
            None => true,
        };
        if beats {
            winners.insert(decl.property(), (decl, DeclarationOrigin::Inline, key));
        }
    }
    Ok(ComputedStyle {
        inline_skipped: false,
        declarations: winners
            .into_values()
            .map(|(decl, origin, _)| ComputedDeclaration {
                declaration: decl.clone(),
                origin,
            })
            .collect(),
    })
}

/// 要素の computed style（カスケード後の有効宣言一覧）を返す入口。
///
/// `sheets` の渡し方は [`match_rules`] と同じ。`element` が要素でなければ `Ok` の空を返す。
/// TASK-100（`PLUG-8`）はこの関数を呼ぶ想定。
///
/// # エラー
///
/// [`match_rules`] のエラー（走査ルール総数・照合キャッシュの上限超過）をそのまま伝播する。
/// `style` 属性が宣言入力長・宣言数の上限を超える場合は、その属性だけ捨てて継続し
/// [`ComputedStyle::inline_skipped`] を `true` にする。カスケード対象の宣言総数が
/// [`MAX_CASCADE_DECLARATIONS`] を超える場合は [`Error::InvalidInput`] を返す。部分結果は返さない。
pub fn computed_style<'a>(
    document: &Document,
    element: NodeId,
    sheets: impl IntoIterator<Item = &'a Stylesheet>,
) -> Result<ComputedStyle> {
    if !document.is_element(element) {
        return Ok(ComputedStyle::default());
    }
    let matched = match_rules(document, element, sheets)?;
    // 上限違反の style 属性はその源だけ捨てる（collect_document_styles と同じ契約）。
    let (inline, inline_skipped) = match parse_style_attribute(document, element) {
        Ok(v) => (v.unwrap_or_default(), false),
        Err(Error::InvalidInput { .. }) => (Vec::new(), true),
        Err(e) => return Err(e),
    };
    let mut style = cascade(&matched, &inline)?;
    style.inline_skipped = inline_skipped;
    Ok(style)
}

/// [`collect_document_styles`](super::collect_document_styles) の収集結果を使って computed style を返す入口。
///
/// `<style>` は採用済みの源だけ、inline は収集済みの宣言だけを使い、文書全体の上限で
/// 捨てた源（[`DocumentStyles::skipped_sources`]）は再解析せず捨てたまま扱う
/// （`style` 属性なら [`ComputedStyle::inline_skipped`] を `true` にする）。
/// 採否が収集結果と食い違わない（`CORE-5`・TASK-105.7・#261）。エラーは [`computed_style`] と同じ。
pub fn computed_style_in_document(
    document: &Document,
    element: NodeId,
    styles: &DocumentStyles,
) -> Result<ComputedStyle> {
    if !document.is_element(element) {
        return Ok(ComputedStyle::default());
    }
    let matched = match_rules(
        document,
        element,
        styles
            .style_sheets()
            .iter()
            .map(|s| s.parsed().stylesheet()),
    )?;
    let inline: &[Declaration] = styles.inline_declarations_for(element).unwrap_or(&[]);
    let inline_skipped = styles.is_inline_skipped(element);
    let mut style = cascade(&matched, inline)?;
    style.inline_skipped = inline_skipped;
    Ok(style)
}

#[cfg(test)]
mod tests {
    use super::super::matcher::MAX_SCANNED_RULES;
    use super::super::parse_stylesheet;
    use super::*;
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

    /// CORE-5（TASK-105.7）: 文書全体の上限で捨てた inline は computed 側でも再適用しない。
    #[test]
    fn core_5_document_skipped_inline_is_not_reapplied() {
        let html = format!(
            "{}<p style=\"color:red\">a</p>",
            "<style></style>".repeat(super::super::stylesheet::MAX_DOCUMENT_STYLE_SOURCES)
        );
        let d = doc(&html);
        let styles = super::super::collect_document_styles(&d).expect("must collect");
        assert_eq!(styles.skipped_sources().len(), 1);
        let p = find(&d, "p");
        let s = computed_style_in_document(&d, p, &styles).expect("ok");
        assert!(s.get("color").is_none());
        assert!(s.inline_skipped());
        // 収集結果を使わない単体入口は属性を再解析する（従来契約）。
        let legacy = computed_style(&d, p, std::iter::empty::<&Stylesheet>()).expect("ok");
        assert_eq!(legacy.get("color").map(|c| c.value()), Some("red"));
    }

    /// CORE-5（TASK-105.7）: skip 記録の上限（256 件）を超えた後の inline も捨てた扱いになる。
    #[test]
    fn core_5_inline_skipped_is_detected_beyond_record_cap() {
        let n = super::super::stylesheet::MAX_SKIPPED_STYLE_SOURCES + 10;
        let html = format!(
            "{}{}",
            "<style></style>".repeat(super::super::stylesheet::MAX_DOCUMENT_STYLE_SOURCES),
            "<p style=\"color:red\">a</p>".repeat(n)
        );
        let d = doc(&html);
        let styles = super::super::collect_document_styles(&d).expect("must collect");
        assert_eq!(
            styles.skipped_sources().len(),
            super::super::stylesheet::MAX_SKIPPED_STYLE_SOURCES
        );
        let last = d
            .descendants(d.root())
            .filter(|&i| d.is_element(i) && d.attribute(i, "style").is_some())
            .last()
            .expect("p");
        let s = computed_style_in_document(&d, last, &styles).expect("ok");
        assert!(s.inline_skipped());
        assert!(s.get("color").is_none());
    }

    /// CORE-5: 収集済みの inline と `<style>` はそのままカスケードされる。
    #[test]
    fn core_5_in_document_applies_adopted_sources() {
        let d = doc("<style>p{color:blue;margin:1px}</style><p style=\"color:red\">a</p>");
        let styles = super::super::collect_document_styles(&d).expect("must collect");
        let s = computed_style_in_document(&d, find(&d, "p"), &styles).expect("ok");
        assert_eq!(s.get("color").map(|c| c.value()), Some("red"));
        assert_eq!(s.get("margin").map(|c| c.value()), Some("1px"));
        assert!(!s.inline_skipped());
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
        assert_eq!(
            cascade(&[], &[]).expect("空は成功"),
            ComputedStyle::default()
        );
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

    /// CORE-5: カスケード対象の宣言総数の上限（ちょうどは成功・超過は Err）。
    #[test]
    fn core5_cascade_declaration_total_is_capped() {
        let d = doc("<p>a</p>");
        let decls = "color:red;".repeat(4_096);
        let big = sheet(&format!("p{{{decls}}}"));
        let n = MAX_CASCADE_DECLARATIONS / 4_096;
        let sheets: Vec<&Stylesheet> = vec![&big; n];
        let got =
            computed_style(&d, find(&d, "p"), sheets.iter().copied()).expect("上限ちょうどは成功");
        assert_eq!(got.len(), 1);

        let mut over = sheets.clone();
        over.push(&big);
        let err = computed_style(&d, find(&d, "p"), over.iter().copied()).unwrap_err();
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-5: `!important` は詳細度・ソース順より優先する。
    #[test]
    fn core_5_important_beats_later_normal() {
        let s = run("<p>a</p>", &["p{color:red !important} p{color:blue}"]);
        let c = s.get("color").expect("color がある");
        assert_eq!(c.value(), "red");
        assert_eq!(c.importance(), Importance::Important);
        assert_eq!(c.origin(), rule(0, 0, 0, 0, 1));
    }

    /// CORE-5: stylesheet の `!important` は inline の通常宣言に勝つ。
    #[test]
    fn core_5_important_rule_beats_normal_inline() {
        let s = run(
            r#"<p style="color:green">a</p>"#,
            &["p{color:red !important}"],
        );
        assert_eq!(s.get("color").expect("color がある").value(), "red");
    }

    /// CORE-5: `!important` 同士は高詳細度が勝ち、同 importance では inline が勝つ。
    #[test]
    fn core_5_important_ties_resolved_by_specificity_and_inline() {
        let s = run(
            r#"<p id="x">a</p>"#,
            &["#x{color:red !important} p{color:blue !important}"],
        );
        assert_eq!(s.get("color").expect("color がある").value(), "red");
        let s = run(
            r#"<p style="color:green !important">a</p>"#,
            &["p{color:red !important}"],
        );
        assert_eq!(
            s.get("color").expect("color がある").origin(),
            DeclarationOrigin::Inline
        );
    }

    /// CORE-5: 公開 `cascade` も宣言総数の上限を検査する。
    #[test]
    fn core_5_cascade_rejects_over_limit_directly() {
        let inline: Vec<Declaration> = (0..=MAX_CASCADE_DECLARATIONS)
            .map(|_| Declaration::new("color", "red", Importance::Normal))
            .collect();
        let err = cascade(&[], &inline).unwrap_err();
        assert!(matches!(err, Error::InvalidInput { .. }));
        assert!(cascade(&[], &inline[..MAX_CASCADE_DECLARATIONS]).is_ok());
    }
}

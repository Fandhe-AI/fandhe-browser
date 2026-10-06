//! セレクタマッチング: 1 要素に当たる [`StyleRule`] を詳細度・ソース順つきで列挙する
//! （TASK-105.5・#259・MS-8・ビヘイビア `CORE-5`）。
//!
//! 呼び出し元: TASK-105.6（#260）のカスケード解決・computed style API が、本モジュールの
//! 結果を入力にする。呼び出し先: 照合は [`crate::query`] の crate 内ヘルパー
//! （`matching_selector_indices`）に任せ、第 2 の照合器は持たない。詳細度は
//! [`super::specificity`] で計算する。
//!
//! # 契約
//!
//! - 順序: 戻り値は「シートの渡された順 → シート内のルール順」のソース順。詳細度では
//!   並べ替えない（並べ替え・競合解決は #260 の責務）。
//! - 詳細度: セレクタリスト内の複数の複雑セレクタが一致した場合は、一致したものの最大値
//!   （Selectors の規定）。ルールは 1 回だけ返す。
//! - 要素でない・範囲外の `NodeId` は `Ok` の空 `Vec`（`element_matches` の契約に揃える）。
//! - エラー: [`crate::error::Error::MatchCacheLimitExceeded`] を「不一致」に丸めず伝播する。
//!
//! # 対象外（REPAIR-3: 実装済みを装わない）
//!
//! - inline `style` 属性（#260 が `InlineStyle` を別入力として扱う）
//! - カスケード・`!important`・起源の区別（#260 以降）
//! - 全要素分を回したときの総量の上限（#261。1 回の呼び出しの結果総数は
//!   [`MAX_MATCHED_RULES`] で制限済み）
//! - CSSOM の可観測性計装（`OperationKind` に対応 variant がない）
//! - 疑似クラス等の未対応セレクタ（`crate::selector` が `Unsupported` にし、AST に現れない）

use crate::dom::{Document, NodeId};
use crate::error::{Error, Result};
use crate::query::matching_selector_indices;

use super::{Specificity, StyleRule, Stylesheet, specificity};

/// 1 回の [`match_rules`] が返すマッチ結果の総数の上限。
///
/// 任意長のシート列を渡されてもメモリを無制限に確保しないための上限
/// （シートあたり最大 16,384 ルール × 4 シート分。AGENTS.md「リソース上限」）。
pub const MAX_MATCHED_RULES: usize = 65_536;

/// 1 要素にマッチした 1 ルール分の結果。
///
/// 将来の拡張（起源・レイヤ等）に備え、フィールドは private でアクセサ経由とする（REPAIR-4）。
/// `(sheet_index, rule_index)` は #260 がソース順の比較にそのまま使える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchedRule<'a> {
    rule: &'a StyleRule,
    sheet_index: usize,
    rule_index: usize,
    specificity: Specificity,
}

impl<'a> MatchedRule<'a> {
    /// マッチしたルール本体。
    pub fn rule(&self) -> &'a StyleRule {
        self.rule
    }

    /// 渡されたシート列での位置。
    pub fn sheet_index(&self) -> usize {
        self.sheet_index
    }

    /// そのシートの [`Stylesheet::rules`] における位置（構築時に捨てられたルールは数えない）。
    pub fn rule_index(&self) -> usize {
        self.rule_index
    }

    /// 一致した複雑セレクタのうち最大の詳細度。
    pub fn specificity(&self) -> Specificity {
        self.specificity
    }
}

/// `element` にマッチするルールを、ソース順・詳細度つきで返す。
///
/// 単一シートは `[&sheet]`、文書全体は
/// `styles.style_sheets().iter().map(|s| s.parsed().stylesheet())` を渡す
/// （`&Stylesheet` 単体ではシート間のソース順が失われるため、シート列を受け取る）。
///
/// # エラー
///
/// 照合メモ化キャッシュの上限超過（[`crate::error::Error::MatchCacheLimitExceeded`]）を伝播する。
/// マッチ結果の総数が [`MAX_MATCHED_RULES`] を超える場合は
/// [`crate::error::Error::InvalidInput`] を返す（部分結果は返さない）。
pub fn match_rules<'a>(
    document: &Document,
    element: NodeId,
    sheets: impl IntoIterator<Item = &'a Stylesheet>,
) -> Result<Vec<MatchedRule<'a>>> {
    // 結果の件数は入力由来なので事前確保しない。
    let mut matched = Vec::new();
    if !document.is_element(element) {
        return Ok(matched);
    }
    for (sheet_index, sheet) in sheets.into_iter().enumerate() {
        for (rule_index, rule) in sheet.rules().iter().enumerate() {
            let selectors = rule.selectors();
            let indices = matching_selector_indices(document, element, selectors)?;
            let best = indices
                .iter()
                .filter_map(|&i| selectors.selectors().get(i))
                .map(specificity)
                .max();
            if let Some(specificity) = best {
                if matched.len() >= MAX_MATCHED_RULES {
                    return Err(Error::InvalidInput {
                        message: format!("matched rule count exceeds {MAX_MATCHED_RULES}"),
                    });
                }
                matched.push(MatchedRule {
                    rule,
                    sheet_index,
                    rule_index,
                    specificity,
                });
            }
        }
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cssom::parse_stylesheet;
    use crate::parse::{ParseOptions, parse_document};

    // `MatchCacheLimitExceeded` の伝播は `?` のみ。発生条件（100 万エントリ）は
    // query 側の既存テストが担保するため、ここでは再現しない。

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

    fn run<'a>(d: &Document, el: NodeId, sheets: &'a [Stylesheet]) -> Vec<MatchedRule<'a>> {
        match_rules(d, el, sheets.iter()).expect("上限に達しない")
    }

    /// CORE-5: 単一一致は rule_index と詳細度を返し、declarations は元ルールのもの。
    #[test]
    fn core_5_single_match() {
        let d = doc(r#"<p class="note">x</p>"#);
        let sheets = [sheet("p.note { color: red }")];
        let got = run(&d, find(&d, "p"), &sheets);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rule_index(), 0);
        assert_eq!(got[0].sheet_index(), 0);
        assert_eq!(got[0].specificity(), Specificity::new(0, 1, 1));
        let decl = got[0].rule().declarations().first().expect("宣言がある");
        assert_eq!((decl.property(), decl.value()), ("color", "red"));
    }

    /// CORE-5: 不一致・非要素・空シートは空。
    #[test]
    fn core_5_no_match_is_empty() {
        let d = doc("<p>x</p>");
        let sheets = [sheet("div { color: red }")];
        let p = find(&d, "p");
        let got = run(&d, p, &sheets);
        assert_eq!(got.len(), 0);
        assert!(got.is_empty());

        let text = d.children(p).next().expect("テキストノード");
        assert!(run(&d, text, &sheets).is_empty());
        assert!(run(&d, d.root(), &sheets).is_empty());
        assert!(run(&d, NodeId::new(usize::MAX), &sheets).is_empty());
        assert!(run(&d, p, &[sheet("")]).is_empty());
        assert!(run(&d, p, &[]).is_empty());
    }

    /// CORE-5: リスト内は一致した最大の詳細度、低い側だけ一致なら低い値。
    #[test]
    fn core_5_list_uses_max_of_matching_selectors() {
        let sheets = [sheet("p, #x { color: red }")];
        let d = doc(r#"<p id="x">a</p>"#);
        let got = run(&d, find(&d, "p"), &sheets);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].specificity(), Specificity::new(1, 0, 0));

        let d = doc("<p>a</p>");
        let got = run(&d, find(&d, "p"), &sheets);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].specificity(), Specificity::new(0, 0, 1));
    }

    /// CORE-5: ソース順を保ち、詳細度では並べ替えない。不一致を挟むと添字が飛ぶ。
    #[test]
    fn core_5_preserves_source_order() {
        let d = doc(r#"<p id="x" class="c">a</p>"#);
        let sheets = [sheet(
            "#x{color:red} p{color:blue} .c{color:red} p{color:green}",
        )];
        let got = run(&d, find(&d, "p"), &sheets);
        let idx: Vec<usize> = got.iter().map(|m| m.rule_index()).collect();
        assert_eq!(idx, vec![0, 1, 2, 3]);
        let sp: Vec<Specificity> = got.iter().map(|m| m.specificity()).collect();
        assert_eq!(
            sp,
            vec![
                Specificity::new(1, 0, 0),
                Specificity::new(0, 0, 1),
                Specificity::new(0, 1, 0),
                Specificity::new(0, 0, 1),
            ]
        );

        let sheets = [sheet("p{color:red} span{color:red} .c{color:red}")];
        let got = run(&d, find(&d, "p"), &sheets);
        let idx: Vec<usize> = got.iter().map(|m| m.rule_index()).collect();
        assert_eq!(idx, vec![0, 2]);
    }

    /// CORE-5: 複数シートは (sheet_index, rule_index) の順。
    #[test]
    fn core_5_multiple_sheets() {
        let d = doc("<p>a</p>");
        let sheets = [
            sheet("p{color:red} div{color:red}"),
            sheet("p{color:red} span{color:red} p{color:blue}"),
        ];
        let got = run(&d, find(&d, "p"), &sheets);
        let pos: Vec<(usize, usize)> = got
            .iter()
            .map(|m| (m.sheet_index(), m.rule_index()))
            .collect();
        assert_eq!(pos, vec![(0, 0), (1, 0), (1, 2)]);
    }

    /// CORE-5: 結果総数の上限は境界で効き、超過は Err（部分結果を返さない）。
    #[test]
    fn core_5_matched_rule_count_limit() {
        let d = doc("<p>a</p>");
        let big = sheet(&"p{color:red}".repeat(16_384));
        let sheets: Vec<&Stylesheet> = vec![&big; MAX_MATCHED_RULES / 16_384];
        let got =
            match_rules(&d, find(&d, "p"), sheets.iter().copied()).expect("上限ちょうどは成功");
        assert_eq!(got.len(), MAX_MATCHED_RULES);

        let one = sheet("p{color:red}");
        let mut over = sheets.clone();
        over.push(&one);
        let err = match_rules(&d, find(&d, "p"), over.iter().copied()).expect_err("超過は失敗");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-5: 結合子は query の照合規則に従う。
    #[test]
    fn core_5_combinators() {
        let d = doc("<section><div><p>a</p></div></section>");
        let sheets = [sheet(
            "div > p{color:red} section p{color:red} section > p{color:red}",
        )];
        let got = run(&d, find(&d, "p"), &sheets);
        let idx: Vec<usize> = got.iter().map(|m| m.rule_index()).collect();
        assert_eq!(idx, vec![0, 1]);
        assert_eq!(got[0].specificity(), Specificity::new(0, 0, 2));
    }

    /// CORE-5: quirks mode の大文字小文字規則が query からそのまま流れる。
    #[test]
    fn core_5_quirks_mode_class_matching() {
        let sheets = [sheet(".Note{color:red}")];
        let quirks = doc(r#"<p class="note">a</p>"#);
        assert_eq!(run(&quirks, find(&quirks, "p"), &sheets).len(), 1);
        let standards = doc(r#"<!DOCTYPE html><p class="note">a</p>"#);
        assert_eq!(run(&standards, find(&standards, "p"), &sheets).len(), 0);
    }
}

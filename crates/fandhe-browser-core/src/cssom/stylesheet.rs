//! CSS テキストを「セレクタ部 + `{ ... }` の中身」のルールブロックへ分割する字句処理
//! （TASK-105.4.1・#551・MS-8・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! 呼び出し元は #552 が集める `<style>` 要素の中身と外部スタイルシート文字列で、どちらも
//! 外部入力（ネットワーク取得した CSS）である。出力の [`RuleBlock::prelude`] は
//! [`crate::selector::parse_selector_list`] へ、[`RuleBlock::body`] は
//! [`super::parse_declarations`] へそのまま渡せる。本モジュールは字句分割だけを担い、
//! セレクタ・宣言の解析も [`super::StyleRule`] / [`super::Stylesheet`] の組み立ても行わない
//! （後者は #552 の責務）。新規依存は使わない（#263 の決定）。
//!
//! # エラー方針
//!
//! - コメント `/* */` はどこでも読み飛ばす。文字列・エスケープ・括弧の内側の `{` `}` は
//!   区切りとして扱わない
//! - at-rule（`@import ...;` / `@media ... { ... }` 等）は丸ごと読み飛ばし、件数だけ数える
//! - 構造的に壊れたルール（セレクタ部が空・ブロック無し・余分な `}`）はそのルールだけを
//!   捨てて [`RuleBlockError`] に記録し、残りの処理を続ける
//! - コメント・文字列・ブロックが EOF まで閉じない場合は、EOF で暗黙に閉じたものとする
//! - 入力長・ルール数が上限を超えたら [`Error::InvalidInput`]（確保前・`push` 前に判定。
//!   メッセージに入力は含めない）。エラー記録は上限までで、超過分は件数だけ数える
//!
//! # 未実装（REPAIR-3: 実装済みを装わない）
//!
//! - セレクタ・宣言レベルの解析失敗の記録（#552 が [`RuleBlockErrorKind`] へ variant を
//!   足して行う。`#[non_exhaustive]`）
//! - 条件付きグループ（`@media` / `@supports` / `@layer`）内のルールの取り込みと
//!   `@import` の取得（いずれも読み飛ばすだけ）
//! - CSS ネスト（body 内の入れ子ルール）の展開。body に入れ子のまま残す
//! - CDO / CDC（`<!--` / `-->`）の読み飛ばし。セレクタ部に残り、#552 のセレクタ解析で
//!   失敗として記録される
//! - 文字列中の改行で文字列を打ち切る CSS Syntax のエラー回復（現状は EOF まで文字列扱い）
//! - 括弧の種類を区別した対応づけ（単一カウンタのため `(` を `}` で閉じても深さが戻る）
//! - 上限値は暫定。見直しは #261（TASK-105.7）が担う

use std::iter::Peekable;
use std::str::CharIndices;

use super::is_css_whitespace;
use crate::error::{Error, Result};

/// 受け付ける CSS テキストの最大バイト数（暫定。見直しは #261）。
pub const MAX_STYLESHEET_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// 1 スタイルシートで受け付けるルールブロックの最大件数（暫定。見直しは #261）。
pub const MAX_RULES_PER_STYLESHEET: usize = 16_384;

/// 記録する [`RuleBlockError`] の最大件数（暫定。超過分は件数のみ。見直しは #261）。
pub const MAX_RULE_BLOCK_ERRORS: usize = 256;

/// 切り出した 1 ルール分のテキスト。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleBlock {
    prelude: String,
    body: String,
    offset: usize,
}

impl RuleBlock {
    /// コメント除去・前後空白除去済みのセレクタ部（`parse_selector_list` の入力）。
    pub fn prelude(&self) -> &str {
        &self.prelude
    }

    /// `{` と対応する `}` の間の生テキスト（コメント込み。`parse_declarations` の入力）。
    pub fn body(&self) -> &str {
        &self.body
    }

    /// ルール先頭（セレクタ部の最初の非空白文字）の入力内バイト位置。
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// 構造的に壊れたルールの種別。#552 が解析失敗の variant を追加する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuleBlockErrorKind {
    /// セレクタ部が空のブロック（`{a:b}`）。ブロックごと捨てる。
    EmptyPrelude,
    /// セレクタ部の後に `{` が現れないまま EOF に達した。
    MissingBlock,
    /// 対応する `{` の無い `}`。
    UnexpectedCloseBrace,
}

/// 分割中に記録した壊れたルールの情報（種別とバイト位置のみ。入力テキストは含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleBlockError {
    kind: RuleBlockErrorKind,
    offset: usize,
}

impl RuleBlockError {
    /// エラー種別。
    pub fn kind(&self) -> RuleBlockErrorKind {
        self.kind
    }

    /// 入力内のバイト位置。
    pub fn offset(&self) -> usize {
        self.offset
    }
}

/// [`split_rule_blocks`] の結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleBlocks {
    blocks: Vec<RuleBlock>,
    errors: Vec<RuleBlockError>,
    dropped_errors: usize,
    skipped_at_rules: usize,
}

impl RuleBlocks {
    /// ソース順のルールブロック。
    pub fn blocks(&self) -> &[RuleBlock] {
        &self.blocks
    }

    /// 記録したエラー（最大 [`MAX_RULE_BLOCK_ERRORS`] 件）。
    pub fn errors(&self) -> &[RuleBlockError] {
        &self.errors
    }

    /// 上限超過で記録しなかったエラーの件数。
    pub fn dropped_errors(&self) -> usize {
        self.dropped_errors
    }

    /// 読み飛ばした at-rule の件数。
    pub fn skipped_at_rules(&self) -> usize {
        self.skipped_at_rules
    }

    fn push_error(&mut self, kind: RuleBlockErrorKind, offset: usize) {
        if self.errors.len() >= MAX_RULE_BLOCK_ERRORS {
            self.dropped_errors = self.dropped_errors.saturating_add(1);
        } else {
            self.errors.push(RuleBlockError { kind, offset });
        }
    }
}

type Chars<'a> = Peekable<CharIndices<'a>>;

/// `/` を消費した直後（次が `*`）に呼び、`*/` までコメントを読み飛ばす。未終端は EOF まで。
fn skip_comment(it: &mut Chars<'_>) {
    it.next(); // `*`
    while let Some((_, c)) = it.next() {
        if c == '*' && matches!(it.peek(), Some((_, '/'))) {
            it.next();
            return;
        }
    }
}

/// 開き引用符を消費した直後に呼び、閉じ引用符までを読む。`out` があれば閉じ引用符まで
/// 含めて追記する。未終端は EOF まで。
fn read_string(it: &mut Chars<'_>, quote: char, mut out: Option<&mut String>) {
    while let Some((_, c)) = it.next() {
        if let Some(o) = out.as_deref_mut() {
            o.push(c);
        }
        if c == '\\' {
            if let Some((_, n)) = it.next()
                && let Some(o) = out.as_deref_mut()
            {
                o.push(n);
            }
        } else if c == quote {
            return;
        }
    }
}

/// `{` を消費した直後に呼び、対応する `}` を消費して、その位置（無ければ入力長）を返す。
fn read_body(it: &mut Chars<'_>, len: usize) -> usize {
    let mut depth: usize = 0;
    while let Some((i, c)) = it.next() {
        match c {
            '/' if matches!(it.peek(), Some((_, '*'))) => skip_comment(it),
            '"' | '\'' => read_string(it, c, None),
            '\\' => {
                it.next();
            }
            '(' | '[' | '{' => depth = depth.saturating_add(1),
            '}' if depth == 0 => return i,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    len
}

/// at-rule を、文形式は深さ 0 の `;` まで、ブロック形式は対応する `}` まで読み飛ばす。
fn skip_at_rule(it: &mut Chars<'_>) {
    let mut depth: usize = 0;
    while let Some((_, c)) = it.next() {
        match c {
            '/' if matches!(it.peek(), Some((_, '*'))) => skip_comment(it),
            '"' | '\'' => read_string(it, c, None),
            '\\' => {
                it.next();
            }
            ';' if depth == 0 => return,
            '(' | '[' | '{' => depth = depth.saturating_add(1),
            '}' if depth <= 1 => return,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
}

/// CSS テキストをルールブロックの列へ分割する。
///
/// #552 の `<style>` / 外部スタイルシート収集から呼ばれ、結果の各ブロックを
/// `parse_selector_list` と `parse_declarations` へ渡す。1 パス・再帰なしで走査し、
/// 添字アクセス・`unwrap` は使わない。壊れたルールは [`RuleBlocks::errors`] に記録して
/// 継続し、`Err` は入力長・ルール数の上限違反（[`Error::InvalidInput`]）だけ。
/// 対応ビヘイビア: `CORE-5`（TASK-105.4.1・#551）。
pub fn split_rule_blocks(input: &str) -> Result<RuleBlocks> {
    if input.len() > MAX_STYLESHEET_INPUT_BYTES {
        return Err(Error::InvalidInput {
            message: format!("stylesheet input exceeds {MAX_STYLESHEET_INPUT_BYTES} bytes"),
        });
    }

    let len = input.len();
    let mut out = RuleBlocks::default();
    let mut it = input.char_indices().peekable();
    let mut prelude = String::new();
    let mut start: Option<usize> = None;
    let mut depth: usize = 0;

    while let Some((i, c)) = it.next() {
        match c {
            '/' if matches!(it.peek(), Some((_, '*'))) => {
                skip_comment(&mut it);
                // 前後のトークンを融合させない。
                if start.is_some() && !prelude.ends_with(' ') {
                    prelude.push(' ');
                }
            }
            c if is_css_whitespace(c) => {
                if start.is_some() && !prelude.ends_with(' ') {
                    prelude.push(' ');
                }
            }
            '@' if start.is_none() => {
                skip_at_rule(&mut it);
                out.skipped_at_rules = out.skipped_at_rules.saturating_add(1);
            }
            '}' if depth == 0 => {
                out.push_error(RuleBlockErrorKind::UnexpectedCloseBrace, i);
                prelude.clear();
                start = None;
            }
            '{' if depth == 0 => {
                let body_start = i.saturating_add(1);
                let end = read_body(&mut it, len);
                let trimmed = prelude.trim_matches(is_css_whitespace);
                match start {
                    Some(offset) if !trimmed.is_empty() => {
                        if out.blocks.len() >= MAX_RULES_PER_STYLESHEET {
                            return Err(Error::InvalidInput {
                                message: format!(
                                    "rule count exceeds {MAX_RULES_PER_STYLESHEET} per stylesheet"
                                ),
                            });
                        }
                        out.blocks.push(RuleBlock {
                            prelude: trimmed.to_string(),
                            body: input.get(body_start..end).unwrap_or_default().to_string(),
                            offset,
                        });
                    }
                    _ => out.push_error(RuleBlockErrorKind::EmptyPrelude, i),
                }
                prelude.clear();
                start = None;
            }
            _ => {
                start.get_or_insert(i);
                prelude.push(c);
                match c {
                    '"' | '\'' => read_string(&mut it, c, Some(&mut prelude)),
                    '\\' => {
                        if let Some((_, n)) = it.next() {
                            prelude.push(n);
                        }
                    }
                    '(' | '[' | '{' => depth = depth.saturating_add(1),
                    ')' | ']' | '}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
        }
    }

    if let Some(offset) = start
        && !prelude.trim_matches(is_css_whitespace).is_empty()
    {
        out.push_error(RuleBlockErrorKind::MissingBlock, offset);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(input: &str) -> Vec<(String, String, usize)> {
        split_rule_blocks(input)
            .expect("must split")
            .blocks()
            .iter()
            .map(|b| (b.prelude().to_string(), b.body().to_string(), b.offset()))
            .collect()
    }

    fn t(p: &str, b: &str, o: usize) -> (String, String, usize) {
        (p.to_string(), b.to_string(), o)
    }

    /// CORE-5: 2 ルールを prelude・body・offset 付きで切り出す。
    #[test]
    fn core_5_splits_two_rules() {
        assert_eq!(
            pb("a{color:red} .b { margin:0 }"),
            vec![t("a", "color:red", 0), t(".b", " margin:0 ", 13)]
        );
    }

    /// CORE-5: コメントはどこにあっても分割を崩さず、body 内では生のまま残る。
    #[test]
    fn core_5_skips_comments() {
        let got = pb("/* x { */ a /* } */ b { c:d /* } */ } /* e */ p{q:r}");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "a b");
        assert_eq!(got[0].1, " c:d /* } */ ");
        assert_eq!(got[0].2, 10);
        assert_eq!(got[1].0, "p");
        assert_eq!(got[1].1, "q:r");
    }

    /// CORE-5: at-rule は文形式・ブロック形式とも読み飛ばして件数を数える。
    #[test]
    fn core_5_skips_at_rules() {
        let r = split_rule_blocks("@import url(x.css); @media (min-width:1px) { a{} b{} } p{c:d}")
            .expect("must split");
        assert_eq!(r.blocks().len(), 1);
        assert_eq!(r.blocks()[0].prelude(), "p");
        assert_eq!(r.skipped_at_rules(), 2);
        assert!(r.errors().is_empty());
    }

    /// CORE-5: 文字列内の括弧は区切りにしない。
    #[test]
    fn core_5_braces_in_strings() {
        assert_eq!(
            pb("a[title=\"}{\"]{content:\"}\"}"),
            vec![t("a[title=\"}{\"]", "content:\"}\"", 0)]
        );
    }

    /// CORE-5: body 内の入れ子 `{}` はそのまま残る。
    #[test]
    fn core_5_nested_braces_kept() {
        assert_eq!(pb("a{ b{c:d} e:f }"), vec![t("a", " b{c:d} e:f ", 0)]);
    }

    /// CORE-5: 余分な `}` は記録して後続ルールを取る。
    #[test]
    fn core_5_unexpected_close_brace() {
        let r = split_rule_blocks("} a{b:c}").expect("must split");
        assert_eq!(r.errors().len(), 1);
        assert_eq!(
            r.errors()[0].kind(),
            RuleBlockErrorKind::UnexpectedCloseBrace
        );
        assert_eq!(r.errors()[0].offset(), 0);
        assert_eq!(r.blocks().len(), 1);
        assert_eq!(r.blocks()[0].prelude(), "a");
    }

    /// CORE-5: セレクタ部が空のブロックは記録して後続ルールを取る。
    #[test]
    fn core_5_empty_prelude() {
        let r = split_rule_blocks("{a:b} p{c:d}").expect("must split");
        assert_eq!(r.errors().len(), 1);
        assert_eq!(r.errors()[0].kind(), RuleBlockErrorKind::EmptyPrelude);
        assert_eq!(r.errors()[0].offset(), 0);
        assert_eq!(r.blocks().len(), 1);
        assert_eq!(r.blocks()[0].prelude(), "p");
    }

    /// CORE-5: ブロックの無い末尾は MissingBlock として記録する。
    #[test]
    fn core_5_missing_block() {
        let r = split_rule_blocks("a{b:c} div").expect("must split");
        assert_eq!(r.errors().len(), 1);
        assert_eq!(r.errors()[0].kind(), RuleBlockErrorKind::MissingBlock);
        assert_eq!(r.errors()[0].offset(), 7);
        assert_eq!(r.blocks().len(), 1);
    }

    /// CORE-5: 未終端ブロックは暗黙に閉じる。
    #[test]
    fn core_5_unterminated_block() {
        let r = split_rule_blocks("a{b:c").expect("must split");
        assert_eq!(r.blocks().len(), 1);
        assert_eq!(r.blocks()[0].body(), "b:c");
        assert!(r.errors().is_empty());
    }

    /// CORE-5: 未終端コメント・文字列で panic しない。
    #[test]
    fn core_5_unterminated_comment_and_string() {
        assert!(
            split_rule_blocks("a{b:c} /* open")
                .expect("ok")
                .errors()
                .is_empty()
        );
        assert_eq!(pb("a{b:\"open}"), vec![t("a", "b:\"open}", 0)]);
        assert!(split_rule_blocks("a[x=\"open").is_ok());
        assert!(split_rule_blocks("a\\").is_ok());
        assert!(split_rule_blocks("@").is_ok());
    }

    /// CORE-5: 空入力・空白・コメントのみはブロック 0 件・エラー 0 件。
    #[test]
    fn core_5_empty_inputs() {
        for s in ["", "  \n\t", "/* c */", " /* a */ /* b */ "] {
            let r = split_rule_blocks(s).expect("must split");
            assert!(r.blocks().is_empty(), "input {s:?}");
            assert!(r.errors().is_empty(), "input {s:?}");
        }
    }

    /// CORE-5: マルチバイトでも panic せず offset はバイト位置。
    #[test]
    fn core_5_multibyte() {
        // "/* あ */ " は 10 バイト（あ が 3 バイト）なので `.い` は 10 バイト目。
        let got = pb("/* あ */ .い{ content:\"う\" }");
        assert_eq!(got, vec![t(".い", " content:\"う\" ", 10)]);
    }

    /// CORE-5: 入力長の上限は境界（ちょうど = Ok・+1 = Err）で効く。
    #[test]
    fn core_5_input_limit_boundary() {
        let ok = " ".repeat(MAX_STYLESHEET_INPUT_BYTES);
        assert!(split_rule_blocks(&ok).is_ok());
        let ng = " ".repeat(MAX_STYLESHEET_INPUT_BYTES + 1);
        assert!(matches!(
            split_rule_blocks(&ng),
            Err(Error::InvalidInput { .. })
        ));
    }

    /// CORE-5: ルール数の上限は境界で効く。
    #[test]
    fn core_5_rule_count_boundary() {
        let ok = "a{}".repeat(MAX_RULES_PER_STYLESHEET);
        assert_eq!(
            split_rule_blocks(&ok).expect("ok").blocks().len(),
            MAX_RULES_PER_STYLESHEET
        );
        let ng = "a{}".repeat(MAX_RULES_PER_STYLESHEET + 1);
        assert!(matches!(
            split_rule_blocks(&ng),
            Err(Error::InvalidInput { .. })
        ));
    }

    /// CORE-5: エラー記録は上限で止まり、超過分は件数だけ数える。
    #[test]
    fn core_5_error_cap() {
        let r = split_rule_blocks(&"}".repeat(MAX_RULE_BLOCK_ERRORS + 3)).expect("ok");
        assert_eq!(r.errors().len(), MAX_RULE_BLOCK_ERRORS);
        assert_eq!(r.dropped_errors(), 3);
    }
}

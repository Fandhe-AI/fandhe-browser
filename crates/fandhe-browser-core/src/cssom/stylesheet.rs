//! CSS テキストのルールブロック分割（TASK-105.4.1・#551）と、DOM からのスタイル源収集・
//! [`Stylesheet`] 構築（TASK-105.4.2・#552）。MS-8・ビヘイビア `CORE-5`（`PLUG-8` の前提条件）。
//!
//! 入口は 3 つ。外部スタイルシート文字列・`<style>` の中身を [`parse_stylesheet`] へ、
//! 文書全体を [`collect_document_styles`] へ、単一要素の `style` 属性を
//! [`parse_style_attribute`] へ渡す。入力はいずれも外部入力（ネットワーク取得した
//! HTML / CSS）。結果は後続の #259（マッチング）・#260（カスケード・computed style）が
//! 入力にする。inline の `style` 属性は `Stylesheet` ではなく要素ごとの宣言列
//! （[`InlineStyle`]）で表す（`StyleRule` はセレクタ必須で単一ノードを指せず、#260 は
//! inline をマッチ結果とは別のカスケード入力として扱うため）。
//!
//! 分割結果の [`RuleBlock::prelude`] は [`crate::selector::parse_selector_list`] へ、
//! [`RuleBlock::body`] は [`super::parse_declarations`] へ渡す。新規依存は使わない（#263 の決定）。
//!
//! 外部スタイルシート（`<link rel="stylesheet">`・`@import`）の取得は本モジュールの責務外で、
//! ネットワークには触れない。呼び出し側が fetch 済み（scheme・アドレス検証済み）の文字列を
//! [`parse_stylesheet`] へ渡す。
//!
//! # エラー方針
//!
//! - コメント `/* */` はどこでも読み飛ばす。文字列・エスケープ・括弧の内側の `{` `}` は
//!   区切りとして扱わない
//! - at-rule（`@import ...;` / `@media ... { ... }` 等）は丸ごと読み飛ばし、件数だけ数える
//! - 構造的に壊れたルール（セレクタ部が空・ブロック無し・余分な `}`）はそのルールだけを
//!   捨てて [`RuleBlockError`] に記録し、残りの処理を続ける
//! - コメント・文字列・ブロックが EOF まで閉じない場合は、EOF で暗黙に閉じたものとする
//! - セレクタ・宣言の解析に失敗したルール（[`RuleBlockErrorKind::UnsupportedSelector`] /
//!   `InvalidSelector` / `InvalidDeclarations`）もそのルールだけを捨てて記録し、残りを
//!   続ける。記録は offset 昇順で、種別とバイト位置のみ（入力テキストは含めない）
//! - 宣言 0 件のルール（`a{}`）は捨てずに残す
//! - 文書単位の [`collect_document_styles`] は、上限違反の 1 スタイル源（`<style>` /
//!   `style` 属性）だけを捨てて [`DocumentStyles::skipped_sources`] に記録し、残りを収集する
//!   （1 つの悪性源で文書全体が失敗しないため。TASK-105.7・#261）。文書全体の累積バイト数・
//!   累積ルール数・処理した源の件数（[`MAX_DOCUMENT_STYLE_SOURCES`]）・処理量
//!   （[`MAX_DOCUMENT_STYLE_ATTEMPT_BYTES`]）を超える源も同様にその源だけを捨てる
//! - 入力長・ルール数が上限を超えたら [`Error::InvalidInput`]（確保前・`push` 前に判定。
//!   メッセージに入力は含めない）。エラー記録は上限までで、超過分は件数だけ数える
//!
//! # 未実装（REPAIR-3: 実装済みを装わない）
//!
//! - `<link rel="stylesheet">` の解決・取得と `@import` の取得（取得は呼び出し側の責務）
//! - `<style>` の `media` 属性（無視する）、SVG の `<style>`（対象外）、Shadow DOM、
//!   `disabled` 等の状態、JS による動的変更の反映、起源（UA / user / author）の区別
//! - CSSOM 用の可観測性計装（`OperationKind` に対応 variant が無い）
//! - 条件付きグループ（`@media` / `@supports` / `@layer`）内のルールの取り込みと
//!   `@import` の取得（いずれも読み飛ばすだけ）
//! - CSS ネスト（body 内の入れ子ルール）の展開。body に入れ子のまま残す
//! - CDO / CDC（`<!--` / `-->`）の読み飛ばし。セレクタ部に残り、#552 のセレクタ解析で
//!   失敗として記録される
//! - 文字列中の改行で文字列を打ち切る CSS Syntax のエラー回復（現状は EOF まで文字列扱い）
//! - 括弧の種類を区別した対応づけ（単一カウンタのため `(` を `}` で閉じても深さが戻る）

use std::iter::Peekable;
use std::str::CharIndices;

use super::{Declaration, StyleRule, Stylesheet, is_css_whitespace, parse_declarations};
use crate::dom::{Document, HTML_NAMESPACE_URI, NodeData, NodeId};
use crate::error::{Error, Result};
use crate::selector::{html_local_name_eq, parse_selector_list};

/// 受け付ける CSS テキストの最大バイト数。
///
/// 一般的な大規模サイトの CSS（数百 KiB）に十分な余裕を持たせた値で、解析前（確保前）に判定する
/// （TASK-105.7・#261・`CORE-5`）。
pub const MAX_STYLESHEET_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// 1 スタイルシートで受け付けるルールブロックの最大件数。
///
/// 1 ルールは最小 4 バイト（`a{}`）程度のため、入力長上限だけでは件数が約 100 万に達しうる。
/// ルール 1 件は構造体・ヒープで数十バイトへ膨らむので、件数側でも `push` 前に頭打ちにする
/// （TASK-105.7・#261）。
pub const MAX_RULES_PER_STYLESHEET: usize = 16_384;

/// 記録する [`RuleBlockError`] の最大件数（超過分は件数のみ。記録自体が無制限確保にならない
/// ための頭打ち。TASK-105.7・#261）。
pub const MAX_RULE_BLOCK_ERRORS: usize = 256;

/// 文書全体で採用する `<style>` テキストと `style` 属性値の累積最大バイト数。
///
/// 1 シート上限（[`MAX_STYLESHEET_INPUT_BYTES`]）の 4 シート分。宣言 1 件は入力数バイトに対し
/// 構造体・ヒープで数十バイトへ膨らむため、バイト累積で文書全体の総確保量を抑える
/// （`CORE-5`・TASK-105.7・#261）。超過する源は [`collect_document_styles`] がその源だけ捨てる。
pub const MAX_DOCUMENT_STYLE_BYTES: usize = 4 * MAX_STYLESHEET_INPUT_BYTES;

/// 文書全体で採用する全スタイルシートの累積最大ルール数。
///
/// `match_rules` の 1 回あたり走査上限（`MAX_SCANNED_RULES`）と同値。これを超える文書は
/// `match_rules` が必ず `Err` になるため、収集段階で止める（`CORE-5`・TASK-105.7・#261）。
pub const MAX_DOCUMENT_STYLE_RULES: usize = 65_536;

/// 文書内で処理対象にするスタイル源（`<style>` と `style` 属性）の最大件数。採否を問わず数える。
///
/// 超えた源はパースせず捨てる（`CORE-5`・TASK-105.7・#261）。
pub const MAX_DOCUMENT_STYLE_SOURCES: usize = 4_096;

/// 文書内で処理対象にしたスタイル源の累積最大バイト数。採否を問わず数える。
///
/// 超過する源はパースせず捨てる。ルール数超過で捨てる源の繰り返し解析を抑える
/// （`CORE-5`・TASK-105.7・#261）。
pub const MAX_DOCUMENT_STYLE_ATTEMPT_BYTES: usize = 2 * MAX_DOCUMENT_STYLE_BYTES;

/// 上限違反で捨てたスタイル源を記録する最大件数（超過分は件数のみ）。
pub const MAX_SKIPPED_STYLE_SOURCES: usize = 256;

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

/// 壊れたルールの種別（構造エラーと、セレクタ・宣言の解析失敗）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuleBlockErrorKind {
    /// セレクタ部が空のブロック（`{a:b}`）。ブロックごと捨てる。
    EmptyPrelude,
    /// セレクタ部の後に `{` が現れないまま EOF に達した。
    MissingBlock,
    /// 対応する `{` の無い `}`。
    UnexpectedCloseBrace,
    /// セレクタが構文上は正しいがサブセット外（`a:hover` 等。`Error::Unsupported`）。
    UnsupportedSelector,
    /// セレクタの構文不正・セレクタ上限超過（CDO / CDC 混入を含む）。
    InvalidSelector,
    /// 宣言ブロックが上限（入力長・宣言数）を超えた。
    InvalidDeclarations,
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

/// 識別子の一部になりうる文字（コメント前後でトークンが融合するかの判定用）。
fn is_ident_like(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii()
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

/// at-rule を、文形式は括弧外・ブロック外の `;` まで、ブロック形式は at-rule 自身のブロックを
/// 閉じる `}` まで読み飛ばす。`(` / `[` 内の `{` `}` は括弧が閉じるまでブロック境界として
/// 数えない（`url(foo}bar)` で読み飛ばしが早期終了しない。CORE-5・#551）。
///
/// ブロックを開いていない状態（深さ 0）で `}` に達した場合は at-rule が `;` なしで終端した
/// 余分な `}` とみなし、その `}` の位置を返す（呼び出し側が `UnexpectedCloseBrace` を記録する）。
fn skip_at_rule(it: &mut Chars<'_>) -> Option<usize> {
    let mut paren: usize = 0;
    let mut braces: usize = 0;
    while let Some((i, c)) = it.next() {
        match c {
            '/' if matches!(it.peek(), Some((_, '*'))) => skip_comment(it),
            '"' | '\'' => read_string(it, c, None),
            '\\' => {
                it.next();
            }
            ';' if paren == 0 && braces == 0 => return None,
            '(' | '[' => paren = paren.saturating_add(1),
            ')' | ']' => paren = paren.saturating_sub(1),
            '{' if paren == 0 => braces = braces.saturating_add(1),
            '}' if paren == 0 && braces == 0 => return Some(i),
            '}' if paren == 0 => {
                braces = braces.saturating_sub(1);
                if braces == 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
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
                // コメントは空白ではない（空白にすると `a/**/.b` が子孫結合子になる）ため何も
                // 挿入しない。ただし前後が識別子文字同士だと別トークンが融合するので、その場合だけ
                // 区切りを入れる。
                if start.is_some()
                    && prelude.chars().next_back().is_some_and(is_ident_like)
                    && it.peek().is_some_and(|&(_, n)| is_ident_like(n))
                {
                    prelude.push(' ');
                }
            }
            c if is_css_whitespace(c) => {
                if start.is_some() && !prelude.ends_with(' ') {
                    prelude.push(' ');
                }
            }
            '@' if start.is_none() => {
                let stray_close = skip_at_rule(&mut it);
                out.skipped_at_rules = out.skipped_at_rules.saturating_add(1);
                if let Some(offset) = stray_close {
                    out.push_error(RuleBlockErrorKind::UnexpectedCloseBrace, offset);
                }
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

/// [`parse_stylesheet`] の結果。組み立て済みの [`Stylesheet`] と、捨てたルールの記録。
#[derive(Debug, Clone, Default)]
pub struct ParsedStylesheet {
    stylesheet: Stylesheet,
    errors: Vec<RuleBlockError>,
    dropped_errors: usize,
    skipped_at_rules: usize,
}

impl ParsedStylesheet {
    /// 構築したスタイルシート（ソース順のルール）。
    pub fn stylesheet(&self) -> &Stylesheet {
        &self.stylesheet
    }

    /// 所有権付きでスタイルシートを取り出す。
    pub fn into_stylesheet(self) -> Stylesheet {
        self.stylesheet
    }

    /// 捨てたルールの記録（offset 昇順・最大 [`MAX_RULE_BLOCK_ERRORS`] 件）。
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
}

/// CSS テキスト（外部スタイルシート文字列・`<style>` の中身）から [`Stylesheet`] を構築する。
///
/// [`split_rule_blocks`] の結果を `parse_selector_list` / `parse_declarations` に通し、失敗した
/// ルールだけを捨てて [`ParsedStylesheet::errors`] へ記録して継続する。`Err` は入力長・ルール数の
/// 上限違反（[`Error::InvalidInput`]）のみ。ネットワークには触れない。
/// 対応ビヘイビア: `CORE-5`（TASK-105.4.2・#552）。
pub fn parse_stylesheet(css: &str) -> Result<ParsedStylesheet> {
    let mut split = split_rule_blocks(css)?;
    let blocks = std::mem::take(&mut split.blocks);
    let mut rules = Vec::with_capacity(blocks.len());
    // 解析エラーは上限を掛けずに集め、構造エラーと統合・offset 順に並べてから上限を適用する
    // （先に構造エラーで枠を使い切ると、前半の解析エラーが後半の構造エラーに押し出されるため）。
    // 構造エラーは offset 昇順で先頭 MAX 件を保持済みで、捨てた分はそれより後ろなので結果は厳密。
    let mut parse_errors: Vec<RuleBlockError> = Vec::new();
    for block in &blocks {
        let selectors = match parse_selector_list(block.prelude()) {
            Ok(s) => s,
            Err(Error::Unsupported { .. }) => {
                parse_errors.push(RuleBlockError {
                    kind: RuleBlockErrorKind::UnsupportedSelector,
                    offset: block.offset(),
                });
                continue;
            }
            Err(_) => {
                parse_errors.push(RuleBlockError {
                    kind: RuleBlockErrorKind::InvalidSelector,
                    offset: block.offset(),
                });
                continue;
            }
        };
        match parse_declarations(block.body()) {
            Ok(declarations) => rules.push(StyleRule::new(selectors, declarations)),
            Err(_) => parse_errors.push(RuleBlockError {
                kind: RuleBlockErrorKind::InvalidDeclarations,
                offset: block.offset(),
            }),
        }
    }
    // 構造エラーと解析エラーが混在するため、ソース順（offset 昇順・安定）に揃える。
    split.errors.extend(parse_errors);
    split.errors.sort_by_key(|e| e.offset);
    if split.errors.len() > MAX_RULE_BLOCK_ERRORS {
        let excess = split.errors.len().saturating_sub(MAX_RULE_BLOCK_ERRORS);
        split.errors.truncate(MAX_RULE_BLOCK_ERRORS);
        split.dropped_errors = split.dropped_errors.saturating_add(excess);
    }
    Ok(ParsedStylesheet {
        stylesheet: Stylesheet::new(rules),
        errors: split.errors,
        dropped_errors: split.dropped_errors,
        skipped_at_rules: split.skipped_at_rules,
    })
}

/// 1 つの `<style>` 要素から構築したスタイルシート。
#[derive(Debug, Clone)]
pub struct StyleElementSheet {
    node: NodeId,
    parsed: ParsedStylesheet,
}

impl StyleElementSheet {
    /// 元の `<style>` 要素のノード。
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// 構築結果。
    pub fn parsed(&self) -> &ParsedStylesheet {
        &self.parsed
    }
}

/// `style` 属性を持つ要素と、その宣言列（#260 がマッチ結果とは別の inline 入力として使う）。
#[derive(Debug, Clone)]
pub struct InlineStyle {
    node: NodeId,
    declarations: Vec<Declaration>,
}

impl InlineStyle {
    /// `style` 属性を持つ要素のノード。
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// 属性値を解析した宣言（ソース順。`style=""` は空）。
    pub fn declarations(&self) -> &[Declaration] {
        &self.declarations
    }
}

/// 上限違反で捨てたスタイル源の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StyleSourceKind {
    /// `<style>` 要素。
    StyleElement,
    /// `style` 属性。
    InlineStyle,
}

/// 上限違反で捨てたスタイル源の記録（ノードと種別のみ。入力テキストは含めない）。
///
/// [`collect_document_styles`] が作り、呼び出し側（TASK-100 等）が「黙って捨てた」ことを
/// 観測できるようにする（`REPAIR-3`・`REPAIR-4`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkippedStyleSource {
    node: NodeId,
    kind: StyleSourceKind,
}

impl SkippedStyleSource {
    /// 捨てたスタイル源を持つ要素のノード。
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// 捨てたスタイル源の種別。
    pub fn kind(&self) -> StyleSourceKind {
        self.kind
    }
}

/// 文書から集めたスタイル源（`<style>` 要素のシートと inline 宣言。いずれも文書順）。
#[derive(Debug, Clone, Default)]
pub struct DocumentStyles {
    style_sheets: Vec<StyleElementSheet>,
    inline_styles: Vec<InlineStyle>,
    skipped: Vec<SkippedStyleSource>,
    dropped_skipped: usize,
}

impl DocumentStyles {
    /// `<style>` 要素ごとのスタイルシート（文書順）。
    pub fn style_sheets(&self) -> &[StyleElementSheet] {
        &self.style_sheets
    }

    /// `style` 属性を持つ要素ごとの宣言列（文書順）。
    pub fn inline_styles(&self) -> &[InlineStyle] {
        &self.inline_styles
    }

    /// 上限違反で捨てたスタイル源（文書順・最大 [`MAX_SKIPPED_STYLE_SOURCES`] 件）。
    pub fn skipped_sources(&self) -> &[SkippedStyleSource] {
        &self.skipped
    }

    /// 記録上限を超えて記録しなかった、捨てたスタイル源の件数。
    pub fn dropped_skipped_sources(&self) -> usize {
        self.dropped_skipped
    }

    fn push_skipped(&mut self, node: NodeId, kind: StyleSourceKind) {
        if self.skipped.len() >= MAX_SKIPPED_STYLE_SOURCES {
            self.dropped_skipped = self.dropped_skipped.saturating_add(1);
        } else {
            self.skipped.push(SkippedStyleSource { node, kind });
        }
    }
}

/// 文書全体の累積バイト数・ルール数の予算。採用したスタイル源だけを加算する。
#[derive(Debug, Default)]
struct StyleBudget {
    bytes: usize,
    rules: usize,
    /// 採否に関係なく処理対象にした源の件数。
    sources: usize,
    /// 採否に関係なく処理対象にした源の累積バイト数。
    attempted_bytes: usize,
}

impl StyleBudget {
    /// 源を処理対象にしてよいか判定し、よければ件数・試行バイト数へ加算する。
    ///
    /// 採否に関係なく処理した量を数え、パース・確保の前に頭打ちにする（空・捨てる源の
    /// 繰り返し処理による DoS 防止。`CORE-5`・TASK-105.7・#261）。false の源は何も処理しない。
    fn admit(&mut self, n: usize) -> bool {
        let attempted = self.attempted_bytes.saturating_add(n);
        if self.sources >= MAX_DOCUMENT_STYLE_SOURCES
            || attempted > MAX_DOCUMENT_STYLE_ATTEMPT_BYTES
        {
            return false;
        }
        self.sources += 1;
        self.attempted_bytes = attempted;
        true
    }

    /// 採用済みバイト数に `n` を足しても上限内か（加算はしない）。
    fn fits_bytes(&self, n: usize) -> bool {
        self.bytes.saturating_add(n) <= MAX_DOCUMENT_STYLE_BYTES
    }

    fn try_add_bytes(&mut self, n: usize) -> Result<()> {
        let total = self.bytes.saturating_add(n);
        if total > MAX_DOCUMENT_STYLE_BYTES {
            return Err(Error::InvalidInput {
                message: format!("document style input exceeds {MAX_DOCUMENT_STYLE_BYTES} bytes"),
            });
        }
        self.bytes = total;
        Ok(())
    }

    fn try_add_rules(&mut self, n: usize) -> Result<()> {
        let total = self.rules.saturating_add(n);
        if total > MAX_DOCUMENT_STYLE_RULES {
            return Err(Error::InvalidInput {
                message: format!("document rule count exceeds {MAX_DOCUMENT_STYLE_RULES}"),
            });
        }
        self.rules = total;
        Ok(())
    }
}

/// `<style>` を読み飛ばすべき `type` か（値が非空で `text/css` 以外。HTML 仕様に合わせる）。
fn style_type_is_not_css(value: &str) -> bool {
    let v = value.trim_matches(is_css_whitespace);
    !v.is_empty() && !v.eq_ignore_ascii_case("text/css")
}

/// `<style>` 要素の子テキストの合計バイト数（連結せずに数える。予算の事前検査用）。
fn style_element_text_len(document: &Document, id: NodeId) -> usize {
    document
        .children(id)
        .filter_map(|c| match document.node_data(c) {
            Some(NodeData::Text { contents }) => Some(contents.len()),
            _ => None,
        })
        .fold(0usize, |a, n| a.saturating_add(n))
}

/// `<style>` 要素の子テキストを連結する。累積長を確保前に検査する（無制限確保の防止）。
fn style_element_text(document: &Document, id: NodeId) -> Result<String> {
    let mut text = String::new();
    for child in document.children(id) {
        if let Some(NodeData::Text { contents }) = document.node_data(child) {
            if text.len().saturating_add(contents.len()) > MAX_STYLESHEET_INPUT_BYTES {
                return Err(Error::InvalidInput {
                    message: format!("stylesheet input exceeds {MAX_STYLESHEET_INPUT_BYTES} bytes"),
                });
            }
            text.push_str(contents);
        }
    }
    Ok(text)
}

/// 文書の `<style>` 要素と `style` 属性を収集し、`<style>` は [`Stylesheet`] へ構築する。
///
/// `<style>` は HTML 名前空間のものだけが対象（SVG は対象外）で、`type` が空以外かつ
/// `text/css` 以外なら読み飛ばす。`<template>` の contents は含まれない。`style` 属性は
/// 全名前空間の要素が対象。
///
/// 1 スタイル源の上限違反（[`Error::InvalidInput`]）はその源だけを捨てて
/// [`DocumentStyles::skipped_sources`] に記録し、残りを収集する。文書全体の累積バイト数・
/// ルール数・処理した源の件数・処理量の超過も同様にその源だけを捨てる（採用済みの源は失わない）。
/// `Err` は `InvalidInput` 以外のエラーのみ。単一要素の [`parse_style_attribute`] は従来どおり
/// 上限違反を `Err` にする。
/// 対応ビヘイビア: `CORE-5`（TASK-105.4.2・#552、TASK-105.7・#261）。
pub fn collect_document_styles(document: &Document) -> Result<DocumentStyles> {
    let mut out = DocumentStyles::default();
    let mut budget = StyleBudget::default();
    for id in document.descendants(document.root()) {
        if !document.is_element(id) {
            continue;
        }
        let is_html_style = document.namespace_url(id) == Some(HTML_NAMESPACE_URI)
            && document
                .local_name(id)
                .is_some_and(|n| html_local_name_eq(n, "style"));
        if is_html_style
            && !document
                .attribute(id, "type")
                .is_some_and(style_type_is_not_css)
        {
            // 連結・パースの前に、処理量（源の件数・試行バイト数）と採用の残り予算を確認する。
            // いずれの超過もその源だけを捨てる（採用済みの源は失わない）。
            let raw_len = style_element_text_len(document, id);
            if !budget.admit(raw_len) || !budget.fits_bytes(raw_len) {
                out.push_skipped(id, StyleSourceKind::StyleElement);
            } else {
                match style_element_text(document, id)
                    .and_then(|text| parse_stylesheet(&text).map(|parsed| (text.len(), parsed)))
                {
                    Ok((text_len, parsed)) => {
                        if budget.try_add_bytes(text_len).is_ok()
                            && budget
                                .try_add_rules(parsed.stylesheet().rules().len())
                                .is_ok()
                        {
                            out.style_sheets
                                .push(StyleElementSheet { node: id, parsed });
                        } else {
                            out.push_skipped(id, StyleSourceKind::StyleElement);
                        }
                    }
                    Err(Error::InvalidInput { .. }) => {
                        out.push_skipped(id, StyleSourceKind::StyleElement);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        if let Some(value) = document.attribute(id, "style") {
            if !budget.admit(value.len()) || !budget.fits_bytes(value.len()) {
                out.push_skipped(id, StyleSourceKind::InlineStyle);
            } else {
                match parse_declarations(value) {
                    Ok(declarations) => {
                        if budget.try_add_bytes(value.len()).is_ok() {
                            out.inline_styles.push(InlineStyle {
                                node: id,
                                declarations,
                            });
                        } else {
                            out.push_skipped(id, StyleSourceKind::InlineStyle);
                        }
                    }
                    Err(Error::InvalidInput { .. }) => {
                        out.push_skipped(id, StyleSourceKind::InlineStyle);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(out)
}

/// 単一要素の `style` 属性を宣言列へ解析する。#260 が要素単位で呼ぶ入口。
///
/// 属性なし・要素以外・範囲外は `Ok(None)`、`style=""` は `Ok(Some(vec![]))`。
/// 対応ビヘイビア: `CORE-5`（TASK-105.4.2・#552）。
pub fn parse_style_attribute(document: &Document, id: NodeId) -> Result<Option<Vec<Declaration>>> {
    document
        .attribute(id, "style")
        .map(parse_declarations)
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cssom::{MAX_DECLARATION_INPUT_BYTES, MAX_DECLARATIONS_PER_BLOCK};

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
    fn core_5_at_rule_paren_brace_does_not_end_skip() {
        assert_eq!(
            pb("@import url(foo}bar); a{color:red}"),
            vec![t("a", "color:red", 22)]
        );
    }

    #[test]
    fn core_5_comment_is_not_descendant_combinator() {
        assert_eq!(pb("a/**/.b{x:y}")[0].0, "a.b");
        assert_eq!(pb("a /**/.b{x:y}")[0].0, "a .b");
        assert_eq!(pb("a/**/b{x:y}")[0].0, "a b");
    }

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

    /// CORE-5: at-rule 直後の余分な `}` も構造エラーとして記録し、後続ルールは取る。
    #[test]
    fn core_5_stray_close_brace_after_at_rule() {
        let r = split_rule_blocks("@unknown } a{b:c}").expect("must split");
        assert_eq!(r.errors().len(), 1);
        assert_eq!(
            r.errors()[0].kind(),
            RuleBlockErrorKind::UnexpectedCloseBrace
        );
        assert_eq!(r.errors()[0].offset(), 9);
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

    fn rules_of(css: &str) -> ParsedStylesheet {
        parse_stylesheet(css).expect("must parse")
    }

    /// CORE-5: 外部文字列から 2 ルールを具体値で構築する。
    #[test]
    fn core_5_parse_stylesheet_builds_rules() {
        let p = rules_of("a{color:red} .b{margin:0 !important}");
        assert_eq!(p.stylesheet().len(), 2);
        let d0 = p.stylesheet().rules()[0].declarations();
        assert_eq!(
            (d0[0].property(), d0[0].value(), d0[0].importance()),
            ("color", "red", crate::cssom::Importance::Normal)
        );
        let d1 = p.stylesheet().rules()[1].declarations();
        assert_eq!(
            (d1[0].property(), d1[0].value(), d1[0].importance()),
            ("margin", "0", crate::cssom::Importance::Important)
        );
        assert!(p.errors().is_empty());
    }

    /// CORE-5: 未対応セレクタのルールだけ捨てて記録する。
    #[test]
    fn core_5_unsupported_selector_recorded() {
        let p = rules_of("a:hover{color:red} p{top:1px}");
        assert_eq!(p.stylesheet().len(), 1);
        assert_eq!(p.errors().len(), 1);
        assert_eq!(
            p.errors()[0].kind(),
            RuleBlockErrorKind::UnsupportedSelector
        );
        assert_eq!(p.errors()[0].offset(), 0);
    }

    /// CORE-5: 構文不正セレクタは InvalidSelector、後続は残る。
    #[test]
    fn core_5_invalid_selector_recorded() {
        let p = rules_of("p{a:b} a>>b{c:d} q{e:f}");
        assert_eq!(p.stylesheet().len(), 2);
        assert_eq!(p.errors().len(), 1);
        assert_eq!(p.errors()[0].kind(), RuleBlockErrorKind::InvalidSelector);
        assert_eq!(p.errors()[0].offset(), 7);
    }

    /// CORE-5: 宣言ブロックの上限超過は InvalidDeclarations、正常ルールは残る。
    #[test]
    fn core_5_invalid_declarations_recorded() {
        let big = "x".repeat(crate::cssom::MAX_DECLARATION_INPUT_BYTES + 1);
        let p = rules_of(&format!("a{{{big}}} b{{c:d}}"));
        assert_eq!(p.stylesheet().len(), 1);
        assert_eq!(p.errors().len(), 1);
        assert_eq!(
            p.errors()[0].kind(),
            RuleBlockErrorKind::InvalidDeclarations
        );
        assert_eq!(p.errors()[0].offset(), 0);
    }

    /// CORE-5: 構造エラーと解析エラーは offset 昇順で並び、at-rule 件数を引き継ぐ。
    #[test]
    fn core_5_errors_sorted_by_offset() {
        let p = rules_of("a:hover{x:y} @import 'x'; } b{c:d}");
        let got: Vec<_> = p.errors().iter().map(|e| (e.kind(), e.offset())).collect();
        assert_eq!(
            got,
            vec![
                (RuleBlockErrorKind::UnsupportedSelector, 0),
                (RuleBlockErrorKind::UnexpectedCloseBrace, 26),
            ]
        );
        assert_eq!(p.skipped_at_rules(), 1);
        assert_eq!(p.stylesheet().len(), 1);
    }

    /// CORE-5: エラー記録は上限で止まり超過分は件数のみ。
    #[test]
    fn core_5_parse_error_cap() {
        let p = rules_of(&"a:hover{x:y}".repeat(MAX_RULE_BLOCK_ERRORS + 3));
        assert_eq!(p.errors().len(), MAX_RULE_BLOCK_ERRORS);
        assert_eq!(p.dropped_errors(), 3);
    }

    /// CORE-5: 宣言 0 件のルールは残る。空入力は空シート。
    #[test]
    fn core_5_empty_rule_kept_and_empty_input() {
        let p = rules_of("a{}");
        assert_eq!(p.stylesheet().len(), 1);
        assert!(p.stylesheet().rules()[0].declarations().is_empty());
        let e = rules_of("");
        assert!(e.stylesheet().is_empty());
        assert!(e.errors().is_empty());
    }

    /// CORE-5: 上限違反は Err のまま。
    /// 後半の大量の余分な `}` があっても、前半の解析エラーが ソース順・上限件数の契約で残る（CORE-5）。
    #[test]
    fn core_5_parse_errors_not_displaced_by_late_structural() {
        let css = format!("a:hover{{x:y}} {}", "} ".repeat(MAX_RULE_BLOCK_ERRORS + 10));
        let r = parse_stylesheet(&css).expect("must parse");
        assert_eq!(r.errors().len(), MAX_RULE_BLOCK_ERRORS);
        assert_eq!(r.errors()[0].offset(), 0);
        assert_eq!(
            r.errors()[0].kind(),
            RuleBlockErrorKind::UnsupportedSelector
        );
        assert_eq!(r.dropped_errors(), 11);
        assert!(
            r.errors()
                .windows(2)
                .all(|w| w[0].offset() <= w[1].offset())
        );
    }

    #[test]
    fn core_5_parse_stylesheet_limits_are_err() {
        let ng = " ".repeat(MAX_STYLESHEET_INPUT_BYTES + 1);
        assert!(matches!(
            parse_stylesheet(&ng),
            Err(Error::InvalidInput { .. })
        ));
        let ng = "a{}".repeat(MAX_RULES_PER_STYLESHEET + 1);
        assert!(matches!(
            parse_stylesheet(&ng),
            Err(Error::InvalidInput { .. })
        ));
    }

    fn doc_of(html: &str) -> Document {
        crate::parse_document(html, &crate::ParseOptions::default())
            .expect("must parse")
            .document
    }

    /// CORE-5（TASK-105.7）: 文書累積バイト数は上限ちょうどで Ok・+1 で Err。
    #[test]
    fn core_5_budget_bytes_boundary() {
        let mut b = StyleBudget::default();
        b.try_add_bytes(MAX_DOCUMENT_STYLE_BYTES).expect("at limit");
        match b.try_add_bytes(1) {
            Err(Error::InvalidInput { message }) => {
                assert_eq!(message, "document style input exceeds 16777216 bytes");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // 飽和加算: usize::MAX でも panic しない。
        assert!(b.try_add_bytes(usize::MAX).is_err());
    }

    /// CORE-5（TASK-105.7）: 文書累積ルール数は上限ちょうどで Ok・+1 で Err。
    #[test]
    fn core_5_budget_rules_boundary() {
        let mut b = StyleBudget::default();
        b.try_add_rules(MAX_DOCUMENT_STYLE_RULES).expect("at limit");
        match b.try_add_rules(1) {
            Err(Error::InvalidInput { message }) => {
                assert_eq!(message, "document rule count exceeds 65536");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// CORE-5（TASK-105.7）: 過大な `<style>` は捨てて記録し、残りを収集する。
    #[test]
    fn core_5_oversize_style_element_is_skipped_not_fatal() {
        let big = " ".repeat(MAX_STYLESHEET_INPUT_BYTES + 1);
        let html = format!(
            "<html><head><style>{big}</style><style>a{{color:red}}</style></head>\
             <body><p style=\"top:1px\">x</p></body></html>"
        );
        let doc = doc_of(&html);
        let styles = collect_document_styles(&doc).expect("must collect");
        assert_eq!(styles.style_sheets().len(), 1);
        assert_eq!(styles.inline_styles().len(), 1);
        assert_eq!(styles.skipped_sources().len(), 1);
        assert_eq!(
            styles.skipped_sources()[0].kind(),
            StyleSourceKind::StyleElement
        );
        assert_eq!(styles.dropped_skipped_sources(), 0);
    }

    /// CORE-5（TASK-105.7）: ルール数超過の `<style>` も源単位で捨てる。
    #[test]
    fn core_5_too_many_rules_style_is_skipped() {
        let css = "a{}".repeat(MAX_RULES_PER_STYLESHEET + 1);
        let doc = doc_of(&format!("<style>{css}</style><style>b{{c:d}}</style>"));
        let styles = collect_document_styles(&doc).expect("must collect");
        assert_eq!(styles.style_sheets().len(), 1);
        assert_eq!(styles.skipped_sources().len(), 1);
    }

    /// CORE-5（TASK-105.7）: 過大・宣言数超過の `style` 属性は該当要素だけ捨てる。
    #[test]
    fn core_5_oversize_inline_style_is_skipped() {
        let big = format!("a:{}", "x".repeat(MAX_DECLARATION_INPUT_BYTES));
        let many = "a:b;".repeat(MAX_DECLARATIONS_PER_BLOCK + 1);
        let html =
            format!("<p style=\"{big}\">1</p><p style=\"{many}\">2</p><p style=\"c:d\">3</p>");
        let doc = doc_of(&html);
        let styles = collect_document_styles(&doc).expect("must collect");
        assert_eq!(styles.inline_styles().len(), 1);
        assert_eq!(styles.skipped_sources().len(), 2);
        assert!(
            styles
                .skipped_sources()
                .iter()
                .all(|s| s.kind() == StyleSourceKind::InlineStyle)
        );
    }

    /// CORE-5（TASK-105.7）: 文書全体のルール累積が上限を超えた源は捨てる（Err にしない）。
    #[test]
    fn core_5_document_rule_budget_skips_excess_source() {
        let sheet = format!("<style>{}</style>", "a{}".repeat(MAX_RULES_PER_STYLESHEET));
        let four = doc_of(&sheet.repeat(4));
        let styles = collect_document_styles(&four).expect("4 sheets fit");
        assert_eq!(styles.style_sheets().len(), 4);
        let five = doc_of(&format!("{}<style>a{{}}</style>", sheet.repeat(4)));
        let styles = collect_document_styles(&five).expect("must collect");
        assert_eq!(styles.style_sheets().len(), 4);
        assert_eq!(styles.skipped_sources().len(), 1);
    }

    /// CORE-5（TASK-105.7）: 源の件数上限を超えた分は空でも捨てて記録する。
    #[test]
    fn core_5_source_count_is_capped() {
        let html = "<style></style>".repeat(MAX_DOCUMENT_STYLE_SOURCES + 5);
        let doc = doc_of(&html);
        let styles = collect_document_styles(&doc).expect("must collect");
        assert_eq!(styles.style_sheets().len(), MAX_DOCUMENT_STYLE_SOURCES);
        assert_eq!(styles.skipped_sources().len(), 5);
    }

    /// CORE-5（TASK-105.7）: 残り予算を超える源は Err にせず、採用済みの源を保つ。
    #[test]
    fn core_5_budget_overflow_source_keeps_adopted_styles() {
        let mut b = StyleBudget::default();
        b.try_add_bytes(MAX_DOCUMENT_STYLE_BYTES - 1).expect("fits");
        assert!(!b.fits_bytes(2));
        assert!(b.fits_bytes(1));
    }

    /// CORE-5（TASK-105.7）: 処理量（試行バイト数）の上限を超える源はパースせず捨てる。
    #[test]
    fn core_5_attempted_bytes_are_capped() {
        let mut b = StyleBudget::default();
        assert!(b.admit(MAX_DOCUMENT_STYLE_ATTEMPT_BYTES));
        assert!(!b.admit(1));
        assert!(!StyleBudget::default().admit(usize::MAX));
    }

    /// CORE-5（TASK-105.7）: skip 記録は上限まで、超過分は件数のみ。
    #[test]
    fn core_5_skipped_records_are_capped() {
        let mut d = DocumentStyles::default();
        let node = doc_of("<p>x</p>").root();
        for _ in 0..MAX_SKIPPED_STYLE_SOURCES + 3 {
            d.push_skipped(node, StyleSourceKind::InlineStyle);
        }
        assert_eq!(d.skipped_sources().len(), MAX_SKIPPED_STYLE_SOURCES);
        assert_eq!(d.dropped_skipped_sources(), 3);
    }
}

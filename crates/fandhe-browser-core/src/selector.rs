//! selector: CSS セレクタ文字列を AST（構文木）へ解析するモジュール。
//!
//! 役割: `query`（TASK-24.10・Issue #418）が自作 DOM（TASK-24.5・#39）へ
//! 照合する前段として、セレクタ文字列を型付きの AST に変換する。DOM への
//! 照合そのもの・`query_selector` / `query_selector_all` 相当の API は
//! 本モジュールの対象外であり、#418 が本モジュールの出力を入力として使う。
//! 将来は `cdp`（`DOM.querySelector` 系）・`ai`（簡約 DOM 抽出）からも
//! 間接的に使われる想定である。
//!
//! 自作する理由: PoC-2 の core-proto は `scraper`（内部で `selectors` /
//! `cssparser` に依存）を使っていたが、それらは MPL-2.0 を引き込むため
//! Issue #35 の依存承認判断で不採用となった。本モジュールは stdlib のみで
//! セレクタ解析を実装する（dependency-policy.md）。
//!
//! 対応する構文（サブセット）: 型セレクタ（`div`）・ID（`#id`）・
//! クラス（`.class`）・属性の存在（`[a]`）・属性の完全一致
//! （`[a=v]` / `[a="v"]` / `[a='v']`）・子孫結合子（空白）・子結合子（`>`）・
//! カンマ区切りのセレクタリスト。
//!
//! 対象外の構文（`Error::Unsupported` を返す）: `*`（全称セレクタ）・
//! 疑似クラス / 疑似要素（`:`・`::`）・隣接 / 一般兄弟結合子（`+` / `~`）・
//! 名前空間（`|`）・`=` 以外の属性演算子（`~=` `|=` `^=` `$=` `*=`）・
//! 属性フラグ（`i` / `s`）・エスケープ（`\`）・入れ子（`&`）。
//! これらは実需が出た段階で別 Issue で拡張する（各 enum は
//! `#[non_exhaustive]` のため非破壊で追加できる）。
//!
//! 詳細度（specificity）の計算は本モジュールでは行わない。TASK-105（CSSOM・
//! MS-8）で本モジュールの AST を再利用して追加する予定（REPAIR-3: 実装済みを
//! 装わない）。
//!
//! 対応 ID: `CORE-1`・TASK-24（24.7）・MS-1。

use crate::error::{Error, Result};

/// 解析対象として受け付ける入力の最大バイト数。
///
/// これを超える入力は、`String` / `Vec` を 1 つも確保する前に
/// `Error::InvalidInput` を返す（外部入力に対する DoS 対策。security.md
/// 「不安全な設計」）。TASK-24.10（#418）・CDP の実運用で不足した場合は
/// この定数の変更で対応する（暫定値）。
pub const MAX_SELECTOR_INPUT_BYTES: usize = 4096;

/// カンマ区切りセレクタリストに含められる複雑セレクタの最大件数。
pub const MAX_SELECTORS_PER_LIST: usize = 64;

/// 1 個の複雑セレクタに含められる複合セレクタの最大件数
/// （結合子で連結される複合セレクタの総数。先頭の複合セレクタを含む）。
pub const MAX_COMPOUNDS_PER_COMPLEX: usize = 32;

/// 1 個の複合セレクタに含められる単純セレクタ（ID・クラス・属性）の
/// 最大件数。型名（`div` 等）はこの件数に数えない。
pub const MAX_SIMPLE_SELECTORS_PER_COMPOUND: usize = 32;

/// CSS の空白文字（U+000B 垂直タブは含まない。CSS Syntax の whitespace 定義）。
fn is_css_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0C}')
}

/// 制御文字（whitespace を除く U+0000〜U+001F と U+007F）。
///
/// 外部入力にこれらが含まれる場合は構文的に不正として `InvalidInput` を返す。
fn is_control_char(c: char) -> bool {
    (('\u{0}'..='\u{1F}').contains(&c) && !is_css_whitespace(c)) || c == '\u{7F}'
}

/// 識別子の先頭に置ける文字（ASCII 英字・`_`・非 ASCII）。
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

/// 識別子の先頭またはハイフンの直後に置ける文字
/// （先頭文字 + `-` 自身。`-foo` のような開始を許すため）。
fn is_ident_start_or_hyphen(c: char) -> bool {
    is_ident_start(c) || c == '-'
}

/// 識別子の 2 文字目以降に置ける文字（英数字・`_`・`-`・非 ASCII）。
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii()
}

/// セレクタリスト全体。1 個以上の [`ComplexSelector`] を持つ
/// （空リストにはならない不変条件を `selectors()` 経由でのみ公開して守る）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorList {
    selectors: Vec<ComplexSelector>,
}

impl SelectorList {
    /// リストが持つ複雑セレクタを参照する（カンマ区切りの各要素に対応）。
    pub fn selectors(&self) -> &[ComplexSelector] {
        &self.selectors
    }
}

impl std::str::FromStr for SelectorList {
    type Err = Error;

    fn from_str(input: &str) -> Result<Self> {
        parse_selector_list(input)
    }
}

/// 複合セレクタを結合子で連結した複雑セレクタ（例: `div > span.a b`）。
///
/// `first` が最も左の複合セレクタ、`rest` が右側へ続く
/// `(結合子, 複合セレクタ)` の列。#418 が右から左へ照合する際に
/// `rest` を逆順に辿れるようにこの形にしてある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplexSelector {
    first: CompoundSelector,
    rest: Vec<(Combinator, CompoundSelector)>,
}

impl ComplexSelector {
    /// 最も左の複合セレクタを参照する。
    pub fn first(&self) -> &CompoundSelector {
        &self.first
    }

    /// 先頭以降、結合子と複合セレクタの組を左から右の順に参照する。
    pub fn rest(&self) -> &[(Combinator, CompoundSelector)] {
        &self.rest
    }
}

/// 型名・ID・クラス・属性セレクタが単一要素にかかる複合セレクタ
/// （例: `a.link#top[href]`）。型名か単純セレクタのどちらかが
/// 1 個以上ある（空にはならない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundSelector {
    type_name: Option<String>,
    simple_selectors: Vec<SimpleSelector>,
}

impl CompoundSelector {
    /// 型名（ASCII 小文字に正規化済み）。型セレクタを持たない場合は `None`。
    pub fn type_name(&self) -> Option<&str> {
        self.type_name.as_deref()
    }

    /// ID・クラス・属性セレクタの列を参照する。
    pub fn simple_selectors(&self) -> &[SimpleSelector] {
        &self.simple_selectors
    }
}

/// 複合セレクタを構成する単純セレクタ。
///
/// 詳細度計算（TASK-105）が種別ごとに再パースなしで扱えるよう、
/// 種類ごとに variant を分けてある。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SimpleSelector {
    /// `#id`。値は大文字小文字を区別したまま保持する。
    Id(String),
    /// `.class`。値は大文字小文字を区別したまま保持する。
    Class(String),
    /// `[name]` / `[name=value]`。
    Attribute(AttributeSelector),
}

/// 属性セレクタ（`[name]` または `[name=value]`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeSelector {
    /// 属性名（ASCII 小文字に正規化済み）。
    pub name: String,
    /// 照合方法。
    pub matcher: AttributeMatcher,
}

/// 属性セレクタの照合方法。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttributeMatcher {
    /// `[name]`: 属性が存在するかどうかのみを見る。
    Exists,
    /// `[name=value]`: 属性値が完全一致するかを見る
    /// （値は大文字小文字を区別したまま保持する）。
    Equals(String),
}

/// 複合セレクタ間の結合子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Combinator {
    /// 空白（子孫結合子）。
    Descendant,
    /// `>`（子結合子）。
    Child,
}

/// 入力文字列を走査するカーソル。バイトオフセットを保持し、
/// マルチバイト文字の境界を跨いだ添字アクセスをしない
/// （外部入力の走査は `str::get` / `chars()` のみで行う。coding-rust.md）。
struct Cursor<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a str) -> Self {
        Cursor { input, pos: 0 }
    }

    /// 現在位置の文字を読み取る（消費しない）。入力末尾では `None`。
    fn peek(&self) -> Option<char> {
        self.input.get(self.pos..).and_then(|s| s.chars().next())
    }

    /// 現在位置の文字を消費して 1 文字分進める。
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// 現在のバイトオフセット（エラーメッセージ用）。
    fn offset(&self) -> usize {
        self.pos
    }

    /// 入力末尾まで読み切ったか。
    fn is_eof(&self) -> bool {
        self.peek().is_none()
    }

    /// CSS whitespace を読み飛ばし、1 文字以上読み飛ばしたかを返す。
    fn skip_whitespace(&mut self) -> bool {
        let mut skipped = false;
        while let Some(c) = self.peek() {
            if is_css_whitespace(c) {
                self.bump();
                skipped = true;
            } else {
                break;
            }
        }
        skipped
    }
}

/// 構文として不正な入力に対するエラーを組み立てる
/// （入力文字列そのものは埋め込まず、種別とバイトオフセットのみを含める）。
fn invalid_input_at(offset: usize, reason: &str) -> Error {
    Error::InvalidInput {
        message: format!("{reason} at byte offset {offset}"),
    }
}

/// サブセット外の構文に対するエラーを組み立てる。
fn unsupported_at(offset: usize, feature: &str) -> Error {
    Error::Unsupported {
        message: format!("unsupported selector syntax ({feature}) at byte offset {offset}"),
    }
}

/// 識別子を解析する（型名・ID・クラス・属性名の下地として共用）。
/// 呼び出し時点でカーソルは識別子の先頭を指している前提。
fn parse_ident(cursor: &mut Cursor<'_>) -> Result<String> {
    let start_offset = cursor.offset();
    let first = cursor
        .peek()
        .ok_or_else(|| invalid_input_at(start_offset, "expected identifier, found end of input"))?;

    if first == '\\' {
        return Err(unsupported_at(start_offset, "escape"));
    }
    if !is_ident_start_or_hyphen(first) {
        return Err(invalid_input_at(start_offset, "expected identifier"));
    }
    if first == '-' {
        // `-` 単独、または `-` の次が識別子として続けられない文字なら不正
        // （数字始まり `#1` 相当を弾く目的も含む）。
        let next_offset = start_offset + first.len_utf8();
        let next = cursor
            .input
            .get(next_offset..)
            .and_then(|s| s.chars().next());
        match next {
            Some(c) if is_ident_start_or_hyphen(c) => {}
            _ => return Err(invalid_input_at(start_offset, "invalid identifier")),
        }
    }

    let mut ident = String::new();
    ident.push(first);
    cursor.bump();

    while let Some(c) = cursor.peek() {
        if c == '\\' {
            return Err(unsupported_at(cursor.offset(), "escape"));
        }
        if is_ident_continue(c) {
            ident.push(c);
            cursor.bump();
        } else {
            break;
        }
    }

    Ok(ident)
}

/// 引用符付き文字列（`'...'` / `"..."`）を解析する。
/// 呼び出し時点でカーソルは開き引用符を指している前提。
fn parse_quoted_string(cursor: &mut Cursor<'_>) -> Result<String> {
    let quote_offset = cursor.offset();
    let quote = cursor
        .bump()
        .ok_or_else(|| invalid_input_at(quote_offset, "expected quote"))?;

    let mut value = String::new();
    loop {
        let c = cursor
            .peek()
            .ok_or_else(|| invalid_input_at(quote_offset, "unterminated string"))?;
        if c == quote {
            cursor.bump();
            return Ok(value);
        }
        if c == '\\' {
            return Err(unsupported_at(cursor.offset(), "escape"));
        }
        if c == '\n' || c == '\r' || c == '\u{0C}' {
            return Err(invalid_input_at(cursor.offset(), "newline in string"));
        }
        if is_control_char(c) {
            return Err(invalid_input_at(cursor.offset(), "control character"));
        }
        value.push(c);
        cursor.bump();
    }
}

/// 属性セレクタ（`[` から `]` まで）を解析する。
/// 呼び出し時点でカーソルは `[` を指している前提。
fn parse_attribute_selector(cursor: &mut Cursor<'_>) -> Result<AttributeSelector> {
    let open_offset = cursor.offset();
    cursor.bump(); // `[`
    cursor.skip_whitespace();

    let name_offset = cursor.offset();
    let name = match cursor.peek() {
        Some(c) if is_ident_start_or_hyphen(c) => parse_ident(cursor)?,
        Some('\\') => return Err(unsupported_at(name_offset, "escape")),
        _ => return Err(invalid_input_at(name_offset, "expected attribute name")),
    };
    let name = name.to_ascii_lowercase();

    cursor.skip_whitespace();

    let matcher = match cursor.peek() {
        Some(']') => {
            cursor.bump();
            AttributeMatcher::Exists
        }
        Some('=') => {
            cursor.bump();
            cursor.skip_whitespace();
            let value_offset = cursor.offset();
            let value = match cursor.peek() {
                Some('\'') | Some('"') => parse_quoted_string(cursor)?,
                Some(c) if is_ident_start_or_hyphen(c) => parse_ident(cursor)?,
                Some('\\') => return Err(unsupported_at(value_offset, "escape")),
                _ => return Err(invalid_input_at(value_offset, "expected attribute value")),
            };
            cursor.skip_whitespace();
            // 属性フラグ（`i` / `s`）は値の直後に識別子が続く形で現れる。
            if let Some(c) = cursor.peek()
                && is_ident_start_or_hyphen(c)
            {
                return Err(unsupported_at(cursor.offset(), "attribute flag"));
            }
            match cursor.peek() {
                Some(']') => {
                    cursor.bump();
                }
                _ => return Err(invalid_input_at(cursor.offset(), "expected ']'")),
            }
            AttributeMatcher::Equals(value)
        }
        Some('~') | Some('|') | Some('^') | Some('$') | Some('*') => {
            return Err(unsupported_at(cursor.offset(), "attribute operator"));
        }
        _ => {
            return Err(invalid_input_at(
                open_offset,
                "unterminated attribute selector",
            ));
        }
    };

    Ok(AttributeSelector { name, matcher })
}

/// 複合セレクタ（型名 + `#`/`.`/`[` の繰り返し）を解析する。
/// 呼び出し時点でカーソルは複合セレクタの先頭を指している前提。
fn parse_compound_selector(cursor: &mut Cursor<'_>) -> Result<CompoundSelector> {
    let start_offset = cursor.offset();
    let mut type_name = None;
    let mut simple_selectors = Vec::new();

    if let Some(c) = cursor.peek()
        && is_ident_start_or_hyphen(c)
    {
        let ident = parse_ident(cursor)?;
        type_name = Some(ident.to_ascii_lowercase());
    }

    loop {
        match cursor.peek() {
            Some('#') => {
                let hash_offset = cursor.offset();
                cursor.bump();
                let ident_offset = cursor.offset();
                match cursor.peek() {
                    Some(c) if is_ident_start_or_hyphen(c) => {
                        let ident = parse_ident(cursor)?;
                        if simple_selectors.len() >= MAX_SIMPLE_SELECTORS_PER_COMPOUND {
                            return Err(invalid_input_at(
                                hash_offset,
                                "too many simple selectors in compound selector",
                            ));
                        }
                        simple_selectors.push(SimpleSelector::Id(ident));
                    }
                    Some('\\') => return Err(unsupported_at(ident_offset, "escape")),
                    _ => {
                        return Err(invalid_input_at(
                            hash_offset,
                            "expected identifier after '#'",
                        ));
                    }
                }
            }
            Some('.') => {
                let dot_offset = cursor.offset();
                cursor.bump();
                let ident_offset = cursor.offset();
                match cursor.peek() {
                    Some(c) if is_ident_start_or_hyphen(c) => {
                        let ident = parse_ident(cursor)?;
                        if simple_selectors.len() >= MAX_SIMPLE_SELECTORS_PER_COMPOUND {
                            return Err(invalid_input_at(
                                dot_offset,
                                "too many simple selectors in compound selector",
                            ));
                        }
                        simple_selectors.push(SimpleSelector::Class(ident));
                    }
                    Some('\\') => return Err(unsupported_at(ident_offset, "escape")),
                    _ => {
                        return Err(invalid_input_at(
                            dot_offset,
                            "expected identifier after '.'",
                        ));
                    }
                }
            }
            Some('[') => {
                let bracket_offset = cursor.offset();
                let attr = parse_attribute_selector(cursor)?;
                if simple_selectors.len() >= MAX_SIMPLE_SELECTORS_PER_COMPOUND {
                    return Err(invalid_input_at(
                        bracket_offset,
                        "too many simple selectors in compound selector",
                    ));
                }
                simple_selectors.push(SimpleSelector::Attribute(attr));
            }
            _ => break,
        }
    }

    if type_name.is_none() && simple_selectors.is_empty() {
        return Err(classify_compound_start_error(cursor, start_offset));
    }

    Ok(CompoundSelector {
        type_name,
        simple_selectors,
    })
}

/// 複合セレクタの先頭で何も解析できなかった場合に、次に来ている文字から
/// 適切なエラー（`Unsupported` か `InvalidInput` か）を判定する。
fn classify_compound_start_error(cursor: &Cursor<'_>, start_offset: usize) -> Error {
    match cursor.peek() {
        Some('*') => unsupported_at(start_offset, "universal selector"),
        Some(':') => unsupported_at(start_offset, "pseudo-class"),
        Some('&') => unsupported_at(start_offset, "nesting"),
        Some('|') => unsupported_at(start_offset, "namespace"),
        Some('\\') => unsupported_at(start_offset, "escape"),
        Some('+') | Some('~') => unsupported_at(start_offset, "sibling combinator"),
        None => invalid_input_at(
            start_offset,
            "expected compound selector, found end of input",
        ),
        Some(_) => invalid_input_at(start_offset, "expected compound selector"),
    }
}

/// 複合セレクタ間の結合子を解析する。次の複合セレクタが続く場合は
/// `Some((結合子, 消費した空白の有無))` を返し、複雑セレクタの終端
/// （カンマ・入力末尾）ならそのまま `None` を返す。
fn parse_combinator(cursor: &mut Cursor<'_>) -> Result<Option<Combinator>> {
    let had_leading_whitespace = cursor.skip_whitespace();

    match cursor.peek() {
        None | Some(',') => Ok(None),
        Some('>') => {
            let child_offset = cursor.offset();
            cursor.bump();
            cursor.skip_whitespace();
            match cursor.peek() {
                None | Some(',') | Some('>') => {
                    Err(invalid_input_at(child_offset, "dangling child combinator"))
                }
                _ => Ok(Some(Combinator::Child)),
            }
        }
        Some('+') | Some('~') => Err(unsupported_at(cursor.offset(), "sibling combinator")),
        _ if had_leading_whitespace => Ok(Some(Combinator::Descendant)),
        _ => {
            // 空白を挟まず、`>`/`,`/EOF/兄弟結合子でもない場合は、複合セレクタの
            // 解析側（`classify_compound_start_error` 相当の判定）に委ねる。
            // ここでは「結合子ではない」として扱い、複合セレクタの解析を試み、
            // その結果がそのままエラーになるようにする。
            Ok(Some(Combinator::Descendant))
        }
    }
}

/// 複雑セレクタ（複合セレクタ + 結合子の連なり）を解析する。
fn parse_complex_selector(cursor: &mut Cursor<'_>) -> Result<ComplexSelector> {
    let first = parse_compound_selector(cursor)?;
    let mut rest = Vec::new();

    while let Some(combinator) = parse_combinator(cursor)? {
        let compound_offset = cursor.offset();
        let compound = parse_compound_selector(cursor)?;
        if rest.len() + 1 >= MAX_COMPOUNDS_PER_COMPLEX {
            return Err(invalid_input_at(
                compound_offset,
                "too many compound selectors in complex selector",
            ));
        }
        rest.push((combinator, compound));
    }

    Ok(ComplexSelector { first, rest })
}

/// セレクタ文字列（カンマ区切りリストを含む）を [`SelectorList`] へ解析する。
///
/// 入力バイト数の上限（[`MAX_SELECTOR_INPUT_BYTES`]）を、`String` / `Vec` を
/// 1 つも確保する前に検証する。以降、複雑セレクタ・複合セレクタ・単純
/// セレクタを `push` する直前にそれぞれの件数上限を検証する
/// （security.md「不安全な設計」・DoS 対策）。
///
/// # エラー
///
/// - サブセット外だが CSS として正しい構文（`*`・疑似クラス・兄弟結合子・
///   属性演算子の拡張・エスケープ等）には [`Error::Unsupported`] を返す。
/// - 空入力・宙に浮いた結合子・未終端の属性/文字列等、構文として不正な
///   入力には [`Error::InvalidInput`] を返す。
/// - いずれの場合も panic せず、エラーメッセージは入力文字列を含まない
///   （種別とバイトオフセットのみ。security.md）。
pub fn parse_selector_list(input: &str) -> Result<SelectorList> {
    if input.len() > MAX_SELECTOR_INPUT_BYTES {
        return Err(Error::InvalidInput {
            message: format!("selector input exceeds {MAX_SELECTOR_INPUT_BYTES} bytes limit"),
        });
    }

    let mut cursor = Cursor::new(input);
    cursor.skip_whitespace();
    if cursor.is_eof() {
        return Err(invalid_input_at(cursor.offset(), "empty selector list"));
    }

    let mut selectors = Vec::new();
    loop {
        let complex = parse_complex_selector(&mut cursor)?;
        if selectors.len() >= MAX_SELECTORS_PER_LIST {
            return Err(invalid_input_at(
                cursor.offset(),
                "too many selectors in selector list",
            ));
        }
        selectors.push(complex);

        cursor.skip_whitespace();
        match cursor.peek() {
            Some(',') => {
                cursor.bump();
                cursor.skip_whitespace();
                if cursor.is_eof() || cursor.peek() == Some(',') {
                    return Err(invalid_input_at(cursor.offset(), "empty selector in list"));
                }
            }
            None => break,
            Some(_) => {
                // parse_combinator / parse_complex_selector が終端まで
                // 消費し切らなかった場合（通常は起きない）の安全側フォールバック。
                return Err(invalid_input_at(
                    cursor.offset(),
                    "unexpected trailing input",
                ));
            }
        }
    }

    if !cursor.is_eof() {
        return Err(invalid_input_at(
            cursor.offset(),
            "unexpected trailing input",
        ));
    }

    Ok(SelectorList { selectors })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_type(name: &str) -> ComplexSelector {
        ComplexSelector {
            first: CompoundSelector {
                type_name: Some(name.to_string()),
                simple_selectors: Vec::new(),
            },
            rest: Vec::new(),
        }
    }

    /// CORE-1: 型セレクタ単体を解析できる。
    #[test]
    fn core_1_parses_type_selector() {
        let list = parse_selector_list("div").expect("div は解析できるはず");
        assert_eq!(list.selectors(), &[simple_type("div")]);
    }

    /// CORE-1: ID セレクタを解析できる。
    #[test]
    fn core_1_parses_id_selector() {
        let list = parse_selector_list("#main").expect("#main は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Id("main".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: クラスセレクタを解析できる。
    #[test]
    fn core_1_parses_class_selector() {
        let list = parse_selector_list(".item").expect(".item は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Class("item".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 属性の存在セレクタを解析できる。
    #[test]
    fn core_1_parses_attribute_exists_selector() {
        let list = parse_selector_list("[href]").expect("[href] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Attribute(AttributeSelector {
                    name: "href".to_string(),
                    matcher: AttributeMatcher::Exists,
                })],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 属性の完全一致セレクタ（識別子値）を解析できる。
    #[test]
    fn core_1_parses_attribute_equals_ident_value() {
        let list = parse_selector_list("[type=text]").expect("[type=text] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Attribute(AttributeSelector {
                    name: "type".to_string(),
                    matcher: AttributeMatcher::Equals("text".to_string()),
                })],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 属性の完全一致セレクタ（二重引用符の文字列値。空白を含む）を解析できる。
    #[test]
    fn core_1_parses_attribute_equals_double_quoted_value() {
        let list =
            parse_selector_list("[title=\"a b\"]").expect("[title=\"a b\"] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Attribute(AttributeSelector {
                    name: "title".to_string(),
                    matcher: AttributeMatcher::Equals("a b".to_string()),
                })],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 属性の完全一致セレクタ（単一引用符の文字列値）を解析できる。
    #[test]
    fn core_1_parses_attribute_equals_single_quoted_value() {
        let list = parse_selector_list("[data-x='v']").expect("[data-x='v'] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Attribute(AttributeSelector {
                    name: "data-x".to_string(),
                    matcher: AttributeMatcher::Equals("v".to_string()),
                })],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 空文字列値の属性セレクタを受け付ける。
    #[test]
    fn core_1_parses_attribute_equals_empty_value() {
        let list = parse_selector_list("[a=\"\"]").expect("[a=\"\"] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Attribute(AttributeSelector {
                    name: "a".to_string(),
                    matcher: AttributeMatcher::Equals(String::new()),
                })],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 型名・ID・クラス・属性が混在する複合セレクタを解析できる。
    #[test]
    fn core_1_parses_compound_selector_mix() {
        let list =
            parse_selector_list("a.link#top[href]").expect("a.link#top[href] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: Some("a".to_string()),
                simple_selectors: vec![
                    SimpleSelector::Class("link".to_string()),
                    SimpleSelector::Id("top".to_string()),
                    SimpleSelector::Attribute(AttributeSelector {
                        name: "href".to_string(),
                        matcher: AttributeMatcher::Exists,
                    }),
                ],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 子孫結合子（空白）を解析できる。
    #[test]
    fn core_1_parses_descendant_combinator() {
        let list = parse_selector_list("ul li").expect("ul li は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: Some("ul".to_string()),
                simple_selectors: Vec::new(),
            },
            rest: vec![(
                Combinator::Descendant,
                CompoundSelector {
                    type_name: Some("li".to_string()),
                    simple_selectors: Vec::new(),
                },
            )],
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 子結合子（`>`。前後に空白あり）を解析できる。
    #[test]
    fn core_1_parses_child_combinator_with_spaces() {
        let list = parse_selector_list("ul > li").expect("ul > li は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: Some("ul".to_string()),
                simple_selectors: Vec::new(),
            },
            rest: vec![(
                Combinator::Child,
                CompoundSelector {
                    type_name: Some("li".to_string()),
                    simple_selectors: Vec::new(),
                },
            )],
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 子結合子（`>`。前後に空白なし）を解析できる。
    #[test]
    fn core_1_parses_child_combinator_without_spaces() {
        let list = parse_selector_list("ul>li").expect("ul>li は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: Some("ul".to_string()),
                simple_selectors: Vec::new(),
            },
            rest: vec![(
                Combinator::Child,
                CompoundSelector {
                    type_name: Some("li".to_string()),
                    simple_selectors: Vec::new(),
                },
            )],
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 子結合子と子孫結合子が混在する複雑セレクタを解析できる。
    #[test]
    fn core_1_parses_mixed_combinators() {
        let list =
            parse_selector_list("div > ul li.item").expect("div > ul li.item は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: Some("div".to_string()),
                simple_selectors: Vec::new(),
            },
            rest: vec![
                (
                    Combinator::Child,
                    CompoundSelector {
                        type_name: Some("ul".to_string()),
                        simple_selectors: Vec::new(),
                    },
                ),
                (
                    Combinator::Descendant,
                    CompoundSelector {
                        type_name: Some("li".to_string()),
                        simple_selectors: vec![SimpleSelector::Class("item".to_string())],
                    },
                ),
            ],
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: カンマ区切りのセレクタリスト（空白の揺れを含む）を解析できる。
    #[test]
    fn core_1_parses_selector_list_with_commas() {
        let list = parse_selector_list("h1, h2 , h3").expect("h1, h2 , h3 は解析できるはず");
        assert_eq!(
            list.selectors(),
            &[simple_type("h1"), simple_type("h2"), simple_type("h3")]
        );
    }

    /// CORE-1: 前後の空白は無視される。
    #[test]
    fn core_1_trims_leading_and_trailing_whitespace() {
        let list = parse_selector_list("  div  ").expect("前後の空白は無視されるはず");
        assert_eq!(list.selectors(), &[simple_type("div")]);
    }

    /// CORE-1: 型名・属性名は ASCII 小文字に正規化される。
    #[test]
    fn core_1_normalizes_type_and_attribute_name_case() {
        let list = parse_selector_list("DIV").expect("DIV は解析できるはず");
        assert_eq!(list.selectors(), &[simple_type("div")]);

        let list = parse_selector_list("[HREF]").expect("[HREF] は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Attribute(AttributeSelector {
                    name: "href".to_string(),
                    matcher: AttributeMatcher::Exists,
                })],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: クラス・ID は大文字小文字を区別したまま保持される。
    #[test]
    fn core_1_preserves_class_and_id_case() {
        let list = parse_selector_list(".Foo").expect(".Foo は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Class("Foo".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);

        let list = parse_selector_list("#Bar").expect("#Bar は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Id("Bar".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: 非 ASCII 文字を含む識別子を解析できる。
    #[test]
    fn core_1_parses_non_ascii_identifier() {
        let list = parse_selector_list(".日本語").expect(".日本語 は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Class("日本語".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// CORE-1: `-` から始まる識別子を解析できる。
    #[test]
    fn core_1_parses_hyphen_leading_identifier() {
        let list = parse_selector_list(".-foo").expect(".-foo は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Class("-foo".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);

        let list = parse_selector_list(".--x").expect(".--x は解析できるはず");
        let expected = ComplexSelector {
            first: CompoundSelector {
                type_name: None,
                simple_selectors: vec![SimpleSelector::Class("--x".to_string())],
            },
            rest: Vec::new(),
        };
        assert_eq!(list.selectors(), &[expected]);
    }

    /// サブセット外の構文が `Error::Unsupported` になることを確認する。
    #[test]
    fn core_1_rejects_unsupported_syntax() {
        let unsupported_inputs = [
            "*",
            "a:hover",
            "a::before",
            "a + b",
            "a ~ b",
            "svg|a",
            "[a~=v]",
            "[a^=v]",
            "[a$=v]",
            "[a*=v]",
            "[a|=v]",
            "[a=v i]",
            ".a\\:b",
            "&",
        ];
        for input in unsupported_inputs {
            let err = parse_selector_list(input)
                .expect_err(&format!("{input:?} は Unsupported になるはず"));
            assert!(
                matches!(err, Error::Unsupported { .. }),
                "{input:?} は Unsupported になるはず: {err:?}"
            );
        }
    }

    /// 構文として不正な入力が `Error::InvalidInput` になることを確認する。
    #[test]
    fn core_1_rejects_invalid_input() {
        let invalid_inputs = [
            "",
            "   ",
            "> a",
            "a >",
            "a > > b",
            "a,,b",
            ",a",
            "a,",
            "#",
            ".",
            "#1",
            ".1a",
            "[a",
            "[a='v",
            "[]",
            "[a=\"x\ny\"]",
            "a\u{0}",
        ];
        for input in invalid_inputs {
            let err = parse_selector_list(input)
                .expect_err(&format!("{input:?} は InvalidInput になるはず"));
            assert!(
                matches!(err, Error::InvalidInput { .. }),
                "{input:?} は InvalidInput になるはず: {err:?}"
            );
        }
    }

    /// CORE-1: 入力バイト数の上限の境界を確認する
    /// （上限ちょうどは `Ok`、上限 + 1 は `InvalidInput`）。
    #[test]
    fn core_1_input_byte_limit_boundary() {
        let at_limit = format!(".{}", "a".repeat(MAX_SELECTOR_INPUT_BYTES - 1));
        assert_eq!(at_limit.len(), MAX_SELECTOR_INPUT_BYTES);
        assert!(parse_selector_list(&at_limit).is_ok());

        let over_limit = format!(".{}", "a".repeat(MAX_SELECTOR_INPUT_BYTES));
        assert_eq!(over_limit.len(), MAX_SELECTOR_INPUT_BYTES + 1);
        let err = parse_selector_list(&over_limit).expect_err("上限超過は InvalidInput のはず");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-1: セレクタリストの要素数上限の境界を確認する。
    #[test]
    fn core_1_selector_list_count_limit_boundary() {
        let at_limit = "a,".repeat(MAX_SELECTORS_PER_LIST - 1) + "a";
        let list = parse_selector_list(&at_limit).expect("上限ちょうどは Ok のはず");
        assert_eq!(list.selectors().len(), MAX_SELECTORS_PER_LIST);

        let over_limit = "a,".repeat(MAX_SELECTORS_PER_LIST) + "a";
        let err = parse_selector_list(&over_limit).expect_err("上限超過は InvalidInput のはず");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-1: 複合セレクタ数（結合子で連結される数）の上限の境界を確認する。
    #[test]
    fn core_1_compound_count_limit_boundary() {
        let at_limit = vec!["a"; MAX_COMPOUNDS_PER_COMPLEX].join(" ");
        let list = parse_selector_list(&at_limit).expect("上限ちょうどは Ok のはず");
        assert_eq!(
            list.selectors()[0].rest().len(),
            MAX_COMPOUNDS_PER_COMPLEX - 1
        );

        let over_limit = vec!["a"; MAX_COMPOUNDS_PER_COMPLEX + 1].join(" ");
        let err = parse_selector_list(&over_limit).expect_err("上限超過は InvalidInput のはず");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-1: 複合セレクタ内の単純セレクタ数の上限の境界を確認する
    /// （型名は件数に数えない）。
    #[test]
    fn core_1_simple_selector_count_limit_boundary() {
        let at_limit = ".a".repeat(MAX_SIMPLE_SELECTORS_PER_COMPOUND);
        let list = parse_selector_list(&at_limit).expect("上限ちょうどは Ok のはず");
        assert_eq!(
            list.selectors()[0].first().simple_selectors().len(),
            MAX_SIMPLE_SELECTORS_PER_COMPOUND
        );

        let over_limit = ".a".repeat(MAX_SIMPLE_SELECTORS_PER_COMPOUND + 1);
        let err = parse_selector_list(&over_limit).expect_err("上限超過は InvalidInput のはず");
        assert!(matches!(err, Error::InvalidInput { .. }));
    }

    /// CORE-1: エラーメッセージが入力文字列を反響しないことを確認する
    /// （目印となる文字列がメッセージに含まれない）。
    #[test]
    fn core_1_error_message_does_not_echo_input() {
        let marker = "SECRET_MARKER_VALUE";
        let input = format!("[{marker}");
        let err = parse_selector_list(&input).expect_err("未終端の属性は InvalidInput のはず");
        assert!(!err.to_string().contains(marker));
    }

    /// CORE-1: 不正な入力に対しても panic しないことを確認する
    /// （マルチバイト文字の途中で途切れるものを含む）。
    #[test]
    fn core_1_never_panics_on_malformed_input() {
        let inputs = [
            "[あ",
            "#é",
            "a\u{FFFD}>",
            "'",
            "]",
            ">",
            ",",
            "a[",
            "[a=",
            ".#",
            "#.",
        ];
        for input in inputs {
            let _ = parse_selector_list(input);
        }
    }

    /// CORE-1: `FromStr` 経由でも解析できることを確認する。
    #[test]
    fn core_1_from_str_parses_selector_list() {
        use std::str::FromStr;
        let list = SelectorList::from_str("div").expect("div は解析できるはず");
        assert_eq!(list.selectors(), &[simple_type("div")]);
    }
}

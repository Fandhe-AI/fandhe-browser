//! 宣言列テキスト（`prop: value; prop2: value2`）を [`Declaration`] の列へ変換するパーサー
//! （TASK-105.2・#256・MS-8・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! 呼び出し元は inline の `style` 属性値と、#551 が切り出すルールブロック `{ ... }` の
//! 中身の 2 系統で、どちらも外部入力（ネットワーク取得した HTML/CSS）である。生成した
//! [`Declaration`] は [`super::StyleRule`] へ渡され、#260 のカスケードと TASK-100 の
//! property gating が参照する。#263 の決定により新規依存は使わず手書きの最小実装とする
//! （`cssparser` は MPL-2.0 のため core では使えない）。
//!
//! # エラー方針
//!
//! - 空入力・空白のみ・`;` のみは `Ok(vec![])`（`style=""` を失敗させない）
//! - 個々の宣言が不正（コロン無し・property 名が不正・値が空・不正な `!` 指定）なら
//!   **その宣言だけを捨てて継続**する（CSS Syntax の宣言単位のエラー回復）
//! - 文字列・コメント・括弧が EOF まで閉じない場合は、EOF で暗黙に閉じたものとして扱う
//! - 入力長・宣言数が上限を超えたら [`Error::InvalidInput`]（確保前・`push` 前に判定）
//!
//! # 未実装（REPAIR-3: 実装済みを装わない）
//!
//! - 捨てた宣言の個別報告（位置・理由）。将来はルール単位のエラー記録（#551・#258）で扱う
//! - 値の型付き解釈（長さ・色・`url()` 等）。値は文字列のまま保持し、解釈も取得もしない
//! - エスケープ付き property 名（`\66oo` 等）と `*zoom` 等のハック。いずれも捨てる
//! - 同名 property の重複解決。ソース順に全件残し、後勝ちはカスケード（#260）の責務
//! - 上限値は暫定。見直しは #261（TASK-105.7）が担う

use super::is_css_whitespace;
use super::types::{Declaration, Importance};
use crate::error::{Error, Result};

/// 受け付ける宣言列テキストの最大バイト数（暫定。見直しは #261）。
pub const MAX_DECLARATION_INPUT_BYTES: usize = 1024 * 1024;

/// 1 ブロックで受け付ける有効な宣言の最大件数（暫定。見直しは #261）。
pub const MAX_DECLARATIONS_PER_BLOCK: usize = 4096;

/// CSS 識別子の先頭以外に使える文字（英数字・`-`・`_`・非 ASCII）。
/// 非 ASCII は CSS Syntax の ident code point（U+0080 以上）に従い許可する。
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii()
}

/// 識別子の開始文字（英字・`_`・非 ASCII。数字と単独の `-` は不可）。
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

/// property 名が CSS 識別子の規則を満たすか。`--x`（カスタムプロパティ）と
/// `-webkit-x` 形式は許可し、`1color`・`-`・`--`・`-1a` は不可とする。
fn is_valid_property_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let rest = if first == '-' {
        match chars.next() {
            // `--` 単独は予約で不可。`--x` は以降を ident 文字で検証する。
            Some('-') => return name.len() > 2 && chars.all(is_ident_char),
            Some(second) if is_ident_start(second) => chars,
            _ => return false,
        }
    } else if is_ident_start(first) {
        chars
    } else {
        return false;
    };
    rest.into_iter().all(is_ident_char)
}

fn trim_css(s: &str) -> &str {
    s.trim_matches(is_css_whitespace)
}

/// 宣言列テキストを [`Declaration`] の列へ変換する。
///
/// 1 パス・再帰なしで走査する。文字列（`"` `'`）・コメント（`/* */`）・括弧
/// （`(` `[` `{`）の内側の `;` `:` は区切りとして扱わない。最初の `:` だけが property と
/// value の区切りで、`!important` は `Importance::Important` として切り出す。
/// エラー方針はモジュール doc を参照。入力文字列はエラーメッセージに含めない。
pub fn parse_declarations(input: &str) -> Result<Vec<Declaration>> {
    if input.len() > MAX_DECLARATION_INPUT_BYTES {
        return Err(Error::InvalidInput {
            message: format!("declaration input exceeds {MAX_DECLARATION_INPUT_BYTES} bytes"),
        });
    }

    let mut out: Vec<Declaration> = Vec::new();
    let mut cur = Current::default();
    let mut depth: usize = 0;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                // 閉じ `*/` まで読み飛ばす。未終端なら EOF で終わる。
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                // property 名側はトークンを分断せず単に除去する（`col/*x*/or` は `color`）。
                // 値側は `a/**/b` が 1 語に融合しないよう空白 1 つへ置換する。
                if cur.in_value {
                    cur.push(' ');
                }
            }
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '(' | '[' | '{' => {
                depth = depth.saturating_add(1);
                cur.push(c);
            }
            ')' | ']' | '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ';' if depth == 0 => {
                cur.finish(&mut out)?;
                cur = Current::default();
            }
            ':' if depth == 0 && !cur.in_value => {
                cur.in_value = true;
            }
            '!' if depth == 0 && cur.in_value => {
                cur.bang = Some(cur.value.len());
                cur.push(c);
            }
            _ => cur.push(c),
        }
    }
    cur.finish(&mut out)?;
    Ok(out)
}

/// 走査中の 1 宣言分のバッファ。
#[derive(Default)]
struct Current {
    property: String,
    value: String,
    in_value: bool,
    /// `value` 内で最後に現れた文字列外・深さ 0 の `!` のバイト位置。
    bang: Option<usize>,
}

impl Current {
    fn push(&mut self, c: char) {
        if self.in_value {
            self.value.push(c);
        } else {
            self.property.push(c);
        }
    }

    /// 宣言を検証し、有効なら `out` へ追加する。不正なら黙って捨てる。
    fn finish(&self, out: &mut Vec<Declaration>) -> Result<()> {
        if !self.in_value {
            return Ok(());
        }
        let property = trim_css(&self.property);
        if !is_valid_property_name(property) {
            return Ok(());
        }

        let mut value = self.value.as_str();
        let mut importance = Importance::Normal;
        if let Some(pos) = self.bang {
            let (Some(head), Some(tail)) = (value.get(..pos), value.get(pos + 1..)) else {
                return Ok(());
            };
            if !trim_css(tail).eq_ignore_ascii_case("important") {
                return Ok(());
            }
            value = head;
            importance = Importance::Important;
        }
        let value = trim_css(value);
        if value.is_empty() && !property.starts_with("--") {
            return Ok(());
        }

        if out.len() >= MAX_DECLARATIONS_PER_BLOCK {
            return Err(Error::InvalidInput {
                message: format!(
                    "declaration count exceeds {MAX_DECLARATIONS_PER_BLOCK} per block"
                ),
            });
        }
        out.push(Declaration::new(property, value, importance));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(input: &str) -> Vec<(String, String, Importance)> {
        parse_declarations(input)
            .expect("must parse")
            .iter()
            .map(|d| {
                (
                    d.property().to_owned(),
                    d.value().to_owned(),
                    d.importance(),
                )
            })
            .collect()
    }

    fn n(p: &str, v: &str) -> (String, String, Importance) {
        (p.to_owned(), v.to_owned(), Importance::Normal)
    }

    #[test]
    fn core_5_single_and_multiple() {
        assert_eq!(pairs("color: red"), vec![n("color", "red")]);
        assert_eq!(
            pairs("color: red; margin: 0 auto"),
            vec![n("color", "red"), n("margin", "0 auto")]
        );
    }

    #[test]
    fn core_5_tolerates_noise() {
        assert_eq!(pairs("color:red;"), vec![n("color", "red")]);
        assert_eq!(pairs(";;color:red;;"), vec![n("color", "red")]);
        assert_eq!(pairs("  color \n:\t red \n; "), vec![n("color", "red")]);
        assert!(pairs("").is_empty());
        assert!(pairs("   \n").is_empty());
        assert!(pairs(";;;").is_empty());
    }

    #[test]
    fn core_5_normalization() {
        assert_eq!(pairs("COLOR: Red"), vec![n("color", "Red")]);
        assert_eq!(pairs("--Main-Color: #FFF"), vec![n("--Main-Color", "#FFF")]);
        assert_eq!(pairs("--x:;"), vec![n("--x", "")]);
    }

    #[test]
    fn core_5_important() {
        let imp = |v: &str| (String::from("color"), v.to_owned(), Importance::Important);
        assert_eq!(pairs("color: red !important"), vec![imp("red")]);
        assert_eq!(pairs("color: red !IMPORTANT"), vec![imp("red")]);
        assert_eq!(pairs("color: red! important;"), vec![imp("red")]);
        assert!(pairs("color: red !foo").is_empty());
        assert_eq!(
            pairs("content: \"!important\""),
            vec![n("content", "\"!important\"")]
        );
    }

    #[test]
    fn core_5_important_with_comments_between_tokens() {
        let imp = |v: &str| (String::from("color"), v.to_owned(), Importance::Important);
        assert_eq!(pairs("color: red !/*x*/important"), vec![imp("red")]);
        assert_eq!(pairs("color: red ! /*x*/ important"), vec![imp("red")]);
        assert_eq!(
            pairs("color: red /*x*/!/*y*/important/*z*/;"),
            vec![imp("red")]
        );
        assert_eq!(pairs("color: red !impor/*x*/tant"), vec![]);
    }

    #[test]
    fn core_5_delimiters_are_protected() {
        assert_eq!(
            pairs("background: url(data:image/png;base64,AAAA)"),
            vec![n("background", "url(data:image/png;base64,AAAA)")]
        );
        assert_eq!(pairs("content: \";\""), vec![n("content", "\";\"")]);
        assert_eq!(pairs("content: 'a:b'"), vec![n("content", "'a:b'")]);
        assert_eq!(
            pairs("content: \"a\\\";b\"; color: red"),
            vec![n("content", "\"a\\\";b\""), n("color", "red")]
        );
        assert_eq!(
            pairs("background:url(http://x/y)"),
            vec![n("background", "url(http://x/y)")]
        );
    }

    #[test]
    fn core_5_comments() {
        assert_eq!(
            pairs("/* c */ color: red; /* x; y: z */ margin: 0"),
            vec![n("color", "red"), n("margin", "0")]
        );
        assert_eq!(
            pairs("content: \"/* k */\""),
            vec![n("content", "\"/* k */\"")]
        );
    }

    #[test]
    fn core_5_invalid_declarations_are_dropped() {
        let want = vec![n("margin", "0")];
        assert_eq!(pairs("color red; margin: 0"), want);
        assert_eq!(pairs(": red; margin: 0"), want);
        assert_eq!(pairs("color:; margin: 0"), want);
        assert_eq!(pairs("*zoom: 1; margin: 0"), want);
    }

    #[test]
    fn core_5_comment_does_not_split_property_name() {
        assert_eq!(pairs("col/*x*/or: red"), vec![n("color", "red")]);
        assert_eq!(pairs("color/*x*/: red"), vec![n("color", "red")]);
        assert_eq!(pairs("color: re/**/d"), vec![n("color", "re d")]);
    }

    #[test]
    fn core_5_property_name_follows_ident_rules() {
        let want = vec![n("margin", "0")];
        assert_eq!(pairs("1color: red; margin: 0"), want);
        assert_eq!(pairs("-: red; margin: 0"), want);
        assert_eq!(pairs("--: red; margin: 0"), want);
        assert_eq!(pairs("-1a: red; margin: 0"), want);
        assert_eq!(pairs("a!b: red; margin: 0"), want);
        assert_eq!(pairs("-webkit-x: 1"), vec![n("-webkit-x", "1")]);
        assert_eq!(pairs("_a1: 1"), vec![n("_a1", "1")]);
        assert_eq!(pairs("--1: 1"), vec![n("--1", "1")]);
    }

    #[test]
    fn core_5_duplicates_keep_source_order() {
        assert_eq!(
            pairs("color: red; color: blue"),
            vec![n("color", "red"), n("color", "blue")]
        );
    }

    #[test]
    fn core_5_unterminated_constructs_do_not_panic() {
        assert_eq!(pairs("content: \"abc"), vec![n("content", "\"abc")]);
        assert_eq!(pairs("color: red /* x"), vec![n("color", "red")]);
        assert_eq!(pairs("background: url(x"), vec![n("background", "url(x")]);
        assert_eq!(pairs("a: b\\"), vec![n("a", "b\\")]);
        assert_eq!(pairs(")))]]}}; a: b"), vec![n("a", "b")]);
    }

    #[test]
    fn core_5_non_ascii() {
        assert_eq!(
            pairs("content: \"日本語\""),
            vec![n("content", "\"日本語\"")]
        );
        assert_eq!(pairs("é: 1"), vec![n("é", "1")]);
        assert_eq!(pairs("a: \\日"), vec![n("a", "\\日")]);
    }

    #[test]
    fn core_5_input_size_limit() {
        let prefix = "a:";
        let at = format!(
            "{prefix}{}",
            "x".repeat(MAX_DECLARATION_INPUT_BYTES - prefix.len())
        );
        assert_eq!(parse_declarations(&at).expect("at limit").len(), 1);
        let over = format!("{at}x");
        match parse_declarations(&over) {
            Err(Error::InvalidInput { message }) => {
                assert_eq!(message, "declaration input exceeds 1048576 bytes");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn core_5_declaration_count_limit() {
        let at = "a:1;".repeat(MAX_DECLARATIONS_PER_BLOCK);
        assert_eq!(
            parse_declarations(&at).expect("at limit").len(),
            MAX_DECLARATIONS_PER_BLOCK
        );
        let over = format!("{at}a:1");
        match parse_declarations(&over) {
            Err(Error::InvalidInput { message }) => {
                assert_eq!(message, "declaration count exceeds 4096 per block");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

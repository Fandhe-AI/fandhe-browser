//! 生 DOM シリアライズのトークン量を算出するモジュール
//! （TASK-23.1・`AISNAP-15`・Issue #129・`MS-2`）。
//!
//! 役割: 各フィクスチャを core の `parse_document` → `raw_dom::serialize_raw_dom`
//! （`script` / `style` 等を除去した outerHTML 相当）→ `tokens::TokenCounter` で数える。
//! 呼び出し元はベンチ本体（`raw_dom_serialize_reduction.rs`）とユニットテスト。
//!
//! `AISNAP-1` の分母は生 HTML、`AISNAP-15` の分母は本モジュールが数える生 DOM
//! シリアライズで別指標。対生 DOM 比の削減率・ページ平均は `raw_dom_reduction.rs`
//! （#130・TASK-23.2）、85% 達成可否の判断は #131（人間担当）の責務で、ここは分母の算出に徹し
//! 目標値に対する assert を持たない。
//! 本モジュールは `crate::tokens` / `crate::raw_dom` が解決できる前提で使う。

use std::fmt;
use std::fs;
use std::path::Path;

use fandhe_browser_core::parse::{ParseOptions, parse_document};

use crate::raw_dom::serialize_raw_dom;
use crate::tokens::{TokenCountError, TokenCounter, measure_raw_html};

/// 生 DOM トークン測定で起きるエラー（ファイル名を含める）。
#[derive(Debug)]
pub enum RawDomTokensError {
    /// 生 HTML 計測・読み込みの失敗。
    Tokens(TokenCountError),
    /// HTML のパース失敗。
    Parse { name: String, message: String },
}

impl fmt::Display for RawDomTokensError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tokens(e) => write!(f, "{e}"),
            Self::Parse { name, message } => write!(f, "failed to parse {name}: {message}"),
        }
    }
}

impl std::error::Error for RawDomTokensError {}

/// 1 フィクスチャ分の測定結果。削減率は持たない（`raw_dom_reduction.rs` の責務）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDomTokens {
    pub name: String,
    pub bytes: usize,
    pub raw_html_tokens: usize,
    /// 生 DOM シリアライズ（`script` / `style` 等除去後）のトークン数。
    pub raw_dom_tokens: usize,
}

/// HTML 文字列の生 DOM シリアライズのトークン数を返す。`name` はエラー表示用。
pub fn count_raw_dom_tokens(
    counter: &TokenCounter,
    name: &str,
    html: &str,
) -> Result<usize, RawDomTokensError> {
    let parsed =
        parse_document(html, &ParseOptions::default()).map_err(|e| RawDomTokensError::Parse {
            name: name.to_string(),
            message: format!("{e:?}"),
        })?;
    Ok(counter.count(&serialize_raw_dom(&parsed.document)))
}

/// `dir` 内の全 `.html` を計測する（ファイル名昇順）。
pub fn measure_raw_dom(
    counter: &TokenCounter,
    dir: &Path,
) -> Result<Vec<RawDomTokens>, RawDomTokensError> {
    let raws = measure_raw_html(counter, dir).map_err(RawDomTokensError::Tokens)?;
    let mut out = Vec::with_capacity(raws.len());
    for raw in raws {
        let path = dir.join(&raw.name);
        let html = fs::read_to_string(&path).map_err(|source| {
            RawDomTokensError::Tokens(TokenCountError::Io {
                path: path.clone(),
                source,
            })
        })?;
        let raw_dom_tokens = count_raw_dom_tokens(counter, &raw.name, &html)?;
        out.push(RawDomTokens {
            name: raw.name,
            bytes: raw.bytes,
            raw_html_tokens: raw.tokens,
            raw_dom_tokens,
        });
    }
    Ok(out)
}

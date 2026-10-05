//! 生 HTML のトークン量を `cl100k_base` で数える純関数モジュール
//! （TASK-14.2・`AISNAP-1`・Issue #93・`MS-2`）。
//!
//! 役割: TASK-14 の削減率測定の分母となる「生 HTML のトークン量」を算出する。
//! PoC-4 の `measure.mjs` の `tokCount`（`gpt-tokenizer` の `encode(text).length`・
//! `cl100k_base` 相当）の Rust 移植で、エンコーディングを揃えて PoC-4 と連続比較できる
//! ようにする。呼び出し元はベンチ本体（`token_reduction.rs`）とユニットテスト
//! （`tokens_tests.rs`）で、どちらも `#[path]` で本ファイルを取り込む。
//!
//! 未実装（#94・TASK-14.3）: snapshot 経由のトークン数・削減率・平均集計。
//! #94 は [`RawHtmlTokens`] へフィールドを足して拡張する。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use tiktoken_rs::CoreBPE;

/// トークン計測で起きるエラー。
#[derive(Debug)]
pub enum TokenCountError {
    /// トークナイザ（BPE テーブル）の構築失敗。
    Tokenizer(String),
    /// フィクスチャの読み込み失敗。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for TokenCountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tokenizer(msg) => write!(f, "failed to build tokenizer: {msg}"),
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for TokenCountError {}

/// `cl100k_base` のトークンカウンタ。BPE テーブル構築は重いため 1 回だけ作って使い回す。
pub struct TokenCounter {
    bpe: CoreBPE,
}

impl TokenCounter {
    /// `cl100k_base` を構築する。
    pub fn new() -> Result<Self, TokenCountError> {
        let bpe =
            tiktoken_rs::cl100k_base().map_err(|e| TokenCountError::Tokenizer(e.to_string()))?;
        Ok(Self { bpe })
    }

    /// `text` のトークン数を返す。
    ///
    /// `encode_ordinary` を使い、HTML 中の `<|endoftext|>` 等を特殊トークンとして
    /// 解釈せず通常テキストとして数える。現行フィクスチャは特殊トークン文字列を
    /// 含まないため、この選択で値に差は出ない。
    pub fn count(&self, text: &str) -> usize {
        self.bpe.encode_ordinary(text).len()
    }
}

/// 1 フィクスチャ分の生 HTML 計測結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHtmlTokens {
    /// ファイル名（例: `example-minimal.html`）。
    pub name: String,
    /// ファイルのバイト数。
    pub bytes: usize,
    /// 生 HTML 全体の `cl100k_base` トークン数。
    pub tokens: usize,
}

/// フィクスチャディレクトリ（`benches/fixtures`）の絶対パス。
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
}

/// `dir` 内の `.html` をファイル名昇順に読み、前処理なしの全文を数える。
pub fn measure_raw_html(
    counter: &TokenCounter,
    dir: &Path,
) -> Result<Vec<RawHtmlTokens>, TokenCountError> {
    let io_err = |path: &Path, source| TokenCountError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| io_err(dir, e))? {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".html") {
            names.push(name);
        }
    }
    names.sort();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let path = dir.join(&name);
        let text = fs::read_to_string(&path).map_err(|e| io_err(&path, e))?;
        out.push(RawHtmlTokens {
            bytes: text.len(),
            tokens: counter.count(&text),
            name,
        });
    }
    Ok(out)
}

//! 生 HTML のトークン量を `cl100k_base` で数える純関数モジュール
//! （TASK-14.2・`AISNAP-1`・Issue #93・`MS-2`）。
//!
//! 役割: TASK-14 の削減率測定の分母となる「生 HTML のトークン量」を算出する。
//! PoC-4 の `measure.mjs` の `tokCount`（`gpt-tokenizer` の `encode(text).length`・
//! `cl100k_base` 相当）の Rust 移植で、エンコーディングを揃えて PoC-4 と連続比較できる
//! ようにする。呼び出し元はベンチ本体（`token_reduction.rs`）とユニットテスト
//! （`tokens_tests.rs`）で、どちらも `#[path]` で本ファイルを取り込む。
//!
//! 入力ディレクトリは既定で `benches/fixtures`（合成 17 ページ）だが、環境変数
//! [`INPUT_DIR_ENV`] で任意のディレクトリへ切り替えられる（Issue #927・実ページ測定用。
//! 手順は `docs/design/token-reduction-real-page-measurement.md`）。切替は
//! `token_reduction`・`raw_dom_serialize_reduction` の 2 ベンチの `main` 経路だけに閉じ、
//! [`fixtures_dir`] を直接使う他ターゲット・テストには影響しない。
//!
//! snapshot 経由のトークン数・削減率・平均集計は `reduction.rs`
//! （TASK-14.3・Issue #94）が担い、本モジュールの [`measure_raw_html`] を分母に使う。

use std::ffi::OsString;
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
    #[allow(
        dead_code,
        reason = "mcp_envelope 系ターゲットはフィクスチャ読込に失敗しない経路のみ使う"
    )]
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
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、mcp_envelope 系ターゲットでは未使用"
)]
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
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、mcp_envelope 系ターゲットでは未使用"
)]
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
        if is_html_name(&name) {
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

/// 測定対象とみなすファイル名か（`*.html`）。計測と入力検証で定義を共有する。
fn is_html_name(name: &str) -> bool {
    name.ends_with(".html")
}

/// 入力ディレクトリを指定する環境変数名（Issue #927）。
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、入力切替を使う 2 ベンチ以外では未使用"
)]
pub const INPUT_DIR_ENV: &str = "FANDHE_BROWSER_BENCH_HTML_DIR";

/// 測定入力の出どころ。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、入力切替を使う 2 ベンチ以外では未使用"
)]
pub enum InputSource {
    /// 同梱の合成 fixture（`benches/fixtures`）。
    Fixtures(PathBuf),
    /// 環境変数で指定された外部ディレクトリ（実ページ HTML 等）。
    External(PathBuf),
}

#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、入力切替を使う 2 ベンチ以外では未使用"
)]
impl InputSource {
    /// 入力ディレクトリ。
    pub fn dir(&self) -> &Path {
        match self {
            Self::Fixtures(p) | Self::External(p) => p,
        }
    }

    /// 外部指定か。fixture 名に依存する検査の実行可否の判定に使う。
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// 出力用の種別名（`fixtures` / `external`）。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Fixtures(_) => "fixtures",
            Self::External(_) => "external",
        }
    }
}

/// 入力ディレクトリ解決のエラー。0 件・設定ミスを成功にしない（fail-closed）。
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、入力切替を使う 2 ベンチ以外では未使用"
)]
pub enum InputDirError {
    /// 環境変数が設定されているが空文字。
    Empty,
    /// 指定パスがディレクトリではない（存在しない場合を含む）。
    NotADirectory { path: PathBuf },
    /// ディレクトリの読み取り失敗。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 直下に `*.html` が 1 件も無い。
    NoHtml { path: PathBuf },
}

impl fmt::Display for InputDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "{INPUT_DIR_ENV} is set but empty"),
            Self::NotADirectory { path } => write!(
                f,
                "{INPUT_DIR_ENV}={} is not an existing directory",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::NoHtml { path } => {
                write!(f, "no *.html files found in {}", path.display())
            }
        }
    }
}

impl std::error::Error for InputDirError {}

/// 環境変数の値（未設定は `None`）から測定入力を解決する純関数。
///
/// `None` は同梱 fixture（中身は検査しない。現状維持）。値があれば、空文字・非ディレクトリ・
/// `*.html` 0 件をエラーにする。環境変数を直接読まないのでテストで `set_var` が要らない。
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、入力切替を使う 2 ベンチ以外では未使用"
)]
pub fn resolve_input(value: Option<OsString>) -> Result<InputSource, InputDirError> {
    let Some(value) = value else {
        return Ok(InputSource::Fixtures(fixtures_dir()));
    };
    if value.is_empty() {
        return Err(InputDirError::Empty);
    }
    let path = PathBuf::from(value);
    if !fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(InputDirError::NotADirectory { path });
    }
    let io_err = |source| InputDirError::Io {
        path: path.clone(),
        source,
    };
    let mut found = false;
    for entry in fs::read_dir(&path).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        if is_html_name(&entry.file_name().to_string_lossy()) {
            found = true;
            break;
        }
    }
    if !found {
        return Err(InputDirError::NoHtml { path });
    }
    Ok(InputSource::External(path))
}

/// [`INPUT_DIR_ENV`] を読んで測定入力を解決する。呼び出し元は 2 ベンチの `run()`。
#[allow(
    dead_code,
    reason = "bench 間で共有する tokens.rs のため、入力切替を使う 2 ベンチ以外では未使用"
)]
pub fn input_from_env() -> Result<InputSource, InputDirError> {
    resolve_input(std::env::var_os(INPUT_DIR_ENV))
}

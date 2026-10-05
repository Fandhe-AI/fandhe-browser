//! snapshot 経由のトークン量と、生 HTML 比の削減率を算出するモジュール
//! （TASK-14.3・`AISNAP-1`・Issue #94・`MS-2`）。
//!
//! 役割: 各フィクスチャを core の `parse_document` → ai の `build_snapshot` →
//! `snapshot_text::render_snapshot` でテキスト化し、`tokens.rs` のカウンタで数えて
//! ページごとの削減率と全ページの平均を出す。分母の生 HTML トークン数は
//! `tokens::measure_raw_html`（TASK-14.2・Issue #93）の結果を使う。
//! 呼び出し元はベンチ本体（`token_reduction.rs`）とユニットテスト。
//!
//! 85% 目標（`AISNAP-1`）の達成可否判定は本モジュールの責務ではない（#97・人間担当）。
//! ここは数値を算出するだけで、目標値に対する assert を持たない。

use std::fmt;
use std::fs;
use std::path::Path;

use fandhe_browser_ai::snapshot::build_snapshot;
use fandhe_browser_core::parse::{ParseOptions, parse_document};

use crate::snapshot_text::render_snapshot;
use crate::tokens::{TokenCountError, TokenCounter, measure_raw_html};

/// 削減率測定で起きるエラー（ファイル名を含める）。
#[derive(Debug)]
pub enum ReductionError {
    /// 生 HTML 計測・読み込みの失敗。
    Tokens(TokenCountError),
    /// フィクスチャのパース失敗。
    Parse { name: String, message: String },
    /// snapshot 構築失敗。
    Snapshot { name: String, message: String },
    /// 生 HTML のトークン数が 0 で削減率を定義できない。
    EmptyRaw { name: String },
}

impl fmt::Display for ReductionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tokens(e) => write!(f, "{e}"),
            Self::Parse { name, message } => write!(f, "failed to parse {name}: {message}"),
            Self::Snapshot { name, message } => {
                write!(f, "failed to build snapshot of {name}: {message}")
            }
            Self::EmptyRaw { name } => write!(f, "raw HTML of {name} has zero tokens"),
        }
    }
}

impl std::error::Error for ReductionError {}

/// 1 フィクスチャ分の削減率測定結果。
#[derive(Debug, Clone, PartialEq)]
pub struct ReductionRow {
    pub name: String,
    pub bytes: usize,
    pub raw_html_tokens: usize,
    pub snapshot_tokens: usize,
    /// `(1 - snapshot / raw) * 100`。増加した場合は負値。
    pub reduction_pct: f64,
    pub snapshot_truncated: bool,
}

/// 全ページの集計。
#[derive(Debug, Clone, PartialEq)]
pub struct ReductionSummary {
    pub pages: usize,
    /// ページごとの削減率の算術平均（PoC-4 `measure.mjs` の `avg` と同じ定義）。
    pub mean_reduction_pct: f64,
    /// snapshot トークン数の中央値（`AISNAP-4` の参考値）。
    pub median_snapshot_tokens: f64,
    pub min_reduction_pct: f64,
    pub max_reduction_pct: f64,
}

/// 削減率（%）。`raw == 0` は `None`。
pub fn reduction_pct(raw: usize, reduced: usize) -> Option<f64> {
    if raw == 0 {
        return None;
    }
    Some((1.0 - reduced as f64 / raw as f64) * 100.0)
}

/// 行の集計。空なら `None`。
pub fn summarize(rows: &[ReductionRow]) -> Option<ReductionSummary> {
    if rows.is_empty() {
        return None;
    }
    let n = rows.len() as f64;
    let mean = rows.iter().map(|r| r.reduction_pct).sum::<f64>() / n;
    let min = rows
        .iter()
        .map(|r| r.reduction_pct)
        .fold(f64::INFINITY, f64::min);
    let max = rows
        .iter()
        .map(|r| r.reduction_pct)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut toks: Vec<usize> = rows.iter().map(|r| r.snapshot_tokens).collect();
    toks.sort_unstable();
    let mid = toks.len() / 2;
    let median = if toks.len() % 2 == 1 {
        *toks.get(mid)? as f64
    } else {
        (*toks.get(mid.checked_sub(1)?)? + *toks.get(mid)?) as f64 / 2.0
    };
    Some(ReductionSummary {
        pages: rows.len(),
        mean_reduction_pct: mean,
        median_snapshot_tokens: median,
        min_reduction_pct: min,
        max_reduction_pct: max,
    })
}

/// `dir` 内の全 `.html` を計測する（ファイル名昇順）。
pub fn measure_reduction(
    counter: &TokenCounter,
    dir: &Path,
) -> Result<Vec<ReductionRow>, ReductionError> {
    let raws = measure_raw_html(counter, dir).map_err(ReductionError::Tokens)?;
    let mut out = Vec::with_capacity(raws.len());
    for raw in raws {
        let path = dir.join(&raw.name);
        let html = fs::read_to_string(&path).map_err(|source| {
            ReductionError::Tokens(TokenCountError::Io {
                path: path.clone(),
                source,
            })
        })?;
        let parsed =
            parse_document(&html, &ParseOptions::default()).map_err(|e| ReductionError::Parse {
                name: raw.name.clone(),
                message: format!("{e:?}"),
            })?;
        let snapshot = build_snapshot(&parsed.document).map_err(|e| ReductionError::Snapshot {
            name: raw.name.clone(),
            message: e.to_string(),
        })?;
        let snapshot_tokens = counter.count(&render_snapshot(&snapshot));
        let pct =
            reduction_pct(raw.tokens, snapshot_tokens).ok_or_else(|| ReductionError::EmptyRaw {
                name: raw.name.clone(),
            })?;
        out.push(ReductionRow {
            name: raw.name,
            bytes: raw.bytes,
            raw_html_tokens: raw.tokens,
            snapshot_tokens,
            reduction_pct: pct,
            snapshot_truncated: snapshot.truncated,
        });
    }
    Ok(out)
}

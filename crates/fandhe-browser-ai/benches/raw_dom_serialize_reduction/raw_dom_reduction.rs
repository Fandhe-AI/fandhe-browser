//! 対生 DOM シリアライズ比の削減率・平均集計モジュール
//! （TASK-23.2・`AISNAP-15`・Issue #130・`MS-2`）。
//!
//! 役割: #129 の `raw_dom_tokens::measure_raw_dom`（分母＝生 DOM シリアライズのトークン数）と、
//! TASK-14.3 の `reduction::measure_reduction`（分子＝snapshot のトークン数）をフィクスチャ名で
//! 突き合わせ、ページごとの削減率と全ページの平均・最小・最大を返す。
//! 呼び出し元はベンチ本体（`raw_dom_serialize_reduction.rs`）とユニットテスト。
//!
//! `AISNAP-1` の分母は生 HTML、`AISNAP-15` の分母は生 DOM シリアライズで別指標。
//! 85% 達成可否の判断は #131（人間担当）の責務で、ここでは `TARGET_PCT` を参考表示用に
//! 公開するのみで目標値に対する assert を持たない。
//! 本モジュールは `crate::raw_dom_tokens` / `crate::reduction` が解決できる前提で使う。

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crate::raw_dom_tokens::{RawDomTokens, RawDomTokensError, measure_raw_dom};
use crate::reduction::{ReductionError, ReductionRow, measure_reduction, reduction_pct};
use crate::tokens::TokenCounter;

/// `AISNAP-15` の目標削減率（%）。参考表示専用で、判定は #131 が担う。
pub const TARGET_PCT: f64 = 85.0;

/// 対生 DOM 比の測定で起きるエラー。
#[derive(Debug)]
pub enum RawDomReductionError {
    /// 生 DOM トークン測定の失敗。
    RawDom(RawDomTokensError),
    /// snapshot 測定の失敗。
    Snapshot(ReductionError),
    /// 2 測定のフィクスチャ名集合が一致しない。
    FixtureMismatch { name: String },
    /// 生 DOM のトークン数が 0 で削減率を定義できない。
    EmptyRawDom { name: String },
}

impl fmt::Display for RawDomReductionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RawDom(e) => write!(f, "{e}"),
            Self::Snapshot(e) => write!(f, "{e}"),
            Self::FixtureMismatch { name } => {
                write!(f, "fixture {name} is missing from one of the measurements")
            }
            Self::EmptyRawDom { name } => write!(f, "raw DOM of {name} has zero tokens"),
        }
    }
}

impl std::error::Error for RawDomReductionError {}

/// 1 フィクスチャ分の対生 DOM 比測定結果。
#[derive(Debug, Clone, PartialEq)]
pub struct RawDomReductionRow {
    pub name: String,
    pub bytes: usize,
    pub raw_html_tokens: usize,
    pub raw_dom_tokens: usize,
    pub snapshot_tokens: usize,
    /// `AISNAP-15` の指標 `(1 - snapshot / raw_dom) * 100`。増加した場合は負値。
    pub reduction_vs_raw_dom_pct: f64,
    /// 参考値。`AISNAP-1` と同じ分母（生 HTML）。
    pub reduction_vs_raw_html_pct: f64,
    pub snapshot_truncated: bool,
}

/// 全ページの集計。
#[derive(Debug, Clone, PartialEq)]
pub struct RawDomReductionSummary {
    pub pages: usize,
    /// ページごとの削減率の算術平均（`reduction::summarize` と同じ定義）。
    pub mean_reduction_pct: f64,
    pub min_reduction_pct: f64,
    pub max_reduction_pct: f64,
}

/// 生 DOM 測定と snapshot 測定を名前キーで突き合わせる。出力は名前昇順。
///
/// 片方にしか無い名前は `FixtureMismatch`、生 DOM が 0 トークンなら `EmptyRawDom`（fail-closed）。
pub fn join_rows(
    raw: Vec<RawDomTokens>,
    snap: Vec<ReductionRow>,
) -> Result<Vec<RawDomReductionRow>, RawDomReductionError> {
    let mut snaps: BTreeMap<String, ReductionRow> =
        snap.into_iter().map(|r| (r.name.clone(), r)).collect();
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let Some(s) = snaps.remove(&r.name) else {
            return Err(RawDomReductionError::FixtureMismatch { name: r.name });
        };
        let Some(pct) = reduction_pct(r.raw_dom_tokens, s.snapshot_tokens) else {
            return Err(RawDomReductionError::EmptyRawDom { name: r.name });
        };
        out.push(RawDomReductionRow {
            name: r.name,
            bytes: r.bytes,
            raw_html_tokens: r.raw_html_tokens,
            raw_dom_tokens: r.raw_dom_tokens,
            snapshot_tokens: s.snapshot_tokens,
            reduction_vs_raw_dom_pct: pct,
            reduction_vs_raw_html_pct: s.reduction_pct,
            snapshot_truncated: s.snapshot_truncated,
        });
    }
    if let Some(name) = snaps.into_keys().next() {
        return Err(RawDomReductionError::FixtureMismatch { name });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 行の集計。空なら `None`。
pub fn summarize(rows: &[RawDomReductionRow]) -> Option<RawDomReductionSummary> {
    if rows.is_empty() {
        return None;
    }
    let pcts = || rows.iter().map(|r| r.reduction_vs_raw_dom_pct);
    Some(RawDomReductionSummary {
        pages: rows.len(),
        mean_reduction_pct: pcts().sum::<f64>() / rows.len() as f64,
        min_reduction_pct: pcts().fold(f64::INFINITY, f64::min),
        max_reduction_pct: pcts().fold(f64::NEG_INFINITY, f64::max),
    })
}

/// `dir` 内の全 `.html` について対生 DOM 比の削減率を測定する（ファイル名昇順）。
pub fn measure_raw_dom_reduction(
    counter: &TokenCounter,
    dir: &Path,
) -> Result<Vec<RawDomReductionRow>, RawDomReductionError> {
    let raw = measure_raw_dom(counter, dir).map_err(RawDomReductionError::RawDom)?;
    let snap = measure_reduction(counter, dir).map_err(RawDomReductionError::Snapshot)?;
    join_rows(raw, snap)
}

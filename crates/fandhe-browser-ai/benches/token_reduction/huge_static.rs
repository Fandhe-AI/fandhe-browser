//! 巨大静的ページ単体の削減率測定（TASK-14.5・`AISNAP-5`・Issue #96・`MS-2`）。
//!
//! 役割: 巨大静的ページ 1 件（`wikipedia-article.html`）について、snapshot を
//! 「生 DOM シリアライズ」（`raw_dom.rs`）比で測る。`AISNAP-1` の分母は生 HTML、
//! `AISNAP-5` の分母は生 DOM と異なるため、`reduction.rs` の行とは別に持つ。
//! 呼び出し元はベンチ本体（`token_reduction.rs`）とユニットテスト。
//!
//! 85% 目標の達成可否判定は本モジュールの責務ではない（#97・人間担当）。
//! 目標値に対する assert を持たない。

use std::fs;
use std::path::Path;

use fandhe_browser_ai::snapshot::build_snapshot;
use fandhe_browser_core::parse::{ParseOptions, parse_document};

use crate::raw_dom::serialize_raw_dom;
use crate::reduction::{ReductionError, reduction_pct};
use crate::snapshot_text::render_snapshot;
use crate::tokens::{TokenCountError, TokenCounter};

/// 巨大静的ページとして測るフィクスチャ名（`fixtures_inventory.rs` の `huge-static` 類型）。
pub const HUGE_STATIC_FIXTURE: &str = "wikipedia-article.html";

/// `AISNAP-5` の目標削減率（%）。参考表示専用。
pub const TARGET_PCT: f64 = 85.0;

/// 巨大静的ページ単体の測定結果。
#[derive(Debug, Clone, PartialEq)]
pub struct HugeStaticRow {
    pub name: String,
    pub bytes: usize,
    pub raw_html_tokens: usize,
    pub raw_dom_tokens: usize,
    pub snapshot_tokens: usize,
    /// `AISNAP-5` の指標。`(1 - snapshot / raw_dom) * 100`。
    pub reduction_vs_raw_dom_pct: f64,
    /// 参考。`AISNAP-1` と同じ分母（生 HTML）での削減率。
    pub reduction_vs_raw_html_pct: f64,
    pub snapshot_truncated: bool,
}

/// `dir` の巨大静的フィクスチャを測定する。
pub fn measure_huge_static(
    counter: &TokenCounter,
    dir: &Path,
) -> Result<HugeStaticRow, ReductionError> {
    let name = HUGE_STATIC_FIXTURE.to_string();
    let path = dir.join(HUGE_STATIC_FIXTURE);
    let html = fs::read_to_string(&path).map_err(|source| {
        ReductionError::Tokens(TokenCountError::Io {
            path: path.clone(),
            source,
        })
    })?;
    let parsed =
        parse_document(&html, &ParseOptions::default()).map_err(|e| ReductionError::Parse {
            name: name.clone(),
            message: format!("{e:?}"),
        })?;
    let snapshot = build_snapshot(&parsed.document).map_err(|e| ReductionError::Snapshot {
        name: name.clone(),
        message: e.to_string(),
    })?;
    let snapshot_tokens = counter.count(&render_snapshot(&snapshot));
    let raw_dom_tokens = counter.count(&serialize_raw_dom(&parsed.document));
    let raw_html_tokens = counter.count(&html);
    let empty = || ReductionError::EmptyRaw { name: name.clone() };
    let vs_dom = reduction_pct(raw_dom_tokens, snapshot_tokens).ok_or_else(empty)?;
    let vs_html = reduction_pct(raw_html_tokens, snapshot_tokens).ok_or_else(empty)?;
    Ok(HugeStaticRow {
        bytes: html.len(),
        name,
        raw_html_tokens,
        raw_dom_tokens,
        snapshot_tokens,
        reduction_vs_raw_dom_pct: vs_dom,
        reduction_vs_raw_html_pct: vs_html,
        snapshot_truncated: snapshot.truncated,
    })
}

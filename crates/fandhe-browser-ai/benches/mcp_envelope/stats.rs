//! MCP エンベロープ測定ハーネスの統計・整形の純関数群（TASK-97.1・Issue #385・`PLUG-5`・`MS-9`）。
//!
//! `mcp_envelope.rs` の main が、測定したトークン数・レイテンシ列を渡して集計と TSV 整形を行う。
//! I/O を持たず、`stats_tests.rs` が 3 OS で具体値を検証する。パーセンタイルは nearest-rank 法
//! （`rank = ceil(p/100 * n)`）。実測値の確定と判断は #386（TASK-97.2）・#387 が担う。

/// 1 fixture 分のトークン・バイト計測値（4 系列）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeRow {
    pub name: String,
    pub raw_html_tokens: usize,
    /// 方式 B 本体（`GET /ai/snapshot` の応答本文）。
    pub direct_tokens: usize,
    /// MCP `result.content[0].text`。
    pub mcp_text_tokens: usize,
    /// MCP 応答行全体（改行除く）。
    pub mcp_line_tokens: usize,
    pub direct_bytes: usize,
    pub mcp_line_bytes: usize,
}

impl EnvelopeRow {
    /// エンベロープ分のトークン増分（応答行全体 - 方式 B 本体）。
    pub fn envelope_tokens(&self) -> i64 {
        self.mcp_line_tokens as i64 - self.direct_tokens as i64
    }
    /// 方式 B 本体の対生 HTML 削減率（%）。
    pub fn direct_reduction_pct(&self) -> Option<f64> {
        reduction_pct(self.raw_html_tokens, self.direct_tokens)
    }
    /// MCP 応答行全体の対生 HTML 削減率（%）。
    pub fn mcp_reduction_pct(&self) -> Option<f64> {
        reduction_pct(self.raw_html_tokens, self.mcp_line_tokens)
    }
}

/// 削減率（%）。`raw == 0` は `None`（`token_reduction/reduction.rs` と同じ定義）。
pub fn reduction_pct(raw: usize, reduced: usize) -> Option<f64> {
    if raw == 0 {
        return None;
    }
    Some((1.0 - reduced as f64 / raw as f64) * 100.0)
}

/// nearest-rank パーセンタイル。空なら `None`。`p` は 0..=100 に丸める。
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p.clamp(0.0, 100.0) / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.max(1) - 1).copied()
}

/// 平均。空なら `None`。
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// レイテンシ列（ms）の要約。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySummary {
    pub p50: f64,
    pub p95: f64,
    pub mean: f64,
}

/// レイテンシ列を要約する。空なら `None`。
pub fn summarize_latency(values: &[f64]) -> Option<LatencySummary> {
    Some(LatencySummary {
        p50: percentile(values, 50.0)?,
        p95: percentile(values, 95.0)?,
        mean: mean(values)?,
    })
}

/// 同じ反復番号どうしの差（MCP - 直接）。長さが異なれば短い方に合わせる。
pub fn increments(mcp: &[f64], direct: &[f64]) -> Vec<f64> {
    mcp.iter().zip(direct).map(|(m, d)| m - d).collect()
}

/// 値群の集計（平均・最小・最大）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aggregate {
    pub mean: f64,
    pub min: f64,
    pub max: f64,
}

/// 値群を集計する。空なら `None`。
pub fn aggregate(values: &[f64]) -> Option<Aggregate> {
    let mean = mean(values)?;
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(Aggregate { mean, min, max })
}

/// TSV ヘッダ行。
pub fn tsv_header() -> &'static str {
    "name\trawHtmlTokens\tdirectTokens\tmcpTextTokens\tmcpLineTokens\tenvelopeTokens\tdirectBytes\tmcpLineBytes\tdirectReductionPct\tmcpReductionPct"
}

fn pct_cell(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.1}"))
}

/// TSV の 1 行。
pub fn tsv_row(r: &EnvelopeRow) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        r.name,
        r.raw_html_tokens,
        r.direct_tokens,
        r.mcp_text_tokens,
        r.mcp_line_tokens,
        r.envelope_tokens(),
        r.direct_bytes,
        r.mcp_line_bytes,
        pct_cell(r.direct_reduction_pct()),
        pct_cell(r.mcp_reduction_pct()),
    )
}

/// レイテンシ要約の TSV 行（ms、小数 3 桁）。
pub fn latency_row(label: &str, s: &LatencySummary) -> String {
    format!("{label}\t{:.3}\t{:.3}\t{:.3}", s.p50, s.p95, s.mean)
}

//! 呼び出しレイテンシ計測の型・パーセンタイル・TSV 整形（TASK-95.3・Issue #377・`PLUG-3`・`MS-9`）。
//!
//! `call_latency.rs` が採取した標本（ms）を受け取り、`plugin_overhead.rs` の main が p50・p95 を含む
//! TSV を stdout へ出す。I/O を持たない純関数のため `call_latency_tests.rs` が全 OS で検証する。
//! パーセンタイルは nearest-rank 法（補間なし）で、PoC-15 と `mcp_envelope/stats.rs` と同じ定義。
//! 値（`CallLatencyMeasurement`）と整形（`render_call_latency`）を分け、TASK-95.4 が同じ値を
//! TASK-10 形式のレポートへ流せるようにする（REPAIR-4）。目標の達成判定はここでは行わない（#379）。

use std::fmt;

/// 呼び出しレイテンシの標本（呼び出し順の ms）。
#[derive(Debug, Clone, PartialEq)]
pub struct CallLatencyMeasurement {
    pub samples_ms: Vec<f64>,
}

/// nearest-rank 法のパーセンタイル（`rank = ceil(p/100 * n)`、1 始まり）。空は `None`。
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted.get(idx).copied()
}

impl CallLatencyMeasurement {
    pub fn p50_ms(&self) -> Option<f64> {
        percentile(&self.samples_ms, 50.0)
    }
    pub fn p95_ms(&self) -> Option<f64> {
        percentile(&self.samples_ms, 95.0)
    }
    pub fn mean_ms(&self) -> Option<f64> {
        if self.samples_ms.is_empty() {
            return None;
        }
        Some(self.samples_ms.iter().sum::<f64>() / self.samples_ms.len() as f64)
    }
    pub fn min_ms(&self) -> Option<f64> {
        self.samples_ms
            .iter()
            .copied()
            .min_by(|a, b| a.total_cmp(b))
    }
    pub fn max_ms(&self) -> Option<f64> {
        self.samples_ms
            .iter()
            .copied()
            .max_by(|a, b| a.total_cmp(b))
    }
}

/// 整形時の検証エラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallLatencyError {
    /// 標本が 0 件。
    NoSamples,
    /// 標本に非有限値または負値がある。
    InvalidSample,
}

impl fmt::Display for CallLatencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSamples => f.write_str("no latency samples were collected"),
            Self::InvalidSample => {
                f.write_str("latency sample is not a finite non-negative number")
            }
        }
    }
}

impl std::error::Error for CallLatencyError {}

/// TSV（見出し行・値行・参考目標行）へ整形する。目標は参考行として出すだけで判定しない。
pub fn render_call_latency(
    m: &CallLatencyMeasurement,
    p50_target_ms: u64,
    p95_target_ms: u64,
) -> Result<String, CallLatencyError> {
    if m.samples_ms.is_empty() {
        return Err(CallLatencyError::NoSamples);
    }
    if m.samples_ms.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err(CallLatencyError::InvalidSample);
    }
    let stat = |v: Option<f64>| v.ok_or(CallLatencyError::NoSamples);
    let samples = m
        .samples_ms
        .iter()
        .map(|v| format!("{v:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "# plugin overhead: call latency, MCP tools/call snapshot round trip (PLUG-3)\n\
         # wall clock per call measured by the bench client; nearest-rank percentiles; 1 warm-up call excluded\n\
         metric\tcalls\tp50_ms\tp95_ms\tmean_ms\tmin_ms\tmax_ms\tsamples_ms\n\
         call_latency\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\n\
         # reference target (PLUG-3, official tier): p50_ms <= {p50_target_ms}, p95_ms <= {p95_target_ms} (not judged here)",
        m.samples_ms.len(),
        stat(m.p50_ms())?,
        stat(m.p95_ms())?,
        stat(m.mean_ms())?,
        stat(m.min_ms())?,
        stat(m.max_ms())?,
        samples,
    ))
}

//! cold start 計測値の型・統計・TSV 整形（TASK-95.2・Issue #376・`PLUG-3`・`MS-9`）。
//!
//! 計測手順（I/O）を持つ `cold_start.rs`、bench main（`plugin_overhead.rs`）、ユニットテスト
//! （`cold_start_tests.rs`）から使う純関数群。TASK-95.4 が同じ値を TASK-10 形式へ流せるよう
//! （REPAIR-4）、計測値の型と整形を分けている。外部入力を扱わないが、添字アクセスと `unwrap` は使わない。

use std::fmt;
use std::time::Duration;

/// cold start の計測結果。`handshake_ms` は試行順の「プロセス起動から MCP ハンドシェイク完了まで」[ms]。
#[derive(Debug, Clone, PartialEq)]
pub struct ColdStartMeasurement {
    pub handshake_ms: Vec<f64>,
}

/// 計測結果の整形失敗。メッセージは固定の英語文言（値や外部入力を含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStartError {
    NoSamples,
    InvalidSample,
}

impl fmt::Display for ColdStartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSamples => f.write_str("no cold start samples"),
            Self::InvalidSample => {
                f.write_str("cold start sample is not a finite non-negative number")
            }
        }
    }
}

impl std::error::Error for ColdStartError {}

/// `Duration` を ms（小数）へ換算する。`mcp_client.rs` の経過時間換算と同じ式。
pub fn duration_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    v
}

impl ColdStartMeasurement {
    /// 中央値（偶数件は中央 2 件の平均。空なら `None`）。PoC-15 と同じ代表値。
    pub fn median_ms(&self) -> Option<f64> {
        let v = sorted(&self.handshake_ms);
        let n = v.len();
        if n == 0 {
            return None;
        }
        let hi = *v.get(n / 2)?;
        if n % 2 == 1 {
            Some(hi)
        } else {
            let lo = *v.get(n / 2 - 1)?;
            Some((lo + hi) / 2.0)
        }
    }

    pub fn min_ms(&self) -> Option<f64> {
        sorted(&self.handshake_ms).first().copied()
    }

    pub fn max_ms(&self) -> Option<f64> {
        sorted(&self.handshake_ms).last().copied()
    }
}

/// TSV 出力の見出し行（コメント 2 行 + 列名）。
pub const COLD_START_HEADER: &str = "# plugin overhead: cold start, plugin spawn to MCP initialize handshake (PLUG-3)\n# wall clock measured by the bench client; each trial uses a fresh host and a fresh plugin process\nmetric\ttrials\tmedian_ms\tmin_ms\tmax_ms\tsamples_ms";

/// 結果を TSV へ整形する。`target_ms` は参考値として末尾に出すのみで、達成判定はしない（#379）。
pub fn render_cold_start(
    m: &ColdStartMeasurement,
    target_ms: u64,
) -> Result<String, ColdStartError> {
    if m.handshake_ms.is_empty() {
        return Err(ColdStartError::NoSamples);
    }
    if m.handshake_ms.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err(ColdStartError::InvalidSample);
    }
    let median = m.median_ms().ok_or(ColdStartError::NoSamples)?;
    let min = m.min_ms().ok_or(ColdStartError::NoSamples)?;
    let max = m.max_ms().ok_or(ColdStartError::NoSamples)?;
    let samples = m
        .handshake_ms
        .iter()
        .map(|v| format!("{v:.3}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{COLD_START_HEADER}\ncold_start\t{}\t{median:.3}\t{min:.3}\t{max:.3}\t{samples}\n# reference target (PLUG-3, official tier): median_ms <= {target_ms} (not judged here)",
        m.handshake_ms.len()
    ))
}

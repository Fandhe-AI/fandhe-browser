//! 計測結果を TASK-10 の可観測性出力形式（JSON Lines）へ整形する純関数群（TASK-95.4・Issue #378・
//! `PLUG-3`・`MS-9`・`REPAIR-9`）。
//!
//! `plugin_overhead.rs` の main が `--format jsonl` のとき、`rss.rs`・`cold_start_stats.rs`・
//! `call_latency_stats.rs` の計測値（各 `*Measurement`）をここへ渡して 1 指標 1 行の JSON にする。
//! I/O を持たず、時刻も引数で受けるため `report_tests.rs` が全 OS で決定的に検証できる。
//!
//! core の `OperationRecord` / `OperationKind` は 1 操作 1 レイテンシの形で RSS 増分や p50/p95 を表せないため
//! 変更せず、`docs/design/observability.md` §7（起動時ログを別種イベントとして同じ規約で出す前例）に倣い
//! bench 専用イベント `plugin_overhead` として出す。エンコード規約は `OperationRecord::to_json_line`
//! （同 §6）と同じで、キーは固定の snake_case・順序固定、値は固定 ASCII 文字列と整数のみ（浮動小数なし。
//! 時間は `_us`、メモリは `_bytes`）。自由形式の文字列（パス・id・エラー本文）は入れないため、手書きで
//! エスケープ不要。配列（全試行・全標本）は含めず、詳細は TSV 形式で見る。
//!
//! 目標（`PLUG-3`）は参考値として載せるだけで、達成判定はしない（#379・人間担当）。

use std::fmt;

use crate::call_latency_stats::CallLatencyMeasurement;
use crate::cold_start_stats::ColdStartMeasurement;
use crate::rss::{IdleRssMeasurement, median};

/// 整形の失敗。メッセージは固定の英語文言（値や外部入力を含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportError {
    /// 標本が 0 件。
    NoSamples,
    /// 標本に非有限値または負値がある。
    InvalidSample,
    /// 単位換算の結果が整数型に収まらない。
    OutOfRange,
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoSamples => "no samples to report",
            Self::InvalidSample => "sample is not a finite non-negative number",
            Self::OutOfRange => "converted value is out of integer range",
        })
    }
}

impl std::error::Error for ReportError {}

/// レポートに載せる参考目標（`PLUG3_TARGETS` 由来。判定には使わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Targets {
    pub idle_rss_increase_max_bytes: u64,
    pub cold_start_max_ms: u64,
    pub call_latency_p50_max_ms: u64,
    pub call_latency_p95_max_ms: u64,
}

/// 2^63（i64 の範囲検査用。f64 で正確に表現できる）。
const I64_LIMIT: f64 = 9_223_372_036_854_775_808.0;
/// 2^64（u64 の範囲検査用）。
const U64_LIMIT: f64 = 18_446_744_073_709_551_616.0;

/// ms（小数）を µs の整数へ四捨五入で換算する。非有限・負・範囲外は失敗にし、黙って飽和させない。
pub fn ms_to_us(ms: f64) -> Result<u64, ReportError> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(ReportError::InvalidSample);
    }
    let us = (ms * 1000.0).round();
    if !(0.0..U64_LIMIT).contains(&us) {
        return Err(ReportError::OutOfRange);
    }
    // 範囲検査済みのため切り詰め・飽和は起きない。
    Ok(us as u64)
}

/// KiB（小数。中央値なので整数または .5）を bytes の符号付き整数へ換算する。
pub fn kib_to_bytes(kib: f64) -> Result<i64, ReportError> {
    if !kib.is_finite() {
        return Err(ReportError::InvalidSample);
    }
    let bytes = (kib * 1024.0).round();
    if !(-I64_LIMIT..I64_LIMIT).contains(&bytes) {
        return Err(ReportError::OutOfRange);
    }
    // 範囲検査済みのため切り詰め・飽和は起きない。
    Ok(bytes as i64)
}

fn kib_to_bytes_unsigned(kib: f64) -> Result<u64, ReportError> {
    u64::try_from(kib_to_bytes(kib)?).map_err(|_| ReportError::InvalidSample)
}

fn check_samples(values: &[f64]) -> Result<(), ReportError> {
    if values.is_empty() {
        return Err(ReportError::NoSamples);
    }
    if values.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err(ReportError::InvalidSample);
    }
    Ok(())
}

fn target_ms_to_us(ms: u64) -> Result<u64, ReportError> {
    ms.checked_mul(1000).ok_or(ReportError::OutOfRange)
}

/// 3 指標共通の先頭キー（`event`・`metric`・`tier`）。
fn head(metric: &str) -> String {
    format!("{{\"event\":\"plugin_overhead\",\"metric\":\"{metric}\",\"tier\":\"official\"")
}

fn idle_rss_line(
    m: &IdleRssMeasurement,
    target_bytes: u64,
    ts: u64,
) -> Result<String, ReportError> {
    let host = median(&m.host_only_kib).ok_or(ReportError::NoSamples)?;
    let both = median(&m.host_plus_plugin_kib).ok_or(ReportError::NoSamples)?;
    let plugin = median(&m.plugin_kib).ok_or(ReportError::NoSamples)?;
    let inc = m.increment_kib().ok_or(ReportError::NoSamples)?;
    Ok(format!(
        "{},\"samples\":{},\"host_only_bytes\":{},\"host_plus_plugin_bytes\":{},\"plugin_bytes\":{},\"increment_bytes\":{},\"target_increment_max_bytes\":{target_bytes},\"timestamp_unix_ms\":{ts}}}",
        head("idle_rss"),
        m.host_plus_plugin_kib.len(),
        kib_to_bytes_unsigned(host)?,
        kib_to_bytes_unsigned(both)?,
        kib_to_bytes_unsigned(plugin)?,
        kib_to_bytes(inc)?,
    ))
}

fn cold_start_line(
    m: &ColdStartMeasurement,
    target_ms: u64,
    ts: u64,
) -> Result<String, ReportError> {
    check_samples(&m.handshake_ms)?;
    let median = m.median_ms().ok_or(ReportError::NoSamples)?;
    let min = m.min_ms().ok_or(ReportError::NoSamples)?;
    let max = m.max_ms().ok_or(ReportError::NoSamples)?;
    Ok(format!(
        "{},\"trials\":{},\"median_us\":{},\"min_us\":{},\"max_us\":{},\"target_median_max_us\":{},\"timestamp_unix_ms\":{ts}}}",
        head("cold_start"),
        m.handshake_ms.len(),
        ms_to_us(median)?,
        ms_to_us(min)?,
        ms_to_us(max)?,
        target_ms_to_us(target_ms)?,
    ))
}

fn call_latency_line(
    m: &CallLatencyMeasurement,
    p50_target_ms: u64,
    p95_target_ms: u64,
    ts: u64,
) -> Result<String, ReportError> {
    check_samples(&m.samples_ms)?;
    let us = |v: Option<f64>| ms_to_us(v.ok_or(ReportError::NoSamples)?);
    Ok(format!(
        "{},\"calls\":{},\"p50_us\":{},\"p95_us\":{},\"mean_us\":{},\"min_us\":{},\"max_us\":{},\"target_p50_max_us\":{},\"target_p95_max_us\":{},\"timestamp_unix_ms\":{ts}}}",
        head("call_latency"),
        m.samples_ms.len(),
        us(m.p50_ms())?,
        us(m.p95_ms())?,
        us(m.mean_ms())?,
        us(m.min_ms())?,
        us(m.max_ms())?,
        target_ms_to_us(p50_target_ms)?,
        target_ms_to_us(p95_target_ms)?,
    ))
}

/// 与えられた指標を `idle_rss` → `cold_start` → `call_latency` の固定順で 1 指標 1 行にし、
/// 各行を `\n` で終端して連結する（最終行の後ろにも改行が付く）。1 指標でも失敗すれば全体を失敗にし、
/// 部分的なレポートを返さない（fail-closed）。`timestamp_unix_ms` は呼び出し側が 1 回だけ取って全行で共有する。
pub fn render_jsonl(
    idle: Option<&IdleRssMeasurement>,
    cold: Option<&ColdStartMeasurement>,
    call: Option<&CallLatencyMeasurement>,
    targets: &Targets,
    timestamp_unix_ms: u64,
) -> Result<String, ReportError> {
    let mut out = String::new();
    if let Some(m) = idle {
        out.push_str(&idle_rss_line(
            m,
            targets.idle_rss_increase_max_bytes,
            timestamp_unix_ms,
        )?);
        out.push('\n');
    }
    if let Some(m) = cold {
        out.push_str(&cold_start_line(
            m,
            targets.cold_start_max_ms,
            timestamp_unix_ms,
        )?);
        out.push('\n');
    }
    if let Some(m) = call {
        out.push_str(&call_latency_line(
            m,
            targets.call_latency_p50_max_ms,
            targets.call_latency_p95_max_ms,
            timestamp_unix_ms,
        )?);
        out.push('\n');
    }
    if out.is_empty() {
        return Err(ReportError::NoSamples);
    }
    Ok(out)
}

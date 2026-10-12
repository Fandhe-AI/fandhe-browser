//! 呼び出しレイテンシ計測の純関数・引数解析のユニットテスト（TASK-95.3・Issue #377・`PLUG-3`）。
//!
//! bench は `cargo test` で実行されないため `[[test]]` として別登録している。I/O を持たない
//! 純関数のみを対象とするため全 OS で動く。

#[path = "call_latency_stats.rs"]
mod call_latency_stats;
#[path = "options.rs"]
mod options;

use call_latency_stats::{
    CallLatencyError, CallLatencyMeasurement, percentile, render_call_latency,
};
use options::{Metric, Options};

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// 順不同の 1.0..=30.0。
fn thirty() -> Vec<f64> {
    let mut v: Vec<f64> = (1..=30).map(f64::from).collect();
    v.reverse();
    v.swap(0, 17);
    v
}

#[test]
fn plug3_percentile_nearest_rank_of_thirty_samples() {
    let v = thirty();
    assert_eq!(percentile(&v, 50.0), Some(15.0));
    assert_eq!(percentile(&v, 95.0), Some(29.0));
}

#[test]
fn plug3_percentile_small_and_empty_inputs() {
    assert_eq!(percentile(&[9.0, 1.0], 50.0), Some(1.0));
    assert_eq!(percentile(&[9.0, 1.0], 95.0), Some(9.0));
    assert_eq!(percentile(&[4.5], 95.0), Some(4.5));
    assert_eq!(percentile(&[], 50.0), None);
}

#[test]
fn plug3_mean_min_max_are_exact() {
    let m = CallLatencyMeasurement {
        samples_ms: vec![2.0, 8.0, 5.0],
    };
    assert_eq!(m.mean_ms(), Some(5.0));
    assert_eq!(m.min_ms(), Some(2.0));
    assert_eq!(m.max_ms(), Some(8.0));
    assert_eq!(m.p50_ms(), Some(5.0));
    assert_eq!(m.p95_ms(), Some(8.0));
}

#[test]
fn plug3_render_call_latency_matches_expected_tsv() {
    let m = CallLatencyMeasurement {
        samples_ms: vec![2.0, 8.0, 5.0],
    };
    let out = render_call_latency(&m, 20, 100).expect("render");
    let expected = "# plugin overhead: call latency, MCP tools/call snapshot round trip (PLUG-3)\n\
# wall clock per call measured by the bench client; nearest-rank percentiles; 1 warm-up call excluded\n\
metric\tcalls\tp50_ms\tp95_ms\tmean_ms\tmin_ms\tmax_ms\tsamples_ms\n\
call_latency\t3\t5.000\t8.000\t5.000\t2.000\t8.000\t2.000,8.000,5.000\n\
# reference target (PLUG-3, official tier): p50_ms <= 20, p95_ms <= 100 (not judged here)";
    assert_eq!(out, expected);
}

#[test]
fn plug3_render_rejects_empty_and_invalid_samples() {
    let empty = CallLatencyMeasurement { samples_ms: vec![] };
    assert_eq!(
        render_call_latency(&empty, 20, 100),
        Err(CallLatencyError::NoSamples)
    );
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        let m = CallLatencyMeasurement {
            samples_ms: vec![1.0, bad],
        };
        assert_eq!(
            render_call_latency(&m, 20, 100),
            Err(CallLatencyError::InvalidSample)
        );
    }
    assert_eq!(
        CallLatencyError::NoSamples.to_string(),
        "no latency samples were collected"
    );
    assert_eq!(
        CallLatencyError::InvalidSample.to_string(),
        "latency sample is not a finite non-negative number"
    );
}

#[test]
fn plug3_options_call_latency_defaults_and_flags() {
    let d = Options::parse(&[]).expect("default");
    assert_eq!(d.calls, 30);
    assert_eq!(d.metrics, vec![Metric::IdleRss]);
    let o = Options::parse(&args(&["--metric", "call-latency", "--calls", "7"])).expect("parse");
    assert_eq!(o.calls, 7);
    assert_eq!(o.metrics, vec![Metric::CallLatency]);
    let dup = Options::parse(&args(&[
        "--metric",
        "call-latency",
        "--metric",
        "call-latency",
    ]))
    .expect("dup");
    assert_eq!(dup.metrics, vec![Metric::CallLatency]);
}

#[test]
fn plug3_options_reject_invalid_calls() {
    for bad in [
        args(&["--calls", "0"]),
        args(&["--calls", "1001"]),
        args(&["--calls"]),
        args(&["--calls", "abc"]),
    ] {
        assert!(Options::parse(&bad).is_err(), "{bad:?}");
    }
}

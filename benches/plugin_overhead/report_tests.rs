//! JSON Lines レポート整形と `--format` / `--metric all` 引数のユニットテスト
//! （TASK-95.4・Issue #378・`PLUG-3`・`REPAIR-9`）。
//!
//! I/O を持たない純関数のみを対象とするため全 OS で動く。期待値は固定入力・固定時刻に対する逐語の具体値。

// 計測値の型だけを使うため、取り込んだ stats モジュールの他の関数は未使用になる（bench main が使う）。
#![allow(dead_code)]

#[path = "call_latency_stats.rs"]
mod call_latency_stats;
#[path = "cold_start_stats.rs"]
mod cold_start_stats;
#[path = "options.rs"]
mod options;
#[path = "report.rs"]
mod report;
#[path = "rss.rs"]
mod rss;

use call_latency_stats::CallLatencyMeasurement;
use cold_start_stats::ColdStartMeasurement;
use options::{Format, Metric, Options};
use report::{ReportError, Targets, kib_to_bytes, ms_to_us, render_jsonl};
use rss::IdleRssMeasurement;

const TS: u64 = 1_700_000_000_123;

fn targets() -> Targets {
    Targets {
        idle_rss_increase_max_bytes: 10 * 1024 * 1024,
        cold_start_max_ms: 50,
        call_latency_p50_max_ms: 20,
        call_latency_p95_max_ms: 100,
    }
}

fn idle() -> IdleRssMeasurement {
    IdleRssMeasurement {
        host_only_kib: vec![1000, 1002, 1001],
        host_plus_plugin_kib: vec![3000, 3003, 3001],
        plugin_kib: vec![2000, 2001, 2000],
    }
}

fn cold() -> ColdStartMeasurement {
    ColdStartMeasurement {
        handshake_ms: vec![25.3, 17.0, 17.1904, 16.9, 17.5],
    }
}

fn call() -> CallLatencyMeasurement {
    CallLatencyMeasurement {
        samples_ms: vec![1.0, 2.0, 3.0, 4.0],
    }
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn plug3_jsonl_three_lines_match_verbatim() {
    let out = render_jsonl(Some(&idle()), Some(&cold()), Some(&call()), &targets(), TS).unwrap();
    let expected = concat!(
        "{\"event\":\"plugin_overhead\",\"metric\":\"idle_rss\",\"tier\":\"official\",\"samples\":3,\"host_only_bytes\":1025024,\"host_plus_plugin_bytes\":3073024,\"plugin_bytes\":2048000,\"increment_bytes\":2048000,\"target_increment_max_bytes\":10485760,\"timestamp_unix_ms\":1700000000123}\n",
        "{\"event\":\"plugin_overhead\",\"metric\":\"cold_start\",\"tier\":\"official\",\"trials\":5,\"median_us\":17190,\"min_us\":16900,\"max_us\":25300,\"target_median_max_us\":50000,\"timestamp_unix_ms\":1700000000123}\n",
        "{\"event\":\"plugin_overhead\",\"metric\":\"call_latency\",\"tier\":\"official\",\"calls\":4,\"p50_us\":2000,\"p95_us\":4000,\"mean_us\":2500,\"min_us\":1000,\"max_us\":4000,\"target_p50_max_us\":20000,\"target_p95_max_us\":100000,\"timestamp_unix_ms\":1700000000123}\n",
    );
    assert_eq!(out, expected);
}

#[test]
fn plug3_jsonl_lines_are_valid_json_objects_with_integers() {
    let out = render_jsonl(Some(&idle()), Some(&cold()), Some(&call()), &targets(), TS).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3);
    let metrics = ["idle_rss", "cold_start", "call_latency"];
    for (line, metric) in lines.iter().zip(metrics) {
        assert!(!line.contains('#'));
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj["event"], "plugin_overhead");
        assert_eq!(obj["metric"], metric);
        assert_eq!(obj["timestamp_unix_ms"].as_u64(), Some(TS));
        for (k, val) in obj {
            if k != "event" && k != "metric" && k != "tier" {
                assert!(val.is_i64() || val.is_u64(), "{k} must be an integer");
            }
        }
    }
}

#[test]
fn plug3_jsonl_negative_increment_is_kept_signed() {
    let m = IdleRssMeasurement {
        host_only_kib: vec![2000],
        host_plus_plugin_kib: vec![1999],
        plugin_kib: vec![100],
    };
    let out = render_jsonl(Some(&m), None, None, &targets(), TS).unwrap();
    assert!(out.contains("\"increment_bytes\":-1024,"), "{out}");
}

#[test]
fn plug3_jsonl_half_kib_median_converts_exactly() {
    let m = IdleRssMeasurement {
        host_only_kib: vec![1000, 1001],
        host_plus_plugin_kib: vec![3000, 3001],
        plugin_kib: vec![2000, 2001],
    };
    let out = render_jsonl(Some(&m), None, None, &targets(), TS).unwrap();
    assert!(out.contains("\"host_only_bytes\":1024512,"), "{out}");
    assert!(out.contains("\"increment_bytes\":2048000,"), "{out}");
}

#[test]
fn plug3_jsonl_subset_keeps_fixed_order_and_single_line() {
    let out = render_jsonl(None, Some(&cold()), None, &targets(), TS).unwrap();
    assert_eq!(out.lines().count(), 1);
    assert!(out.starts_with("{\"event\":\"plugin_overhead\",\"metric\":\"cold_start\""));
    assert!(out.ends_with("}\n"));
}

#[test]
fn plug3_ms_to_us_rounds_to_nearest() {
    assert_eq!(ms_to_us(17.1904), Ok(17190));
    assert_eq!(ms_to_us(0.0004), Ok(0));
    assert_eq!(ms_to_us(0.0006), Ok(1));
    assert_eq!(ms_to_us(0.0), Ok(0));
}

#[test]
fn plug3_conversions_reject_out_of_range() {
    assert_eq!(ms_to_us(1.0e30), Err(ReportError::OutOfRange));
    assert_eq!(kib_to_bytes(1.0e30), Err(ReportError::OutOfRange));
    assert_eq!(kib_to_bytes(f64::NAN), Err(ReportError::InvalidSample));
    assert_eq!(kib_to_bytes(1.5), Ok(1536));
    assert_eq!(kib_to_bytes(-1.0), Ok(-1024));
}

#[test]
fn plug3_jsonl_rejects_bad_samples() {
    let t = targets();
    let bad = |v: Vec<f64>| ColdStartMeasurement { handshake_ms: v };
    assert_eq!(
        render_jsonl(None, Some(&bad(vec![])), None, &t, TS),
        Err(ReportError::NoSamples)
    );
    for v in [f64::NAN, f64::INFINITY, -1.0] {
        assert_eq!(
            render_jsonl(None, Some(&bad(vec![1.0, v])), None, &t, TS),
            Err(ReportError::InvalidSample)
        );
    }
    assert_eq!(
        render_jsonl(None, Some(&bad(vec![1.0e30])), None, &t, TS),
        Err(ReportError::OutOfRange)
    );
    let c = CallLatencyMeasurement {
        samples_ms: vec![f64::NAN],
    };
    assert_eq!(
        render_jsonl(None, None, Some(&c), &t, TS),
        Err(ReportError::InvalidSample)
    );
    let empty_rss = IdleRssMeasurement {
        host_only_kib: vec![],
        host_plus_plugin_kib: vec![],
        plugin_kib: vec![],
    };
    assert_eq!(
        render_jsonl(Some(&empty_rss), None, None, &t, TS),
        Err(ReportError::NoSamples)
    );
    // 1 指標でも失敗すれば何も返さない（部分的なレポートなし）。
    assert!(render_jsonl(Some(&idle()), Some(&bad(vec![])), None, &t, TS).is_err());
    assert_eq!(
        render_jsonl(None, None, None, &t, TS),
        Err(ReportError::NoSamples)
    );
}

#[test]
fn plug3_options_format_parsing() {
    assert_eq!(Options::parse(&args(&[])).unwrap().format, Format::Tsv);
    assert_eq!(
        Options::parse(&args(&["--format", "jsonl"]))
            .unwrap()
            .format,
        Format::Jsonl
    );
    assert_eq!(
        Options::parse(&args(&["--format", "tsv"])).unwrap().format,
        Format::Tsv
    );
    assert!(Options::parse(&args(&["--format", "xml"])).is_err());
    assert!(Options::parse(&args(&["--format"])).is_err());
}

#[test]
fn plug3_options_metric_all_expands_in_fixed_order() {
    let o = Options::parse(&args(&["--metric", "all"])).unwrap();
    assert_eq!(
        o.metrics,
        vec![Metric::IdleRss, Metric::ColdStart, Metric::CallLatency]
    );
    let o = Options::parse(&args(&["--metric", "all", "--metric", "idle-rss"])).unwrap();
    assert_eq!(o.metrics.len(), 3);
    let o = Options::parse(&args(&["--metric", "call-latency", "--metric", "all"])).unwrap();
    assert_eq!(
        o.metrics,
        vec![Metric::CallLatency, Metric::IdleRss, Metric::ColdStart]
    );
}

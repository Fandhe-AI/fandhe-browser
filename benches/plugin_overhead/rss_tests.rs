//! `plugin_overhead` の RSS 解析・引数解析のユニットテスト（TASK-95.1・Issue #375・`PLUG-3`）。
//!
//! bench は `cargo test` で実行されないため `[[test]]` として別登録している。I/O を持たない純関数のみを
//! 対象とするため全 OS で動く。

#[path = "options.rs"]
mod options;
#[path = "rss.rs"]
mod rss;

use options::{Metric, Options};
use rss::{IdleRssMeasurement, PsRow, RssError};

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn plug3_parse_ps_table_reads_rows_and_skips_blank_lines() {
    let rows = rss::parse_ps_table("  100     1  4000\n\n 200   100   5000\n").expect("parse");
    assert_eq!(
        rows,
        vec![
            PsRow {
                pid: 100,
                ppid: 1,
                rss_kib: 4000
            },
            PsRow {
                pid: 200,
                ppid: 100,
                rss_kib: 5000
            },
        ]
    );
}

#[test]
fn plug3_parse_ps_table_rejects_malformed_rows() {
    assert_eq!(rss::parse_ps_table("100 1\n"), Err(RssError::MalformedRow));
    assert_eq!(
        rss::parse_ps_table("100 1 abc\n"),
        Err(RssError::MalformedRow)
    );
    assert_eq!(
        rss::parse_ps_table("100 1 2 3\n"),
        Err(RssError::MalformedRow)
    );
}

fn sample_rows() -> Vec<PsRow> {
    vec![
        PsRow {
            pid: 100,
            ppid: 1,
            rss_kib: 4000,
        },
        PsRow {
            pid: 200,
            ppid: 100,
            rss_kib: 5000,
        },
        PsRow {
            pid: 300,
            ppid: 200,
            rss_kib: 3100,
        },
        PsRow {
            pid: 999,
            ppid: 100,
            rss_kib: 1500,
        },
        PsRow {
            pid: 400,
            ppid: 1,
            rss_kib: 70000,
        },
    ]
}

#[test]
fn plug3_tree_rss_sums_descendants_and_excludes_ps() {
    let t = rss::tree_rss(&sample_rows(), 100, Some(999)).expect("tree");
    assert_eq!(t.rss_kib, 12100);
    assert_eq!(t.processes, 3);
}

#[test]
fn plug3_tree_rss_fails_when_root_is_missing() {
    assert_eq!(
        rss::tree_rss(&sample_rows(), 12345, None),
        Err(RssError::RootMissing)
    );
}

#[test]
fn plug3_tree_rss_stops_on_ppid_cycle() {
    let rows = vec![
        PsRow {
            pid: 10,
            ppid: 20,
            rss_kib: 1,
        },
        PsRow {
            pid: 20,
            ppid: 10,
            rss_kib: 2,
        },
    ];
    let t = rss::tree_rss(&rows, 10, None).expect("tree");
    assert_eq!(t.rss_kib, 3);
    assert_eq!(t.processes, 2);
}

#[test]
fn plug3_contains_in_tree_follows_parents() {
    let rows = sample_rows();
    assert!(rss::contains_in_tree(&rows, 100, 300));
    assert!(!rss::contains_in_tree(&rows, 100, 400));
    assert!(!rss::contains_in_tree(&rows, 100, 77777));
}

#[test]
fn plug3_median_handles_odd_even_and_empty() {
    assert_eq!(rss::median(&[3, 1, 2]), Some(2.0));
    assert_eq!(rss::median(&[4, 1, 3, 2]), Some(2.5));
    assert_eq!(rss::median(&[]), None);
}

fn poc15_like() -> IdleRssMeasurement {
    IdleRssMeasurement {
        host_only_kib: vec![7880, 7884, 7900],
        host_plus_plugin_kib: vec![17750, 17756, 17760],
        plugin_kib: vec![9800, 9810, 9820],
    }
}

#[test]
fn plug3_increment_is_median_difference_in_kib_and_mb() {
    let m = poc15_like();
    assert_eq!(m.increment_kib(), Some(9872.0));
    assert_eq!(format!("{:.2}", m.increment_mb().expect("mb")), "9.64");
}

#[test]
fn plug3_negative_increment_is_not_rounded_up() {
    let m = IdleRssMeasurement {
        host_only_kib: vec![2048],
        host_plus_plugin_kib: vec![1024],
        plugin_kib: vec![500],
    };
    assert_eq!(m.increment_kib(), Some(-1024.0));
    assert_eq!(m.increment_mb(), Some(-1.0));
}

#[test]
fn plug3_render_idle_rss_matches_expected_tsv() {
    let out = rss::render_idle_rss(&poc15_like(), 10 * 1024 * 1024).expect("render");
    let expected = "# plugin overhead: idle RSS increment (PLUG-3)\n\
# rss is KiB reported by ps(1); mb = KiB / 1024\n\
metric\tsamples\thost_only_kib\thost_plus_plugin_kib\tplugin_kib\tincrement_kib\tincrement_mb\n\
idle_rss\t3\t7884\t17756\t9810\t9872\t9.64\n\
# reference target (PLUG-3, official tier): increment_mb <= 10.00 (not judged here)";
    assert_eq!(out, expected);
}

#[test]
fn plug3_render_idle_rss_fails_without_samples() {
    let m = IdleRssMeasurement {
        host_only_kib: vec![],
        host_plus_plugin_kib: vec![],
        plugin_kib: vec![],
    };
    assert_eq!(rss::render_idle_rss(&m, 1), Err(RssError::NoSamples));
}

#[test]
fn plug3_options_defaults_match_poc15() {
    let o = Options::parse(&[]).expect("defaults");
    assert_eq!(o.metrics, vec![Metric::IdleRss]);
    assert_eq!((o.samples, o.settle_ms, o.interval_ms), (3, 800, 200));
    assert_eq!(o.trials, 5);
}

#[test]
fn plug3_options_accept_cold_start_metric_and_trials() {
    let o = Options::parse(&args(&[
        "--metric",
        "cold-start",
        "--metric",
        "idle-rss",
        "--metric",
        "cold-start",
        "--trials",
        "7",
    ]))
    .expect("parse");
    assert_eq!(o.metrics, vec![Metric::ColdStart, Metric::IdleRss]);
    assert_eq!(o.trials, 7);
}

#[test]
fn plug3_options_skip_bench_and_dedupe_metrics() {
    let o = Options::parse(&args(&[
        "--bench",
        "--metric",
        "idle-rss",
        "--metric",
        "idle-rss",
        "--samples",
        "5",
        "--settle-ms",
        "0",
        "--interval-ms",
        "10",
    ]))
    .expect("parse");
    assert_eq!(o.metrics, vec![Metric::IdleRss]);
    assert_eq!((o.samples, o.settle_ms, o.interval_ms), (5, 0, 10));
}

#[test]
fn plug3_options_reject_invalid_input() {
    for bad in [
        &["--samples", "0"][..],
        &["--samples", "21"],
        &["--samples"],
        &["--samples", "x"],
        &["--settle-ms", "30001"],
        &["--interval-ms", "5001"],
        &["--trials", "0"],
        &["--trials", "21"],
        &["--trials"],
        &["--trials", "x"],
        &["--metric", "latency"],
        &["--metric"],
        &["--unknown"],
    ] {
        assert!(
            Options::parse(&args(bad)).is_err(),
            "{bad:?} must be rejected"
        );
    }
}

/// 途中で I/O エラーになる reader。先頭に有効な行を返してから失敗する。
struct FailingReader {
    sent: bool,
}

impl std::io::Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent {
            return Err(std::io::Error::other("boom"));
        }
        self.sent = true;
        let data = b"100 1 4000\n";
        buf[..data.len()].copy_from_slice(data);
        Ok(data.len())
    }
}

#[test]
fn plug3_read_limited_propagates_mid_read_io_error() {
    let r = rss::read_limited(FailingReader { sent: false }, 1024);
    assert_eq!(r, Err("failed to read ps output".to_string()));
}

#[test]
fn plug3_read_limited_reads_one_byte_past_limit() {
    let r = rss::read_limited(&b"abcdef"[..], 4).expect("read");
    assert_eq!(r, b"abcde".to_vec());
}

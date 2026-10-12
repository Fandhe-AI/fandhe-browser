//! cold start 計測の純関数のユニットテスト（TASK-95.2・Issue #376・`PLUG-3`）。
//!
//! `cold_start_stats.rs` だけを取り込み、全 OS で動く。bench は `cargo test` で実行されないため別 `[[test]]`。

#[path = "cold_start_stats.rs"]
mod cold_start_stats;

use cold_start_stats::{
    COLD_START_HEADER, ColdStartError, ColdStartMeasurement, duration_ms, render_cold_start,
};
use std::time::Duration;

fn m(v: &[f64]) -> ColdStartMeasurement {
    ColdStartMeasurement {
        handshake_ms: v.to_vec(),
    }
}

#[test]
fn plug3_cold_start_median_handles_odd_even_and_empty() {
    assert_eq!(m(&[25.3, 17.0, 17.19, 16.9, 17.5]).median_ms(), Some(17.19));
    assert_eq!(m(&[10.0, 20.0]).median_ms(), Some(15.0));
    assert_eq!(m(&[]).median_ms(), None);
}

#[test]
fn plug3_cold_start_min_max_are_reported() {
    let x = m(&[25.3, 17.0, 17.19, 16.9, 17.5]);
    assert_eq!(x.min_ms(), Some(16.9));
    assert_eq!(x.max_ms(), Some(25.3));
    assert_eq!(m(&[]).min_ms(), None);
    assert_eq!(m(&[]).max_ms(), None);
}

#[test]
fn plug3_render_cold_start_matches_expected_tsv() {
    let out = render_cold_start(&m(&[25.3, 17.0, 17.19, 16.9, 17.5]), 50).expect("render");
    let expected = format!(
        "{COLD_START_HEADER}\ncold_start\t5\t17.190\t16.900\t25.300\t25.300,17.000,17.190,16.900,17.500\n# reference target (PLUG-3, official tier): median_ms <= 50 (not judged here)"
    );
    assert_eq!(out, expected);
    assert!(out.contains("metric\ttrials\tmedian_ms\tmin_ms\tmax_ms\tsamples_ms\n"));
}

#[test]
fn plug3_render_cold_start_fails_without_samples() {
    assert_eq!(
        render_cold_start(&m(&[]), 50),
        Err(ColdStartError::NoSamples)
    );
    assert_eq!(
        ColdStartError::NoSamples.to_string(),
        "no cold start samples"
    );
}

#[test]
fn plug3_render_cold_start_rejects_non_finite_or_negative_sample() {
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        assert_eq!(
            render_cold_start(&m(&[10.0, bad]), 50),
            Err(ColdStartError::InvalidSample)
        );
    }
}

#[test]
fn plug3_duration_ms_converts_to_milliseconds() {
    assert_eq!(
        format!("{:.3}", duration_ms(Duration::from_micros(17_190))),
        "17.190"
    );
}

//! `stats.rs` のユニットテスト（TASK-97.1・Issue #385・`PLUG-5`）。

#[path = "stats.rs"]
mod stats;

use stats::*;

fn row() -> EnvelopeRow {
    EnvelopeRow {
        name: "example-minimal".to_string(),
        raw_html_tokens: 100,
        direct_tokens: 20,
        mcp_text_tokens: 20,
        mcp_line_tokens: 30,
        direct_bytes: 80,
        mcp_line_bytes: 120,
    }
}

#[test]
fn plug5_percentile_nearest_rank_values() {
    let v: Vec<f64> = (1..=30).map(f64::from).collect();
    assert_eq!(percentile(&v, 50.0), Some(15.0));
    assert_eq!(percentile(&v, 95.0), Some(29.0));
    assert_eq!(percentile(&[10.0, 20.0], 50.0), Some(10.0));
    assert_eq!(percentile(&[10.0, 20.0], 95.0), Some(20.0));
    assert_eq!(percentile(&[7.0], 50.0), Some(7.0));
    assert_eq!(percentile(&[7.0], 95.0), Some(7.0));
    assert_eq!(percentile(&[], 50.0), None);
}

#[test]
fn plug5_percentile_sorts_unordered_input() {
    assert_eq!(percentile(&[30.0, 10.0, 20.0], 50.0), Some(20.0));
}

#[test]
fn plug5_summarize_latency_and_increments() {
    let s = summarize_latency(&[1.0, 2.0, 3.0, 4.0]).expect("summary");
    assert_eq!(s.p50, 2.0);
    assert_eq!(s.p95, 4.0);
    assert_eq!(s.mean, 2.5);
    assert_eq!(summarize_latency(&[]), None);
    assert_eq!(increments(&[5.0, 7.0], &[2.0, 3.0]), vec![3.0, 4.0]);
    assert_eq!(increments(&[5.0, 7.0, 9.0], &[2.0]), vec![3.0]);
}

#[test]
fn plug5_reduction_pct_zero_denominator_is_none() {
    assert_eq!(reduction_pct(0, 5), None);
    assert_eq!(reduction_pct(100, 20), Some(80.0));
}

#[test]
fn plug5_row_derived_values() {
    let r = row();
    assert_eq!(r.envelope_tokens(), 10);
    assert_eq!(r.direct_reduction_pct(), Some(80.0));
    assert_eq!(r.mcp_reduction_pct(), Some(70.0));
}

#[test]
fn plug5_tsv_format_is_exact() {
    assert_eq!(
        tsv_header(),
        "name\trawHtmlTokens\tdirectTokens\tmcpTextTokens\tmcpLineTokens\tenvelopeTokens\tdirectBytes\tmcpLineBytes\tdirectReductionPct\tmcpReductionPct"
    );
    assert_eq!(
        tsv_row(&row()),
        "example-minimal\t100\t20\t20\t30\t10\t80\t120\t80.0\t70.0"
    );
    let mut zero = row();
    zero.raw_html_tokens = 0;
    assert_eq!(
        tsv_row(&zero),
        "example-minimal\t0\t20\t20\t30\t10\t80\t120\tn/a\tn/a"
    );
    let s = LatencySummary {
        p50: 1.0,
        p95: 2.5,
        mean: 1.25,
    };
    assert_eq!(latency_row("direct", &s), "direct\t1.000\t2.500\t1.250");
}

#[test]
fn plug5_aggregate_mean_min_max() {
    let a = aggregate(&[80.0, 70.0, 90.0]).expect("aggregate");
    assert_eq!(a.mean, 80.0);
    assert_eq!(a.min, 70.0);
    assert_eq!(a.max, 90.0);
    assert_eq!(aggregate(&[]), None);
}

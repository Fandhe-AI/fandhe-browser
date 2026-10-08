//! 測定結果の TSV レポート整形（TASK-97.1・Issue #385・`PLUG-5`）。
//!
//! `mcp_envelope.rs` の main と `e2e_tests.rs` が共有する。`stats.rs` の集計・整形関数を
//! すべて通すため、e2e でも未使用項目を作らない。I/O は持たず文字列を返す。

use crate::harness::FixtureResult;
use crate::stats::{aggregate, increments, latency_row, summarize_latency, tsv_header, tsv_row};

/// 全 fixture の計測結果を TSV（行ごとに改行）へ整形する。標本が空なら `Err`。
pub fn render(results: &[FixtureResult], iterations: usize) -> Result<String, String> {
    let mut out: Vec<String> = Vec::new();
    out.push(tsv_header().to_string());
    for r in results {
        out.push(tsv_row(&r.row));
    }
    out.push(String::new());
    out.push("fixture\tpath\tp50Ms\tp95Ms\tmeanMs".to_string());
    let (mut all_direct, mut all_mcp) = (Vec::new(), Vec::new());
    for r in results {
        let inc = increments(&r.mcp_ms, &r.direct_ms);
        for (label, v) in [
            ("direct", &r.direct_ms),
            ("mcp", &r.mcp_ms),
            ("increment", &inc),
        ] {
            let s = summarize_latency(v).ok_or("no latency samples")?;
            out.push(latency_row(&format!("{}\t{label}", r.row.name), &s));
        }
        all_direct.extend_from_slice(&r.direct_ms);
        all_mcp.extend_from_slice(&r.mcp_ms);
    }
    let all_inc = increments(&all_mcp, &all_direct);
    for (label, v) in [
        ("direct", &all_direct),
        ("mcp", &all_mcp),
        ("increment", &all_inc),
    ] {
        let s = summarize_latency(v).ok_or("no latency samples")?;
        out.push(latency_row(&format!("ALL\t{label}"), &s));
    }

    let direct_pct: Vec<f64> = results
        .iter()
        .filter_map(|r| r.row.direct_reduction_pct())
        .collect();
    let mcp_pct: Vec<f64> = results
        .iter()
        .filter_map(|r| r.row.mcp_reduction_pct())
        .collect();
    let env_tokens: Vec<f64> = results
        .iter()
        .map(|r| r.row.envelope_tokens() as f64)
        .collect();
    out.push(String::new());
    out.push("# MCP envelope vs direct (PLUG-5)".to_string());
    out.push(format!("pages\t{}", results.len()));
    out.push(format!("iterations\t{iterations}"));
    for (label, v) in [
        ("DirectReductionPct", &direct_pct),
        ("McpReductionPct", &mcp_pct),
        ("EnvelopeTokens", &env_tokens),
    ] {
        let a = aggregate(v).ok_or("no aggregate")?;
        out.push(format!("mean{label}\t{:.1}", a.mean));
        out.push(format!("min{label}\t{:.1}", a.min));
        out.push(format!("max{label}\t{:.1}", a.max));
    }
    Ok(out.join("\n"))
}

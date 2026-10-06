//! 生 DOM シリアライズのトークン量計測ベンチ（TASK-23.1・`AISNAP-15`・Issue #129・`MS-2`）。
//!
//! 実行: `cargo bench -p fandhe-browser-ai --bench raw_dom_serialize_reduction`
//!
//! `benches/fixtures/` の全ページについて、`script` / `style` 等を除去した生 DOM
//! シリアライズのトークン量（`cl100k_base`）と、snapshot 対生 DOM 比の削減率・全ページ平均
//! （TASK-23.2・Issue #130）を TSV で出力する。`met`（85% 到達）は参考表示のみで終了コードへ
//! 反映しない。達成可否の判断は #131（人間担当）の担当。

#[path = "token_reduction/tokens.rs"]
mod tokens;

#[path = "token_reduction/raw_dom.rs"]
mod raw_dom;

#[path = "raw_dom_serialize_reduction/raw_dom_tokens.rs"]
mod raw_dom_tokens;

#[path = "token_reduction/snapshot_text.rs"]
mod snapshot_text;

// reduction.rs はベンチ側で使わない集計（`summarize` 等）も持つ共有モジュールのため、
// dead_code 警告を理由付きで抑止する。
#[allow(dead_code)]
#[path = "token_reduction/reduction.rs"]
mod reduction;

#[path = "raw_dom_serialize_reduction/raw_dom_reduction.rs"]
mod raw_dom_reduction;

use std::process::ExitCode;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let counter = tokens::TokenCounter::new()?;
    let rows = raw_dom_reduction::measure_raw_dom_reduction(&counter, &tokens::fixtures_dir())?;
    println!("# raw DOM serialize tokens (AISNAP-15)");
    println!(
        "name\tbytes\trawHtmlTokens\trawDomTokens\tsnapshotTokens\treductionVsRawDomPct\treductionVsRawHtmlPct\ttruncated"
    );
    for r in &rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{}",
            r.name,
            r.bytes,
            r.raw_html_tokens,
            r.raw_dom_tokens,
            r.snapshot_tokens,
            r.reduction_vs_raw_dom_pct,
            r.reduction_vs_raw_html_pct,
            r.snapshot_truncated
        );
    }
    println!();
    println!("# token reduction vs raw DOM serialize (AISNAP-15)");
    let summary = raw_dom_reduction::summarize(&rows).ok_or("no fixtures measured")?;
    println!("pages\t{}", summary.pages);
    println!("meanReductionPct\t{:.1}", summary.mean_reduction_pct);
    println!("minReductionPct\t{:.1}", summary.min_reduction_pct);
    println!("maxReductionPct\t{:.1}", summary.max_reduction_pct);
    println!("targetPct\t{:.1}", raw_dom_reduction::TARGET_PCT);
    println!(
        "met\t{}",
        summary.mean_reduction_pct >= raw_dom_reduction::TARGET_PCT
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

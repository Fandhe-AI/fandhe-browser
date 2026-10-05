//! 生 HTML・snapshot のトークン量と削減率の計測ベンチ
//! （TASK-14.2・TASK-14.3・`AISNAP-1`・Issue #93・#94・`MS-2`）。
//!
//! 実行: `cargo bench -p fandhe-browser-ai --bench token_reduction`
//!
//! `benches/fixtures/` の全ページについて、生 HTML と snapshot（`build_snapshot` の
//! 結果を `snapshot_text` で暫定テキスト化したもの）のトークン量（`cl100k_base`）、
//! 生 HTML 比の削減率、全ページ平均を TSV で出力し、続けて情報保持チェック
//! （7 種・TASK-14.4・`AISNAP-3`・Issue #95）の判別結果を出力する。件数は spec 上の
//! 14 ページではなくディレクトリ内の全件。目標値（85%）と情報保持チェックの達成判断は
//! 人間担当（#97）のため終了コードには反映しない。
//! テキスト形式は測定用の暫定形式（TASK-19・`AISNAP-6` で確定後に差し替え）。

#[path = "token_reduction/tokens.rs"]
mod tokens;

#[path = "token_reduction/snapshot_text.rs"]
mod snapshot_text;

#[path = "token_reduction/reduction.rs"]
mod reduction;

#[path = "token_reduction/retention_check.rs"]
mod retention_check;

use std::process::ExitCode;

/// `AISNAP-1` の目標削減率（%）。参考表示専用。
const TARGET_PCT: f64 = 85.0;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let counter = tokens::TokenCounter::new()?;
    let rows = reduction::measure_reduction(&counter, &tokens::fixtures_dir())?;
    let summary = reduction::summarize(&rows).ok_or("no fixtures found")?;
    println!("name\tbytes\trawHtmlTokens\tsnapshotTokens\treductionPct\ttruncated");
    for r in &rows {
        println!(
            "{}\t{}\t{}\t{}\t{:.1}\t{}",
            r.name,
            r.bytes,
            r.raw_html_tokens,
            r.snapshot_tokens,
            r.reduction_pct,
            r.snapshot_truncated
        );
    }
    println!();
    println!("# token reduction vs raw HTML (AISNAP-1)");
    println!("pages\t{}", summary.pages);
    println!("meanReductionPct\t{:.1}", summary.mean_reduction_pct);
    println!("minReductionPct\t{:.1}", summary.min_reduction_pct);
    println!("maxReductionPct\t{:.1}", summary.max_reduction_pct);
    println!(
        "medianSnapshotTokens\t{:.1}\t(AISNAP-4 reference)",
        summary.median_snapshot_tokens
    );
    println!("targetPct\t{TARGET_PCT:.1}");
    println!("met\t{}", summary.mean_reduction_pct >= TARGET_PCT);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => print_retention_checks(),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 情報保持チェック（`AISNAP-3`）の結果表を出す。判定ロジックは `retention_check.rs`。
fn print_retention_checks() -> ExitCode {
    match retention_check::run_retention_checks(&tokens::fixtures_dir()) {
        Ok(checks) => {
            println!();
            println!("# retention check (AISNAP-3)");
            println!("{}", retention_check::TSV_HEADER);
            for c in &checks {
                println!("{}", retention_check::format_row(c));
            }
            println!(
                "identified\t{}/{}",
                retention_check::identified_count(&checks),
                checks.len()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

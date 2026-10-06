//! 生 DOM シリアライズのトークン量計測ベンチ（TASK-23.1・`AISNAP-15`・Issue #129・`MS-2`）。
//!
//! 実行: `cargo bench -p fandhe-browser-ai --bench raw_dom_serialize_reduction`
//!
//! `benches/fixtures/` の全ページについて、`script` / `style` 等を除去した生 DOM
//! シリアライズのトークン量（`cl100k_base`）を TSV で出力する。削減率・平均は #130
//! （TASK-23.2）、85% 達成判断は #131（人間担当）の担当でここでは出さない。

#[path = "token_reduction/tokens.rs"]
mod tokens;

#[path = "token_reduction/raw_dom.rs"]
mod raw_dom;

#[path = "raw_dom_serialize_reduction/raw_dom_tokens.rs"]
mod raw_dom_tokens;

use std::process::ExitCode;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let counter = tokens::TokenCounter::new()?;
    let rows = raw_dom_tokens::measure_raw_dom(&counter, &tokens::fixtures_dir())?;
    println!("# raw DOM serialize tokens (AISNAP-15)");
    println!("name\tbytes\trawHtmlTokens\trawDomTokens");
    for r in &rows {
        println!(
            "{}\t{}\t{}\t{}",
            r.name, r.bytes, r.raw_html_tokens, r.raw_dom_tokens
        );
    }
    println!("pages\t{}", rows.len());
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

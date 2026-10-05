//! 生 HTML トークン量の計測ベンチ（TASK-14.2・`AISNAP-1`・Issue #93・`MS-2`）。
//!
//! 実行: `cargo bench -p fandhe-browser-ai --bench token_reduction`
//!
//! 現時点は `benches/fixtures/` の各ページの生 HTML トークン量（`cl100k_base`）だけを
//! 出力する。snapshot 経由のトークン数・削減率・平均集計は未実装で、#94
//! （TASK-14.3・`AISNAP-1`）が追加する。

#[path = "token_reduction/tokens.rs"]
mod tokens;

use std::process::ExitCode;

fn main() -> ExitCode {
    let result = tokens::TokenCounter::new()
        .and_then(|counter| tokens::measure_raw_html(&counter, &tokens::fixtures_dir()));
    match result {
        Ok(rows) => {
            println!("name\tbytes\trawHtmlTokens");
            for r in rows {
                println!("{}\t{}\t{}", r.name, r.bytes, r.tokens);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

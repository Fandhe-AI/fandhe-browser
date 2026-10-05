//! 生 HTML トークン量の計測ベンチ（TASK-14.2・`AISNAP-1`・Issue #93・`MS-2`）。
//!
//! 実行: `cargo bench -p fandhe-browser-ai --bench token_reduction`
//!
//! `benches/fixtures/` の各ページの生 HTML トークン量（`cl100k_base`）に続けて、
//! 情報保持チェック（7 種・TASK-14.4・`AISNAP-3`・Issue #95）の判別結果を出力する。
//! snapshot 経由のトークン数・削減率・平均集計は未実装で、#94
//! （TASK-14.3・`AISNAP-1`）が追加する。情報保持チェックの達成判断は人間担当（#97）で、
//! 判別数が閾値未満でも終了コードは変えない。

#[path = "token_reduction/tokens.rs"]
mod tokens;

#[path = "token_reduction/retention_check.rs"]
mod retention_check;

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
            print_retention_checks()
        }
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

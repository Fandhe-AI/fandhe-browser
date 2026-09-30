//! `calc.rs`（TASK-29・Issue #558・`PERF-7`・`CORE-3`）のユニットテスト。
//! 独立した `[[test]] lazy_spawn_unused_path_calc` ターゲット（`Cargo.toml`
//! 参照）の入口。

#[path = "calc.rs"]
mod calc;

use calc::{ChildReport, delta, median_i64, parse_child_report};

/// 正常な 1 行は全フィールドへ具体値で解釈される（PERF-7）。
#[test]
fn parses_a_well_formed_report() {
    let got = parse_child_report("mode=engineUnused pidAfterNew=0 children=0 rssKiB=12 pssKiB=-3");
    assert_eq!(
        got,
        Some(ChildReport {
            mode: "engineUnused".to_string(),
            worker_pid_after_new: false,
            child_process_count: 0,
            rss_delta_kib: 12,
            pss_delta_kib: -3,
        })
    );
}

/// キー欠損は `None`。
#[test]
fn rejects_missing_key() {
    assert_eq!(
        parse_child_report("mode=baseline pidAfterNew=0 children=0 rssKiB=1"),
        None
    );
}

/// 重複キー・未知キー・未知モードは `None`。
#[test]
fn rejects_duplicate_unknown_key_and_unknown_mode() {
    let dup = "mode=baseline mode=baseline pidAfterNew=0 children=0 rssKiB=1 pssKiB=1";
    assert_eq!(parse_child_report(dup), None);
    let unknown = "mode=baseline pidAfterNew=0 children=0 rssKiB=1 pssKiB=1 extra=1";
    assert_eq!(parse_child_report(unknown), None);
    let bad_mode = "mode=nope pidAfterNew=0 children=0 rssKiB=1 pssKiB=1";
    assert_eq!(parse_child_report(bad_mode), None);
}

/// 非数値・不正な pid フラグ・長さ超過は `None`。
#[test]
fn rejects_non_numeric_bad_flag_and_oversized_line() {
    assert_eq!(
        parse_child_report("mode=baseline pidAfterNew=0 children=x rssKiB=1 pssKiB=1"),
        None
    );
    assert_eq!(
        parse_child_report("mode=baseline pidAfterNew=2 children=0 rssKiB=1 pssKiB=1"),
        None
    );
    let long = format!("mode=baseline {}", "a".repeat(calc::MAX_REPORT_BYTES));
    assert_eq!(parse_child_report(&long), None);
}

/// 中央値は奇数個で中央値そのもの、偶数個で平均、空は `None`。負値も扱う。
#[test]
fn median_handles_odd_even_negative_and_empty() {
    assert_eq!(median_i64(&[5, -1, 3]), Some(3.0));
    assert_eq!(median_i64(&[-4, 2, 10, 0]), Some(1.0));
    assert_eq!(median_i64(&[]), None);
}

/// 差分は単純な引き算。
#[test]
fn delta_subtracts_baseline() {
    assert_eq!(delta(2.5, 1.0), 1.5);
}

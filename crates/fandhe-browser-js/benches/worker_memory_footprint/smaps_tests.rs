//! `smaps.rs`（[`parse_smaps_rollup`]・[`summarize_kib`]・
//! [`summarize_opt_kib`]。TASK-29・Issue #557・`CORE-3`・`PERF-6`・
//! `PERF-7`）のユニットテスト。独立した
//! `[[test]] worker_memory_footprint_smaps` ターゲット（`Cargo.toml`
//! 参照）の入口であり、`cargo test` から `--test`（libtest 起動）付きで
//! 実行される。
//!
//! `smaps.rs` 本体にテストを同居させない理由:
//! `worker_spawn_latency/stats.rs` と同じ事情。`smaps.rs` は
//! `worker_memory_footprint.rs`（bench 本体）からも `#[path]` で取り
//! 込まれるが、bench ターゲットは `--cfg test` は付くが `--test`
//! （libtest 起動）は付かないため、`#[test]` 関数を同居させると bench
//! 側のコンパイルで本体が展開されず「未使用 `use`」の誤検出を招く。

#[path = "smaps.rs"]
mod smaps;

use smaps::{parse_smaps_rollup, summarize_kib, summarize_opt_kib};

/// `PERF-6`・`PERF-7`・TASK-29（Issue #557）: 実機の `smaps_rollup` に近い
/// 固定文字列から全フィールドを検証する。
#[test]
fn parse_smaps_rollup_realistic_text_extracts_all_fields() {
    let text = "\
55f8a1200000-55f8a1400000 rw-p 00000000 00:00 0\n\
Rss:               12345 kB\n\
Pss:                6789 kB\n\
Pss_Anon:           5000 kB\n\
Pss_File:           1789 kB\n\
Shared_Clean:       2000 kB\n\
Shared_Dirty:        100 kB\n\
Private_Clean:       300 kB\n\
Private_Dirty:      4000 kB\n\
Swap:                  0 kB\n\
";
    let rollup = parse_smaps_rollup(text).expect("整形済みの smaps_rollup は Some を返す");

    assert_eq!(rollup.rss_kib, 12345);
    assert_eq!(rollup.pss_kib, 6789);
    assert_eq!(rollup.pss_anon_kib, Some(5000));
    assert_eq!(rollup.pss_file_kib, Some(1789));
    assert_eq!(rollup.shared_clean_kib, Some(2000));
    assert_eq!(rollup.shared_dirty_kib, Some(100));
    assert_eq!(rollup.private_clean_kib, Some(300));
    assert_eq!(rollup.private_dirty_kib, Some(4000));
    assert_eq!(rollup.swap_kib, Some(0));
}

/// `Rss` 行が無ければ `None`（0 で埋めない。REPAIR-3）。
#[test]
fn parse_smaps_rollup_missing_rss_returns_none() {
    let text = "Pss: 100 kB\n";
    assert_eq!(parse_smaps_rollup(text), None);
}

/// `Pss` が非数値なら `None`。
#[test]
fn parse_smaps_rollup_non_numeric_pss_returns_none() {
    let text = "Rss: 100 kB\nPss: not-a-number kB\n";
    assert_eq!(parse_smaps_rollup(text), None);
}

/// 同じキーが重複した場合は fail-closed で `None`。
#[test]
fn parse_smaps_rollup_duplicate_key_returns_none() {
    let text = "Rss: 100 kB\nPss: 50 kB\nRss: 200 kB\n";
    assert_eq!(parse_smaps_rollup(text), None);
}

/// 空文字列は `Rss`/`Pss` のどちらも無いため `None`。
#[test]
fn parse_smaps_rollup_empty_text_returns_none() {
    assert_eq!(parse_smaps_rollup(""), None);
}

/// 余分な空白が混じる行でも、`Rss`/`Pss` が解釈できれば破綻しない
/// （余分な空白は `trim`・`split_whitespace` で吸収される）。
#[test]
fn parse_smaps_rollup_extra_whitespace_still_parses() {
    let text = "  Rss:    100   kB  \nPss:50kB\n";
    let rollup = parse_smaps_rollup(text).expect("空白の揺れがあっても解釈できる");
    assert_eq!(rollup.rss_kib, 100);
    assert_eq!(rollup.pss_kib, 50);
}

/// `kB` 以外の単位（例: `MB`）が付いた行は fail-closed で `None` に
/// 落ちる。`Rss` 行自体が解釈できないため `parse_smaps_rollup` 全体が
/// `None` になる（想定外の単位を無条件で受理しない。REPAIR-3）。
#[test]
fn parse_smaps_rollup_non_kb_unit_returns_none() {
    let text = "Rss: 100 MB\nPss: 50 kB\n";
    assert_eq!(parse_smaps_rollup(text), None);
}

/// `kB` の後に余分なトークンが残る行（想定外の形式）は fail-closed で
/// `None` に落ちる。
#[test]
fn parse_smaps_rollup_trailing_garbage_after_kb_returns_none() {
    let text = "Rss: 100 kB extra\nPss: 50 kB\n";
    assert_eq!(parse_smaps_rollup(text), None);
}

/// `summarize_kib`: 1..=5 で median 3。
#[test]
fn summarize_kib_1_to_5_median_is_3() {
    let samples: Vec<u64> = (1..=5u64).collect();
    let summary = summarize_kib(&samples).expect("非空の入力は Some を返す");
    assert_eq!(summary.n, 5);
    assert_eq!(summary.min_kib, 1);
    assert_eq!(summary.max_kib, 5);
    assert_eq!(summary.median_kib, 3.0);
}

/// `summarize_kib`: 1..=4 で median 2.5（偶数個は中央 2 値の平均）。
#[test]
fn summarize_kib_1_to_4_median_is_2_5() {
    let samples: Vec<u64> = (1..=4u64).collect();
    let summary = summarize_kib(&samples).expect("非空の入力は Some を返す");
    assert_eq!(summary.n, 4);
    assert_eq!(summary.min_kib, 1);
    assert_eq!(summary.max_kib, 4);
    assert_eq!(summary.median_kib, 2.5);
}

/// 空入力は `None`（0 件を偽装した値で埋めない。REPAIR-3）。
#[test]
fn summarize_kib_empty_input_returns_none() {
    assert_eq!(summarize_kib(&[]), None);
}

/// 未ソートの入力を渡しても、内部でソートしてから集計する。
#[test]
fn summarize_kib_unsorted_input_is_sorted_before_aggregation() {
    let samples: Vec<u64> = vec![5, 1, 3, 2, 4];
    let summary = summarize_kib(&samples).expect("非空の入力は Some を返す");
    assert_eq!(summary.min_kib, 1);
    assert_eq!(summary.max_kib, 5);
    assert_eq!(summary.median_kib, 3.0);
}

/// `summarize_opt_kib`: 全試行が `Some` なら通常どおり要約される。
#[test]
fn summarize_opt_kib_all_some_summarizes_normally() {
    let samples: Vec<Option<u64>> = vec![Some(1), Some(2), Some(3)];
    let summary = summarize_opt_kib(&samples).expect("全 Some の入力は Some を返す");
    assert_eq!(summary.n, 3);
    assert_eq!(summary.median_kib, 2.0);
}

/// `summarize_opt_kib`: 1 件でも `None` が混じっていれば `None`
/// （all-or-none。一部の試行だけを集計して `n` を偽装しない。REPAIR-3）。
#[test]
fn summarize_opt_kib_any_none_returns_none() {
    let samples: Vec<Option<u64>> = vec![Some(1), None, Some(3)];
    assert_eq!(summarize_opt_kib(&samples), None);
}

/// `summarize_opt_kib`: 空入力は `None`。
#[test]
fn summarize_opt_kib_empty_input_returns_none() {
    let samples: Vec<Option<u64>> = vec![];
    assert_eq!(summarize_opt_kib(&samples), None);
}

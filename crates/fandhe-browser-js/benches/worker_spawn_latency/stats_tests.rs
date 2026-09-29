//! `stats.rs`（[`summarize`]。TASK-29・Issue #555・`PERF-6`・`PERF-7`）の
//! ユニットテスト。独立した `[[test]] worker_spawn_latency_stats`
//! ターゲット（`Cargo.toml` 参照）の入口であり、`cargo test` から
//! `--test`（libtest 起動）付きで実行される。
//!
//! `stats.rs` 本体にテストを同居させない理由: `stats.rs` は
//! `worker_spawn_latency.rs`（bench 本体）からも `#[path]` で取り込まれる。
//! bench ターゲットは `--cfg test` は付くが `--test` は付かないため、
//! そちらのコンパイルでは `#[test]` 関数の本体が展開されず、内部の
//! `use` が「未使用」と誤検出される（`stats.rs` 冒頭のドキュメント
//! コメント参照）。テストをこの独立ファイルへ分離することで、bench 側の
//! コンパイル単位には `#[test]` 関数が一切含まれなくなり、この問題を
//! 回避する。

#[path = "stats.rs"]
mod stats;

use std::time::Duration;

use stats::summarize;

/// `PERF-6`・`PERF-7`・TASK-29（Issue #555）: 1..=20 ms の 20 サンプルで
/// nearest-rank の p95（20 番目のうち `ceil(0.95*20) = 19` 番目 = 19ms）と
/// median（偶数個なので 10 番目と 11 番目の平均 = 10.5ms）を検証する。
#[test]
fn summarize_1_to_20ms_matches_expected_median_and_p95() {
    let samples: Vec<Duration> = (1..=20u64).map(Duration::from_millis).collect();
    let summary = summarize(&samples).expect("非空の入力は Some を返す");

    assert_eq!(summary.n, 20);
    assert_eq!(summary.min_ms, 1.0);
    assert_eq!(summary.max_ms, 20.0);
    assert_eq!(summary.median_ms, 10.5);
    assert_eq!(summary.p95_ms, 19.0);
    assert_eq!(summary.mean_ms, 10.5);
}

/// 単一サンプルでは min/median/p95/max/mean がすべて同じ値になる。
#[test]
fn summarize_single_sample_all_fields_equal_that_sample() {
    let samples = vec![Duration::from_millis(42)];
    let summary = summarize(&samples).expect("非空の入力は Some を返す");

    assert_eq!(summary.n, 1);
    assert_eq!(summary.min_ms, 42.0);
    assert_eq!(summary.median_ms, 42.0);
    assert_eq!(summary.p95_ms, 42.0);
    assert_eq!(summary.max_ms, 42.0);
    assert_eq!(summary.mean_ms, 42.0);
}

/// 未ソートの入力を渡しても、内部でソートしてから集計する。
#[test]
fn summarize_unsorted_input_is_sorted_before_aggregation() {
    let samples: Vec<Duration> = [5u64, 1, 3, 2, 4]
        .into_iter()
        .map(Duration::from_millis)
        .collect();
    let summary = summarize(&samples).expect("非空の入力は Some を返す");

    assert_eq!(summary.min_ms, 1.0);
    assert_eq!(summary.max_ms, 5.0);
    assert_eq!(summary.median_ms, 3.0);
}

/// 空入力は `None`（0 件を偽装した値で埋めない。REPAIR-3）。
#[test]
fn summarize_empty_input_returns_none() {
    assert_eq!(summarize(&[]), None);
}

/// 奇数個（5 サンプル）での中央値・p95（`ceil(0.95*5) = 5` 番目 = 最大値）
/// を確認する。
#[test]
fn summarize_odd_count_median_is_middle_element() {
    let samples: Vec<Duration> = (1..=5u64).map(Duration::from_millis).collect();
    let summary = summarize(&samples).expect("非空の入力は Some を返す");

    assert_eq!(summary.median_ms, 3.0);
    assert_eq!(summary.p95_ms, 5.0);
}

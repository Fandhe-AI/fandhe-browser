//! `calc.rs`（[`inter_arrival_deltas`]・[`per_call_from_batch_medians`]。
//! TASK-29・Issue #556・`PERF-6`・`PERF-7`）のユニットテスト。独立した
//! `[[test]] native_call_roundtrip_calc` ターゲット（`Cargo.toml` 参照）の
//! 入口。`calc.rs` 本体にテストを同居させない理由は `calc.rs` 冒頭を参照。

#[path = "calc.rs"]
mod calc;

use std::time::{Duration, Instant};

use calc::{inter_arrival_deltas, per_call_from_batch_medians};

/// 等間隔 3 点（0・10・20 ms）は 10 ms の差分 2 件になる。
#[test]
fn inter_arrival_of_three_evenly_spaced_points_yields_two_10ms_deltas() {
    let base = Instant::now();
    let arrivals = [
        base,
        base + Duration::from_millis(10),
        base + Duration::from_millis(20),
    ];
    assert_eq!(
        inter_arrival_deltas(&arrivals),
        vec![Duration::from_millis(10), Duration::from_millis(10)]
    );
}

/// 0 点・1 点は差分を作れないので空になる。
#[test]
fn inter_arrival_of_zero_or_one_point_is_empty() {
    assert!(inter_arrival_deltas(&[]).is_empty());
    assert!(inter_arrival_deltas(&[Instant::now()]).is_empty());
}

/// 時刻が逆転した組は捨てる（0 で埋めない）。
#[test]
fn inter_arrival_drops_non_monotonic_pairs() {
    let base = Instant::now();
    let arrivals = [base + Duration::from_millis(5), base];
    assert!(inter_arrival_deltas(&arrivals).is_empty());
}

/// `(2000us - 1000us) / 1000 = 1.0us`。
#[test]
fn per_call_is_difference_divided_by_n() {
    let per_call = per_call_from_batch_medians(
        Duration::from_micros(2000),
        Duration::from_micros(1000),
        1000,
    );
    assert_eq!(per_call, Some(1.0));
}

/// 差が負・`n == 0` は `None`。
#[test]
fn per_call_is_none_for_negative_difference_or_zero_n() {
    assert_eq!(
        per_call_from_batch_medians(Duration::from_micros(1), Duration::from_micros(2), 10),
        None
    );
    assert_eq!(
        per_call_from_batch_medians(Duration::from_micros(2), Duration::from_micros(1), 0),
        None
    );
}

//! `native_call_roundtrip` ベンチ（TASK-29・Issue #556・`PERF-6`・`PERF-7`）が
//! 使う、到着時刻列と評価時間から「1 往復あたりの時間」を求める純関数群。
//!
//! ベンチ本体（`native_call_roundtrip.rs`）が `#[path]` でこのファイルを
//! 取り込んで集計に使う。ユニットテストは同居させない: bench ターゲットは
//! `--cfg test` は付くが `--test`（libtest 起動）は付かないため、`#[test]`
//! 関数の本体が展開されず内部の `use` が「未使用」と誤検出される（
//! `worker_spawn_latency/stats.rs` と同じ事情）。ユニットテストは兄弟
//! ファイル `calc_tests.rs`（独立した `[[test]] native_call_roundtrip_calc`
//! ターゲットの入口。`Cargo.toml` 参照）に置く。
//!
//! `std` のみに依存する。`fandhe_browser_js::process_engine`（`js-v8` 配下）を
//! 参照すると、`required-features` なしの `[[test]]` が既定 feature で
//! コンパイルできなくなるため参照しない。

use std::time::{Duration, Instant};

/// 到着時刻の列（`NativeFn` が呼ばれた瞬間の `Instant`）から、隣接する
/// 2 点の差分を返す。差分 1 件は「`NativeReturn` の返送 → 子 JS のループ
/// 1 反復 → 次の `NativeCall` の到着」、すなわちほぼ 1 往復に当たる。
///
/// 要素数が 0・1 なら空を返す。時刻が逆転している組（単調でない到着）は
/// 0 で埋めずに捨てる（計測を装わない。REPAIR-3）。
pub fn inter_arrival_deltas(arrivals: &[Instant]) -> Vec<Duration> {
    arrivals
        .windows(2)
        .filter_map(|pair| {
            let earlier = pair.first()?;
            let later = pair.get(1)?;
            later.checked_duration_since(*earlier)
        })
        .collect()
}

/// バッチ差分法: 呼び出しありループの評価時間の中央値 `calls` と、同じ形で
/// native 呼び出しを含まないループの中央値 `plain` の差を、呼び出し回数
/// `n` で割って 1 回あたりのマイクロ秒を返す。
///
/// `n == 0`、または `calls < plain`（差が負）なら `None` を返す。負の差を
/// 0 に丸めて「無コスト」と装わない（REPAIR-3）。
pub fn per_call_from_batch_medians(calls: Duration, plain: Duration, n: u32) -> Option<f64> {
    if n == 0 {
        return None;
    }
    let diff = calls.checked_sub(plain)?;
    Some(diff.as_secs_f64() * 1_000_000.0 / f64::from(n))
}

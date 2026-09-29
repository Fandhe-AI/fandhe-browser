//! `worker_spawn_latency` ベンチ（TASK-29・Issue #555・`PERF-6`・`PERF-7`）が
//! 計測したレイテンシのサンプル列から、中央値・p95 等の要約統計を求める
//! 純関数を提供するモジュール。
//!
//! ベンチ本体（`worker_spawn_latency.rs`）が `#[path]` でこのファイルを
//! 取り込んで実測値の集計に使う。ユニットテストは同居させない: bench
//! ターゲットは `--cfg test` は付くが `--test`（libtest 起動）は付かない
//! ため、`#[test]` 関数の本体が展開されず、内部で使う `use` が
//! 「未使用」と誤検出される（bench から `#[path]` で取り込まれる非
//! 対称性がこのファイル固有の落とし穴になるため、この事情をここに
//! 明記する）。ユニットテストは兄弟ファイル `stats_tests.rs`（独立した
//! `[[test]] worker_spawn_latency_stats` ターゲットの入口。`Cargo.toml`
//! 参照）に置く。
//!
//! v8 crate・子プロセス起動など外部依存を一切持たない（`required-features`
//! 無しでどのビルドでも実行できる）。

use std::time::Duration;

/// レイテンシサンプル列の要約統計（単位はミリ秒）。
///
/// フラットな真偽値・単一の数値ではなく構造体にすることで、将来
/// パーセンタイルの追加や単位変換が必要になっても呼び出し側を壊さず
/// 拡張できる（coding-rust.md「公開 API」節）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencySummary {
    /// サンプル数。
    pub n: usize,
    pub min_ms: f64,
    pub median_ms: f64,
    /// nearest-rank 法（`ceil(0.95 * n)` 番目。1 始まり）による p95。
    pub p95_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

/// `samples` から [`LatencySummary`] を求める。`samples` が空なら `None`
/// を返す（`0` 件を「min=0・max=0」のように偽装しない。REPAIR-3・
/// security.md「偽装・回避機能の禁止」と同じ考え方をベンチ集計にも適用
/// する）。
///
/// 入力を複製してソートするため呼び出し側の順序は変えない。中央値は
/// 要素数が偶数なら中央 2 値の平均、奇数なら中央値そのもの。p95 は
/// nearest-rank 法（`ceil(0.95 * n)` 番目。1 始まりの順位）で求める。
///
/// 添字アクセス（`[]`）は使わず `get()` で明示的に処理する
/// （coding-rust.md「外部入力」節と同じ作法を、ここでは「呼び出し側の
/// 誤り（空配列・境界値）に対しても panic しない」ために適用する）。
pub fn summarize(samples: &[Duration]) -> Option<LatencySummary> {
    let n = samples.len();
    if n == 0 {
        return None;
    }

    let mut sorted_ms: Vec<f64> = samples.iter().map(duration_to_ms).collect();
    // `Duration::as_secs_f64()` は NaN を生成しないため `total_cmp` で
    // 全順序ソートできる（`partial_cmp` の `Option` 処理が不要になる）。
    sorted_ms.sort_by(f64::total_cmp);

    let min_ms = *sorted_ms.first()?;
    let max_ms = *sorted_ms.last()?;
    let sum_ms: f64 = sorted_ms.iter().sum();
    let mean_ms = sum_ms / n as f64;
    let median_ms = median_of_sorted(&sorted_ms)?;
    let p95_ms = percentile_nearest_rank(&sorted_ms, 0.95)?;

    Some(LatencySummary {
        n,
        min_ms,
        median_ms,
        p95_ms,
        max_ms,
        mean_ms,
    })
}

fn duration_to_ms(d: &Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// ソート済み配列の中央値。空なら `None`。
fn median_of_sorted(sorted_ms: &[f64]) -> Option<f64> {
    let n = sorted_ms.len();
    if n == 0 {
        return None;
    }
    if n % 2 == 1 {
        sorted_ms.get(n / 2).copied()
    } else {
        let hi = sorted_ms.get(n / 2)?;
        let lo = sorted_ms.get(n / 2 - 1)?;
        Some((hi + lo) / 2.0)
    }
}

/// ソート済み配列に対する nearest-rank 法でのパーセンタイル。
///
/// `rank = ceil(p * n)`（1 始まり。最小 1・最大 `n` にクランプ）番目の
/// 値を返す。`p95` なら `p = 0.95`。
fn percentile_nearest_rank(sorted_ms: &[f64], p: f64) -> Option<f64> {
    let n = sorted_ms.len();
    if n == 0 {
        return None;
    }
    let raw_rank = (p * n as f64).ceil() as i64;
    let clamped_rank = raw_rank.clamp(1, n as i64);
    // 1 始まりの順位 → 0 始まりの添字。
    let index = usize::try_from(clamped_rank - 1).ok()?;
    sorted_ms.get(index).copied()
}

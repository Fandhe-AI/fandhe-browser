//! `lazy_spawn_unused_path` ベンチ（TASK-29・Issue #558・`CORE-3`・`PERF-6`・
//! `PERF-7`。親 issue #519）が使う純関数群。モード子プロセスが stdout へ返す
//! 1 行レポートの検証付き解析と、符号付きサンプルの中央値・差分を担う。
//!
//! ベンチ本体（`lazy_spawn_unused_path.rs`）が `#[path]` でこのファイルを
//! 取り込む。ユニットテストは同居させない（bench ターゲットは `--test` が
//! 付かず `#[test]` が展開されないため。`worker_spawn_latency/stats.rs` と
//! 同じ事情）。兄弟ファイル `calc_tests.rs`（`[[test]]
//! lazy_spawn_unused_path_calc`。`Cargo.toml` 参照）に置く。
//!
//! `std` のみに依存する（`required-features` 無しの `[[test]]` から
//! 取り込めるようにするため、`process_engine` は参照しない）。

/// モード子が返す 1 行レポートの最大バイト数。親が読む量の上限（外部入力の
/// 長さ検証。coding-rust.md「エラーハンドリング」）。
pub const MAX_REPORT_BYTES: usize = 512;

/// 受理するモード名。親が起動するモードと 1 対 1 に対応する。
pub const MODES: [&str; 4] = ["baseline", "engineUnused", "engineUsedOnce", "bindOnly"];

/// モード子の観測結果。真偽値・件数・差分を構造体で持つ（REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildReport {
    pub mode: String,
    /// `V8ProcessEngine::new()` 直後に worker の pid があったか。
    pub worker_pid_after_new: bool,
    /// 観測時点で自プロセスを親に持つプロセスの数（OS 側の照合）。
    pub child_process_count: u32,
    pub rss_delta_kib: i64,
    pub pss_delta_kib: i64,
}

/// `mode=.. pidAfterNew=0|1 children=N rssKiB=D pssKiB=D` 形式の 1 行を解釈
/// する。キーの欠損・重複・未知キー・非数値・未知モード・長さ超過は
/// すべて `None`（fail-closed。壊れた出力から計測値を捏造しない。REPAIR-3）。
pub fn parse_child_report(line: &str) -> Option<ChildReport> {
    if line.len() > MAX_REPORT_BYTES {
        return None;
    }
    let mut mode: Option<String> = None;
    let mut pid: Option<bool> = None;
    let mut children: Option<u32> = None;
    let mut rss: Option<i64> = None;
    let mut pss: Option<i64> = None;
    for token in line.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        match key {
            "mode" => {
                if mode.is_some() || !MODES.contains(&value) {
                    return None;
                }
                mode = Some(value.to_string());
            }
            "pidAfterNew" => {
                if pid.is_some() {
                    return None;
                }
                pid = Some(match value {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                });
            }
            "children" => {
                if children.is_some() {
                    return None;
                }
                children = Some(value.parse().ok()?);
            }
            "rssKiB" => {
                if rss.is_some() {
                    return None;
                }
                rss = Some(value.parse().ok()?);
            }
            "pssKiB" => {
                if pss.is_some() {
                    return None;
                }
                pss = Some(value.parse().ok()?);
            }
            _ => return None,
        }
    }
    Some(ChildReport {
        mode: mode?,
        worker_pid_after_new: pid?,
        child_process_count: children?,
        rss_delta_kib: rss?,
        pss_delta_kib: pss?,
    })
}

/// 符号付きサンプルの中央値（偶数個は中央 2 値の平均）。空なら `None`。
pub fn median_i64(samples: &[i64]) -> Option<f64> {
    let n = samples.len();
    if n == 0 {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    if n % 2 == 1 {
        sorted.get(n / 2).map(|v| *v as f64)
    } else {
        let hi = *sorted.get(n / 2)?;
        let lo = *sorted.get(n / 2 - 1)?;
        Some((hi as f64 + lo as f64) / 2.0)
    }
}

/// `value - baseline`。モード間の増分（遅延起動のコストの切り出し）を
/// 求める。
pub fn delta(value: f64, baseline: f64) -> f64 {
    value - baseline
}

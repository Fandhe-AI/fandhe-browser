//! JS 評価用子プロセスの「起動＋ハンドシェイク」レイテンシを実測する
//! ベンチ（`TASK-29`・Issue #555・ビヘイビア `CORE-3`・`PERF-6`・
//! `PERF-7`。親 issue #519）。
//!
//! PR #503（#154）で JS 評価を子プロセス（`V8ProcessEngine`）へ分離した
//! （設計は `docs/spec` 側「JS プロセス分離」設計書）。本ベンチは、その
//! 子の「起動してから `Hello` を受信し検証を終えるまで」に何 ms
//! かかるかを実測して記録するためのものであり、目標値との比較・
//! 達成可否の判定は行わない（REPAIR-3「計測結果に達成・未達の判定を
//! 入れない」）。判定はユーザーが行う。
//!
//! # 計測区間の定義
//!
//! `spawn_worker_for_test()`（`process_engine.rs`）の呼び出しから、
//! 戻り値を受け取るまで。内訳:
//! `Command::spawn`（fork/exec・`env_clear`）→ 子の
//! `run_js_worker_if_requested` → メモリ上限の設定（Linux は
//! `RLIMIT_DATA` と `oom_score_adj`、Windows は Job Object）→ V8
//! platform の初期化（protected）→ Isolate と永続 Context の生成 →
//! `Hello` の送信 → 親の reader スレッドが受信 → decode と検証。
//!
//! # 実行方法
//!
//! ```text
//! cargo bench -p fandhe-browser-js --features test-support --bench worker_spawn_latency
//! ```
//!
//! 試行回数は環境変数 `JS_WORKER_SPAWN_TRIALS`（既定 30・範囲 1〜200）で
//! 指定できる。
//!
//! `harness = false`（`Cargo.toml` 参照）: このバイナリ自身が
//! `run_js_worker_if_requested` 経由で子プロセス役を兼ねる
//! （`tests/v8_worker.rs` と同じ作法）。子プロセス役のときの stdout は
//! プロトコルフレーム専用のため、libtest ハーネスが書く文字列は混入
//! させられない。本ベンチも独自の `main` を持ち、結果 JSON 以外を
//! stdout へ出さない。

use std::time::{Duration, Instant};

use fandhe_browser_js::EvaluateOptions;
use fandhe_browser_js::process_engine::V8ProcessEngine;

#[path = "worker_spawn_latency/stats.rs"]
mod stats;

use stats::LatencySummary;

/// 試行回数の既定値。
const DEFAULT_TRIALS: u32 = 30;
/// 試行回数の下限（1 未満は計測にならない）。
const MIN_TRIALS: u32 = 1;
/// 試行回数の上限。無制限なループでプロセスを大量生成しないための
/// リソース上限（coding-rust.md「長さ・件数を上限検証してから」・
/// security.md「不安全な設計」対応）。
const MAX_TRIALS: u32 = 200;

fn main() -> std::process::ExitCode {
    // 最優先: このプロセスが子プロセスとして起動されたものであれば、
    // ワーカーとして動作してそのまま終了する（`tests/v8_worker.rs` と
    // 同じ作法。子プロセス役のときは、これより前に何も stdout へ
    // 出してはならない）。
    if let Some(code) = fandhe_browser_js::run_js_worker_if_requested() {
        return code;
    }

    let trials = match read_trials_from_env() {
        Ok(trials) => trials,
        Err(message) => {
            eprintln!("worker_spawn_latency: {message}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match run_bench(trials) {
        Ok(report) => {
            // 結果は英語の JSON 1 行のみを stdout へ出す（プログラム
            // 出力文字列は英語。japanese-style.md）。目標値との比較・
            // 達成可否のフィールドは含めない（REPAIR-3）。
            println!("{}", report.to_json());
            std::process::ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("worker_spawn_latency: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 環境変数 `JS_WORKER_SPAWN_TRIALS` から試行回数を読む。未設定なら
/// [`DEFAULT_TRIALS`]。非 UTF-8・非数値・範囲外は fail-closed とし、
/// `Err` に英語の理由を返す（JSON を出さず exit 1 にするため。
/// coding-rust.md「外部入力」節）。
fn read_trials_from_env() -> Result<u32, String> {
    let raw = match std::env::var("JS_WORKER_SPAWN_TRIALS") {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_TRIALS),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("JS_WORKER_SPAWN_TRIALS is not valid UTF-8".to_string());
        }
    };
    let trials: u32 = raw
        .trim()
        .parse()
        .map_err(|_| format!("JS_WORKER_SPAWN_TRIALS must be an integer, got {raw:?}"))?;
    if !(MIN_TRIALS..=MAX_TRIALS).contains(&trials) {
        return Err(format!(
            "JS_WORKER_SPAWN_TRIALS must be between {MIN_TRIALS} and {MAX_TRIALS}, got {trials}"
        ));
    }
    Ok(trials)
}

/// ベンチの計測結果。目標達成可否のフィールドは持たない（REPAIR-3）。
struct BenchReport {
    trials: u32,
    first_trial_ms: f64,
    summary: LatencySummary,
    cold_first_evaluate: LatencySummary,
    warm_evaluate: LatencySummary,
}

impl BenchReport {
    /// 英語 JSON 1 行へシリアライズする。新規依存（serde 等）を追加
    /// しない方針（dependency-policy.md）のため、固定フィールド集合を
    /// 手組みで出力する。
    fn to_json(&self) -> String {
        format!(
            "{{\"bench\":\"js_worker_spawn_handshake\",\"unit\":\"ms\",\"profile\":\"bench\",\
             \"os\":\"{os}\",\"arch\":\"{arch}\",\"trials\":{trials},\
             \"firstTrialMs\":{first_trial_ms},\"summary\":{summary},\
             \"percentileMethod\":\"nearest-rank\",\
             \"coldFirstEvaluate\":{cold},\"warmEvaluate\":{warm},\
             \"note\":\"first trial excluded from summary\"}}",
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            trials = self.trials,
            first_trial_ms = self.first_trial_ms,
            summary = summary_to_json(&self.summary),
            cold = summary_to_json(&self.cold_first_evaluate),
            warm = summary_to_json(&self.warm_evaluate),
        )
    }
}

fn summary_to_json(summary: &LatencySummary) -> String {
    format!(
        "{{\"n\":{n},\"min\":{min},\"median\":{median},\"p95\":{p95},\"max\":{max},\"mean\":{mean}}}",
        n = summary.n,
        min = summary.min_ms,
        median = summary.median_ms,
        p95 = summary.p95_ms,
        max = summary.max_ms,
        mean = summary.mean_ms,
    )
}

/// 実試行回数（`trials + 1`。1 試行目はディスクキャッシュが冷えている
/// 分を含むため `firstTrialMs` として別枠に出し、要約統計からは除く）
/// だけ「起動＋ハンドシェイク」を計測し、補助指標（初回評価・2 回目
/// 評価の往復時間）もあわせて記録する。
fn run_bench(trials: u32) -> Result<BenchReport, String> {
    let total_attempts = trials
        .checked_add(1)
        .ok_or_else(|| "trial count overflowed while adding the warm-up attempt".to_string())?;

    // 検証済みの値でだけ確保する（coding-rust.md「長さ・件数を上限検証
    // してからアロケーションに使う」）。
    let mut handshake_samples: Vec<Duration> = Vec::with_capacity(total_attempts as usize);
    let mut cold_evaluate_samples: Vec<Duration> = Vec::with_capacity(total_attempts as usize);
    let mut warm_evaluate_samples: Vec<Duration> = Vec::with_capacity(total_attempts as usize);

    for attempt in 0..total_attempts {
        let mut engine = V8ProcessEngine::new();

        let start = Instant::now();
        engine
            .spawn_worker_for_test()
            .map_err(|err| format!("failed to spawn the JS worker process: {err}"))?;
        let handshake_elapsed = start.elapsed();

        if engine.worker_pid_for_test().is_none() {
            return Err(
                "JS worker process is unexpectedly absent immediately after a successful spawn"
                    .to_string(),
            );
        }

        handshake_samples.push(handshake_elapsed);

        // 補助指標: 同じ子に対する 1 回目（起動直後・cold な 1 往復）
        // と 2 回目（warm な 1 往復）の `evaluate_script` を測る。
        // `cold_start` は `spawn_worker_for_test()` 完了後に打つため、
        // ここで計測しているのは「評価 1 往復」のみであり、起動・
        // ハンドシェイクの時間は含まない（それは上の `handshake_elapsed`
        // が別途計測している）。ハンドシェイク単体の値がもっともらしい
        // かを裏付ける用途（目標判定には使わない）。
        let cold_start = Instant::now();
        engine
            .evaluate_script("0", &EvaluateOptions::default())
            .map_err(|err| format!("first evaluate_script call failed: {err}"))?;
        cold_evaluate_samples.push(cold_start.elapsed());

        let warm_start = Instant::now();
        engine
            .evaluate_script("0", &EvaluateOptions::default())
            .map_err(|err| format!("second evaluate_script call failed: {err}"))?;
        warm_evaluate_samples.push(warm_start.elapsed());

        let pid = engine.worker_pid_for_test();
        drop(engine);

        // 1 試行目の drop 後に、子 PID が確実に消えていること（試行が
        // 重なっていないこと）を Linux でだけ確認する（他 OS では
        // `/proc` に相当する軽量な確認手段が無いため確認しない。
        // `cfg(target_os = "linux")` の中に閉じる。coding-rust.md
        // 「クロスプラットフォーム」節）。
        if attempt == 0 {
            #[cfg(target_os = "linux")]
            if let Some(pid) = pid {
                assert_child_reaped_on_linux(pid)?;
            }
            #[cfg(not(target_os = "linux"))]
            let _ = pid;
        }
    }

    // 1 試行目（ウォームアップ扱い）を要約統計から除く。
    let first_trial_ms = handshake_samples
        .first()
        .map(|d| d.as_secs_f64() * 1000.0)
        .ok_or_else(|| "no trials were run".to_string())?;
    let handshake_without_warmup = handshake_samples.get(1..).unwrap_or(&[]);
    let cold_without_warmup = cold_evaluate_samples.get(1..).unwrap_or(&[]);
    let warm_without_warmup = warm_evaluate_samples.get(1..).unwrap_or(&[]);

    let summary = stats::summarize(handshake_without_warmup)
        .ok_or_else(|| "no trials remained after excluding the warm-up attempt".to_string())?;
    let cold_first_evaluate = stats::summarize(cold_without_warmup)
        .ok_or_else(|| "no cold-evaluate samples remained after excluding warm-up".to_string())?;
    let warm_evaluate = stats::summarize(warm_without_warmup)
        .ok_or_else(|| "no warm-evaluate samples remained after excluding warm-up".to_string())?;

    Ok(BenchReport {
        trials,
        first_trial_ms,
        summary,
        cold_first_evaluate,
        warm_evaluate,
    })
}

/// Linux でのみ: `pid` に対応する `/proc/<pid>` が既に消えている
/// （drop 後の graceful shutdown・reap が同期的に完了している）ことを
/// 確認する。計測している「起動＋ハンドシェイク」以外の副作用
/// （試行の重なり）を検出するための健全性チェックであり、この関数
/// 自体は計測時間に含めない（呼び出し元で `drop` 後に呼ぶ）。
#[cfg(target_os = "linux")]
fn assert_child_reaped_on_linux(pid: u32) -> Result<(), String> {
    let proc_path = std::path::PathBuf::from("/proc").join(pid.to_string());
    if proc_path.exists() {
        return Err(format!(
            "JS worker process {pid} still has a /proc entry after drop; trials may be \
             overlapping"
        ));
    }
    Ok(())
}

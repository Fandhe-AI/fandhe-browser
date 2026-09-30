//! 「JS を使わない経路では遅延起動により子プロセスが起動せず、cold start・
//! アイドル RSS に影響しない」ことを実測するベンチ（`TASK-29`・Issue #558・
//! ビヘイビア `CORE-3`・`PERF-6`・`PERF-7`。親 issue #519。sibling:
//! `worker_spawn_latency`・`native_call_roundtrip`・`worker_memory_footprint`）。
//!
//! `V8ProcessEngine`（`process_engine.rs`）は `new()` では子を起動せず、
//! 最初の注入・評価で遅延起動する（TASK-29.4・Issue #155）。本ベンチは
//! 記録だけを行い、PERF-7「cold start 増分 3ms 以内」等の目標に対する
//! 達成可否の判定はしない（REPAIR-3。判定はユーザー）。
//!
//! # 計測区間とモード
//!
//! 親は自分自身（`current_exe()`）を、環境変数 `FANDHE_BROWSER_JS_LAZY_BENCH_MODE`
//! 付きで子として起動し、`Command::spawn` から終了（`wait`）までの wall time を
//! cold start とする（PERF-7 の「プロセス起動〜終了」に対応）。モードは
//! 交互（interleave）に回し、最初の 1 巡はウォームアップとして要約から外す。
//!
//! | モード | 子の動作 |
//! | ------ | -------- |
//! | `baseline` | 何もしない（基準） |
//! | `engineUnused` | `V8ProcessEngine::new()` して評価も注入もせず drop（JS 不使用経路） |
//! | `engineUsedOnce` | `new()` → `evaluate_script("0")` 1 回 → drop（対照） |
//! | `bindOnly` | `new()` → `inject_global_function` 1 件 → drop（注入でも子が起動することの観察） |
//!
//! 各モード子は自プロセス内で worker の pid 有無、自プロセスを親に持つ
//! プロセス数（`/proc/[pid]/stat` の ppid 走査）、`/proc/self/smaps_rollup` の
//! RSS/PSS 差分（`new()` 前後）を測って 1 行で返す。`engineUnused` では
//! pid 無し・子 0 件でなければ fail-closed（非 0 終了）にする。
//!
//! # 注意（絶対値の意味）
//!
//! 全モードが V8 を静的リンクした同一バイナリ（`test-support` ⊃ `js-v8`）で
//! 動くため、比較できるのはモード間の差分だけである。「JS を使わないのに
//! V8 をリンクしていること」自体のロードコストは分離できない（TASK-30 以降・
//! `core-proto` 相当の feature on/off 再計測で扱う）。cold start の待機は
//! 期限監視のため 50µs 間隔のポーリングであり、全モードに同じ量の
//! 誤差が乗る。
//!
//! # 実行方法
//!
//! ```text
//! cargo bench -p fandhe-browser-js --features test-support --bench lazy_spawn_unused_path
//! ```
//!
//! 試行回数は環境変数 `JS_LAZY_SPAWN_TRIALS`（既定 30・範囲 1〜200）。
//! 異常値は JSON を出さず exit 1。計測は Linux 限定（`/proc`）で、他 OS では
//! 偽の JSON を出さず非 0 で終了する（REPAIR-3）。
//!
//! `harness = false`（`Cargo.toml` 参照）: このバイナリ自身が
//! `run_js_worker_if_requested` 経由でワーカー子プロセス役を兼ねるため、
//! stdout はプロトコルフレーム専用であり libtest の出力を混入できない。

#[cfg(target_os = "linux")]
#[path = "worker_spawn_latency/stats.rs"]
mod stats;

// 親が使わない要約関数（`summarize_kib` 等）を含むが、smaps のパース
// だけを再利用するため未使用警告を抑える。
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "worker_memory_footprint/smaps.rs"]
mod smaps;

#[cfg(target_os = "linux")]
#[path = "lazy_spawn_unused_path/calc.rs"]
mod calc;

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io::Read;
    use std::process::{Command, ExitCode, Stdio};
    use std::time::{Duration, Instant};

    use fandhe_browser_js::process_engine::V8ProcessEngine;
    use fandhe_browser_js::{EvaluateOptions, JsValue};

    use crate::calc::{self, ChildReport, MODES};
    use crate::smaps;
    use crate::stats::{self, LatencySummary};

    const MODE_ENV: &str = "FANDHE_BROWSER_JS_LAZY_BENCH_MODE";
    const TRIALS_ENV: &str = "JS_LAZY_SPAWN_TRIALS";
    const DEFAULT_TRIALS: u32 = 30;
    const MIN_TRIALS: u32 = 1;
    /// 試行回数の上限（プロセス大量生成を防ぐ。security.md「不安全な設計」）。
    const MAX_TRIALS: u32 = 200;
    /// モード子 1 個あたりの期限。超えたら kill して失敗にする。
    const CHILD_DEADLINE: Duration = Duration::from_secs(10);
    /// `/proc` の 1 ファイルから読む最大バイト数。
    const MAX_PROC_READ: u64 = 64 * 1024;
    /// ppid 走査で調べる `/proc` エントリ数の上限。
    const MAX_PROC_ENTRIES: usize = 200_000;

    /// エントリポイント。`MODE_ENV` があればモード子、無ければ親。
    pub(crate) fn run() -> ExitCode {
        let result = match std::env::var(MODE_ENV) {
            Ok(mode) => run_mode_child(&mode),
            Err(std::env::VarError::NotPresent) => run_parent(),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{MODE_ENV} is not valid UTF-8")),
        };
        match result {
            Ok(line) => {
                println!("{line}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("lazy_spawn_unused_path: {message}");
                ExitCode::FAILURE
            }
        }
    }

    // ---- モード子 ----

    fn run_mode_child(mode: &str) -> Result<String, String> {
        let before = read_self_smaps()?;
        let mut pid_after_new = false;
        // engine は観測後に drop する（観測時点で生存させる）。
        let engine_holder: Option<V8ProcessEngine> = match mode {
            "baseline" => None,
            "engineUnused" => {
                let engine = V8ProcessEngine::new();
                pid_after_new = engine.worker_pid_for_test().is_some();
                Some(engine)
            }
            "engineUsedOnce" => {
                let mut engine = V8ProcessEngine::new();
                pid_after_new = engine.worker_pid_for_test().is_some();
                engine
                    .evaluate_script("0", &EvaluateOptions::default())
                    .map_err(|err| format!("evaluate_script failed: {err}"))?;
                Some(engine)
            }
            "bindOnly" => {
                let mut engine = V8ProcessEngine::new();
                pid_after_new = engine.worker_pid_for_test().is_some();
                engine
                    .inject_global_function("hostNoop", Box::new(|_, _| Ok(JsValue::Number(1.0))))
                    .map_err(|err| format!("inject_global_function failed: {err}"))?;
                Some(engine)
            }
            other => return Err(format!("unknown mode {other:?}")),
        };
        let children = count_child_processes()?;
        let after = read_self_smaps()?;
        drop(engine_holder);

        if mode == "engineUnused" && (pid_after_new || children != 0) {
            return Err(format!(
                "JS-unused path spawned a worker (pidAfterNew={pid_after_new}, children={children})"
            ));
        }
        Ok(format!(
            "mode={mode} pidAfterNew={} children={children} rssKiB={} pssKiB={}",
            u8::from(pid_after_new),
            diff(after.rss_kib, before.rss_kib),
            diff(after.pss_kib, before.pss_kib),
        ))
    }

    fn diff(after: u64, before: u64) -> i64 {
        i64::try_from(after)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(before).unwrap_or(i64::MAX))
    }

    fn read_limited(path: &str) -> Result<String, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
        let mut buf = String::new();
        file.take(MAX_PROC_READ)
            .read_to_string(&mut buf)
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        Ok(buf)
    }

    fn read_self_smaps() -> Result<smaps::SmapsRollup, String> {
        let text = read_limited("/proc/self/smaps_rollup")?;
        smaps::parse_smaps_rollup(&text)
            .ok_or_else(|| "unexpected /proc/self/smaps_rollup format".to_string())
    }

    /// `/proc/[0-9]*/stat` を走査し、ppid が自 pid のプロセス数を数える。
    /// `comm` に空白・括弧が含まれうるため、最後の `)` 以降を解析する。
    fn count_child_processes() -> Result<u32, String> {
        let me = std::process::id();
        let entries = std::fs::read_dir("/proc").map_err(|e| format!("cannot read /proc: {e}"))?;
        let mut count = 0u32;
        for entry in entries.take(MAX_PROC_ENTRIES) {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            // 走査中に消えたプロセスは読み飛ばす。
            let Ok(stat) = read_limited(&format!("/proc/{name}/stat")) else {
                continue;
            };
            let Some((_, rest)) = stat.rsplit_once(')') else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let _state = fields.next();
            if fields.next().and_then(|p| p.parse::<u32>().ok()) == Some(me) {
                count = count.saturating_add(1);
            }
        }
        Ok(count)
    }

    // ---- 親 ----

    fn read_trials() -> Result<u32, String> {
        let raw = match std::env::var(TRIALS_ENV) {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_TRIALS),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(format!("{TRIALS_ENV} is not valid UTF-8"));
            }
        };
        let trials: u32 = raw
            .trim()
            .parse()
            .map_err(|_| format!("{TRIALS_ENV} must be an integer, got {raw:?}"))?;
        if !(MIN_TRIALS..=MAX_TRIALS).contains(&trials) {
            return Err(format!(
                "{TRIALS_ENV} must be between {MIN_TRIALS} and {MAX_TRIALS}, got {trials}"
            ));
        }
        Ok(trials)
    }

    /// モード子を 1 個起動し、(spawn〜wait の所要時間, レポート) を返す。
    fn run_one(mode: &str) -> Result<(Duration, ChildReport), String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
        let start = Instant::now();
        let mut child = Command::new(exe)
            .env(MODE_ENV, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to spawn mode child {mode}: {e}"))?;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() > CHILD_DEADLINE {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("mode child {mode} exceeded its deadline"));
                    }
                    std::thread::sleep(Duration::from_micros(50));
                }
                Err(e) => return Err(format!("wait failed for mode child {mode}: {e}")),
            }
        };
        let elapsed = start.elapsed();
        if !status.success() {
            return Err(format!("mode child {mode} exited with {status}"));
        }
        let mut out = String::new();
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(calc::MAX_REPORT_BYTES as u64 + 1)
                .read_to_string(&mut out)
                .map_err(|e| format!("cannot read mode child stdout: {e}"))?;
        }
        let report = calc::parse_child_report(out.trim())
            .ok_or_else(|| format!("mode child {mode} returned an unexpected report"))?;
        if report.mode != mode {
            return Err(format!("mode child {mode} reported mode {}", report.mode));
        }
        if mode == "engineUnused"
            && (report.worker_pid_after_new || report.child_process_count != 0)
        {
            return Err("engineUnused observed a worker process".to_string());
        }
        Ok((elapsed, report))
    }

    fn summary_json(s: &LatencySummary) -> String {
        format!(
            "{{\"n\":{},\"min\":{},\"median\":{},\"p95\":{},\"max\":{},\"mean\":{}}}",
            s.n, s.min_ms, s.median_ms, s.p95_ms, s.max_ms, s.mean_ms
        )
    }

    fn run_parent() -> Result<String, String> {
        let trials = read_trials()?;
        let attempts = trials
            .checked_add(1)
            .ok_or_else(|| "trial count overflowed".to_string())?;
        let mut times: Vec<Vec<Duration>> = MODES.iter().map(|_| Vec::new()).collect();
        let mut reports: Vec<Vec<ChildReport>> = MODES.iter().map(|_| Vec::new()).collect();
        for _ in 0..attempts {
            for (i, mode) in MODES.iter().enumerate() {
                let (elapsed, report) = run_one(mode)?;
                times.get_mut(i).ok_or("mode index")?.push(elapsed);
                reports.get_mut(i).ok_or("mode index")?.push(report);
            }
        }

        let mut cold = Vec::new();
        let mut deltas = Vec::new();
        let mut in_process = Vec::new();
        let mut baseline: Option<LatencySummary> = None;
        for (i, mode) in MODES.iter().enumerate() {
            // 1 巡目（ウォームアップ）を除く。
            let t = times.get(i).and_then(|v| v.get(1..)).unwrap_or(&[]);
            let summary = stats::summarize(t)
                .ok_or_else(|| format!("no samples for mode {mode} after warm-up"))?;
            if *mode == "baseline" {
                baseline = Some(summary);
            }
            let base = baseline.ok_or("baseline must run first")?;
            cold.push(format!("\"{mode}\":{}", summary_json(&summary)));
            deltas.push(format!(
                "\"{mode}\":{{\"medianMs\":{},\"p95Ms\":{}}}",
                calc::delta(summary.median_ms, base.median_ms),
                calc::delta(summary.p95_ms, base.p95_ms)
            ));

            let r = reports.get(i).and_then(|v| v.get(1..)).unwrap_or(&[]);
            let rss: Vec<i64> = r.iter().map(|x| x.rss_delta_kib).collect();
            let pss: Vec<i64> = r.iter().map(|x| x.pss_delta_kib).collect();
            let max_children = r.iter().map(|x| x.child_process_count).max().unwrap_or(0);
            let pid_seen = r.iter().any(|x| x.worker_pid_after_new);
            in_process.push(format!(
                "\"{mode}\":{{\"workerPidAfterNew\":{pid_seen},\"maxChildProcessCount\":{max_children},\
                 \"rssDeltaKiBMedian\":{},\"pssDeltaKiBMedian\":{}}}",
                calc::median_i64(&rss).ok_or("no rss samples")?,
                calc::median_i64(&pss).ok_or("no pss samples")?,
            ));
        }
        Ok(format!(
            "{{\"bench\":\"js_lazy_spawn_unused_path\",\"unit\":\"ms\",\"memoryUnit\":\"KiB\",\
             \"profile\":\"bench\",\"os\":\"{}\",\"arch\":\"{}\",\"trials\":{trials},\
             \"percentileMethod\":\"nearest-rank\",\"coldStart\":{{{}}},\
             \"coldStartDeltaVsBaseline\":{{{}}},\"inProcess\":{{{}}},\
             \"note\":\"first round excluded; self-re-exec of the bench binary (V8 statically \
             linked), not the release CLI; only deltas isolate the lazy-spawn cost\"}}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            cold.join(","),
            deltas.join(","),
            in_process.join(","),
        ))
    }
}

fn main() -> std::process::ExitCode {
    // 最優先: ワーカー子プロセスとして起動された場合はワーカーに徹する
    // （これより前に stdout へ出さない。`worker_spawn_latency.rs` と同じ作法）。
    if let Some(code) = fandhe_browser_js::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::run()
    }

    #[cfg(not(target_os = "linux"))]
    {
        // 偽の JSON は出さない（実装済みを装わない。REPAIR-3）。
        eprintln!(
            "lazy_spawn_unused_path: measurement is only implemented on Linux \
             (/proc/<pid>/stat, smaps_rollup)"
        );
        std::process::ExitCode::FAILURE
    }
}

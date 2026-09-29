//! JS 評価用子プロセスの **アイドル時** と **評価中** のメモリ量
//! （RSS/PSS）を実測するベンチ（`TASK-29`・Issue #557・ビヘイビア
//! `CORE-3`・`PERF-6`・`PERF-7`。親 issue #519。sibling: #555 の
//! `worker_spawn_latency`）。
//!
//! PR #503（#154）で JS 評価を子プロセス（`V8ProcessEngine`）へ分離
//! した。本ベンチは、その子プロセスが実際にどれだけメモリを使うかを
//! 実測して記録するためのものであり、目標値との比較・達成可否の判定
//! は行わない（REPAIR-3「計測結果に達成・未達の判定を入れない」）。
//! 判定はユーザーが行う。
//!
//! # 計測フェーズの定義
//!
//! 1 試行につき、新しい子プロセスを 1 つ起動して次の 3 フェーズを
//! 順に計測する。
//!
//! - **`idleAfterSpawn`**（起動直後のアイドル）: `spawn_worker_for_test`
//!   完了・定着待ち（[`linux_impl::SETTLE`]）後、子と親の
//!   `/proc/<pid>/smaps_rollup` を 1 回ずつ読む
//! - **`evalPeak`**（評価中のピーク）: ワークロード
//!   （`cpuLoop`・`alloc`）ごとに、別スレッドで
//!   [`linux_impl::SAMPLE_INTERVAL`] 間隔で子・親の smaps_rollup を
//!   サンプリングしながら `evaluate_script` を 1 回呼び、子の Rss が
//!   最大だった時点の値を記録する
//! - **`idleAfterEval`**（評価後のアイドル）: 定着待ち後、
//!   `idleAfterSpawn` と同じ方法で読む
//!
//! # PSS についての注意
//!
//! 親と子は同じバイナリの共有ページ（テキストセグメント等）を持つため、
//! 単純に Rss を足すと二重に数えてしまう。PSS
//! （proportional set size）はページをそのページを共有する全プロセス数
//! で割って按分するため、二重計上を避けられるが、按分の相手はこの 2
//! プロセスに限らずシステム全体の共有者（libc 等）を含む。したがって
//! `rssMinusPss`（`sumRss - sumPss`）は「親子間の二重計上分」の
//! **近似値**であり、厳密な等号ではない。
//!
//! # 実行方法
//!
//! ```text
//! cargo bench -p fandhe-browser-js --features test-support --bench worker_memory_footprint
//! ```
//!
//! 試行回数は環境変数 `JS_WORKER_MEMORY_TRIALS`（既定 5・範囲 1〜50）で
//! 指定できる。
//!
//! `harness = false`（`Cargo.toml` 参照）: `worker_spawn_latency` と同じ
//! 作法で、このバイナリ自身が `run_js_worker_if_requested` 経由で子
//! プロセス役を兼ねる。子プロセス役のときの stdout はプロトコル
//! フレーム専用のため、libtest/`cargo bench` の既定ハーネスが書く文字列
//! を混入させられない。
//!
//! # Linux 限定
//!
//! `/proc/<pid>/smaps_rollup` は Linux 固有のため、本ベンチは
//! `#[cfg(target_os = "linux")]` の [`linux_impl`] モジュールでのみ計測を
//! 行う。他 OS では偽の JSON を出さず、非 0 で終了する（実装済みを装わ
//! ない。REPAIR-3）。macOS/Windows には PSS に相当する指標が無いため、
//! 将来計測する場合は別 issue で扱う（out-of-scope-tracking.md）。

// `smaps_rollup` パーサ・要約統計（`worker_spawn_latency/stats.rs` と同じ
// 構成）。ファイルルート（このファイル自身が bench バイナリの crate
// root）の直下で `#[path]` を宣言する: crate root の子モジュールに対する
// `#[path]` はこのファイルと同じディレクトリ（`benches/`）からの相対
// パスになる（`linux_impl` のような入れ子の inline モジュール内で宣言
// すると、その入れ子モジュール自身の暗黙のディレクトリ
// `benches/linux_impl/` が基準になってしまい、意図したパスに解決
// できない）。`linux_impl` からのみ使うが、モジュール自体は `linux_impl`
// と同じ `#[cfg(target_os = "linux")]` を付けて他 OS のコンパイル単位
// には現れないようにする。
#[cfg(target_os = "linux")]
#[path = "worker_memory_footprint/smaps.rs"]
mod smaps;

/// 計測本体・`/proc` の読み込み・集計 JSON 出力を担う Linux 限定
/// モジュール。他 OS からはこのモジュール自体が存在しないため、
/// `V8ProcessEngine` 等のインポートも他 OS のコンパイル単位には現れず、
/// 未使用インポート警告を避けられる。
#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use fandhe_browser_js::EvaluateOptions;
    use fandhe_browser_js::process_engine::V8ProcessEngine;

    use super::smaps::{
        MemorySummary, SmapsRollup, parse_smaps_rollup, summarize_kib, summarize_opt_kib,
    };

    /// 環境変数名（試行回数の上書き用）。
    const ENV_TRIALS: &str = "JS_WORKER_MEMORY_TRIALS";
    /// 試行回数の既定値。
    const DEFAULT_TRIALS: u32 = 5;
    /// 試行回数の下限。
    const MIN_TRIALS: u32 = 1;
    /// 試行回数の上限（無制限なループでプロセスを大量生成しないための
    /// リソース上限。coding-rust.md「長さ・件数を上限検証」・
    /// security.md「不安全な設計」対応）。
    const MAX_TRIALS: u32 = 50;
    /// 起動直後・評価後の「定着待ち」時間。子プロセスの初期化・GC 等が
    /// 落ち着くのを待ってからアイドル値を読む。
    const SETTLE: Duration = Duration::from_millis(200);
    /// 評価中のサンプリング間隔。
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(5);
    /// 1 回の評価あたりのサンプル数上限。`SCRIPT_EXECUTION_TIMEOUT`（親側
    /// 上限は概ねこれに 1 秒加えた値）に対して十分な余裕を持つ値を
    /// `SAMPLE_INTERVAL` で割った程度に設定し、サンプラースレッドが
    /// 無制限にメモリを積み続けないようにする（coding-rust.md「長さ・
    /// 件数を上限検証してからアロケーションに使う」）。
    const MAX_SAMPLES_PER_EVAL: usize = 1024;
    /// `/proc/<pid>/smaps_rollup` を読む際の 1 回あたりの読み込み上限
    /// バイト数（無制限確保を避ける。coding-rust.md「外部入力」節）。
    /// 実際の smaps_rollup は数百バイト程度だが、想定外に巨大な内容が
    /// 返っても確保量を抑える。
    const SMAPS_READ_CAP_BYTES: usize = 64 * 1024;

    /// CPU を消費するだけでほとんど確保しないワークロード。約 400ms で
    /// 切り上げる（`Date.now()` による自己制御）。ヒープ上限・タイム
    /// アウト・親側の RSS kill しきい値のいずれにも届かない
    /// （`v8_engine.rs`・`resource_limits.rs` の各定数に対して十分小さい）。
    /// 末尾は数値（`i`）で終える: 評価結果は親へシリアライズされて
    /// 返るため、巨大な値を返すと結果の往復自体が重くなる
    /// （`MAX_RESULT_STRING_UTF16_UNITS` 対策）。
    const WORKLOAD_CPU_LOOP: &str = r#"(function () {
        var start = Date.now();
        var i = 0;
        while (Date.now() - start < 400) {
            i = (i + 1) % 1000000;
        }
        return i;
    })();"#;

    /// 確保を伴うワークロード。約 400ms のあいだ配列を積み続けるが、
    /// 4096 要素（1 要素 1024 個の `f64` 配列 ≒ 8 KiB・合計約 32 MiB）で
    /// `length = 0` に戻すことで保持量に上限をかける（ヒープ上限
    /// 128 MiB・親側 RSS kill しきい値 320 MiB のいずれにも届かない値）。
    /// 末尾は数値（`bucket.length`）で終える。
    const WORKLOAD_ALLOC: &str = r#"(function () {
        var start = Date.now();
        var bucket = [];
        var i = 0;
        while (Date.now() - start < 400) {
            bucket.push(new Array(1024).fill(i));
            if (bucket.length >= 4096) {
                bucket.length = 0;
            }
            i = i + 1;
        }
        return bucket.length;
    })();"#;

    /// 本ベンチの `main` から呼ばれる Linux 版の実処理入口。
    pub(crate) fn run() -> ExitCode {
        let trials = match read_trials_from_env() {
            Ok(trials) => trials,
            Err(message) => {
                eprintln!("worker_memory_footprint: {message}");
                return ExitCode::FAILURE;
            }
        };

        match run_bench(trials) {
            Ok(json) => {
                // 結果は英語の JSON 1 行のみを stdout へ出す
                // （japanese-style.md「プログラム出力文字列は英語」）。
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("worker_memory_footprint: {message}");
                ExitCode::FAILURE
            }
        }
    }

    /// 環境変数 [`ENV_TRIALS`] から試行回数を読む。未設定なら
    /// [`DEFAULT_TRIALS`]。非 UTF-8・非数値・範囲外は fail-closed とし、
    /// `Err` に英語の理由を返す（`worker_spawn_latency.rs` の
    /// `read_trials_from_env` と同じ作法）。
    fn read_trials_from_env() -> Result<u32, String> {
        let raw = match std::env::var(ENV_TRIALS) {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => return Ok(DEFAULT_TRIALS),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(format!("{ENV_TRIALS} is not valid UTF-8"));
            }
        };
        let trials: u32 = raw
            .trim()
            .parse()
            .map_err(|_| format!("{ENV_TRIALS} must be an integer, got {raw:?}"))?;
        if !(MIN_TRIALS..=MAX_TRIALS).contains(&trials) {
            return Err(format!(
                "{ENV_TRIALS} must be between {MIN_TRIALS} and {MAX_TRIALS}, got {trials}"
            ));
        }
        Ok(trials)
    }

    /// `trials` 回、新しい子プロセスで 3 フェーズ（アイドル・評価中・
    /// アイドル）を計測し、結果を英語 JSON 1 行へ集計する。
    fn run_bench(trials: u32) -> Result<String, String> {
        let mut idle_after_spawn = PhaseAggregate::new();
        let mut idle_after_eval = PhaseAggregate::new();
        let mut cpu_loop_peak = EvalPeakAggregate::new();
        let mut alloc_peak = EvalPeakAggregate::new();

        for _ in 0..trials {
            let mut engine = V8ProcessEngine::new();
            engine
                .spawn_worker_for_test()
                .map_err(|err| format!("failed to spawn the JS worker process: {err}"))?;
            let pid = engine.worker_pid_for_test().ok_or_else(|| {
                "JS worker process is unexpectedly absent immediately after a successful spawn"
                    .to_string()
            })?;

            thread::sleep(SETTLE);

            let (child, parent) = read_pair(pid)?;
            idle_after_spawn.push(&child, &parent)?;

            let cpu_loop_trial = run_workload_and_sample(&mut engine, pid, WORKLOAD_CPU_LOOP)?;
            cpu_loop_peak.push(&cpu_loop_trial);

            let alloc_trial = run_workload_and_sample(&mut engine, pid, WORKLOAD_ALLOC)?;
            alloc_peak.push(&alloc_trial);

            thread::sleep(SETTLE);

            let (child, parent) = read_pair(pid)?;
            idle_after_eval.push(&child, &parent)?;

            // 次の試行のために子プロセスを確実に後始末する
            // （`WorkerHandle::drop` の手順に従う）。
            drop(engine);
        }

        Ok(format!(
            "{{\"bench\":\"js_worker_memory_footprint\",\"unit\":\"KiB\",\"profile\":\"bench\",\
             \"os\":\"{os}\",\"arch\":\"{arch}\",\"trials\":{trials},\
             \"sampleIntervalMs\":{sample_interval_ms},\"settleMs\":{settle_ms},\
             \"idleAfterSpawn\":{idle_after_spawn},\"idleAfterEval\":{idle_after_eval},\
             \"evalPeak\":{{\"cpuLoop\":{cpu_loop},\"alloc\":{alloc}}},\
             \"note\":\"PSS splits shared pages across all sharers system-wide (e.g. libc), \
             so rssMinusPss approximates, not equals, the parent/child double count; parent \
             and child are the same bench binary (self re-exec, built with test-support in \
             bench profile), so absolute values differ from the release CLI\"}}",
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            trials = trials,
            sample_interval_ms = SAMPLE_INTERVAL.as_millis(),
            settle_ms = SETTLE.as_millis(),
            idle_after_spawn = idle_after_spawn.to_json()?,
            idle_after_eval = idle_after_eval.to_json()?,
            cpu_loop = cpu_loop_peak.to_json()?,
            alloc = alloc_peak.to_json()?,
        ))
    }

    /// ワークロードを 1 回評価しつつ、別スレッドで子・親の smaps_rollup を
    /// [`SAMPLE_INTERVAL`] 間隔でサンプリングし、子の Rss が最大だった
    /// 時点の値を返す。
    ///
    /// サンプラースレッドの結果は `JoinHandle` の戻り値として受け取る
    /// （共有 `Mutex` を介さない）。停止は `Arc<AtomicBool>` で指示し、
    /// 評価が戻ったら必ず `join` して回収する（スレッドを detach した
    /// ままにしない）。
    fn run_workload_and_sample(
        engine: &mut V8ProcessEngine,
        pid: u32,
        workload: &str,
    ) -> Result<EvalPeakTrial, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);

        let handle = thread::spawn(move || -> Result<Vec<(SmapsRollup, SmapsRollup)>, String> {
            let mut samples = Vec::new();
            while !stop_for_thread.load(Ordering::Relaxed) && samples.len() < MAX_SAMPLES_PER_EVAL {
                // 子が評価中にクラッシュ・破棄された場合はここで `Err`
                // になる。その場合は試行を中断する（設計 3.3 参照。pid
                // が再利用される恐れがあるため、静かに読み飛ばさない）。
                let pair = read_pair(pid)?;
                samples.push(pair);
                thread::sleep(SAMPLE_INTERVAL);
            }
            Ok(samples)
        });

        let eval_result = engine.evaluate_script(workload, &EvaluateOptions::default());
        stop.store(true, Ordering::Relaxed);

        let joined = handle
            .join()
            .map_err(|_| "memory sampler thread panicked".to_string())?;

        // 評価自体の失敗を優先して報告する（サンプラーの失敗は多くの
        // 場合、子が評価中に落ちたことの副次的な症状に過ぎない）。
        if let Err(err) = eval_result {
            return Err(format!("evaluate_script failed during workload: {err}"));
        }

        let samples = joined?;
        if samples.is_empty() {
            return Err(
                "no memory samples were collected during the workload evaluation".to_string(),
            );
        }

        let mut peak_index = 0usize;
        let mut peak_rss_kib = 0u64;
        for (index, (child, _parent)) in samples.iter().enumerate() {
            if child.rss_kib >= peak_rss_kib {
                peak_rss_kib = child.rss_kib;
                peak_index = index;
            }
        }
        let (peak_child, peak_parent) = samples
            .get(peak_index)
            .ok_or_else(|| "peak sample index was out of range".to_string())?;

        Ok(EvalPeakTrial {
            child_peak_rss_kib: peak_child.rss_kib,
            child_pss_at_peak_kib: peak_child.pss_kib,
            parent_rss_at_peak_kib: peak_parent.rss_kib,
            parent_pss_at_peak_kib: peak_parent.pss_kib,
            samples_per_eval: samples.len(),
        })
    }

    /// 子（`pid`）・親（自プロセス）の `smaps_rollup` を同じタイミングで
    /// 1 回ずつ読む。
    fn read_pair(pid: u32) -> Result<(SmapsRollup, SmapsRollup), String> {
        let child = read_smaps_rollup(&child_smaps_path(pid))?;
        let parent = read_smaps_rollup(Path::new("/proc/self/smaps_rollup"))?;
        Ok((child, parent))
    }

    /// `pid` の `smaps_rollup` パスを組み立てる。文字列連結ではなく
    /// `PathBuf::join` を使う（coding-rust.md「クロスプラットフォーム」
    /// 節。本関数自体は Linux 限定だが、パス構築の作法は統一する）。
    fn child_smaps_path(pid: u32) -> PathBuf {
        PathBuf::from("/proc")
            .join(pid.to_string())
            .join("smaps_rollup")
    }

    /// `path` を [`SMAPS_READ_CAP_BYTES`] までの上限で読み、
    /// [`parse_smaps_rollup`] でパースする。読み込み・パースいずれかが
    /// 失敗したら、`VmRSS` へこっそり切り替えたりせず `Err` を返す
    /// （fail-closed。security.md「偽装・回避機能の禁止」と同じ考え方）。
    fn read_smaps_rollup(path: &Path) -> Result<SmapsRollup, String> {
        let file = std::fs::File::open(path)
            .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
        // 上限 + 1 バイトまで読み、上限を超えたら切断された内容を
        // 有効な計測値として扱わずエラーにする（fail-closed）。
        let mut limited = file.take(SMAPS_READ_CAP_BYTES as u64 + 1);
        let mut buf = Vec::new();
        limited
            .read_to_end(&mut buf)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        if buf.len() > SMAPS_READ_CAP_BYTES {
            return Err(format!(
                "{} exceeds the {SMAPS_READ_CAP_BYTES}-byte read cap (refusing truncated data)",
                path.display()
            ));
        }
        let text =
            String::from_utf8(buf).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
        parse_smaps_rollup(&text).ok_or_else(|| {
            format!(
                "failed to parse {} (missing Rss/Pss, non-numeric value, or duplicate key)",
                path.display()
            )
        })
    }

    /// [`MemorySummary`] を JSON オブジェクトへ変換する。
    fn summary_json(summary: &MemorySummary) -> String {
        format!(
            "{{\"n\":{n},\"min\":{min},\"median\":{median},\"max\":{max}}}",
            n = summary.n,
            min = summary.min_kib,
            median = summary.median_kib,
            max = summary.max_kib,
        )
    }

    /// `Option<MemorySummary>` を JSON へ変換する。`None`（一部の試行で
    /// その値が取れなかった。all-or-none）は `null` として出す（0 で
    /// 埋めて「取れたふり」をしない。REPAIR-3）。
    fn opt_summary_json(summary: &Option<MemorySummary>) -> String {
        match summary {
            Some(summary) => summary_json(summary),
            None => "null".to_string(),
        }
    }

    /// アイドルフェーズ（`idleAfterSpawn`・`idleAfterEval`）1 種類分の
    /// 子・親サンプルを集めて集計する。
    struct PhaseAggregate {
        child: RollupSamples,
        parent: RollupSamples,
        sum_rss_kib: Vec<u64>,
        sum_pss_kib: Vec<u64>,
        rss_minus_pss_kib: Vec<u64>,
    }

    impl PhaseAggregate {
        fn new() -> Self {
            Self {
                child: RollupSamples::new(),
                parent: RollupSamples::new(),
                sum_rss_kib: Vec::new(),
                sum_pss_kib: Vec::new(),
                rss_minus_pss_kib: Vec::new(),
            }
        }

        /// 1 試行分の子・親サンプルを追加する。`sumRss`・`sumPss`・
        /// `rssMinusPss` は試行ごとに計算してから積む（合算値を要約統計
        /// に対して行うと「和の中央値 ≠ 中央値の和」の誤りになるため。
        /// `checked_add`/`checked_sub` を使い、オーバーフロー・
        /// アンダーフロー（PSS がありえず Rss を超えた場合）はエラーとし
        /// て報告する）。
        fn push(&mut self, child: &SmapsRollup, parent: &SmapsRollup) -> Result<(), String> {
            self.child.push(child);
            self.parent.push(parent);

            let sum_rss_kib = child
                .rss_kib
                .checked_add(parent.rss_kib)
                .ok_or_else(|| "sumRss overflowed u64".to_string())?;
            let sum_pss_kib = child
                .pss_kib
                .checked_add(parent.pss_kib)
                .ok_or_else(|| "sumPss overflowed u64".to_string())?;
            let rss_minus_pss_kib = sum_rss_kib.checked_sub(sum_pss_kib).ok_or_else(|| {
                format!(
                    "rssMinusPss underflowed (sumRss={sum_rss_kib} < sumPss={sum_pss_kib}), \
                     which should not happen since Pss <= Rss by definition"
                )
            })?;

            self.sum_rss_kib.push(sum_rss_kib);
            self.sum_pss_kib.push(sum_pss_kib);
            self.rss_minus_pss_kib.push(rss_minus_pss_kib);
            Ok(())
        }

        fn to_json(&self) -> Result<String, String> {
            let sum_rss = summarize_kib(&self.sum_rss_kib)
                .ok_or_else(|| "no sumRss samples were collected".to_string())?;
            let sum_pss = summarize_kib(&self.sum_pss_kib)
                .ok_or_else(|| "no sumPss samples were collected".to_string())?;
            let rss_minus_pss = summarize_kib(&self.rss_minus_pss_kib)
                .ok_or_else(|| "no rssMinusPss samples were collected".to_string())?;

            Ok(format!(
                "{{\"child\":{child},\"parent\":{parent},\"sumRss\":{sum_rss},\
                 \"sumPss\":{sum_pss},\"rssMinusPss\":{rss_minus_pss}}}",
                child = self.child.to_json()?,
                parent = self.parent.to_json()?,
                sum_rss = summary_json(&sum_rss),
                sum_pss = summary_json(&sum_pss),
                rss_minus_pss = summary_json(&rss_minus_pss),
            ))
        }
    }

    /// 1 プロセス（子または親）分の `smaps_rollup` サンプルを積み、
    /// フィールドごとに要約する。パースした全フィールドを JSON へ出す
    /// （一部フィールドだけを出すと、bench ターゲットのコンパイル単位で
    /// 残りのフィールドが「一度も読まれない」warning になるため）。
    struct RollupSamples {
        rss_kib: Vec<u64>,
        pss_kib: Vec<u64>,
        pss_anon_kib: Vec<Option<u64>>,
        pss_file_kib: Vec<Option<u64>>,
        shared_clean_kib: Vec<Option<u64>>,
        shared_dirty_kib: Vec<Option<u64>>,
        private_clean_kib: Vec<Option<u64>>,
        private_dirty_kib: Vec<Option<u64>>,
        swap_kib: Vec<Option<u64>>,
    }

    impl RollupSamples {
        fn new() -> Self {
            Self {
                rss_kib: Vec::new(),
                pss_kib: Vec::new(),
                pss_anon_kib: Vec::new(),
                pss_file_kib: Vec::new(),
                shared_clean_kib: Vec::new(),
                shared_dirty_kib: Vec::new(),
                private_clean_kib: Vec::new(),
                private_dirty_kib: Vec::new(),
                swap_kib: Vec::new(),
            }
        }

        fn push(&mut self, rollup: &SmapsRollup) {
            self.rss_kib.push(rollup.rss_kib);
            self.pss_kib.push(rollup.pss_kib);
            self.pss_anon_kib.push(rollup.pss_anon_kib);
            self.pss_file_kib.push(rollup.pss_file_kib);
            self.shared_clean_kib.push(rollup.shared_clean_kib);
            self.shared_dirty_kib.push(rollup.shared_dirty_kib);
            self.private_clean_kib.push(rollup.private_clean_kib);
            self.private_dirty_kib.push(rollup.private_dirty_kib);
            self.swap_kib.push(rollup.swap_kib);
        }

        fn to_json(&self) -> Result<String, String> {
            let rss = summarize_kib(&self.rss_kib)
                .ok_or_else(|| "no rss samples were collected".to_string())?;
            let pss = summarize_kib(&self.pss_kib)
                .ok_or_else(|| "no pss samples were collected".to_string())?;

            Ok(format!(
                "{{\"rss\":{rss},\"pss\":{pss},\"pssAnon\":{pss_anon},\"pssFile\":{pss_file},\
                 \"sharedClean\":{shared_clean},\"sharedDirty\":{shared_dirty},\
                 \"privateClean\":{private_clean},\"privateDirty\":{private_dirty},\
                 \"swap\":{swap}}}",
                rss = summary_json(&rss),
                pss = summary_json(&pss),
                pss_anon = opt_summary_json(&summarize_opt_kib(&self.pss_anon_kib)),
                pss_file = opt_summary_json(&summarize_opt_kib(&self.pss_file_kib)),
                shared_clean = opt_summary_json(&summarize_opt_kib(&self.shared_clean_kib)),
                shared_dirty = opt_summary_json(&summarize_opt_kib(&self.shared_dirty_kib)),
                private_clean = opt_summary_json(&summarize_opt_kib(&self.private_clean_kib)),
                private_dirty = opt_summary_json(&summarize_opt_kib(&self.private_dirty_kib)),
                swap = opt_summary_json(&summarize_opt_kib(&self.swap_kib)),
            ))
        }
    }

    /// 1 試行分の「評価中のピーク」1 ワークロード分の値。
    struct EvalPeakTrial {
        child_peak_rss_kib: u64,
        child_pss_at_peak_kib: u64,
        parent_rss_at_peak_kib: u64,
        parent_pss_at_peak_kib: u64,
        samples_per_eval: usize,
    }

    /// 1 ワークロード分の [`EvalPeakTrial`] を試行数だけ集めて要約する。
    struct EvalPeakAggregate {
        child_peak_rss_kib: Vec<u64>,
        child_pss_at_peak_kib: Vec<u64>,
        parent_rss_at_peak_kib: Vec<u64>,
        parent_pss_at_peak_kib: Vec<u64>,
        samples_per_eval: Vec<u64>,
    }

    impl EvalPeakAggregate {
        fn new() -> Self {
            Self {
                child_peak_rss_kib: Vec::new(),
                child_pss_at_peak_kib: Vec::new(),
                parent_rss_at_peak_kib: Vec::new(),
                parent_pss_at_peak_kib: Vec::new(),
                samples_per_eval: Vec::new(),
            }
        }

        fn push(&mut self, trial: &EvalPeakTrial) {
            self.child_peak_rss_kib.push(trial.child_peak_rss_kib);
            self.child_pss_at_peak_kib.push(trial.child_pss_at_peak_kib);
            self.parent_rss_at_peak_kib
                .push(trial.parent_rss_at_peak_kib);
            self.parent_pss_at_peak_kib
                .push(trial.parent_pss_at_peak_kib);
            self.samples_per_eval.push(trial.samples_per_eval as u64);
        }

        fn to_json(&self) -> Result<String, String> {
            let child_peak_rss = summarize_kib(&self.child_peak_rss_kib)
                .ok_or_else(|| "no childPeakRss samples were collected".to_string())?;
            let child_pss_at_peak = summarize_kib(&self.child_pss_at_peak_kib)
                .ok_or_else(|| "no childPssAtPeak samples were collected".to_string())?;
            let parent_rss_at_peak = summarize_kib(&self.parent_rss_at_peak_kib)
                .ok_or_else(|| "no parentRssAtPeak samples were collected".to_string())?;
            let parent_pss_at_peak = summarize_kib(&self.parent_pss_at_peak_kib)
                .ok_or_else(|| "no parentPssAtPeak samples were collected".to_string())?;
            let samples_per_eval = summarize_kib(&self.samples_per_eval)
                .ok_or_else(|| "no samplesPerEvalCount samples were collected".to_string())?;

            // `samplesPerEvalCount` はトップレベルの `"unit":"KiB"` とは
            // 無関係（サンプリング回数というカウントであり、KiB 量ではない）。
            // 誤読を防ぐため、KiB 量のフィールド（`childPeakRss` 等）と
            // 区別できる名前にする。
            Ok(format!(
                "{{\"childPeakRss\":{child_peak_rss},\"childPssAtPeak\":{child_pss_at_peak},\
                 \"parentRssAtPeak\":{parent_rss_at_peak},\"parentPssAtPeak\":{parent_pss_at_peak},\
                 \"samplesPerEvalCount\":{samples_per_eval}}}",
                child_peak_rss = summary_json(&child_peak_rss),
                child_pss_at_peak = summary_json(&child_pss_at_peak),
                parent_rss_at_peak = summary_json(&parent_rss_at_peak),
                parent_pss_at_peak = summary_json(&parent_pss_at_peak),
                samples_per_eval = summary_json(&samples_per_eval),
            ))
        }
    }
}

fn main() -> std::process::ExitCode {
    // 最優先: このプロセスが子プロセスとして起動されたものであれば、
    // ワーカーとして動作してそのまま終了する（`worker_spawn_latency.rs`
    // と同じ作法。子プロセス役のときは、これより前に何も stdout へ
    // 出してはならない）。
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
            "worker_memory_footprint: memory footprint measurement is only implemented on \
             Linux (/proc/<pid>/smaps_rollup)"
        );
        std::process::ExitCode::FAILURE
    }
}

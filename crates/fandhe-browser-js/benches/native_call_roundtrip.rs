//! JS 評価用子プロセスと親の間の `NativeCall`→`NativeReturn` 往復時間を
//! 実測するベンチ（`TASK-29`・Issue #556・ビヘイビア `CORE-3`・`PERF-6`・
//! `PERF-7`。親 issue #519。sibling: `worker_spawn_latency`・
//! `worker_memory_footprint`）。
//!
//! JS から注入関数や DOM 風オブジェクトのメンバーを呼ぶたびに、子→親へ
//! `NATIVE_CALL`、親→子へ `NATIVE_RETURN` のフレームが往復する。本ベンチは
//! その 1 往復のコストを記録するためのものであり、目標値との比較・達成
//! 可否の判定は行わない（REPAIR-3）。判定はユーザーが行う。
//!
//! # 計測区間（1 往復に含まれる処理）
//!
//! 1. 子の JS プロキシが引数を encode し `NATIVE_CALL` を書く
//! 2. 親の reader スレッドが受信し、評価スレッドへ渡す
//! 3. `dispatch_native_call` が decode し、`NativeFn` 用スレッドを 1 本
//!    起動して実行する
//! 4. 結果を encode して stdin パイプへ書く
//! 5. 子が decode して JS へ値を返す
//!
//! # 計測法
//!
//! - 主計測（到着間隔法）: `ParentNativeFn` が呼ばれた瞬間の `Instant` を
//!   記録し、1 回の `evaluate_script` 内のタイトループで N 回呼ぶ。隣接
//!   到着の差分が（ほぼ）1 往復で、1 回ずつのサンプルとして分布を取れる。
//!   1 バッチ目はウォームアップとして要約から除き `firstBatchMedianUs`
//!   として別枠に出す。
//! - 副計測（バッチ差分法）: 呼び出しありループと、native 呼び出しを
//!   同じ形の定数式に置き換えたループの `evaluate_script` 経過時間の中央値
//!   の差を N で割る。主計測の裏付け。
//!
//! # バリアント
//!
//! `globalNoArgs`（注入関数）・`domGetter`（`dom.title.length`）・
//! `domMethod`（`dom.setText('x', i)`）・`argPayload{0,1024,65536}`（子→親
//! 方向のペイロード）・`retPayload{0,1024,65536}`（親→子方向）。ペイロードの
//! 上限は `MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`（1 MiB + 64 KiB）・
//! `MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`（3 MiB + 64 KiB）のどちらも大きく
//! 下回る。超えるとフォールバックの `NativeReturn::Err` 経路を測って
//! しまうため、定数を変えるときは両上限を再確認すること。起動時間は
//! `worker_spawn_latency` が測るため含めない（`spawn_worker_for_test` で
//! 起動を済ませてから計測する）。
//!
//! # 健全性ガード
//!
//! `dispatch_native_call` は生存スレッド数が 64 を超えると JS へ例外を返す。
//! エラー経路を測っていないことを保証するため、各バッチで
//! `evaluate_script` が `Ok`・到着件数が N・JS の戻り値が期待する具体値・
//! 子の PID が不変、をすべて確認し、破れたらベンチを失敗させる。
//!
//! # 実行方法
//!
//! ```text
//! cargo bench -p fandhe-browser-js --features test-support --bench native_call_roundtrip
//! ```
//!
//! 環境変数 `JS_NATIVE_CALL_BATCH`（1 バッチの呼び出し回数。既定 1000・
//! 範囲 10〜5000）と `JS_NATIVE_CALL_BATCHES`（バッチ数。既定 10・範囲
//! 2〜100）で指定できる。上限 5000 は、1 回の評価が子の watchdog
//! （2 秒）と親の受信期限（3 秒）に収まるよう、悲観値 200 µs/往復で
//! 1 秒に収まる値として決めた。
//!
//! `harness = false`（`Cargo.toml` 参照）: このバイナリ自身が
//! `run_js_worker_if_requested` 経由で子プロセス役を兼ねる。子プロセス役の
//! ときの stdout はプロトコルフレーム専用のため、結果 JSON 以外を
//! stdout へ出さない。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fandhe_browser_js::process_engine::{DomLikeMemberFn, ParentNativeFn, V8ProcessEngine};
use fandhe_browser_js::{EvaluateOptions, JsEngineError, JsValue};

#[path = "native_call_roundtrip/calc.rs"]
mod calc;
#[path = "worker_spawn_latency/stats.rs"]
mod stats;

use stats::LatencySummary;

const DEFAULT_BATCH: u32 = 1000;
const MIN_BATCH: u32 = 10;
const MAX_BATCH: u32 = 5000;
const DEFAULT_BATCHES: u32 = 10;
/// 1 バッチ目はウォームアップとして除くため、最低 2 バッチ要る。
const MIN_BATCHES: u32 = 2;
const MAX_BATCHES: u32 = 100;

/// ペイロードバリアントのサイズ（バイト）。環境変数からは受け取らない。
const PAYLOAD_SIZES: [usize; 3] = [0, 1024, 65536];

/// DOM 風オブジェクト `dom.title` が返す固定文字列（長さ 4）。
const DOM_TITLE: &str = "abcd";
const DOM_TITLE_LEN: usize = 4;

/// 到着時刻の記録先。全 native 関数が共有し、バッチごとに空にする。
type Arrivals = Arc<Mutex<Vec<Instant>>>;

/// 1 バリアントの定義。JS はコンパイル時定数のテンプレートに検証済みの
/// 整数だけを埋めて作る（環境変数の文字列は連結しない）。
struct Variant {
    id: String,
    /// IIFE 内で最初に実行する文（バリアント固有の準備）。
    prelude: String,
    /// 呼び出しありループの 1 反復の右辺式。
    call_expr: String,
    /// native 呼び出しを置き換える同形の定数式（バッチ差分法用）。
    plain_expr: String,
    /// `call_expr` 1 回あたりの期待値（JS の戻り値の具体値検証用）。
    expected_per_call: f64,
}

fn build_variants() -> Vec<Variant> {
    let mut v = vec![
        Variant {
            id: "globalNoArgs".to_string(),
            prelude: String::new(),
            call_expr: "hostNoop()".to_string(),
            plain_expr: "1".to_string(),
            expected_per_call: 1.0,
        },
        Variant {
            id: "domGetter".to_string(),
            prelude: String::new(),
            call_expr: "dom.title.length".to_string(),
            plain_expr: DOM_TITLE_LEN.to_string(),
            expected_per_call: DOM_TITLE_LEN as f64,
        },
        Variant {
            id: "domMethod".to_string(),
            prelude: String::new(),
            call_expr: "dom.setText('x', i)".to_string(),
            plain_expr: "2".to_string(),
            expected_per_call: 2.0,
        },
    ];
    for size in PAYLOAD_SIZES {
        v.push(Variant {
            id: format!("argPayload{size}"),
            prelude: format!("const p = 'x'.repeat({size});"),
            call_expr: "hostArg(p)".to_string(),
            plain_expr: "p.length".to_string(),
            expected_per_call: size as f64,
        });
    }
    for size in PAYLOAD_SIZES {
        v.push(Variant {
            id: format!("retPayload{size}"),
            prelude: format!("const p = 'x'.repeat({size});"),
            call_expr: format!("hostRet{size}().length"),
            plain_expr: "p.length".to_string(),
            expected_per_call: size as f64,
        });
    }
    v
}

/// IIFE で包む（永続 Context に対する繰り返し評価で `let`/`const` の
/// 再宣言エラーを避ける）。`n` は検証済みの整数。
fn build_script(prelude: &str, expr: &str, n: u32) -> String {
    format!(
        "(function () {{ {prelude} let s = 0; \
         for (let i = 0; i < {n}; i++) {{ s += {expr}; }} return s; }})()"
    )
}

fn main() -> std::process::ExitCode {
    // 最優先: 子プロセスとして起動されたならワーカーとして動作して終了する。
    if let Some(code) = fandhe_browser_js::run_js_worker_if_requested() {
        return code;
    }

    let params = read_u32_env("JS_NATIVE_CALL_BATCH", DEFAULT_BATCH, MIN_BATCH, MAX_BATCH)
        .and_then(|batch| {
            read_u32_env(
                "JS_NATIVE_CALL_BATCHES",
                DEFAULT_BATCHES,
                MIN_BATCHES,
                MAX_BATCHES,
            )
            .map(|batches| (batch, batches))
        });
    let (batch, batches) = match params {
        Ok(p) => p,
        Err(message) => {
            eprintln!("native_call_roundtrip: {message}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match run_bench(batch, batches) {
        Ok(json) => {
            println!("{json}");
            std::process::ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("native_call_roundtrip: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 環境変数 `name` から範囲付きの整数を読む。未設定なら `default`。
/// 非 UTF-8・非数値・範囲外は fail-closed で `Err`（英語の理由）を返す。
fn read_u32_env(name: &str, default: u32, min: u32, max: u32) -> Result<u32, String> {
    let raw = match std::env::var(name) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => return Err(format!("{name} is not valid UTF-8")),
    };
    let value: u32 = raw
        .trim()
        .parse()
        .map_err(|_| format!("{name} must be an integer, got {raw:?}"))?;
    if !(min..=max).contains(&value) {
        return Err(format!(
            "{name} must be between {min} and {max}, got {value}"
        ));
    }
    Ok(value)
}

/// 到着時刻を記録してから `f` の結果を返す `ParentNativeFn` を作る。
fn recording_fn(
    arrivals: &Arrivals,
    f: impl Fn(&[JsValue]) -> Result<JsValue, JsEngineError> + Send + 'static,
) -> ParentNativeFn {
    let arrivals = Arc::clone(arrivals);
    Box::new(move |args, _ctx| {
        if let Ok(mut guard) = arrivals.lock() {
            guard.push(Instant::now());
        }
        f(args)
    })
}

fn register_natives(engine: &mut V8ProcessEngine, arrivals: &Arrivals) -> Result<(), String> {
    let reg = |e: Result<(), JsEngineError>, what: &str| {
        e.map_err(|err| format!("failed to register {what}: {err}"))
    };
    reg(
        engine.inject_global_function(
            "hostNoop",
            recording_fn(arrivals, |_| Ok(JsValue::Number(1.0))),
        ),
        "hostNoop",
    )?;
    reg(
        engine.inject_global_function(
            "hostArg",
            recording_fn(arrivals, |args| match args.first() {
                Some(JsValue::String(s)) => Ok(JsValue::Number(s.len() as f64)),
                _ => Err(JsEngineError::EvaluationFailed(
                    "hostArg expects one string argument".to_string(),
                )),
            }),
        ),
        "hostArg",
    )?;
    for size in PAYLOAD_SIZES {
        let payload = "x".repeat(size);
        reg(
            engine.inject_global_function(
                &format!("hostRet{size}"),
                recording_fn(arrivals, move |_| Ok(JsValue::String(payload.clone()))),
            ),
            "hostRet",
        )?;
    }
    let members = vec![
        (
            "title".to_string(),
            DomLikeMemberFn::Getter(recording_fn(arrivals, |_| {
                Ok(JsValue::String(DOM_TITLE.to_string()))
            })),
        ),
        (
            "setText".to_string(),
            DomLikeMemberFn::Method(recording_fn(arrivals, |args| {
                Ok(JsValue::Number(args.len() as f64))
            })),
        ),
    ];
    engine
        .bind_dom_like_object("dom", members)
        .map(|_| ())
        .map_err(|err| format!("failed to bind the DOM-like object: {err}"))
}

/// 1 バリアントの集計結果。目標達成可否のフィールドは持たない（REPAIR-3）。
struct VariantResult {
    id: String,
    inter_arrival: LatencySummary,
    batch_differential_per_call_us: Option<f64>,
    first_batch_median_us: f64,
}

fn run_bench(batch: u32, batches: u32) -> Result<String, String> {
    let arrivals: Arrivals = Arc::new(Mutex::new(Vec::with_capacity(batch as usize + 1)));
    let mut engine = V8ProcessEngine::new();
    engine
        .spawn_worker_for_test()
        .map_err(|err| format!("failed to spawn the JS worker process: {err}"))?;
    register_natives(&mut engine, &arrivals)?;
    let pid = engine
        .worker_pid_for_test()
        .ok_or_else(|| "JS worker process is absent after a successful spawn".to_string())?;

    let mut results = Vec::new();
    for variant in build_variants() {
        results.push(run_variant(
            &mut engine,
            &arrivals,
            &variant,
            batch,
            batches,
            pid,
        )?);
    }

    drop(engine);
    #[cfg(target_os = "linux")]
    assert_child_reaped_on_linux(pid)?;

    Ok(report_to_json(batch, batches, &results))
}

fn run_variant(
    engine: &mut V8ProcessEngine,
    arrivals: &Arrivals,
    variant: &Variant,
    batch: u32,
    batches: u32,
    pid: u32,
) -> Result<VariantResult, String> {
    let calls_script = build_script(&variant.prelude, &variant.call_expr, batch);
    let plain_script = build_script(&variant.prelude, &variant.plain_expr, batch);
    let expected_calls = variant.expected_per_call * f64::from(batch);
    let id = &variant.id;

    let mut deltas_by_batch: Vec<Vec<Duration>> = Vec::new();
    let mut calls_elapsed: Vec<Duration> = Vec::new();
    let mut plain_elapsed: Vec<Duration> = Vec::new();

    for _ in 0..batches {
        arrivals
            .lock()
            .map_err(|_| "arrival recorder is poisoned".to_string())?
            .clear();

        let start = Instant::now();
        let value = engine
            .evaluate_script(&calls_script, &EvaluateOptions::default())
            .map_err(|err| format!("{id}: call loop failed: {err}"))?;
        calls_elapsed.push(start.elapsed());

        // ガード: 期待した具体値・到着件数・子の PID（黙って再起動していない）。
        match value {
            JsValue::Number(n) if n == expected_calls => {}
            other => {
                return Err(format!(
                    "{id}: unexpected loop result {other:?}, expected Number({expected_calls})"
                ));
            }
        }
        let recorded = {
            let guard = arrivals
                .lock()
                .map_err(|_| "arrival recorder is poisoned".to_string())?;
            let count = guard.len();
            if count != batch as usize {
                return Err(format!(
                    "{id}: recorded {count} native calls, expected {batch}"
                ));
            }
            calc::inter_arrival_deltas(&guard)
        };
        deltas_by_batch.push(recorded);

        let start = Instant::now();
        let plain = engine
            .evaluate_script(&plain_script, &EvaluateOptions::default())
            .map_err(|err| format!("{id}: plain loop failed: {err}"))?;
        plain_elapsed.push(start.elapsed());
        match plain {
            JsValue::Number(n) if n == expected_calls => {}
            other => {
                return Err(format!(
                    "{id}: unexpected plain loop result {other:?}, expected Number({expected_calls})"
                ));
            }
        }

        if engine.worker_pid_for_test() != Some(pid) {
            return Err(format!("{id}: JS worker process changed during the run"));
        }
    }

    // 1 バッチ目はウォームアップ。要約から除いて別枠に出す。
    let first = deltas_by_batch
        .first()
        .and_then(|d| stats::summarize(d))
        .ok_or_else(|| format!("{id}: no inter-arrival samples in the first batch"))?;
    let pooled: Vec<Duration> = deltas_by_batch
        .get(1..)
        .unwrap_or(&[])
        .iter()
        .flatten()
        .copied()
        .collect();
    let inter_arrival = stats::summarize(&pooled)
        .ok_or_else(|| format!("{id}: no inter-arrival samples after the warm-up batch"))?;

    let calls_median = median_duration(calls_elapsed.get(1..).unwrap_or(&[]))?;
    let plain_median = median_duration(plain_elapsed.get(1..).unwrap_or(&[]))?;

    Ok(VariantResult {
        id: variant.id.clone(),
        inter_arrival,
        batch_differential_per_call_us: calc::per_call_from_batch_medians(
            calls_median,
            plain_median,
            batch,
        ),
        first_batch_median_us: first.median_ms * 1000.0,
    })
}

fn median_duration(samples: &[Duration]) -> Result<Duration, String> {
    let summary = stats::summarize(samples)
        .ok_or_else(|| "no batch timings remained after the warm-up batch".to_string())?;
    Ok(Duration::from_secs_f64(summary.median_ms / 1000.0))
}

/// 英語 JSON 1 行。serde は使わず手組みする（dependency-policy.md）。
/// `stats` は ms なので、ここで µs へ変換する。
fn report_to_json(batch: u32, batches: u32, results: &[VariantResult]) -> String {
    let variants: Vec<String> = results
        .iter()
        .map(|r| {
            let diff = match r.batch_differential_per_call_us {
                Some(us) => us.to_string(),
                None => "null".to_string(),
            };
            format!(
                "\"{id}\":{{\"interArrival\":{summary},\"batchDifferentialPerCallUs\":{diff},\
                 \"firstBatchMedianUs\":{first}}}",
                id = r.id,
                summary = summary_us_json(&r.inter_arrival),
                first = r.first_batch_median_us,
            )
        })
        .collect();
    format!(
        "{{\"bench\":\"js_native_call_roundtrip\",\"unit\":\"us\",\"profile\":\"bench\",\
         \"os\":\"{os}\",\"arch\":\"{arch}\",\"batchSize\":{batch},\"batches\":{batches},\
         \"percentileMethod\":\"nearest-rank\",\"variants\":{{{variants}}},\
         \"note\":\"first batch excluded from interArrival and batch differential\"}}",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        variants = variants.join(","),
    )
}

fn summary_us_json(s: &LatencySummary) -> String {
    format!(
        "{{\"n\":{n},\"min\":{min},\"median\":{median},\"p95\":{p95},\"max\":{max},\"mean\":{mean}}}",
        n = s.n,
        min = s.min_ms * 1000.0,
        median = s.median_ms * 1000.0,
        p95 = s.p95_ms * 1000.0,
        max = s.max_ms * 1000.0,
        mean = s.mean_ms * 1000.0,
    )
}

/// Linux でのみ: drop 後に `/proc/<pid>` が消えている（子が reap 済み）ことを
/// 確認する健全性チェック（計測時間には含めない）。
#[cfg(target_os = "linux")]
fn assert_child_reaped_on_linux(pid: u32) -> Result<(), String> {
    let proc_path = std::path::PathBuf::from("/proc").join(pid.to_string());
    if proc_path.exists() {
        return Err(format!(
            "JS worker process {pid} still has a /proc entry after drop"
        ));
    }
    Ok(())
}

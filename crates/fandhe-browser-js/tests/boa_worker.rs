//! boa 子プロセス分離（`JS-1`・`TASK-32.2`・Issue #166）の結合テスト。
//!
//! codex レビュー指摘（PR #656・AGENTS.md「リソース上限」P0）「boa の実行
//! 時間・メモリ使用量に上限が無い」への回答として、boa を V8 と同じ子プロセス
//! 分離基盤の上で動かしたことを、実際の子プロセスで確認する。
//!
//! - 実時間: 正規表現のバックトラッキング（既定 options）や `while (true) {}`
//!   （`timeout = 200ms`。`JS-6`・`TASK-109`・Issue #776）が親の期限を超え、
//!   `JsEngineError::Timeout` で呼び出しが戻ること
//!   （子は kill され、次の評価は新しい子で動くこと）
//! - 結果サイズ上限（`max_result_bytes`）が子プロセス境界を越えて効くこと
//! - メモリ: 巨大確保が子の中で閉じ、ホストは
//!   `JsEngineError::ResourceLimitExceeded` を受け取って生き続けること
//! - 逆方向 RPC（ホスト関数の注入）が子を越えて往復すること
//!
//! `harness = false`（`Cargo.toml` 参照）。このバイナリ自身が子プロセス役を
//! 兼ねるため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ
//! （`tests/conformance.rs` と同じ作法）。実行コマンド:
//! `cargo test -p fandhe-browser-js --features js-boa --test boa_worker`。

use std::process::ExitCode;
#[cfg(not(target_os = "macos"))]
use std::time::{Duration, Instant};

use fandhe_browser_js::{EngineKind, create_engine};
#[cfg(not(target_os = "macos"))]
use fandhe_browser_js::{EvaluateOptions, JsEngine, JsEngineError, JsValue};

/// 親の評価期限（既定 options では `timeout` 2 秒 + 猶予 1 秒 = 3 秒）に猶予を足した
/// 上限。これ以内に呼び出しが戻らなければ実時間の上限が効いていない。
#[cfg(not(target_os = "macos"))]
const TIMEOUT_TEST_DEADLINE: Duration = Duration::from_secs(15);

#[cfg(not(target_os = "macos"))]
fn boa_engine() -> Box<dyn JsEngine> {
    create_engine(EngineKind::Boa).expect("the js-boa feature is enabled for this test")
}

#[cfg(not(target_os = "macos"))]
fn eval(engine: &mut dyn JsEngine, script: &str) -> Result<JsValue, JsEngineError> {
    engine.evaluate_script(script, &EvaluateOptions::default())
}

/// 基本評価が子プロセスを経由して動くこと。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_evaluates_in_a_child_process() {
    let mut engine = boa_engine();
    assert_eq!(
        eval(&mut *engine, "1 + 2").expect("eval"),
        JsValue::Number(3.0)
    );
    assert_eq!(
        eval(&mut *engine, "'a' + 'b'").expect("eval"),
        JsValue::String("ab".to_string())
    );
    // 永続 Context（状態が評価をまたいで残る）。
    eval(&mut *engine, "globalThis.counter = 40").expect("eval");
    assert_eq!(
        eval(&mut *engine, "counter + 2").expect("eval"),
        JsValue::Number(42.0)
    );
}

/// 実時間の上限: ループ反復上限（boa 内蔵）に数えられない処理（正規表現の
/// 破滅的バックトラッキング）が親の期限で
/// `Timeout` になり、その後の評価は新しい子で成功すること。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_wall_clock_limit_kills_the_child_and_recovers() {
    let mut engine = boa_engine();
    let started = Instant::now();
    let result = eval(
        &mut *engine,
        "var n = 1; /^(a+)+$/.test('a'.repeat(60) + 'b'); n",
    );
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(JsEngineError::Timeout(_))),
        "a non-loop busy computation must be stopped by the wall-clock limit, got {result:?}"
    );
    assert!(
        elapsed < TIMEOUT_TEST_DEADLINE,
        "the call must return near the deadline, took {elapsed:?}"
    );
    // 子は kill 済み。次の評価は新しい子（新しい Context）で動く。
    assert_eq!(
        eval(&mut *engine, "typeof n").expect("eval after kill"),
        JsValue::String("undefined".to_string())
    );
    assert_eq!(
        eval(&mut *engine, "6 * 7").expect("eval after kill"),
        JsValue::Number(42.0)
    );
}

/// メモリの上限（親の RSS 監視）: バッファ 1 つあたりの上限（128 MiB）未満の
/// 確保を保持し続けて積み上げ、ホストが `ResourceLimitExceeded` を受け取り
/// （子は親の RSS 監視で kill される）、その後も評価を続けられること。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_memory_limit_is_enforced_by_the_child_boundary() {
    let mut engine = boa_engine();
    let started = Instant::now();
    let result = eval(
        &mut *engine,
        "var keep = []; for (var i = 0; i < 900000; i++) { keep.push(new ArrayBuffer(67108864)); } keep.length",
    );
    let elapsed = started.elapsed();
    eprintln!("memory case took {elapsed:?}: {result:?}");
    assert!(
        matches!(result, Err(JsEngineError::ResourceLimitExceeded(_))),
        "memory exhaustion must be contained by the child, got {result:?}"
    );
    assert!(
        elapsed < TIMEOUT_TEST_DEADLINE,
        "the call must return near the deadline, took {elapsed:?}"
    );
    assert_eq!(
        eval(&mut *engine, "1 + 1").expect("eval after memory kill"),
        JsValue::Number(2.0)
    );
}

/// 単発の巨大確保（上限 128 MiB 超）は boa のホストフックで拒否され、catch 可能な
/// `RangeError` になること（子と Context は生き残り、既存のグローバルも残る）。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_oversized_buffer_is_rejected_without_killing_the_child() {
    let mut engine = boa_engine();
    eval(&mut *engine, "var marker = 7").expect("eval");
    match eval(
        &mut *engine,
        "try { new ArrayBuffer(536870912); 'allocated' } catch (e) { e.name }",
    )
    .expect("eval")
    {
        JsValue::String(name) => assert_eq!(name, "RangeError"),
        other => panic!("unexpected value {other:?}"),
    }
    assert_eq!(
        eval(&mut *engine, "marker").expect("same context"),
        JsValue::Number(7.0)
    );
}

/// `JS-6`・`TASK-109`・Issue #776: `timeout = 200ms` の `while (true) {}` が
/// 2 秒未満で `Timeout` になる（boa は親が子を kill して強制するため Context は
/// 破棄され、メッセージに "context was discarded" を含む）。次の評価は新しい
/// 子で成功する。
#[cfg(not(target_os = "macos"))]
fn js_6_boa_infinite_loop_times_out_with_custom_timeout() {
    let mut engine = boa_engine();
    eval(&mut *engine, "var before = 11").expect("eval");
    let options = EvaluateOptions::default().with_timeout(Duration::from_millis(200));
    let started = Instant::now();
    let result = engine.evaluate_script("while (true) {}", &options);
    let elapsed = started.elapsed();
    match result {
        Err(JsEngineError::Timeout(message)) => assert!(
            message.contains("context was discarded"),
            "the killed child must be reported as discarded, got {message:?}"
        ),
        other => panic!("expected Timeout, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "the call must return within 2s, took {elapsed:?}"
    );
    assert_eq!(
        eval(&mut *engine, "typeof before").expect("fresh context"),
        JsValue::String("undefined".to_string())
    );
}

/// `JS-1`: 再帰上限（boa 内蔵）は `ResourceLimitExceeded` としてワイヤを越えて
/// 伝わり、子は kill されず Context も残ること（"context was discarded" を含まない）。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_recursion_limit_is_reported_as_resource_limit_exceeded() {
    let mut engine = boa_engine();
    eval(&mut *engine, "var before = 11").expect("eval");
    match eval(&mut *engine, "function f() { return f() + 1 } f()") {
        Err(JsEngineError::ResourceLimitExceeded(message)) => assert!(
            !message.contains("context was discarded"),
            "the child is alive, got {message:?}"
        ),
        other => panic!("expected ResourceLimitExceeded, got {other:?}"),
    }
    assert_eq!(
        eval(&mut *engine, "before").expect("same context"),
        JsValue::Number(11.0)
    );
}

/// `JS-6`: 結果サイズ上限が子プロセス境界を越えて効く（16 バイトちょうどは成功、
/// 17 は `EvaluationFailed`。既定 options の UTF-16 上限境界も従来どおり）。
#[cfg(not(target_os = "macos"))]
fn js_6_boa_result_size_limit_crosses_the_process_boundary() {
    let mut engine = boa_engine();
    let small = EvaluateOptions::default().with_max_result_bytes(16);
    match engine
        .evaluate_script("'a'.repeat(16)", &small)
        .expect("16 bytes is within the limit")
    {
        JsValue::String(v) => assert_eq!(v.len(), 16),
        other => panic!("unexpected value {other:?}"),
    }
    match engine.evaluate_script("'a'.repeat(17)", &small) {
        Err(JsEngineError::EvaluationFailed(message)) => assert_eq!(
            message,
            "string result of 17 bytes exceeds the result size limit of 16 bytes"
        ),
        other => panic!("expected EvaluationFailed, got {other:?}"),
    }
    assert!(matches!(
        eval(&mut *engine, "'a'.repeat(1048577)"),
        Err(JsEngineError::EvaluationFailed(_))
    ));
}

/// 逆方向 RPC: ホスト関数・DOM 風オブジェクトのメソッドが子を越えて呼べること。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_native_calls_cross_the_process_boundary() {
    let mut engine = boa_engine();
    engine
        .inject_global_function(
            "hostAdd",
            Box::new(|args: &[JsValue]| match args {
                [JsValue::Number(a), JsValue::Number(b)] => Ok(JsValue::Number(a + b)),
                _ => Err(JsEngineError::EvaluationFailed("bad args".to_string())),
            }),
        )
        .expect("inject");
    assert_eq!(
        eval(&mut *engine, "hostAdd(20, 22)").expect("eval"),
        JsValue::Number(42.0)
    );
    match eval(&mut *engine, "try { hostAdd('x') } catch (e) { e.message }").expect("eval") {
        JsValue::String(message) => assert!(
            message == "script evaluation failed: bad args",
            "the host error must carry exactly one prefix (as on V8), got {message:?}"
        ),
        other => panic!("unexpected value {other:?}"),
    }
    engine
        .bind_dom_like_object(
            "dom",
            vec![(
                "echo".to_string(),
                Box::new(|args: &[JsValue]| Ok(args.first().cloned().unwrap_or(JsValue::Null))),
            )],
        )
        .expect("bind");
    assert_eq!(
        eval(&mut *engine, "dom.echo('hi')").expect("eval"),
        JsValue::String("hi".to_string())
    );
}

/// boa の `ArrayBuffer` 上限の単位（バイト）の固定: 64 MiB は確保でき、
/// 128 MiB 超は `RangeError` になること（PR #656 レビュー指摘への回答。
/// `HostHooks::max_buffer_size` はバイト単位）。
#[cfg(not(target_os = "macos"))]
fn js_1_boa_buffer_limit_unit_is_bytes() {
    let mut engine = boa_engine();
    assert_eq!(
        eval(
            &mut *engine,
            "var b = new ArrayBuffer(67108864); b.byteLength"
        )
        .expect("eval"),
        JsValue::Number(67108864.0)
    );
    assert_eq!(
        eval(
            &mut *engine,
            "try { new ArrayBuffer(134217729); 'allocated' } catch (e) { e.name }"
        )
        .expect("eval"),
        JsValue::String("RangeError".to_string())
    );
}

/// macOS では boa を無効にしていること（確保時に効くメモリ上限を OS で強制
/// できないため fail-closed。`create_engine` は `NotYetImplemented`）。
#[cfg(target_os = "macos")]
fn js_1_boa_is_disabled_on_macos() {
    match create_engine(EngineKind::Boa) {
        Err(fandhe_browser_js::CreateEngineError::NotYetImplemented { requested }) => {
            assert_eq!(requested, EngineKind::Boa);
        }
        Err(other) => panic!("unexpected error {other:?}"),
        Ok(_) => panic!("boa must be disabled on macOS"),
    }
}

fn main() -> ExitCode {
    // 子プロセスとして再実行された場合は、他の出力より前にワーカーへ制御を渡す
    // （stdout はフレーム専用）。
    if let Some(code) = fandhe_browser_js::run_js_worker_if_requested() {
        return code;
    }

    #[cfg(target_os = "macos")]
    {
        eprintln!("case: js_1_boa_is_disabled_on_macos");
        js_1_boa_is_disabled_on_macos();
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("case: js_1_boa_evaluates_in_a_child_process");
        js_1_boa_evaluates_in_a_child_process();
        eprintln!("case: js_1_boa_native_calls_cross_the_process_boundary");
        js_1_boa_native_calls_cross_the_process_boundary();
        eprintln!("case: js_1_boa_wall_clock_limit_kills_the_child_and_recovers");
        js_1_boa_wall_clock_limit_kills_the_child_and_recovers();
        eprintln!("case: js_1_boa_oversized_buffer_is_rejected_without_killing_the_child");
        js_1_boa_oversized_buffer_is_rejected_without_killing_the_child();
        eprintln!("case: js_6_boa_infinite_loop_times_out_with_custom_timeout");
        js_6_boa_infinite_loop_times_out_with_custom_timeout();
        eprintln!("case: js_1_boa_recursion_limit_is_reported_as_resource_limit_exceeded");
        js_1_boa_recursion_limit_is_reported_as_resource_limit_exceeded();
        eprintln!("case: js_6_boa_result_size_limit_crosses_the_process_boundary");
        js_6_boa_result_size_limit_crosses_the_process_boundary();
        eprintln!("case: js_1_boa_memory_limit_is_enforced_by_the_child_boundary");
        js_1_boa_memory_limit_is_enforced_by_the_child_boundary();
        eprintln!("case: js_1_boa_buffer_limit_unit_is_bytes");
        js_1_boa_buffer_limit_unit_is_bytes();
    }
    eprintln!("boa_worker: all cases passed");
    ExitCode::SUCCESS
}

//! boa 子プロセス分離（`JS-1`・`TASK-32.2`・Issue #166）の結合テスト。
//!
//! codex レビュー指摘（PR #656・AGENTS.md「リソース上限」P0）「boa の実行
//! 時間・メモリ使用量に上限が無い」への回答として、boa を V8 と同じ子プロセス
//! 分離基盤の上で動かしたことを、実際の子プロセスで確認する。
//!
//! - 実時間: ループ 1 つあたりの反復上限（boa 内蔵）に当たらないループ反復上限に数えられない処理（正規表現のバックトラッキング）で
//!   親の期限（3 秒）を超えさせ、`JsEngineError::Timeout` で呼び出しが戻ること
//!   （子は kill され、次の評価は新しい子で動くこと）
//! - メモリ: 巨大確保が子の中で閉じ、ホストは
//!   `JsEngineError::ResourceLimitExceeded` を受け取って生き続けること
//! - 逆方向 RPC（ホスト関数の注入）が子を越えて往復すること
//!
//! `harness = false`（`Cargo.toml` 参照）。このバイナリ自身が子プロセス役を
//! 兼ねるため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ
//! （`tests/conformance.rs` と同じ作法）。実行コマンド:
//! `cargo test -p fandhe-browser-js --features js-boa --test boa_worker`。

use std::process::ExitCode;
use std::time::{Duration, Instant};

use fandhe_browser_js::{
    EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue, create_engine,
};

/// 親の評価期限（`process_engine::EVALUATE_RECV_TIMEOUT`。3 秒）に猶予を足した
/// 上限。これ以内に呼び出しが戻らなければ実時間の上限が効いていない。
const TIMEOUT_TEST_DEADLINE: Duration = Duration::from_secs(15);

fn boa_engine() -> Box<dyn JsEngine> {
    create_engine(EngineKind::Boa).expect("the js-boa feature is enabled for this test")
}

fn eval(engine: &mut dyn JsEngine, script: &str) -> Result<JsValue, JsEngineError> {
    engine.evaluate_script(script, &EvaluateOptions::default())
}

/// 基本評価が子プロセスを経由して動くこと。
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

/// ループ反復上限（boa 内蔵）は `ResourceLimitExceeded` としてワイヤを越えて伝わり、
/// 子は kill されず Context も残ること（"context was discarded" を含まない）。
fn js_1_boa_loop_limit_is_reported_as_resource_limit_exceeded() {
    let mut engine = boa_engine();
    eval(&mut *engine, "var before = 11").expect("eval");
    match eval(&mut *engine, "while (true) {}") {
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

/// 逆方向 RPC: ホスト関数・DOM 風オブジェクトのメソッドが子を越えて呼べること。
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

fn main() -> ExitCode {
    // 子プロセスとして再実行された場合は、他の出力より前にワーカーへ制御を渡す
    // （stdout はフレーム専用）。
    if let Some(code) = fandhe_browser_js::run_js_worker_if_requested() {
        return code;
    }

    eprintln!("case: js_1_boa_evaluates_in_a_child_process");
    js_1_boa_evaluates_in_a_child_process();
    eprintln!("case: js_1_boa_native_calls_cross_the_process_boundary");
    js_1_boa_native_calls_cross_the_process_boundary();
    eprintln!("case: js_1_boa_wall_clock_limit_kills_the_child_and_recovers");
    js_1_boa_wall_clock_limit_kills_the_child_and_recovers();
    eprintln!("case: js_1_boa_oversized_buffer_is_rejected_without_killing_the_child");
    js_1_boa_oversized_buffer_is_rejected_without_killing_the_child();
    eprintln!("case: js_1_boa_loop_limit_is_reported_as_resource_limit_exceeded");
    js_1_boa_loop_limit_is_reported_as_resource_limit_exceeded();
    eprintln!("case: js_1_boa_memory_limit_is_enforced_by_the_child_boundary");
    js_1_boa_memory_limit_is_enforced_by_the_child_boundary();
    eprintln!("boa_worker: all cases passed");
    ExitCode::SUCCESS
}

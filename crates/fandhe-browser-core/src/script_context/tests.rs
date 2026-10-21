//! `script_context` の単体テスト（TASK-44.5・Issue #1216・ビヘイビア `CDP-2`・`JS-1`・`JS-6`・`PERF-7`）。
//!
//! 実エンジンは使わず、`JsRuntime::from_engine_for_test` で包んだ偽エンジンで状態遷移・上限・
//! 結果の写像を検証する（実エンジンは `tests/support/script_context_cases.rs`）。
//! 偽エンジンの契約: `__dom` の bind を受け付ける・shim には `Ok(Undefined)`・テストのソースは
//! 文字列で振る舞いを分岐する。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fandhe_browser_js::{EngineKind, JsEngine, NativeFn};

use super::*;
use crate::js_shim::JS_SHIM_SOURCES;
use crate::state::NavigationState;

/// 偽エンジン間で共有する観測点。
#[derive(Default)]
struct Probe {
    factory_calls: AtomicUsize,
    engine_drops: AtomicUsize,
    evals: AtomicUsize,
    blocked: AtomicUsize,
    /// 1 回目の factory 呼び出しだけ panic させる。
    panic_first_factory: AtomicBool,
    gate: (Mutex<bool>, Condvar),
}

impl Probe {
    fn open_gate(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

struct FakeEngine {
    probe: Arc<Probe>,
    counter: u64,
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        self.probe.engine_drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl JsEngine for FakeEngine {
    fn evaluate_script(
        &mut self,
        script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        if JS_SHIM_SOURCES.iter().any(|s| s.source == script) {
            return Ok(JsValue::Undefined);
        }
        self.probe.evals.fetch_add(1, Ordering::SeqCst);
        if let Some(n) = script.strip_prefix("len:") {
            return Ok(JsValue::String("a".repeat(n.parse().unwrap_or(0))));
        }
        if let Some(n) = script.strip_prefix("throw:") {
            return Err(JsEngineError::EvaluationFailed(
                "x".repeat(n.parse().unwrap_or(1)),
            ));
        }
        match script {
            "count" => {
                self.counter += 1;
                Ok(JsValue::String(self.counter.to_string()))
            }
            "num" => Ok(JsValue::Number(1.0)),
            "timeout" => Err(JsEngineError::Timeout("watchdog".to_string())),
            "discard-timeout" => Err(JsEngineError::Timeout(
                "killed; context was discarded".to_string(),
            )),
            "oom" => Err(JsEngineError::ResourceLimitExceeded(
                "heap limit".to_string(),
            )),
            "oom-discard" => Err(JsEngineError::ResourceLimitExceeded(
                "heap limit; context was discarded".to_string(),
            )),
            "unavailable" => Err(JsEngineError::EngineUnavailable("gone".to_string())),
            "block" => {
                self.probe.blocked.fetch_add(1, Ordering::SeqCst);
                let (lock, cv) = &self.probe.gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
                Ok(JsValue::String("unblocked".to_string()))
            }
            other => Ok(JsValue::String(other.to_string())),
        }
    }

    fn inject_global_function(&mut self, _: &str, _: NativeFn) -> Result<(), JsEngineError> {
        Ok(())
    }

    fn bind_dom_like_object(
        &mut self,
        _: &str,
        _: Vec<(String, NativeFn)>,
    ) -> Result<(), JsEngineError> {
        Ok(())
    }
}

fn handle_with(probe: &Arc<Probe>, options: ScriptContextOptions) -> ScriptContextHandle {
    let probe = Arc::clone(probe);
    ScriptContextHandle::with_factory_for_test(
        Arc::new(move || {
            let n = probe.factory_calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 && probe.panic_first_factory.load(Ordering::SeqCst) {
                panic!("injected factory panic");
            }
            Ok(JsRuntime::from_engine_for_test(
                EngineKind::V8,
                Box::new(FakeEngine {
                    probe: Arc::clone(&probe),
                    counter: 0,
                }),
            ))
        }),
        options,
    )
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timeout waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn doc() -> Arc<NavigationResult> {
    Arc::new(NavigationResult::new(
        "https://example.test/",
        "<h1>Hello</h1>",
    ))
}

/// 1 ターゲット分の scope と、世代の払い出し役。
struct Target {
    nav: NavigationState,
}

impl Target {
    fn new() -> Self {
        Self {
            nav: NavigationState::new(),
        }
    }
    fn scope(&self) -> NavigationScope {
        self.nav.scope()
    }
    fn next_generation(&self) -> NavigationGeneration {
        self.nav.begin_navigation().expect("generation")
    }
}

fn eval(
    h: &ScriptContextHandle,
    key: ScriptContextKey,
    src: &str,
) -> Result<String, ScriptContextError> {
    block_on(h.evaluate(key, doc(), src, &ScriptEvalOptions::default()))
}

fn eval_with(
    h: &ScriptContextHandle,
    key: ScriptContextKey,
    src: &str,
    opts: &ScriptEvalOptions,
) -> Result<String, ScriptContextError> {
    block_on(h.evaluate(key, doc(), src, opts))
}

#[test]
fn js_1_disabled_handle_returns_unavailable() {
    let h = ScriptContextHandle::disabled();
    assert!(!h.is_enabled());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(
        eval(&h, key, "1"),
        Err(ScriptContextError::Unavailable {
            reason: UnavailableReason::Disabled
        })
    );
    assert_eq!(h.stats().worker_spawns(), 0);
    // 無効なハンドルへの通知は何もしない。
    h.begin_generation(t.scope(), t.next_generation());
    h.release_scope(t.scope());
    assert_eq!(h.stats().worker_spawns(), 0);
}

#[test]
fn perf_7_no_worker_until_first_evaluate() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    assert!(h.is_enabled());
    let t = Target::new();
    let g1 = t.next_generation();
    h.begin_generation(t.scope(), g1);
    h.release_scope(t.scope());
    h.begin_generation(t.scope(), g1);
    assert_eq!(h.stats().worker_spawns(), 0);
    assert!(!h.stats().worker_running());
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 0);

    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g1, 1), "hi"),
        Ok("hi".to_string())
    );
    assert_eq!(h.stats().worker_spawns(), 1);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.stats().live_contexts(), 1);
}

#[test]
fn cdp_2_same_key_reuses_runtime() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 7);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(eval(&h, key, "count"), Ok("2".to_string()));
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.stats().worker_spawns(), 1);
}

#[test]
fn cdp_2_different_worlds_get_separate_runtimes() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g = t.next_generation();
    let a = ScriptContextKey::new(t.scope(), g, 1);
    let b = ScriptContextKey::new(t.scope(), g, 2);
    assert_eq!(eval(&h, a, "count"), Ok("1".to_string()));
    assert_eq!(eval(&h, b, "count"), Ok("1".to_string()));
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.stats().live_contexts(), 2);
}

#[test]
fn cdp_2_stale_generation_is_not_found() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let g2 = t.next_generation();
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g2, 1), "a"),
        Ok("a".to_string())
    );
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g1, 1), "a"),
        Err(ScriptContextError::NotFound)
    );
    // 別 scope の世代には影響しない。
    let other = Target::new();
    let k = ScriptContextKey::new(other.scope(), other.next_generation(), 1);
    assert_eq!(eval(&h, k, "b"), Ok("b".to_string()));
}

#[test]
fn cdp_2_newer_generation_drops_older_contexts() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let g2 = t.next_generation();
    let old = ScriptContextKey::new(t.scope(), g1, 1);
    assert_eq!(eval(&h, old, "count"), Ok("1".to_string()));
    assert_eq!(h.stats().live_contexts(), 1);
    let new = ScriptContextKey::new(t.scope(), g2, 1);
    assert_eq!(eval(&h, new, "count"), Ok("1".to_string()));
    wait_until("old context dropped", || {
        probe.engine_drops.load(Ordering::SeqCst) == 1
    });
    assert_eq!(h.stats().live_contexts(), 1);
    assert_eq!(eval(&h, old, "count"), Err(ScriptContextError::NotFound));
}

#[test]
fn cdp_2_begin_generation_drops_contexts() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let g2 = t.next_generation();
    let key = ScriptContextKey::new(t.scope(), g1, 1);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    h.begin_generation(t.scope(), g2);
    wait_until("context dropped and worker exited", || {
        let s = h.stats();
        s.live_contexts() == 0 && !s.worker_running()
    });
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 1);
    // 世代の記録はワーカー終了後も残る。
    assert_eq!(eval(&h, key, "count"), Err(ScriptContextError::NotFound));
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g2, 1), "count"),
        Ok("1".to_string())
    );
}

#[test]
fn cdp_2_release_scope_drops_contexts_and_worker_exits() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g = t.next_generation();
    for world in 1..=3 {
        assert_eq!(
            eval(&h, ScriptContextKey::new(t.scope(), g, world), "x"),
            Ok("x".to_string())
        );
    }
    assert_eq!(h.stats().live_contexts(), 3);
    h.release_scope(t.scope());
    wait_until("all dropped and worker exited", || {
        let s = h.stats();
        s.live_contexts() == 0 && !s.worker_running()
    });
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 3);
    // release 後の再評価は上限内で新規作成になる（呼び出し側の契約外の挙動）。
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g, 1), "count"),
        Ok("1".to_string())
    );
    assert_eq!(h.stats().worker_spawns(), 2);
}

#[test]
fn js_6_default_limits_are_16_total_and_4_per_scope() {
    assert_eq!(DEFAULT_MAX_LIVE_SCRIPT_CONTEXTS, 16);
    assert_eq!(DEFAULT_MAX_SCRIPT_CONTEXTS_PER_SCOPE, 4);
    assert_eq!(DEFAULT_SCRIPT_CONTEXT_QUEUE_CAPACITY, 64);
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let targets: Vec<Target> = (0..5).map(|_| Target::new()).collect();
    let gens: Vec<NavigationGeneration> = targets.iter().map(Target::next_generation).collect();
    // 4 scope x 4 world = 16 件（各 scope は上限 4 ちょうど）。
    for (t, g) in targets.iter().zip(&gens).take(4) {
        for world in 1..=4 {
            let key = ScriptContextKey::new(t.scope(), *g, world);
            assert_eq!(eval(&h, key, "ok"), Ok("ok".to_string()));
        }
    }
    assert_eq!(h.stats().live_contexts(), 16);
    // 17 件目（5 つ目の scope）は全体の上限で拒否される。
    let seventeenth = ScriptContextKey::new(targets[4].scope(), gens[4], 1);
    assert_eq!(
        eval(&h, seventeenth, "ok"),
        Err(ScriptContextError::LimitExceeded {
            limit: ScriptContextLimit::LiveContexts { limit: 16 }
        })
    );
    // 上限超過のコンテキストは作られていない。
    assert_eq!(h.stats().live_contexts(), 16);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 16);
}

#[test]
fn js_6_fifth_context_in_one_scope_is_rejected() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g = t.next_generation();
    for world in 1..=4 {
        assert_eq!(
            eval(&h, ScriptContextKey::new(t.scope(), g, world), "ok"),
            Ok("ok".to_string())
        );
    }
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g, 5), "ok"),
        Err(ScriptContextError::LimitExceeded {
            limit: ScriptContextLimit::ContextsPerScope { limit: 4 }
        })
    );
    // 既存のキーは引き続き使える。
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g, 1), "count"),
        Ok("1".to_string())
    );
}

#[test]
fn js_6_source_over_1_mib_is_rejected_before_engine() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(MAX_SCRIPT_CONTEXT_SOURCE_BYTES, 1_048_576);
    let big = "a".repeat(1_048_577);
    assert_eq!(
        eval(&h, key, &big),
        Err(ScriptContextError::LimitExceeded {
            limit: ScriptContextLimit::SourceBytes {
                len: 1_048_577,
                limit: 1_048_576
            }
        })
    );
    assert_eq!(probe.evals.load(Ordering::SeqCst), 0);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.stats().worker_spawns(), 0);
    // 上限ちょうどは受け付ける。
    let exact = "a".repeat(1_048_576);
    assert_eq!(eval(&h, key, &exact), Ok(exact));
}

#[test]
fn js_6_result_size_limit() {
    assert_eq!(DEFAULT_MAX_SCRIPT_RESULT_BYTES, 3_145_728);
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    let opts = ScriptEvalOptions::default().with_max_result_bytes(16);
    assert_eq!(opts.max_result_bytes(), 16);
    assert_eq!(
        eval_with(&h, key, "len:17", &opts),
        Err(ScriptContextError::LimitExceeded {
            limit: ScriptContextLimit::ResultBytes { len: 17, limit: 16 }
        })
    );
    assert_eq!(eval_with(&h, key, "len:16", &opts), Ok("a".repeat(16)));
    // 天井を超える指定は天井に丸める。
    let widened = ScriptEvalOptions::default().with_max_result_bytes(usize::MAX);
    assert_eq!(widened.max_result_bytes(), 3_145_728);
    // 超過してもコンテキストは残る。
    assert_eq!(h.stats().live_contexts(), 1);
}

#[test]
fn js_6_timeout_keeps_context() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    let opts = ScriptEvalOptions::default().with_timeout(Duration::from_millis(200));
    assert_eq!(
        eval_with(&h, key, "timeout", &opts),
        Err(ScriptContextError::Timeout {
            limit: Duration::from_millis(200)
        })
    );
    assert_eq!(eval(&h, key, "count"), Ok("2".to_string()));
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 0);
}

#[test]
fn js_6_discarded_timeout_is_engine_restarted() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(
        eval(&h, key, "discard-timeout"),
        Err(ScriptContextError::EngineRestarted {
            cause: RestartCause::Timeout
        })
    );
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 1);
    // 状態は失われ、次の評価は新しいコンテキストになる。
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn js_6_engine_unavailable_is_engine_restarted_and_next_evaluate_recreates() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(
        eval(&h, key, "unavailable"),
        Err(ScriptContextError::EngineRestarted {
            cause: RestartCause::Crashed
        })
    );
    assert_eq!(h.stats().live_contexts(), 0);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 2);
}

#[test]
fn js_6_resource_limit_maps_without_restart() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(eval(&h, key, "oom"), Err(ScriptContextError::ResourceLimit));
    assert_eq!(h.stats().live_contexts(), 1);
    assert_eq!(
        eval(&h, key, "oom-discard"),
        Err(ScriptContextError::EngineRestarted {
            cause: RestartCause::ResourceLimit
        })
    );
    assert_eq!(h.stats().live_contexts(), 0);
}

#[test]
fn js_1_evaluation_failed_is_truncated_and_not_echoed_in_display() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    let err = eval(&h, key, "throw:5000").expect_err("must fail");
    let ScriptContextError::EvaluationFailed { message } = &err else {
        panic!("expected EvaluationFailed, got {err:?}");
    };
    assert_eq!(message.len(), 4096);
    assert_eq!(err.to_string(), "script evaluation failed");
    let debug = format!("{err:?}");
    assert!(debug.contains("message_len: 4096"), "{debug}");
    assert!(!debug.contains("xxxx"), "{debug}");
    assert_eq!(h.stats().live_contexts(), 1);
}

#[test]
fn js_1_non_string_result_is_evaluation_failed() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(
        eval(&h, key, "num"),
        Err(ScriptContextError::EvaluationFailed {
            message: "evaluation result is not a string".to_string()
        })
    );
}

#[test]
fn js_6_queue_full_returns_limit_exceeded() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(
        &probe,
        ScriptContextOptions::default().with_queue_capacity(2),
    );
    let t = Target::new();
    let g = t.next_generation();
    let key = ScriptContextKey::new(t.scope(), g, 1);
    let opts = ScriptEvalOptions::default();
    let running = h.evaluate(key, doc(), "block", &opts);
    wait_until("worker is blocked in the engine", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    let q1 = h.evaluate(key, doc(), "q1", &opts);
    let q2 = h.evaluate(key, doc(), "q2", &opts);
    assert_eq!(h.stats().queued(), 2);
    assert_eq!(
        block_on(h.evaluate(key, doc(), "q3", &opts)),
        Err(ScriptContextError::LimitExceeded {
            limit: ScriptContextLimit::Queue { capacity: 2 }
        })
    );
    probe.open_gate();
    assert_eq!(block_on(running), Ok("unblocked".to_string()));
    assert_eq!(block_on(q1), Ok("q1".to_string()));
    assert_eq!(block_on(q2), Ok("q2".to_string()));
}

#[test]
fn js_6_dropped_future_leaves_the_queue() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    let opts = ScriptEvalOptions::default();
    let running = h.evaluate(key, doc(), "block", &opts);
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    let dropped = h.evaluate(key, doc(), "never", &opts);
    assert_eq!(h.stats().queued(), 1);
    drop(dropped);
    assert_eq!(h.stats().queued(), 0);
    probe.open_gate();
    assert_eq!(block_on(running), Ok("unblocked".to_string()));
    assert_eq!(probe.evals.load(Ordering::SeqCst), 1);
}

#[test]
fn js_6_timeout_option_is_clamped() {
    assert_eq!(
        ScriptEvalOptions::default().timeout(),
        Duration::from_secs(5)
    );
    assert_eq!(DEFAULT_SCRIPT_EVAL_TIMEOUT, Duration::from_secs(5));
    assert_eq!(MAX_SCRIPT_EVAL_TIMEOUT, Duration::from_secs(60));
    assert_eq!(
        ScriptEvalOptions::default()
            .with_timeout(Duration::from_secs(3600))
            .timeout(),
        Duration::from_secs(60)
    );
    assert_eq!(
        ScriptEvalOptions::default()
            .with_timeout(Duration::from_millis(200))
            .timeout(),
        Duration::from_millis(200)
    );
}

#[test]
fn js_6_options_are_clamped_to_at_least_one() {
    let o = ScriptContextOptions::default()
        .with_max_live_contexts(0)
        .with_max_contexts_per_scope(0)
        .with_queue_capacity(0);
    assert_eq!(o.max_live_contexts(), 1);
    assert_eq!(o.max_contexts_per_scope(), 1);
    assert_eq!(o.queue_capacity(), 1);
}

#[test]
fn js_1_worker_panic_is_recovered_by_the_next_evaluate() {
    let probe = Arc::new(Probe::default());
    probe.panic_first_factory.store(true, Ordering::SeqCst);
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(
        eval(&h, key, "x"),
        Err(ScriptContextError::Unavailable {
            reason: UnavailableReason::Terminated
        })
    );
    wait_until("worker guard ran", || !h.stats().worker_running());
    assert_eq!(h.stats().live_contexts(), 0);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(h.stats().worker_spawns(), 2);
}

#[test]
fn js_1_dropping_the_last_handle_stops_the_worker_and_drops_engines() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(eval(&h, key, "x"), Ok("x".to_string()));
    drop(h);
    wait_until("engine dropped on the worker thread", || {
        probe.engine_drops.load(Ordering::SeqCst) == 1
    });
}

#[test]
fn js_1_debug_and_display_do_not_echo_inputs() {
    let t = Target::new();
    let h = ScriptContextHandle::disabled();
    assert!(format!("{h:?}").contains("enabled: false"));
    let e = ScriptContextError::EvaluationFailed {
        message: "secret-token".to_string(),
    };
    assert!(!format!("{e:?}").contains("secret-token"));
    assert!(!e.to_string().contains("secret-token"));
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 9);
    assert_eq!(key.world(), 9);
}

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
            "block-panic" => {
                self.probe.blocked.fetch_add(1, Ordering::SeqCst);
                let (lock, cv) = &self.probe.gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
                // ゲートの Mutex を poison させないよう、panic の前にガードを手放す。
                drop(open);
                panic!("injected eval panic");
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

/// 1 回だけ poll する（`Waker::noop`）。受理の時点で決まる結果を初回 poll で確かめるのに使う。
fn poll_once(f: &mut Pin<Box<ScriptEvalFuture>>) -> Poll<EvalResult> {
    let mut cx = Context::from_waker(Waker::noop());
    f.as_mut().poll(&mut cx)
}

/// future の完了を待って結果文字列にする。
fn finish(f: impl Future<Output = EvalResult>) -> Result<String, ScriptContextError> {
    block_on(f).map(ScriptEvalOutput::into_value)
}

/// ハンドル内の scope の世代記録（非公開の項目を直接見る）。
fn record(h: &ScriptContextHandle, scope: NavigationScope) -> Option<ScopeRecord> {
    let inner = h.inner.as_ref().expect("enabled");
    inner.shared.lock().scopes.get(&scope).copied()
}

fn latest(h: &ScriptContextHandle, scope: NavigationScope) -> Option<NavigationGeneration> {
    record(h, scope).map(|r| r.latest)
}

/// 追跡中の scope 数。
fn tracked(h: &ScriptContextHandle) -> usize {
    let inner = h.inner.as_ref().expect("enabled");
    inner.shared.lock().scopes.len()
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
        .map(ScriptEvalOutput::into_value)
}

fn eval_with(
    h: &ScriptContextHandle,
    key: ScriptContextKey,
    src: &str,
    opts: &ScriptEvalOptions,
) -> Result<String, ScriptContextError> {
    block_on(h.evaluate(key, doc(), src, opts)).map(ScriptEvalOutput::into_value)
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
    h.begin_generation(t.scope(), t.next_generation())
        .expect("notify");
    h.release_scope(t.scope()).expect("notify");
    assert_eq!(h.stats().worker_spawns(), 0);
}

#[test]
fn perf_7_no_worker_until_first_evaluate() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    assert!(h.is_enabled());
    let t = Target::new();
    let g1 = t.next_generation();
    h.begin_generation(t.scope(), g1).expect("notify");
    h.release_scope(t.scope()).expect("notify");
    h.begin_generation(t.scope(), g1).expect("notify");
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
    h.begin_generation(t.scope(), g2).expect("notify");
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
    h.release_scope(t.scope()).expect("notify");
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

/// `JS-6`・`CDP-2`: ワーカーが評価で止まっている間に通知を連投しても何も溜まらない（記録は
/// 受理時に確定し、待ち行列は評価だけ）。知らない scope への release は追跡枠も使わず、
/// 生存 scope のコンテキスト（カウンタ）は保たれ、古い世代の評価は `NotFound` になる。
#[test]
fn js_6_notification_flood_is_bounded_and_still_marks_stale() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(
        &probe,
        ScriptContextOptions::default().with_queue_capacity(2),
    );
    let busy = Target::new();
    let busy_key = ScriptContextKey::new(busy.scope(), busy.next_generation(), 1);
    assert_eq!(eval(&h, busy_key, "count"), Ok("1".to_string()));
    let running = h.evaluate(busy_key, doc(), "block", &ScriptEvalOptions::default());
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    let t = Target::new();
    let g1 = t.next_generation();
    let key = ScriptContextKey::new(t.scope(), g1, 1);
    assert_eq!(h.begin_generation(t.scope(), g1), Ok(()));
    let mut newest = g1;
    for _ in 0..1000 {
        let g = t.next_generation();
        assert_eq!(h.begin_generation(t.scope(), g), Ok(()));
        newest = g;
        assert_eq!(h.release_scope(Target::new().scope()), Ok(()));
    }
    assert_eq!(h.stats().queued(), 0);
    assert_eq!(tracked(&h), 2);
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
    // 生存 scope のコンテキストは破棄されていない。
    assert_eq!(eval(&h, busy_key, "count"), Ok("2".to_string()));
    assert_eq!(eval(&h, key, "count"), Err(ScriptContextError::NotFound));
    let fresh = ScriptContextKey::new(t.scope(), newest, 1);
    assert_eq!(eval(&h, fresh, "count"), Ok("1".to_string()));
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
    assert_eq!(
        block_on(running).map(ScriptEvalOutput::into_value),
        Ok("unblocked".to_string())
    );
    assert_eq!(
        block_on(q1).map(ScriptEvalOutput::into_value),
        Ok("q1".to_string())
    );
    assert_eq!(
        block_on(q2).map(ScriptEvalOutput::into_value),
        Ok("q2".to_string())
    );
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
    assert_eq!(
        block_on(running).map(ScriptEvalOutput::into_value),
        Ok("unblocked".to_string())
    );
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

/// アイドル終了した旧ワーカーのガードが、その後に起動した新ワーカーの状態を壊さない
/// （`CDP-2`・`JS-6`。旧世代の Drop は `worker_alive`・件数・待ち行列に触れない）。
#[test]
fn cdp_2_stale_worker_guard_does_not_clobber_newer_worker() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g1, 1), "hi"),
        Ok("hi".to_string())
    );
    assert_eq!(h.stats().worker_spawns(), 1);
    assert!(h.stats().worker_running());
    assert_eq!(h.stats().live_contexts(), 1);

    // 世代 0 は既に置き換えられた旧ワーカー相当。
    let shared = Arc::clone(&h.inner.as_ref().expect("enabled").shared);
    drop(WorkerGuard {
        shared,
        generation: 0,
    });
    assert!(h.stats().worker_running());
    assert_eq!(h.stats().live_contexts(), 1);
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g1, 1), "again"),
        Ok("again".to_string())
    );
    assert_eq!(h.stats().worker_spawns(), 1);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 1);
}

/// `CDP-2`・`JS-6`: ワーカーが評価で止まっている間の `release_scope` は、戻った時点で記録が消えており、
/// 通知が失われずワーカーが次のループでコンテキスト（エンジン）を破棄する。
#[test]
fn cdp_2_release_scope_during_busy_worker_drops_context() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let closing = Target::new();
    let busy = Target::new();
    let closing_key = ScriptContextKey::new(closing.scope(), closing.next_generation(), 1);
    assert_eq!(eval(&h, closing_key, "x"), Ok("x".to_string()));
    let busy_key = ScriptContextKey::new(busy.scope(), busy.next_generation(), 1);
    let running = h.evaluate(busy_key, doc(), "block", &ScriptEvalOptions::default());
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    assert_eq!(h.release_scope(closing.scope()), Ok(()));
    assert!(record(&h, closing.scope()).is_none());
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
    wait_until("closed scope's context is dropped", || {
        h.stats().live_contexts() == 1
    });
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 1);
}

/// `CDP-2`（R3-b）: 受理済みでまだ走っていない評価は、あとから来た begin（より新しい世代）で
/// 実行直前の照合に落ち `NotFound` になる。その世代のコンテキストは作られない。
#[test]
fn cdp_2_queued_evaluate_superseded_by_later_begin_is_not_found() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let g2 = t.next_generation();
    let g3 = t.next_generation();
    let busy = Target::new();
    let opts = ScriptEvalOptions::default();
    let running = h.evaluate(
        ScriptContextKey::new(busy.scope(), busy.next_generation(), 1),
        doc(),
        "block",
        &opts,
    );
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    let e1 = h.evaluate(ScriptContextKey::new(t.scope(), g1, 1), doc(), "e1", &opts);
    assert_eq!(latest(&h, t.scope()), Some(g1));
    assert_eq!(h.begin_generation(t.scope(), g2), Ok(()));
    let e2 = h.evaluate(ScriptContextKey::new(t.scope(), g2, 1), doc(), "e2", &opts);
    assert_eq!(h.begin_generation(t.scope(), g3), Ok(()));
    assert_eq!(latest(&h, t.scope()), Some(g3));
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
    assert_eq!(finish(e1), Err(ScriptContextError::NotFound));
    assert_eq!(finish(e2), Err(ScriptContextError::NotFound));
    // busy の 1 件だけが作られ、t のコンテキストは作られていない。
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 1);
}

/// `CDP-2`（R3-a）: `begin_generation` が `Ok` で戻ったあとの古い世代の評価は、ワーカーを使わず
/// 受理の時点（初回 poll）で `NotFound` になる。
#[test]
fn cdp_2_begin_returned_ok_rejects_older_evaluate_at_admission() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let g2 = t.next_generation();
    assert_eq!(h.begin_generation(t.scope(), g2), Ok(()));
    assert_eq!(latest(&h, t.scope()), Some(g2));
    let mut f = Box::pin(h.evaluate(
        ScriptContextKey::new(t.scope(), g1, 1),
        doc(),
        "x",
        &ScriptEvalOptions::default(),
    ));
    match poll_once(&mut f) {
        Poll::Ready(Err(ScriptContextError::NotFound)) => {}
        other => panic!("expected an immediate NotFound, got {other:?}"),
    }
    assert_eq!(h.stats().worker_spawns(), 0);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 0);
}

/// `CDP-2`: 待ち行列にある世代通知（begin）のあとの release は、記録を残さず（release が最後に適用された
/// 扱い）、コンテキストを破棄する。記録は呼び出しの時点で決まるため、到着順が入れ替わらない。
#[test]
fn cdp_2_begin_then_release_while_busy_leaves_no_record() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let key = ScriptContextKey::new(t.scope(), g1, 1);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    let busy = Target::new();
    let running = h.evaluate(
        ScriptContextKey::new(busy.scope(), busy.next_generation(), 1),
        doc(),
        "block",
        &ScriptEvalOptions::default(),
    );
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    let g2 = t.next_generation();
    assert_eq!(h.begin_generation(t.scope(), g2), Ok(()));
    assert_eq!(h.release_scope(t.scope()), Ok(()));
    assert!(record(&h, t.scope()).is_none());
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
    wait_until("t's context is dropped", || h.stats().live_contexts() == 1);
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 1);
    // 記録は残っていないので、g1 は stale 扱い（NotFound）にならず新規作成になる。
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
}

/// `CDP-2`（R3-c）: release をはさんで受理された評価は実行直前の照合で `NotFound` になり、
/// 2 回目の release のあとに記録が残らない。
#[test]
fn cdp_2_release_between_admission_and_run_is_not_found_and_leaves_no_record() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    let busy = Target::new();
    let opts = ScriptEvalOptions::default();
    let running = h.evaluate(
        ScriptContextKey::new(busy.scope(), busy.next_generation(), 1),
        doc(),
        "block",
        &opts,
    );
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    assert_eq!(h.release_scope(t.scope()), Ok(()));
    let e = h.evaluate(key, doc(), "e", &opts);
    assert_eq!(tracked(&h), 2);
    assert_eq!(h.release_scope(t.scope()), Ok(()));
    assert!(record(&h, t.scope()).is_none());
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
    assert_eq!(finish(e), Err(ScriptContextError::NotFound));
    assert_eq!(tracked(&h), 1);
    assert_eq!(h.stats().live_contexts(), 1);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 1);
}

/// `CDP-2`（R3-d）: release のあとに同じキーで受理した評価は、release 前のコンテキストを再利用せず
/// （incarnation が違う）、新しいコンテキストで走る。
#[test]
fn cdp_2_release_then_readmit_same_key_does_not_reuse_context() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let key = ScriptContextKey::new(t.scope(), t.next_generation(), 1);
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    let busy = Target::new();
    let opts = ScriptEvalOptions::default();
    let running = h.evaluate(
        ScriptContextKey::new(busy.scope(), busy.next_generation(), 1),
        doc(),
        "block",
        &opts,
    );
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    assert_eq!(h.release_scope(t.scope()), Ok(()));
    let e = h.evaluate(key, doc(), "count", &opts);
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
    assert_eq!(finish(e), Ok("1".to_string()));
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 1);
    assert_eq!(probe.factory_calls.load(Ordering::SeqCst), 3);
}

/// `JS-6`: 破棄待ち（記録と合わない）コンテキストは全体上限に数えない。ワーカーは破棄してから作る。
#[test]
fn js_6_doomed_contexts_do_not_count_toward_live_limit() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let targets: Vec<Target> = (0..5).map(|_| Target::new()).collect();
    let gens: Vec<NavigationGeneration> = targets.iter().map(Target::next_generation).collect();
    for (t, g) in targets.iter().zip(&gens).take(4) {
        for world in 1..=4 {
            let key = ScriptContextKey::new(t.scope(), *g, world);
            assert_eq!(eval(&h, key, "ok"), Ok("ok".to_string()));
        }
    }
    assert_eq!(h.stats().live_contexts(), 16);
    let g2 = targets[0].next_generation();
    assert_eq!(h.begin_generation(targets[0].scope(), g2), Ok(()));
    let fifth = ScriptContextKey::new(targets[4].scope(), gens[4], 1);
    assert_eq!(eval(&h, fifth, "ok"), Ok("ok".to_string()));
    wait_until("doomed contexts dropped", || {
        h.stats().live_contexts() == 13
    });
    assert_eq!(probe.engine_drops.load(Ordering::SeqCst), 4);
}

/// `CDP-2`・`JS-6`: 追跡上限の純粋な状態遷移（スレッドを使わない）。上限・満杯時の既知 scope の更新・
/// release での空き・再登録での incarnation・`is_current` / `is_stale` の真理値。
#[test]
fn state_track_limit_incarnation_and_is_current() {
    let gt = Target::new();
    let g1 = gt.next_generation();
    let g2 = gt.next_generation();
    let scopes: Vec<NavigationScope> = (0..=MAX_TRACKED_SCOPES)
        .map(|_| Target::new().scope())
        .collect();
    let key = |scope, g| ScriptContextKey::new(scope, g, 1);
    let mut st = State::new();
    for (i, s) in scopes.iter().take(MAX_TRACKED_SCOPES).enumerate() {
        assert_eq!(st.track(*s, g1), Ok(i as u64));
    }
    let limit = ScriptContextError::LimitExceeded {
        limit: ScriptContextLimit::TrackedScopes { limit: 1024 },
    };
    assert_eq!(st.track(scopes[1024], g1), Err(limit));
    assert_eq!(st.scopes.len(), 1024);

    // 満杯でも既知の scope は更新できる。小さい世代では下げない。
    assert_eq!(st.track(scopes[0], g2), Ok(0));
    assert_eq!(st.track(scopes[0], g1), Ok(0));
    assert!(st.is_current(key(scopes[0], g2), 0));
    assert!(!st.is_current(key(scopes[0], g1), 0));
    assert!(!st.is_current(key(scopes[0], g2), 7));
    assert!(st.is_stale(key(scopes[0], g1)));
    assert!(!st.is_stale(key(scopes[0], g2)));
    // 記録の無い scope は stale でも current でもない。
    assert!(!st.is_stale(key(scopes[1024], g1)));
    assert!(!st.is_current(key(scopes[1024], g1), 0));

    // release で空き、再登録で incarnation が変わる。
    st.scopes.remove(&scopes[0]);
    assert!(!st.is_current(key(scopes[0], g2), 0));
    assert_eq!(st.track(scopes[1024], g1), Ok(1024));
    st.scopes.remove(&scopes[1]);
    assert_eq!(st.track(scopes[0], g1), Ok(1025));
    assert!(!st.is_current(key(scopes[0], g1), 0));
    assert!(st.is_current(key(scopes[0], g1), 1025));
}

/// `CDP-2`（codex P1-1）: 別々の `Clone` ハンドルから並行に begin / release を繰り返しても、
/// 記録と追跡数が食い違わず、上限が正確に効く。
#[test]
fn cdp_2_concurrent_clone_handles_begin_release_leave_no_record() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let shared = Arc::new(Target::new());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let joins: Vec<_> = (0..8)
        .map(|i| {
            let h = h.clone();
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let own = Target::new();
                barrier.wait();
                for _ in 0..500 {
                    let g = own.next_generation();
                    assert_eq!(h.begin_generation(own.scope(), g), Ok(()));
                    if i < 2 {
                        let sg = shared.next_generation();
                        assert_eq!(h.begin_generation(shared.scope(), sg), Ok(()));
                    }
                    assert_eq!(h.release_scope(own.scope()), Ok(()));
                    if i < 2 {
                        assert_eq!(h.release_scope(shared.scope()), Ok(()));
                    }
                }
                own.scope()
            })
        })
        .collect();
    let owns: Vec<NavigationScope> = joins
        .into_iter()
        .map(|j| j.join().expect("thread"))
        .collect();
    // 共有 scope は最後の順序が実行ごとに違うため、ここで確定的に閉じる。
    assert_eq!(h.release_scope(shared.scope()), Ok(()));
    for s in owns {
        assert!(record(&h, s).is_none());
    }
    assert_eq!(tracked(&h), 0);
    // 食い違いがあれば、ここで上限が効かない（または早く効く）。
    for _ in 0..MAX_TRACKED_SCOPES {
        let t = Target::new();
        assert_eq!(h.begin_generation(t.scope(), t.next_generation()), Ok(()));
    }
    let t = Target::new();
    assert_eq!(
        h.begin_generation(t.scope(), t.next_generation()),
        Err(ScriptContextError::LimitExceeded {
            limit: ScriptContextLimit::TrackedScopes { limit: 1024 }
        })
    );
}

/// `CDP-2`・`JS-6`（codex P1-1）: 並行する begin / evaluate が新しい scope を登録しても、
/// 追跡数は上限（1024）を超えず、受理数は空き枠と一致する。
#[test]
fn cdp_2_tracked_scope_limit_holds_under_concurrency() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(
        &probe,
        ScriptContextOptions::default().with_queue_capacity(4096),
    );
    let busy = Target::new();
    let running = h.evaluate(
        ScriptContextKey::new(busy.scope(), busy.next_generation(), 1),
        doc(),
        "block",
        &ScriptEvalOptions::default(),
    );
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let joins: Vec<_> = (0..8)
        .map(|_| {
            let h = h.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let tracked_limit = ScriptContextError::LimitExceeded {
                    limit: ScriptContextLimit::TrackedScopes { limit: 1024 },
                };
                let mut accepted = 0usize;
                let mut pending = Vec::new();
                barrier.wait();
                for n in 0..200 {
                    let t = Target::new();
                    let g = t.next_generation();
                    if n % 2 == 0 {
                        match h.begin_generation(t.scope(), g) {
                            Ok(()) => accepted += 1,
                            Err(e) => assert_eq!(e, tracked_limit),
                        }
                    } else {
                        let mut f = Box::pin(h.evaluate(
                            ScriptContextKey::new(t.scope(), g, 1),
                            doc(),
                            "q",
                            &ScriptEvalOptions::default(),
                        ));
                        match poll_once(&mut f) {
                            Poll::Pending => {
                                accepted += 1;
                                pending.push(f);
                            }
                            Poll::Ready(Err(e)) => assert_eq!(e, tracked_limit),
                            Poll::Ready(Ok(_)) => panic!("evaluation cannot finish while blocked"),
                        }
                    }
                }
                (accepted, pending)
            })
        })
        .collect();
    let mut accepted = 0;
    let mut futures = Vec::new();
    for j in joins {
        let (a, p) = j.join().expect("thread");
        accepted += a;
        futures.extend(p);
    }
    assert_eq!(accepted, MAX_TRACKED_SCOPES - 1);
    assert_eq!(tracked(&h), MAX_TRACKED_SCOPES);
    drop(futures);
    probe.open_gate();
    assert_eq!(finish(running), Ok("unblocked".to_string()));
}

/// `CDP-2`（codex P1-2）: 評価の実行中に受理した begin は、ワーカーが panic しても失われず、
/// 古い世代の評価は受理の時点で `NotFound` のままになる。
#[test]
fn cdp_2_begin_during_running_eval_survives_worker_panic() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let g2 = t.next_generation();
    let opts = ScriptEvalOptions::default();
    let running = h.evaluate(
        ScriptContextKey::new(t.scope(), g1, 1),
        doc(),
        "block-panic",
        &opts,
    );
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    assert_eq!(h.begin_generation(t.scope(), g2), Ok(()));
    assert_eq!(latest(&h, t.scope()), Some(g2));
    let mut q = Box::pin(h.evaluate(ScriptContextKey::new(t.scope(), g2, 1), doc(), "q", &opts));
    assert!(matches!(poll_once(&mut q), Poll::Pending));
    probe.open_gate();
    let terminated = Err(ScriptContextError::Unavailable {
        reason: UnavailableReason::Terminated,
    });
    assert_eq!(finish(running), terminated);
    assert_eq!(finish(q), terminated);
    wait_until("worker guard ran", || !h.stats().worker_running());
    assert_eq!(latest(&h, t.scope()), Some(g2));
    let mut old =
        Box::pin(h.evaluate(ScriptContextKey::new(t.scope(), g1, 1), doc(), "old", &opts));
    match poll_once(&mut old) {
        Poll::Ready(Err(ScriptContextError::NotFound)) => {}
        other => panic!("expected an immediate NotFound, got {other:?}"),
    }
    assert_eq!(
        eval(&h, ScriptContextKey::new(t.scope(), g2, 1), "count"),
        Ok("1".to_string())
    );
    assert_eq!(h.stats().worker_spawns(), 2);
}

/// `CDP-2`（codex P1-2）: 評価の実行中に受理した release も、ワーカーが panic しても失われない
/// （記録は無いまま。次の評価は新規作成になる）。
#[test]
fn cdp_2_release_during_running_eval_survives_worker_panic() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let t = Target::new();
    let g1 = t.next_generation();
    let key = ScriptContextKey::new(t.scope(), g1, 1);
    let running = h.evaluate(key, doc(), "block-panic", &ScriptEvalOptions::default());
    wait_until("worker is blocked", || {
        probe.blocked.load(Ordering::SeqCst) == 1
    });
    assert_eq!(h.release_scope(t.scope()), Ok(()));
    assert!(record(&h, t.scope()).is_none());
    probe.open_gate();
    assert_eq!(
        finish(running),
        Err(ScriptContextError::Unavailable {
            reason: UnavailableReason::Terminated
        })
    );
    wait_until("worker guard ran", || !h.stats().worker_running());
    assert!(record(&h, t.scope()).is_none());
    assert_eq!(eval(&h, key, "count"), Ok("1".to_string()));
    assert_eq!(h.stats().worker_spawns(), 2);
}

/// `CDP-2`（Bugbot High・R3-e）: 並行する呼び出しでも、スレッド内の送信順（begin が `Ok` で戻る →
/// 古い世代の評価）は必ず守られ、古い評価は常に `NotFound` になる。
#[test]
fn cdp_2_concurrent_begin_then_older_evaluate_is_always_not_found() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let joins: Vec<_> = (0..4)
        .map(|_| {
            let h = h.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let own = Target::new();
                barrier.wait();
                for n in 0..200 {
                    let g_old = own.next_generation();
                    let g_new = own.next_generation();
                    assert_eq!(h.begin_generation(own.scope(), g_new), Ok(()));
                    let mut f = Box::pin(h.evaluate(
                        ScriptContextKey::new(own.scope(), g_old, 1),
                        doc(),
                        "x",
                        &ScriptEvalOptions::default(),
                    ));
                    match poll_once(&mut f) {
                        Poll::Ready(Err(ScriptContextError::NotFound)) => {}
                        other => panic!("expected an immediate NotFound, got {other:?}"),
                    }
                    if n % 10 == 0 {
                        assert_eq!(
                            eval(&h, ScriptContextKey::new(own.scope(), g_new, 1), "count"),
                            Ok("1".to_string())
                        );
                    }
                }
            })
        })
        .collect();
    for j in joins {
        j.join().expect("thread");
    }
}

/// `CDP-2`: 未解放の scope の世代記録は、別の scope が大量に通知されても消えない。
/// begin(A, g2) のあと別 scope を 1500 個通知しても、A への g1 の評価は `NotFound` になる。
#[test]
fn cdp_2_generation_record_survives_many_other_scopes() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let a = Target::new();
    let g1 = a.next_generation();
    let g2 = a.next_generation();
    h.begin_generation(a.scope(), g2).expect("notify");
    let mut accepted = 1;
    for _ in 0..1500 {
        if h.begin_generation(Target::new().scope(), g2).is_ok() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1024);
    assert_eq!(
        eval(&h, ScriptContextKey::new(a.scope(), g1, 1), "count"),
        Err(ScriptContextError::NotFound)
    );
    assert_eq!(
        eval(&h, ScriptContextKey::new(a.scope(), g2, 1), "count"),
        Ok("1".to_string())
    );
}

/// `CDP-2`・`JS-6`: 追跡 scope 数が上限（1024）に達したら新しい scope を `TrackedScopes` で拒否し、
/// 既知の scope は受け付ける。release で空きができたら新しい scope を受け付ける。
#[test]
fn cdp_2_tracked_scope_limit_rejects_new_scopes_until_release() {
    let probe = Arc::new(Probe::default());
    let h = handle_with(&probe, ScriptContextOptions::default());
    let first = Target::new();
    let g = first.next_generation();
    h.begin_generation(first.scope(), g).expect("notify");
    for _ in 1..1024 {
        h.begin_generation(Target::new().scope(), g)
            .expect("notify");
    }
    let limit = ScriptContextError::LimitExceeded {
        limit: ScriptContextLimit::TrackedScopes { limit: 1024 },
    };
    let fresh = Target::new();
    assert_eq!(h.begin_generation(fresh.scope(), g), Err(limit.clone()));
    assert_eq!(
        eval(&h, ScriptContextKey::new(fresh.scope(), g, 1), "x"),
        Err(limit)
    );
    // 既知の scope は上限に関係なく受け付ける。
    let g_next = first.next_generation();
    assert_eq!(h.begin_generation(first.scope(), g_next), Ok(()));
    h.release_scope(first.scope()).expect("release");
    assert_eq!(h.begin_generation(fresh.scope(), g), Ok(()));
    assert_eq!(
        eval(&h, ScriptContextKey::new(fresh.scope(), g, 1), "x"),
        Ok("x".to_string())
    );
}

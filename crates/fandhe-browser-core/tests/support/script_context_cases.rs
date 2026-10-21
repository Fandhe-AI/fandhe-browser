//! V8 / boa 共通の評価コンテキストのテストケース（TASK-44.5・Issue #1216・ビヘイビア
//! `CDP-2` / `JS-1` / `JS-6` / `PERF-7`）。
//!
//! `tests/script_context_v8.rs` と `tests/script_context_boa.rs` が `#[path]` で取り込む。実エンジンの
//! 子プロセスを使うため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ独自 `main` が前提。
//! スレッド数・子プロセス数の実測は Linux のみ（`/proc`）。他 OS は `stats()` で同じ受入基準を見る。
//! 期待値は具体値で書く。

use std::sync::Arc;
use std::time::{Duration, Instant};

use fandhe_browser_core::{
    Config, DomBridgeLimits, NavigationGeneration, NavigationResult, NavigationState, RestartCause,
    ScriptContextError, ScriptContextHandle, ScriptContextKey, ScriptContextLimit,
    ScriptContextOptions, ScriptEvalOptions, UnavailableReason,
};

const URL: &str = "https://example.test/page";
const HTML: &str = "<html><body><h1>Hello</h1></body></html>";

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timeout waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// このプロセスのスレッド数（Linux のみ。他 OS では 0 固定で比較が自明に成立する）。
fn thread_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/task").map_or(0, |d| d.count())
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// 直下の子プロセス数（Linux のみ。`/proc/*/stat` の ppid を走査する）。
fn child_process_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        let me = std::process::id();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return 0;
        };
        dir.flatten()
            .filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok())
            .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
            .filter(|stat| {
                // 形式: pid (comm) state ppid ...。comm は括弧を含み得るので最後の ')' 以降を読む。
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok())
                    == Some(me)
            })
            .count()
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

fn config_for(engine: &str) -> Config {
    Config::from_toml_str(&format!("[js]\nengine = \"{engine}\"\n")).expect("config")
}

fn handle_with(engine: &str, options: ScriptContextOptions) -> ScriptContextHandle {
    ScriptContextHandle::with_options(config_for(engine).js(), options)
}

fn document() -> Arc<NavigationResult> {
    Arc::new(NavigationResult::new(URL, HTML))
}

/// 1 ターゲット分の scope と世代の払い出し役。
struct Target {
    nav: NavigationState,
}

impl Target {
    fn new() -> Self {
        Self {
            nav: NavigationState::new(),
        }
    }
    fn generation(&self) -> NavigationGeneration {
        self.nav.begin_navigation().expect("generation")
    }
    fn key(&self, generation: NavigationGeneration, world: u64) -> ScriptContextKey {
        ScriptContextKey::new(self.nav.scope(), generation, world)
    }
}

fn eval(
    rt: &tokio::runtime::Runtime,
    h: &ScriptContextHandle,
    key: ScriptContextKey,
    doc: &Arc<NavigationResult>,
    src: &str,
) -> Result<String, ScriptContextError> {
    eval_with(rt, h, key, doc, src, &ScriptEvalOptions::default())
}

fn eval_with(
    rt: &tokio::runtime::Runtime,
    h: &ScriptContextHandle,
    key: ScriptContextKey,
    doc: &Arc<NavigationResult>,
    src: &str,
    opts: &ScriptEvalOptions,
) -> Result<String, ScriptContextError> {
    rt.block_on(h.evaluate(key, Arc::clone(doc), src, opts))
}

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。
pub fn run(engine: &str) {
    let rt = rt();
    let doc = document();
    // 子プロセス数の基準値。ハンドルの破棄は非同期（ワーカースレッドが子を片付ける）ため、
    // 以降のケースは前のケースの子が消えるのを待ってから基準値と比べる。
    let run_base_children = child_process_count();

    eprintln!("case: PERF-7 nothing is spawned until the first evaluate");
    {
        let base_threads = thread_count();
        let base_children = child_process_count();
        let h = handle_with(engine, ScriptContextOptions::default());
        assert!(h.is_enabled());
        let t = Target::new();
        let g = t.generation();
        h.begin_generation(t.nav.scope(), g);
        h.release_scope(t.nav.scope());
        assert_eq!(h.stats().worker_spawns(), 0);
        assert!(!h.stats().worker_running());
        assert_eq!(thread_count(), base_threads);
        assert_eq!(child_process_count(), base_children);

        assert_eq!(
            eval(&rt, &h, t.key(g, 1), &doc, "'ok'"),
            Ok("ok".to_string())
        );
        let stats = h.stats();
        assert!(stats.worker_running());
        assert_eq!(stats.worker_spawns(), 1);
        assert_eq!(stats.live_contexts(), 1);
        assert_eq!(
            child_process_count(),
            base_children + usize::from(cfg!(target_os = "linux"))
        );

        eprintln!("case: CDP-2 release leaves no thread or child");
        h.release_scope(t.nav.scope());
        wait_until("worker gone after release", || {
            let s = h.stats();
            s.live_contexts() == 0
                && !s.worker_running()
                && thread_count() == base_threads
                && child_process_count() == base_children
        });
    }

    eprintln!("case: CDP-2 DOM read of the settled document");
    {
        let h = handle_with(engine, ScriptContextOptions::default());
        let t = Target::new();
        let key = t.key(t.generation(), 1);
        assert_eq!(
            eval(
                &rt,
                &h,
                key,
                &doc,
                "document.querySelector('h1').textContent"
            ),
            Ok("Hello".to_string())
        );
        assert_eq!(
            eval(&rt, &h, key, &doc, "document.readyState"),
            Ok("complete".to_string())
        );
        assert_eq!(
            eval(&rt, &h, key, &doc, "location.href"),
            Ok(URL.to_string())
        );

        eprintln!("case: CDP-2 state persists across evaluations");
        assert_eq!(
            eval(&rt, &h, key, &doc, "globalThis.x = 41; 'ok'"),
            Ok("ok".to_string())
        );
        assert_eq!(
            eval(&rt, &h, key, &doc, "String(x + 1)"),
            Ok("42".to_string())
        );

        eprintln!("case: REPAIR-3 DOM changes are not written back to NavigationResult");
        assert_eq!(
            eval(
                &rt,
                &h,
                key,
                &doc,
                "document.body.innerHTML = '<p>x</p>'; 'ok'"
            ),
            Ok("ok".to_string())
        );
        assert_eq!(doc.html(), HTML);
        assert_eq!(
            eval(
                &rt,
                &h,
                key,
                &doc,
                "String(document.querySelector('p') !== null)"
            ),
            Ok("true".to_string())
        );

        eprintln!("case: REPAIR-3 the event loop is not run");
        assert_eq!(
            eval(
                &rt,
                &h,
                key,
                &doc,
                "setTimeout(() => { globalThis.fired = 1 }, 0); 'ok'"
            ),
            Ok("ok".to_string())
        );
        assert_eq!(
            eval(&rt, &h, key, &doc, "String(globalThis.fired)"),
            Ok("undefined".to_string())
        );

        eprintln!("case: CDP-2 worlds of the same document do not share changes");
        let other = t.key(key.generation(), 2);
        assert_eq!(
            eval(&rt, &h, other, &doc, "String(typeof x)"),
            Ok("undefined".to_string())
        );
        assert_eq!(
            eval(
                &rt,
                &h,
                other,
                &doc,
                "String(document.querySelector('p') === null)"
            ),
            Ok("true".to_string())
        );
    }

    eprintln!("case: JS-6 timeout");
    {
        let h = handle_with(engine, ScriptContextOptions::default());
        let t = Target::new();
        let key = t.key(t.generation(), 1);
        assert_eq!(
            eval(&rt, &h, key, &doc, "globalThis.x = 7; 'ok'"),
            Ok("ok".to_string())
        );
        let opts = ScriptEvalOptions::default().with_timeout(Duration::from_millis(200));
        let result = eval_with(&rt, &h, key, &doc, "while (true) {}", &opts);
        if engine == "v8" {
            // V8 は watchdog で止まり、コンテキストが残る。
            assert_eq!(
                result,
                Err(ScriptContextError::Timeout {
                    limit: Duration::from_millis(200)
                })
            );
            assert_eq!(eval(&rt, &h, key, &doc, "String(x)"), Ok("7".to_string()));
        } else {
            // boa は親が子を kill する。状態は失われ、次の評価は新しいコンテキストになる。
            assert_eq!(
                result,
                Err(ScriptContextError::EngineRestarted {
                    cause: RestartCause::Timeout
                })
            );
            assert_eq!(
                eval(&rt, &h, key, &doc, "typeof x"),
                Ok("undefined".to_string())
            );
        }
    }

    eprintln!("case: JS-6 result size");
    {
        let h = handle_with(engine, ScriptContextOptions::default());
        let t = Target::new();
        let key = t.key(t.generation(), 1);
        let opts = ScriptEvalOptions::default().with_max_result_bytes(16);
        assert_eq!(
            eval_with(&rt, &h, key, &doc, "'a'.repeat(17)", &opts),
            Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::ResultBytes { len: 17, limit: 16 }
            })
        );
        assert_eq!(
            eval_with(&rt, &h, key, &doc, "'a'.repeat(16)", &opts),
            Ok("a".repeat(16))
        );
        // 既定の上限（エンジンの天井）を超える結果は、切り詰めた Ok にならない。
        let big = eval(&rt, &h, key, &doc, "'a'.repeat(1048577)");
        assert!(
            matches!(big, Err(ScriptContextError::EvaluationFailed { .. })),
            "oversized result must be an error, got {big:?}"
        );
    }

    eprintln!("case: JS-6 DOM operation limit");
    {
        // shim の注入自体が操作を消費するため、初期化に成功する最小の N を探してから余裕を足す。
        let mut chosen = None;
        for n in 1u64..=256 {
            let h = handle_with(
                engine,
                ScriptContextOptions::default()
                    .with_bridge_limits(DomBridgeLimits::default().with_max_ops_per_page(n)),
            );
            let t = Target::new();
            let key = t.key(t.generation(), 1);
            match eval(&rt, &h, key, &doc, "'ok'") {
                Ok(_) => {
                    chosen = Some(n);
                    break;
                }
                Err(ScriptContextError::Unavailable {
                    reason: UnavailableReason::ContextInit,
                }) => {}
                Err(other) => panic!("unexpected error while probing shim cost: {other:?}"),
            }
        }
        let shim_floor = chosen.expect("shim installs within 256 operations");
        let headroom = 10u64;
        let limit = shim_floor + headroom;
        let h = handle_with(
            engine,
            ScriptContextOptions::default()
                .with_bridge_limits(DomBridgeLimits::default().with_max_ops_per_page(limit)),
        );
        let t = Target::new();
        let key = t.key(t.generation(), 1);
        let count: usize = eval(
            &rt,
            &h,
            key,
            &doc,
            "var n = 0; for (var i = 0; i < 1000; i++) { try { document.querySelector('h1'); n++; } catch (e) { break; } } String(n)",
        )
        .expect("loop evaluates")
        .parse()
        .expect("count");
        assert!(
            (1..=headroom as usize).contains(&count),
            "successful operations must stay within the headroom, got {count} (limit {limit})"
        );
        assert_eq!(
            eval(
                &rt,
                &h,
                key,
                &doc,
                "try { document.querySelector('h1'); 'allowed' } catch (e) { 'blocked' }"
            ),
            Ok("blocked".to_string())
        );
    }

    eprintln!("case: JS-6 live context limits");
    {
        let h = handle_with(
            engine,
            ScriptContextOptions::default()
                .with_max_live_contexts(2)
                .with_max_contexts_per_scope(1),
        );
        let a = Target::new();
        let ga = a.generation();
        assert_eq!(
            eval(&rt, &h, a.key(ga, 1), &doc, "'a'"),
            Ok("a".to_string())
        );
        assert_eq!(
            eval(&rt, &h, a.key(ga, 2), &doc, "'a2'"),
            Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::ContextsPerScope { limit: 1 }
            })
        );
        let b = Target::new();
        let gb = b.generation();
        assert_eq!(
            eval(&rt, &h, b.key(gb, 1), &doc, "'b'"),
            Ok("b".to_string())
        );
        let c = Target::new();
        let gc = c.generation();
        assert_eq!(
            eval(&rt, &h, c.key(gc, 1), &doc, "'c'"),
            Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::LiveContexts { limit: 2 }
            })
        );
        assert_eq!(h.stats().live_contexts(), 2);
    }

    eprintln!("case: CDP-2 generations");
    {
        wait_until("children of earlier cases are gone", || {
            child_process_count() == run_base_children
        });
        let base_children = run_base_children;
        let h = handle_with(engine, ScriptContextOptions::default());
        let t = Target::new();
        let g1 = t.generation();
        let g2 = t.generation();
        assert_eq!(
            eval(&rt, &h, t.key(g1, 1), &doc, "globalThis.v = 1; 'ok'"),
            Ok("ok".to_string())
        );
        assert_eq!(
            eval(&rt, &h, t.key(g2, 1), &doc, "typeof v"),
            Ok("undefined".to_string())
        );
        assert_eq!(
            eval(&rt, &h, t.key(g1, 1), &doc, "typeof v"),
            Err(ScriptContextError::NotFound)
        );
        // 古い世代の子プロセスは破棄済みで、残るのは新しい世代の 1 つだけ。
        wait_until("older generation context dropped", || {
            h.stats().live_contexts() == 1
                && child_process_count() == base_children + usize::from(cfg!(target_os = "linux"))
        });
        h.release_scope(t.nav.scope());
        wait_until("released", || {
            !h.stats().worker_running() && child_process_count() == base_children
        });
    }
}

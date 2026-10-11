//! V8 / boa 共通のページスクリプト実行アクターのテストケース（TASK-112・Issue #784・
//! ビヘイビア `JS-1` / `JS-4` / `JS-6`・`PERF-7`）。
//!
//! `tests/page_actor_v8.rs` と `tests/page_actor_boa.rs` が `#[path]` で取り込む。実エンジンの
//! 子プロセスを使うため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ独自 `main` が前提。
//! スレッド数・子プロセス数の実測は Linux のみ（`/proc`）。他 OS は `stats()` で同じ受入基準を見る。
//! 期待値は具体値で書く。

use std::future::Future;
use std::io::Read;
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fandhe_browser_core::{
    AbortKind, Config, NavigationState, PageRunOptions, PageRunRequest, PageScriptActorOptions,
    PageScriptError, PageScriptExecutor, PageScriptHandle, PageScriptOutcome, PageScriptSkipReason,
    page_script_fetch_options,
};

const BASE: &str = "https://example.test/";

/// 現スレッドの tokio ランタイムで future を駆動する実行器（cli の実行器と同じ形）。
struct TokioExecutor;

impl PageScriptExecutor for TokioExecutor {
    fn block_on<'a>(&self, future: Pin<Box<dyn Future<Output = ()> + 'a>>) -> std::io::Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(future);
        Ok(())
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn handle_with(engine: &str, options: PageScriptActorOptions) -> PageScriptHandle {
    let cfg = Config::from_toml_str(&format!(
        "[js]\nengine = \"{engine}\"\nexecute_page_scripts = true\n"
    ))
    .expect("config");
    PageScriptHandle::with_options(cfg.js(), Arc::new(TokioExecutor), options)
}

fn request(state: &NavigationState, html: &str) -> PageRunRequest {
    let generation = state.begin_navigation().expect("generation");
    PageRunRequest::new(Arc::from(html), BASE, state.scope(), generation)
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

fn executed(result: Result<PageScriptOutcome, PageScriptError>) -> String {
    match result {
        Ok(PageScriptOutcome::Executed(out)) => out.html().to_string(),
        other => panic!("expected Executed, got {other:?}"),
    }
}

/// 要求を受けても応答を返さず接続を保持するサーバー。受けた接続数を返す。
fn spawn_silent_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || {
        let held = Arc::new(Mutex::new(Vec::new()));
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            let held = Arc::clone(&held);
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                // 閉じずに保持する（応答を保留する）。
                if let Ok(mut h) = held.lock() {
                    h.push(stream);
                }
            });
        }
    });
    (port, accepted)
}

const APPEND_SCRIPT: &str = "<body><script>var p=document.createElement('p');\
     p.textContent='from-actor';document.body.appendChild(p)</script></body>";

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。
pub fn run(engine: &str) {
    let rt = rt();
    let state = NavigationState::new();

    // サーバーのスレッドを基準値より前に作る（基準値の比較にスレッド数を使うため）。
    let (port, accepted) = spawn_silent_server();

    eprintln!("case: JS-1 lazy start creates no thread and no child process");
    let base_threads = thread_count();
    let base_children = child_process_count();
    let handle = handle_with(engine, PageScriptActorOptions::default());
    assert!(handle.is_enabled());
    assert_eq!(handle.stats().worker_spawns(), 0);
    let skipped = rt.block_on(handle.run_page_scripts(request(&state, "<p>no script</p>")));
    assert!(matches!(
        skipped,
        Ok(PageScriptOutcome::Skipped(
            PageScriptSkipReason::NoScriptElement
        ))
    ));
    let disabled = PageScriptHandle::disabled();
    let skipped = rt.block_on(disabled.run_page_scripts(request(&state, APPEND_SCRIPT)));
    assert!(matches!(
        skipped,
        Ok(PageScriptOutcome::Skipped(PageScriptSkipReason::Disabled))
    ));
    assert_eq!(handle.stats().worker_spawns(), 0);
    assert_eq!(disabled.stats().worker_spawns(), 0);
    assert_eq!(thread_count(), base_threads);
    assert_eq!(child_process_count(), base_children);

    eprintln!("case: JS-4 executes on the dedicated thread, then nothing is left behind");
    let html = executed(rt.block_on(handle.run_page_scripts(request(&state, APPEND_SCRIPT))));
    assert!(html.contains("<p>from-actor</p>"), "{html}");
    assert_eq!(handle.stats().worker_spawns(), 1);
    wait_until("worker gone", || {
        !handle.stats().worker_running()
            && thread_count() == base_threads
            && child_process_count() == base_children
    });

    eprintln!("case: JS-6 wall time abort leaves no thread or child, and the actor restarts");
    let limited = handle_with(
        engine,
        PageScriptActorOptions::default().with_run_options(
            PageRunOptions::default()
                .with_script_wall_time(Duration::from_millis(200))
                .with_page_wall_time(Duration::from_millis(500)),
        ),
    );
    let out = rt.block_on(
        limited.run_page_scripts(request(&state, "<body><script>for(;;){}</script></body>")),
    );
    match out {
        Ok(PageScriptOutcome::Executed(out)) => assert!(
            matches!(
                out.abort().map(|a| a.kind()),
                Some(AbortKind::WallTime { .. })
            ),
            "{:?}",
            out.abort()
        ),
        other => panic!("expected Executed, got {other:?}"),
    }
    wait_until("worker gone after abort", || {
        !limited.stats().worker_running()
            && thread_count() == base_threads
            && child_process_count() == base_children
    });
    let html = executed(rt.block_on(limited.run_page_scripts(request(&state, APPEND_SCRIPT))));
    assert!(html.contains("<p>from-actor</p>"), "{html}");
    assert_eq!(limited.stats().worker_spawns(), 2);

    eprintln!("case: JS-4 a newer generation cancels a run waiting on the network");
    let waiting = handle_with(
        engine,
        PageScriptActorOptions::default().with_fetch_options(
            page_script_fetch_options().with_allow_private_network_access(true),
        ),
    );
    let scope_state = NavigationState::new();
    let slow = waiting.run_page_scripts(request(
        &scope_state,
        &format!("<body><script src=\"http://127.0.0.1:{port}/slow.js\"></script></body>"),
    ));
    wait_until("server reached", || accepted.load(Ordering::SeqCst) >= 1);
    let fresh = waiting.run_page_scripts(request(&scope_state, APPEND_SCRIPT));
    let started = Instant::now();
    let (slow_result, fresh_result) = rt.block_on(async { tokio::join!(slow, fresh) });
    assert!(
        matches!(slow_result, Err(PageScriptError::Cancelled)),
        "{slow_result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancel must not wait for the fetch timeout: {:?}",
        started.elapsed()
    );
    let html = executed(fresh_result);
    assert!(html.contains("<p>from-actor</p>"), "{html}");
    wait_until("worker gone after cancel", || {
        !waiting.stats().worker_running() && child_process_count() == base_children
    });
}

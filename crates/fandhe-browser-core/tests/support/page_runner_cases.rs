//! V8 / boa 共通のページスクリプトランナーのテストケース（TASK-109・Issue #780・#781・
//! ビヘイビア `JS-4` / `JS-6` / `JS-8`）。
//!
//! `tests/page_runner_v8.rs` と `tests/page_runner_boa.rs` が `#[path]` で取り込む。
//! 期待値は具体値で書く。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use fandhe_browser_core::fetch::{FetchOptions, Fetcher};
use fandhe_browser_core::{
    AbortKind, Config, LifecycleOutcome, PageRunInput, PageRunOptions, PageRunOutput,
    ScriptOutcome, WallTimeScope, run_page_scripts,
};

const BASE: &str = "https://example.test/";

fn run_page(engine: &str, html: &str, options: &PageRunOptions) -> PageRunOutput {
    let fetcher = Fetcher::new(FetchOptions::new()).expect("fetcher");
    run_page_at(engine, html, BASE, options, &fetcher)
}

/// 現スレッドの tokio ランタイム上で async のランナーを実行する（`Fetcher` は tokio 前提）。
fn run_page_at(
    engine: &str,
    html: &str,
    base: &str,
    options: &PageRunOptions,
    fetcher: &Fetcher,
) -> PageRunOutput {
    let cfg = Config::from_toml_str(&format!("[js]\nengine = \"{engine}\"\n")).expect("config");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run_page_scripts(
        &PageRunInput::new(html, base),
        cfg.js(),
        options,
        fetcher,
    ))
    .expect("run")
}

/// `127.0.0.1` の空きポートで、どのパスにも `body` を返す最小サーバーを起動する。
fn spawn_script_server(body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 1024];
                let mut data = Vec::new();
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    port
}

/// どのパスにも `status` と `body` を返す最小サーバー（非 2xx の検査用）。
fn spawn_status_server(status: &'static str, body: String) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let body = body.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 1024];
                let mut data = Vec::new();
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    port
}

fn append(n: &str) -> String {
    format!(
        "<script>console.log('{n}');var p=document.createElement('p');p.textContent='{n}';\
         document.body.appendChild(p)</script>"
    )
}

/// 直下の子プロセス数（Linux のみ。`/proc/*/stat` の ppid を走査する）。
#[cfg(target_os = "linux")]
fn child_process_count() -> usize {
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

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。
pub fn run(engine: &str) {
    let opts = PageRunOptions::default();

    eprintln!("case: JS-4 document order + console order");
    let html = format!("<body>{}{}{}</body>", append("1"), append("2"), append("3"));
    let out = run_page(engine, &html, &opts);
    assert!(
        out.html().contains("<p>1</p><p>2</p><p>3</p>"),
        "{}",
        out.html()
    );
    let texts: Vec<&str> = out
        .bridge_diagnostics()
        .console_messages
        .iter()
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(texts, vec!["1", "2", "3"]);
    assert!(out.abort().is_none());

    eprintln!("case: JS-4 exception does not stop following scripts");
    let html = format!(
        "<body>{}<script>throw new Error('boom')</script>{}</body>",
        append("1"),
        append("3")
    );
    let out = run_page(engine, &html, &opts);
    assert!(out.html().contains("<p>1</p><p>3</p>"), "{}", out.html());
    match out.scripts()[1].outcome() {
        ScriptOutcome::Exception { message, truncated } => {
            assert!(message.contains("boom"), "{message}");
            assert!(!truncated);
        }
        other => panic!("unexpected: {other:?}"),
    }

    eprintln!("case: JS-4 huge exception message stays bounded");
    let out = run_page(
        engine,
        "<body><script>throw new Error('x'.repeat(10000))</script></body>",
        &opts,
    );
    match out.scripts()[0].outcome() {
        // エンジン側が先に 1024 文字へ切り詰めるため `truncated` は問わない（4 KiB の
        // 境界はランナーの単体テストで検証する）。
        ScriptOutcome::Exception { message, .. } => {
            assert!(message.len() <= 4096, "{}", message.len());
        }
        other => panic!("unexpected: {other:?}"),
    }

    eprintln!("case: JS-6 infinite loop is aborted by script wall time");
    let html = format!(
        "<body>{}<script>while(true){{}}</script>{}</body>",
        append("1"),
        append("3")
    );
    let o = PageRunOptions::default().with_script_wall_time(Duration::from_millis(200));
    let out = run_page(engine, &html, &o);
    assert!(out.html().contains("<p>1</p>"), "{}", out.html());
    assert!(!out.html().contains("<p>3</p>"), "{}", out.html());
    match out.scripts()[1].outcome() {
        ScriptOutcome::Aborted { kind, .. } => assert_eq!(
            kind,
            &AbortKind::WallTime {
                scope: WallTimeScope::Script,
                limit: Duration::from_millis(200)
            }
        ),
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(out.scripts()[2].outcome().as_str(), "not_executed");

    eprintln!("case: JS-6 page wall time");
    let o = PageRunOptions::default().with_page_wall_time(Duration::from_millis(300));
    let out = run_page(engine, "<body><script>while(true){}</script></body>", &o);
    let abort = out.abort().expect("abort");
    assert_eq!(
        abort.kind(),
        &AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit: Duration::from_millis(300)
        }
    );

    eprintln!("case: JS-6 globals do not leak between pages (also after abort)");
    let leak = "<body><script>var leaked = 1; globalThis.leaked2 = 2;</script></body>";
    let probe = "<body><script>document.body.setAttribute('data-x', typeof leaked + ',' + typeof leaked2)</script></body>";
    run_page(engine, leak, &opts);
    let out = run_page(engine, probe, &opts);
    assert!(
        out.html().contains("data-x=\"undefined,undefined\""),
        "{}",
        out.html()
    );
    let o = PageRunOptions::default().with_script_wall_time(Duration::from_millis(200));
    run_page(
        engine,
        "<body><script>var leaked = 1; globalThis.leaked2 = 2; while(true){}</script></body>",
        &o,
    );
    let out = run_page(engine, probe, &opts);
    assert!(
        out.html().contains("data-x=\"undefined,undefined\""),
        "{}",
        out.html()
    );

    eprintln!("case: JS-5 currentScript during execution");
    let body = "document.body.setAttribute('data-cs', document.currentScript.textContent)";
    let out = run_page(
        engine,
        &format!("<body><script>{body}</script></body>"),
        &opts,
    );
    assert!(
        out.html().contains(&format!("data-cs=\"{body}\"")),
        "{}",
        out.html()
    );

    eprintln!("case: JS-4 src from local server runs in document order");
    let port = spawn_script_server(
        "var p=document.createElement('p');p.textContent='ext';document.body.appendChild(p)",
    );
    let loopback =
        Fetcher::new(FetchOptions::new().with_allow_private_network_access(true)).expect("fetcher");
    let html = format!(
        "<body>{}<script src=\"/app.js\"></script>{}</body>",
        append("1"),
        append("3")
    );
    let out = run_page_at(
        engine,
        &html,
        &format!("http://127.0.0.1:{port}/"),
        &opts,
        &loopback,
    );
    assert!(
        out.html().contains("<p>1</p><p>ext</p><p>3</p>"),
        "{}",
        out.html()
    );
    // 3 件とも取得・実行まで進んでいる（src 取得失敗でも打ち切りでもない）。
    assert_eq!(out.scripts().len(), 3);
    assert!(out.abort().is_none());
    assert!(
        out.scripts()
            .iter()
            .all(|r| !matches!(r.outcome().as_str(), "src_fetch_failed" | "aborted"))
    );

    eprintln!("case: JS-6 large non-2xx src body is a per-script failure, not a page abort");
    let port = spawn_status_server("404 Not Found", "x".repeat(4096));
    let small = PageRunOptions::default()
        .with_max_src_script_bytes(256)
        .with_max_total_script_bytes(512);
    let html = format!(
        "<body><script src=\"/missing.js\"></script>{}</body>",
        append("after")
    );
    let out = run_page_at(
        engine,
        &html,
        &format!("http://127.0.0.1:{port}/"),
        &small,
        &loopback,
    );
    assert_eq!(out.scripts()[0].outcome().as_str(), "src_fetch_failed");
    assert!(out.abort().is_none(), "{:?}", out.abort());
    assert!(out.html().contains("<p>after</p>"), "{}", out.html());

    eprintln!("case: JS-8 disallowed src is skipped and the page continues");
    let out = run_page(
        engine,
        &format!(
            "<body><script src=\"file:///etc/passwd\"></script><script src=\"http://127.0.0.1:1/a.js\"></script>{}</body>",
            append("2")
        ),
        &opts,
    );
    assert!(out.html().contains("<p>2</p>"), "{}", out.html());
    assert_eq!(out.scripts()[0].outcome().as_str(), "src_fetch_failed");
    assert_eq!(out.scripts()[1].outcome().as_str(), "src_fetch_failed");

    eprintln!("case: JS-4 DOMContentLoaded / load order and readyState");
    let html = "<body><script>\
        console.log('s:' + document.readyState);\
        document.addEventListener('DOMContentLoaded', function () { console.log('dcl:' + document.readyState); });\
        window.addEventListener('load', function () { console.log('load:' + document.readyState); });\
        </script></body>";
    let out = run_page(engine, html, &opts);
    let texts: Vec<&str> = out
        .bridge_diagnostics()
        .console_messages
        .iter()
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(texts, vec!["s:loading", "dcl:interactive", "load:complete"]);
    assert_eq!(
        out.lifecycle()
            .iter()
            .map(|l| l.outcome().clone())
            .collect::<Vec<_>>(),
        vec![LifecycleOutcome::Dispatched, LifecycleOutcome::Dispatched]
    );

    eprintln!("case: JS-4 a throwing listener does not stop the next one");
    let html = "<body><script>\
        document.addEventListener('DOMContentLoaded', function () { throw new Error('bad'); });\
        window.addEventListener('DOMContentLoaded', function () { console.log('second'); });\
        </script></body>";
    let out = run_page(engine, html, &opts);
    assert_eq!(
        out.bridge_diagnostics()
            .console_messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["second"]
    );
    let errs = &out.bridge_diagnostics().listener_errors;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].message.contains("bad"), "{}", errs[0].message);

    eprintln!("case: JS-6 lifecycle is not fired after an abort");
    let o = PageRunOptions::default().with_script_wall_time(Duration::from_millis(200));
    let out = run_page(
        engine,
        "<body><script>window.addEventListener('load', function () { document.body.setAttribute('data-l', '1'); }); while(true){}</script></body>",
        &o,
    );
    assert!(!out.html().contains("data-l=\""), "{}", out.html());
    assert!(
        out.lifecycle()
            .iter()
            .all(|l| l.outcome().as_str() == "not_fired")
    );

    eprintln!("case: JS-6 listener limit in a script stops following scripts");
    let html = "<body><script>for (var i = 0; i < 1100; i++) { document.addEventListener('t' + i, function () {}); }</script>\
        <script>document.body.setAttribute('data-after', '1')</script></body>";
    let out = run_page(engine, html, &opts);
    assert!(!out.html().contains("data-after=\""), "{}", out.html());
    let abort = out.abort().expect("abort");
    assert_eq!(abort.kind(), &AbortKind::ResourceLimit);
    assert_eq!(abort.index(), Some(0));
    // 原因スクリプト自身の結果も Aborted / ResourceLimit になる。
    assert!(
        matches!(
            out.scripts()[0].outcome(),
            ScriptOutcome::Aborted {
                kind: AbortKind::ResourceLimit,
                ..
            }
        ),
        "{:?}",
        out.scripts()[0].outcome()
    );
    assert!(
        out.lifecycle()
            .iter()
            .all(|l| l.outcome().as_str() == "not_fired")
    );

    eprintln!("case: JS-6 listener limit inside DOMContentLoaded skips load");
    let html = "<body><script>\
        document.addEventListener('DOMContentLoaded', function () {\
            for (var i = 0; i < 1100; i++) { window.addEventListener('t' + i, function () {}); }\
        });\
        window.addEventListener('load', function () { document.body.setAttribute('data-load', '1'); });\
        </script></body>";
    let out = run_page(engine, html, &opts);
    assert!(!out.html().contains("data-load=\""), "{}", out.html());
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ResourceLimit)
    );
    assert_eq!(out.lifecycle()[1].outcome().as_str(), "not_fired");

    eprintln!("case: JS-4 tampered Array.prototype does not drop following listeners");
    let html = "<body><script>\
        document.addEventListener('DOMContentLoaded', function () {\
            Array.prototype.indexOf = function () { throw new Error('tampered'); };\
            Array.prototype.slice = function () { throw new Error('tampered'); };\
            Function.prototype.call = function () { throw new Error('tampered'); };\
        });\
        document.addEventListener('DOMContentLoaded', function () { console.log('second'); });\
        </script></body>";
    let out = run_page(engine, html, &opts);
    assert_eq!(
        out.bridge_diagnostics()
            .console_messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["second"]
    );

    eprintln!("case: JS-4 tampered push/splice/slice/species does not stop once listeners");
    let html = "<body><script>\
        Array.prototype.push = function () { throw new Error('tampered'); };\
        Array.prototype.splice = function () { throw new Error('tampered'); };\
        Array.prototype.slice = function () { throw new Error('tampered'); };\
        Object.defineProperty(Array, Symbol.species, { get: function () { throw new Error('tampered'); } });\
        document.addEventListener('DOMContentLoaded', function () { console.log('first'); }, { once: true });\
        document.addEventListener('DOMContentLoaded', function () { console.log('second'); });\
        window.addEventListener('load', function () { console.log('third'); }, { once: true });\
        </script></body>";
    let out = run_page(engine, html, &opts);
    assert_eq!(
        out.bridge_diagnostics()
            .console_messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );

    eprintln!(
        "case: JS-4 index setters on Array.prototype / Object.prototype do not drop listeners"
    );
    let html = "<body><script>\
        Object.defineProperty(Array.prototype, '0', { set: function () { throw new Error('tampered'); }, get: function () { return undefined; }, configurable: true });\
        Object.defineProperty(Object.prototype, '1', { set: function () { throw new Error('tampered'); }, get: function () { return undefined; }, configurable: true });\
        document.addEventListener('DOMContentLoaded', function () { console.log('first'); }, { once: true });\
        document.addEventListener('DOMContentLoaded', function () { console.log('second'); });\
        window.addEventListener('load', function () { console.log('third'); });\
        </script></body>";
    let out = run_page(engine, html, &opts);
    assert_eq!(
        out.bridge_diagnostics()
            .console_messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );

    eprintln!("case: JS-4 polluted Object.prototype descriptor fields do not drop listeners");
    let html = "<body><script>\
        document.addEventListener('DOMContentLoaded', function () { console.log('first'); });\
        window.addEventListener('load', function () { console.log('second'); });\
        Object.prototype.value = 1;\
        Object.prototype.writable = true;\
        </script></body>";
    let out = run_page(engine, html, &opts);
    assert_eq!(
        out.bridge_diagnostics()
            .console_messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );

    #[cfg(target_os = "linux")]
    {
        eprintln!("case: JS-6 no child process remains");
        assert_eq!(child_process_count(), 0);
    }
}

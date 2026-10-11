//! V8 / boa 共通のページスクリプトランナーのテストケース（TASK-109・Issue #780・#781・
//! ビヘイビア `JS-4` / `JS-6` / `JS-8`。タイマー・microtask は TASK-110・Issue #782・`JS-7`）。
//!
//! `tests/page_runner_v8.rs` と `tests/page_runner_boa.rs` が `#[path]` で取り込む。
//! 期待値は具体値で書く。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use std::sync::{Arc, Mutex};

use fandhe_browser_core::dom_bridge::TimerCallbackSource;
use fandhe_browser_core::fetch::{FetchOptions, Fetcher};
use fandhe_browser_core::{
    AbortKind, Config, EventLoopLimits, LifecycleOutcome, NetRejectKind, NetRequestKind,
    PageNetLimits, PageRunInput, PageRunOptions, PageRunOutput, ScriptOutcome, USER_AGENT,
    WallTimeScope, run_page_scripts,
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

/// タイマー・microtask の共通ケース（`JS-7`・`JS-6`）。
fn timer_cases(engine: &str) {
    // 本文へ `<p>` を足すヘルパーを各ページの先頭に置く。
    let page = |script: &str| {
        format!(
            "<body><script>function a(t){{var p=document.createElement('p');\
             p.textContent=t;document.body.appendChild(p)}}{script}</script></body>"
        )
    };
    let opts = PageRunOptions::default();

    eprintln!("case: JS-7 timers fire in deadline order on a virtual clock");
    let out = run_page(
        engine,
        &page("setTimeout(function(){a('100')},100);setTimeout(function(){a('50')},50);"),
        &opts,
    );
    assert!(out.html().contains("<p>50</p><p>100</p>"), "{}", out.html());
    assert!(out.abort().is_none(), "{:?}", out.abort());
    assert_eq!(out.event_loop().fired(), 2);
    assert_eq!(
        out.event_loop().virtual_elapsed(),
        Duration::from_millis(100)
    );
    assert_eq!(out.event_loop().pending_at_end(), 0);

    eprintln!("case: JS-7 a 29s timer does not wait in real time");
    let out = run_page(
        engine,
        &page("setTimeout(function(){a('late')},29000);"),
        &opts,
    );
    assert!(out.abort().is_none(), "{:?}", out.abort());
    assert!(out.html().contains("<p>late</p>"), "{}", out.html());
    assert_eq!(out.event_loop().virtual_elapsed(), Duration::from_secs(29));
    assert!(
        out.elapsed() < Duration::from_secs(5),
        "{:?}",
        out.elapsed()
    );

    eprintln!("case: JS-7 microtask runs before the timer of the same task");
    let out = run_page(
        engine,
        &page("setTimeout(function(){a('t')},0);queueMicrotask(function(){a('m')});a('s');"),
        &opts,
    );
    assert!(
        out.html().contains("<p>s</p><p>m</p><p>t</p>"),
        "{}",
        out.html()
    );

    eprintln!("case: JS-7 clearTimeout / clearInterval cancel timers");
    let out = run_page(
        engine,
        &page(
            "var x=setTimeout(function(){a('x')},10);clearTimeout(x);\
             setTimeout(function(){a('y')},20);\
             var n=0;var i=setInterval(function(){n++;a('i'+n);if(n==3){clearInterval(i)}},5);",
        ),
        &opts,
    );
    assert!(!out.html().contains("<p>x</p>"), "{}", out.html());
    assert!(
        out.html().contains("<p>i1</p><p>i2</p><p>i3</p><p>y</p>"),
        "{}",
        out.html()
    );
    assert!(!out.html().contains("<p>i4</p>"), "{}", out.html());
    assert_eq!(out.event_loop().fired(), 4);
    assert!(out.abort().is_none(), "{:?}", out.abort());

    eprintln!("case: JS-6 runaway setInterval(0) and recursive setTimeout are bounded");
    let small = PageRunOptions::default()
        .with_event_loop_limits(EventLoopLimits::default().with_max_timer_fires(50));
    let out = run_page(
        engine,
        &page(
            "var n=0;setInterval(function(){n++;document.body.setAttribute('data-n',String(n))},0);",
        ),
        &small,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::TimerFires { limit: 50 })
    );
    assert!(out.html().contains("data-n=\"50\""), "{}", out.html());
    let out = run_page(
        engine,
        &page(
            "var n=0;function r(){n++;document.body.setAttribute('data-n',String(n));\
             setTimeout(r,0)}r();",
        ),
        &small,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::TimerFires { limit: 50 })
    );
    assert!(out.html().contains("data-n=\"51\""), "{}", out.html());

    eprintln!("case: JS-6 registered timers limit aborts the page");
    let few = PageRunOptions::default()
        .with_event_loop_limits(EventLoopLimits::default().with_max_registered_timers(5));
    let out = run_page(
        engine,
        &page("for(var i=0;i<6;i++){setTimeout(function(){a('z')},1000)}"),
        &few,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::RegisteredTimers { limit: 5 })
    );
    assert_eq!(out.event_loop().fired(), 0);

    eprintln!("case: JS-6 direct timerClear loop keeps registration and sweeps shim retention");
    let out = run_page(
        engine,
        &page(
            "for(var i=0;i<1000;i++){var id=setTimeout(function(){a('x')},100);\
             __dom.op('timerClear',id)}\
             setTimeout(function(){a('ok')},1);",
        ),
        &few,
    );
    assert_eq!(out.abort().map(|a| a.kind().clone()), None);
    assert_eq!(out.event_loop().fired(), 1);
    assert!(out.html().contains("ok"), "{}", out.html());

    eprintln!("case: JS-6 op budget exhausted before a timer fires aborts with ResourceLimit");
    let tight = PageRunOptions::default().with_bridge_limits(
        fandhe_browser_core::DomBridgeLimits::default().with_max_ops_per_page(6),
    );
    let out = run_page(
        engine,
        &page(
            "setTimeout(function(){a('late')},1);for(var k=0;k<20;k++){try{document.body}catch(e){}}",
        ),
        &tight,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ResourceLimit)
    );
    assert!(!out.html().contains("<p>late</p>"), "{}", out.html());

    eprintln!("case: JS-6 op budget exhausted inside a microtask aborts with ResourceLimit");
    let out = run_page(
        engine,
        &page("queueMicrotask(function(){for(var k=0;k<20;k++){try{document.body}catch(e){}}});"),
        &tight,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ResourceLimit)
    );

    eprintln!("case: JS-7 untrusted delays are normalized");
    let out = run_page(
        engine,
        &page(
            "setTimeout(function(){a('neg')},-5);setTimeout(function(){a('nan')},NaN);\
             setTimeout(function(){a('str')},'abc');setTimeout(function(){a('inf')},Infinity);",
        ),
        &opts,
    );
    assert!(
        out.html().contains("<p>neg</p><p>nan</p><p>str</p>"),
        "{}",
        out.html()
    );
    assert!(!out.html().contains("<p>inf</p>"), "{}", out.html());
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::VirtualTime {
            limit: Duration::from_secs(30)
        })
    );
    assert_eq!(out.event_loop().fired(), 3);

    eprintln!("case: JS-7 callback exceptions are aggregated and later timers continue");
    let out = run_page(
        engine,
        &page(
            "setTimeout(function(){a('1')},1);setTimeout(function(){throw new Error('boom')},2);\
             setTimeout(function(){a('3')},3);",
        ),
        &opts,
    );
    assert!(out.html().contains("<p>1</p><p>3</p>"), "{}", out.html());
    let errors = &out.bridge_diagnostics().timer_errors;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].source, TimerCallbackSource::Timeout);
    assert_eq!(errors[0].message, "boom");
    assert!(out.abort().is_none(), "{:?}", out.abort());

    eprintln!("case: JS-6 page cannot invoke the timer dispatcher directly to bypass limits");
    let out = run_page(
        engine,
        &page(
            "var n=0;var id=setInterval(function(){n++},40000);\
             for(var i=0;i<2000;i++){__fandheRunTimer(id)}a('n'+n);",
        ),
        &opts,
    );
    assert!(out.html().contains("<p>n0</p>"), "{}", out.html());
    assert_eq!(out.event_loop().fired(), 0);

    eprintln!("case: JS-6 pending microtasks are capped and abort the page");
    let out = run_page(
        engine,
        &page("for(var i=0;i<20000;i++){queueMicrotask(function(){})}"),
        &opts,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ResourceLimit)
    );

    eprintln!("case: JS-7 string handlers are not evaluated");
    let out = run_page(
        engine,
        &page("setTimeout(\"document.body.setAttribute('data-x','1')\",0);"),
        &opts,
    );
    assert!(!out.html().contains("data-x=\"1\""), "{}", out.html());
    assert_eq!(out.bridge_diagnostics().timer_string_handlers_ignored, 1);
    assert!(out.abort().is_none(), "{:?}", out.abort());

    eprintln!(
        "case: JS-7 extra arguments, TypeError for non-function microtask, locked dispatcher"
    );
    let out = run_page(
        engine,
        &page(
            "setTimeout(function(x,y){a(x+y)},0,'a','b');\
             try{queueMicrotask(1)}catch(e){a(e.name)}\
             try{Object.defineProperty(globalThis,'__fandheRunTimer',{value:1})}catch(e){a('locked')}",
        ),
        &opts,
    );
    assert!(
        out.html()
            .contains("<p>TypeError</p><p>locked</p><p>ab</p>"),
        "{}",
        out.html()
    );
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
/// 受信した要求の先頭行とヘッダを記録する最小サーバー（到達回数・ヘッダの検査用）。
/// `respond` はパスから `(status 行, 追加ヘッダ, 本文)` を返す。
type Hits = Arc<Mutex<Vec<String>>>;

fn spawn_recording_server(
    respond: impl Fn(&str) -> (&'static str, String, String) + Send + Sync + 'static,
) -> (u16, Hits) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let hits: Hits = Arc::new(Mutex::new(Vec::new()));
    let respond = Arc::new(respond);
    let recorded = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (respond, recorded) = (respond.clone(), recorded.clone());
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
                let head = String::from_utf8_lossy(&data).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                if let Ok(mut r) = recorded.lock() {
                    r.push(head);
                }
                let (status, extra, body) = respond(&path);
                let resp = format!(
                    "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    (port, hits)
}

fn hit_count(hits: &Hits) -> usize {
    hits.lock().map(|h| h.len()).unwrap_or(usize::MAX)
}

fn loopback_fetcher() -> Fetcher {
    Fetcher::new(FetchOptions::new().with_allow_private_network_access(true)).expect("fetcher")
}

fn rejections(out: &PageRunOutput) -> Vec<(NetRequestKind, NetRejectKind)> {
    out.bridge_diagnostics()
        .net_rejections
        .iter()
        .map(|r| (r.request, r.kind))
        .collect()
}

/// XHR の本文を `<p>` として DOM に書くページ（`onload` と `onerror` で別の目印を出す）。
fn xhr_page(url: &str) -> String {
    format!(
        "<body><script>function a(t){{var p=document.createElement('p');p.textContent=t;\
         document.body.appendChild(p)}}\
         var x=new XMLHttpRequest();x.open('GET','{url}');\
         x.onload=function(){{a('load:'+x.status+':'+x.responseText)}};\
         x.onerror=function(){{a('error:'+x.status)}};x.send();</script></body>"
    )
}

/// ページ内 `XMLHttpRequest` の共通ケース（両エンジン。`JS-8`・`JS-6`・`SEC-2`）。
fn net_cases(engine: &str) {
    let opts = PageRunOptions::default();

    eprintln!("case: JS-8 XHR GET resolves against the page URL and sees status + body");
    let (port, hits) = spawn_recording_server(|path| match path {
        "/dir/data.txt" => ("200 OK", String::new(), "hello".to_string()),
        _ => ("404 Not Found", String::new(), "nope".to_string()),
    });
    let base = format!("http://127.0.0.1:{port}/dir/page.html");
    let out = run_page_at(
        engine,
        &xhr_page("data.txt"),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(
        out.html().contains("<p>load:200:hello</p>"),
        "{}",
        out.html()
    );
    assert!(out.abort().is_none(), "{:?}", out.abort());
    assert_eq!(hit_count(&hits), 1);
    assert_eq!(out.net().started(), 1);
    assert_eq!(out.net().completed(), 1);
    assert_eq!(out.net().response_bytes(), 5);
    assert_eq!(out.net().pending_at_end(), 0);
    assert!(rejections(&out).is_empty());

    eprintln!("case: JS-8 a fire-and-forget fetch is accepted and delivered on both engines");
    let out = run_page_at(
        engine,
        "<body><script>fetch('data.txt')</script></body>",
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert_eq!(hit_count(&hits), 2);
    assert_eq!(out.net().started(), 1);
    assert_eq!(out.net().completed(), 1);
    assert_eq!(out.net().response_bytes(), 5);

    eprintln!("case: JS-8 XHR sends no Cookie / Authorization and keeps the honest User-Agent");
    {
        let recorded = hits.lock().expect("hits");
        let head = recorded.first().expect("one request").to_ascii_lowercase();
        assert!(head.starts_with("get /dir/data.txt "), "{head}");
        assert!(!head.contains("cookie:"), "{head}");
        assert!(!head.contains("authorization:"), "{head}");
        assert!(
            head.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())),
            "{head}"
        );
    }

    eprintln!("case: JS-8 XHR calls onload with status 404 for an HTTP error");
    let out = run_page_at(
        engine,
        &xhr_page("missing.txt"),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(
        out.html().contains("<p>load:404:nope</p>"),
        "{}",
        out.html()
    );

    eprintln!("case: JS-8 XHR rejects non-GET, request bodies and synchronous mode");
    let (port, hits) = spawn_recording_server(|_| ("200 OK", String::new(), "x".to_string()));
    let base = format!("http://127.0.0.1:{port}/page.html");
    let html = "<body><script>function a(t){var p=document.createElement('p');p.textContent=t;\
        document.body.appendChild(p)}\
        var x=new XMLHttpRequest();x.open('POST','a');x.onerror=function(){a('e1')};x.send();\
        var y=new XMLHttpRequest();y.open('GET','a');y.onerror=function(){a('e2')};y.send('body');\
        var z=new XMLHttpRequest();try{z.open('GET','a',false)}catch(e){a('sync-throws')}\
        </script></body>";
    let out = run_page_at(engine, html, &base, &opts, &loopback_fetcher());
    assert!(
        out.html().contains("<p>sync-throws</p><p>e1</p><p>e2</p>"),
        "{}",
        out.html()
    );
    assert_eq!(
        rejections(&out),
        vec![
            (NetRequestKind::Xhr, NetRejectKind::UnsupportedMethod),
            (NetRequestKind::Xhr, NetRejectKind::RequestBody),
            (NetRequestKind::Xhr, NetRejectKind::SyncXhr),
        ]
    );
    assert_eq!(hit_count(&hits), 0);
    assert_eq!(out.net().started(), 0);

    eprintln!("case: JS-8 SSRF: internal addresses are refused without touching the network");
    let (port, hits) = spawn_recording_server(|_| ("200 OK", String::new(), "secret".to_string()));
    // 既定の Fetcher（内部アドレス不許可）。ページ自体は公開名の URL。
    let fetcher = Fetcher::new(FetchOptions::new()).expect("fetcher");
    let targets = [
        (
            format!("http://127.0.0.1:{port}/x?secret=1"),
            NetRejectKind::DisallowedAddress,
        ),
        (
            "http://10.0.0.1/".to_string(),
            NetRejectKind::DisallowedAddress,
        ),
        (
            "http://[::1]/".to_string(),
            NetRejectKind::DisallowedAddress,
        ),
        (
            "http://169.254.169.254/latest".to_string(),
            NetRejectKind::DisallowedAddress,
        ),
        ("file:///etc/passwd".to_string(), NetRejectKind::InvalidUrl),
        ("ftp://example.test/".to_string(), NetRejectKind::InvalidUrl),
    ];
    for (url, want) in targets {
        let out = run_page_at(
            engine,
            &xhr_page(&url),
            "https://example.test/",
            &opts,
            &fetcher,
        );
        assert!(
            out.html().contains("<p>error:0</p>"),
            "{url}: {}",
            out.html()
        );
        assert_eq!(rejections(&out), vec![(NetRequestKind::Xhr, want)], "{url}");
        // 診断に URL・クエリ・パスを残さない。
        let dump = format!("{:?}", out.bridge_diagnostics());
        for banned in ["127.0.0.1", "secret", "passwd", "169.254"] {
            assert!(!dump.contains(banned), "{banned} in {dump}");
        }
    }
    assert_eq!(hit_count(&hits), 0);

    eprintln!("case: JS-8 dangerous redirects are refused by the Fetcher");
    let (port, hits) = spawn_recording_server(|path| match path {
        "/to-file" => (
            "302 Found",
            "Location: file:///etc/passwd\r\n".to_string(),
            String::new(),
        ),
        _ => (
            "302 Found",
            "Location: ftp://example.test/x\r\n".to_string(),
            String::new(),
        ),
    });
    let base = format!("http://127.0.0.1:{port}/page.html");
    for path in ["to-file", "to-ftp"] {
        let out = run_page_at(engine, &xhr_page(path), &base, &opts, &loopback_fetcher());
        assert!(
            out.html().contains("<p>error:0</p>"),
            "{path}: {}",
            out.html()
        );
        assert_eq!(
            rejections(&out),
            vec![(NetRequestKind::Xhr, NetRejectKind::DisallowedScheme)],
            "{path}"
        );
    }
    assert_eq!(hit_count(&hits), 2);

    eprintln!("case: JS-6 request count: the Nth request is accepted, the N+1th rejected");
    let (port, hits) = spawn_recording_server(|_| ("200 OK", String::new(), "ok".to_string()));
    let base = format!("http://127.0.0.1:{port}/page.html");
    let html = "<body><script>function a(t){var p=document.createElement('p');p.textContent=t;\
        document.body.appendChild(p)}\
        for(var i=0;i<3;i++){(function(n){var x=new XMLHttpRequest();x.open('GET','r'+n);\
        x.onload=function(){a('ok'+n)};x.onerror=function(){a('err'+n)};x.send()})(i)}\
        </script></body>";
    let limited =
        PageRunOptions::default().with_net_limits(PageNetLimits::default().with_max_requests(2));
    let out = run_page_at(engine, html, &base, &limited, &loopback_fetcher());
    assert!(out.html().contains("<p>err2</p>"), "{}", out.html());
    assert!(
        out.html().contains("<p>ok0</p><p>ok1</p>"),
        "{}",
        out.html()
    );
    assert_eq!(
        rejections(&out),
        vec![(NetRequestKind::Xhr, NetRejectKind::RequestLimit)]
    );
    assert_eq!(hit_count(&hits), 2);
    assert_eq!(out.net().started(), 2);
    assert_eq!(PageNetLimits::default().max_requests(), 50);

    eprintln!(
        "case: JS-6 cumulative bytes: exactly the limit is accepted, then requests are rejected"
    );
    let (port, hits) =
        spawn_recording_server(|_| ("200 OK", String::new(), "0123456789".to_string()));
    let base = format!("http://127.0.0.1:{port}/page.html");
    let limited = PageRunOptions::default()
        .with_net_limits(PageNetLimits::default().with_max_response_bytes(10));
    let html = "<body><script>function a(t){var p=document.createElement('p');p.textContent=t;\
        document.body.appendChild(p)}\
        for(var i=0;i<2;i++){(function(n){var x=new XMLHttpRequest();x.open('GET','r'+n);\
        x.onload=function(){a('ok'+n)};x.onerror=function(){a('err'+n)};x.send()})(i)}\
        </script></body>";
    let out = run_page_at(engine, html, &base, &limited, &loopback_fetcher());
    assert!(
        out.html().contains("<p>ok0</p><p>err1</p>"),
        "{}",
        out.html()
    );
    assert_eq!(out.net().response_bytes(), 10);
    assert!(out.bridge_diagnostics().net_limit_exceeded);
    assert_eq!(
        rejections(&out),
        vec![(NetRequestKind::Xhr, NetRejectKind::ResponseBytesLimit)]
    );
    // 2 件目は上限到達後なのでネットワークへ出ない。
    assert_eq!(hit_count(&hits), 1);

    eprintln!("case: JS-6 a response over the remaining bytes is rejected");
    let limited = PageRunOptions::default()
        .with_net_limits(PageNetLimits::default().with_max_response_bytes(9));
    let out = run_page_at(engine, &xhr_page("r"), &base, &limited, &loopback_fetcher());
    assert!(out.html().contains("<p>error:0</p>"), "{}", out.html());
    assert_eq!(
        rejections(&out),
        vec![(NetRequestKind::Xhr, NetRejectKind::ResponseBytesLimit)]
    );
    assert_eq!(out.net().response_bytes(), 0);
}

/// ページ内 `fetch`（ネイティブ Promise）のケース。
///
/// boa は `evaluate_script` の後に Promise ジョブを drain しないため `.then` の続きが走らない
/// （js crate の制約。TASK-30）。そのため V8 のテストだけが呼ぶ。
#[allow(dead_code)]
pub fn run_fetch_promises(engine: &str) {
    let opts = PageRunOptions::default();
    let (port, hits) = spawn_recording_server(|path| match path {
        "/dir/data.json" => (
            "200 OK",
            String::new(),
            "{\"name\":\"fandhe\",\"n\":3}".to_string(),
        ),
        "/dir/bad.json" => ("200 OK", String::new(), "{oops".to_string()),
        _ => ("404 Not Found", String::new(), "nope".to_string()),
    });
    let base = format!("http://127.0.0.1:{port}/dir/page.html");
    let helper = "function a(t){var p=document.createElement('p');p.textContent=t;\
        document.body.appendChild(p)}";
    let page = |script: &str| format!("<body><script>{helper}{script}</script></body>");

    eprintln!("case: JS-8 fetch().then(json) reaches the DOM with status / ok");
    let out = run_page_at(
        engine,
        &page(
            "fetch('data.json').then(function(r){a('s'+r.status+':'+r.ok);return r.json()})\
             .then(function(o){a(o.name+o.n)})",
        ),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(
        out.html().contains("<p>s200:true</p><p>fandhe3</p>"),
        "{}",
        out.html()
    );
    assert_eq!(out.net().completed(), 1);
    assert_eq!(hit_count(&hits), 1);

    eprintln!("case: JS-8 fetch 404 resolves with ok=false");
    let out = run_page_at(
        engine,
        &page("fetch('zzz').then(function(r){a('s'+r.status+':'+r.ok);return r.text()}).then(a)"),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(
        out.html().contains("<p>s404:false</p><p>nope</p>"),
        "{}",
        out.html()
    );

    eprintln!("case: JS-8 text() twice and invalid JSON reject");
    let out = run_page_at(
        engine,
        &page(
            "fetch('data.json').then(function(r){return r.text().then(function(){return r.text()})})\
             .catch(function(e){a('twice:'+e.name)});\
             fetch('bad.json').then(function(r){return r.json()}).catch(function(e){a('badjson')})",
        ),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(
        out.html().contains("<p>twice:TypeError</p>"),
        "{}",
        out.html()
    );
    assert!(out.html().contains("<p>badjson</p>"), "{}", out.html());

    eprintln!("case: JS-8 fetch rejects POST / body / unresolvable URL and records it");
    let before = hit_count(&hits);
    let out = run_page_at(
        engine,
        &page(
            "fetch('a',{method:'POST'}).catch(function(){a('post')});\
             fetch('a',{body:'x'}).catch(function(){a('body')});\
             fetch('http://[bad').catch(function(){a('url')})",
        ),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(
        out.html().contains("<p>post</p><p>body</p><p>url</p>"),
        "{}",
        out.html()
    );
    assert_eq!(
        rejections(&out),
        vec![
            (NetRequestKind::Fetch, NetRejectKind::UnsupportedMethod),
            (NetRequestKind::Fetch, NetRejectKind::RequestBody),
            (NetRequestKind::Fetch, NetRejectKind::InvalidUrl),
        ]
    );
    assert_eq!(hit_count(&hits), before);

    eprintln!("case: JS-8 fetch never sends headers / credentials from init");
    let out = run_page_at(
        engine,
        &page(
            "fetch('data.json',{headers:{Authorization:'x'},credentials:'include'})\
             .then(function(r){a('ok'+r.status)})",
        ),
        &base,
        &opts,
        &loopback_fetcher(),
    );
    assert!(out.html().contains("<p>ok200</p>"), "{}", out.html());
    assert_eq!(out.bridge_diagnostics().net_options_ignored, 2);
    let recorded = hits.lock().expect("hits");
    let head = recorded.last().expect("request").to_ascii_lowercase();
    assert!(!head.contains("authorization:"), "{head}");
    assert!(!head.contains("cookie:"), "{head}");
    drop(recorded);

    eprintln!("case: JS-8 fetch to an internal address rejects without reaching the server");
    let fetcher = Fetcher::new(FetchOptions::new()).expect("fetcher");
    let before = hit_count(&hits);
    let out = run_page_at(
        engine,
        &page(&format!(
            "fetch('http://127.0.0.1:{port}/dir/data.json').catch(function(e){{a('blocked:'+e.name)}})"
        )),
        "https://example.test/",
        &opts,
        &fetcher,
    );
    assert!(
        out.html().contains("<p>blocked:TypeError</p>"),
        "{}",
        out.html()
    );
    assert_eq!(hit_count(&hits), before);
}
pub fn run(engine: &str) {
    let opts = PageRunOptions::default();
    timer_cases(engine);
    net_cases(engine);

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

    eprintln!("case: JS-6 listener limit is recorded even after the op budget is exhausted");
    let tight = PageRunOptions::default().with_bridge_limits(
        fandhe_browser_core::DomBridgeLimits::default().with_max_ops_per_page(200),
    );
    let html = "<body><script>\
        for (var j = 0; j < 400; j++) { try { document.body; } catch (e) {} }\
        for (var i = 0; i < 1100; i++) { document.addEventListener('t' + i, function () {}); }\
        </script><script>document.body.setAttribute('data-after', '1')</script></body>";
    let out = run_page(engine, html, &tight);
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ResourceLimit)
    );
    assert!(!out.html().contains("data-after=\""), "{}", out.html());

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

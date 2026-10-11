//! V8 / boa 共通の「ページ JS 実行の結合テストと上限検証」ケース（TASK-113・Issue #786・
//! ビヘイビア `JS-4`・`JS-5`・`JS-6`・`JS-7`・`JS-8`・`JS-1`・`PERF-7`）。
//!
//! `tests/page_js_limits_v8.rs` と `tests/page_js_limits_boa.rs` が `#[path]` で取り込む。
//! 外部ネットワークは使わず、`127.0.0.1` のローカル HTTP フィクスチャだけを相手にする。
//!
//! # 二層で検証する
//!
//! - **CDP 層（unix のみ）**: `test_support::LoopbackDispatcher` で `Page.navigate` → `DOM.*` を
//!   実行し、イベント列・DOM・所要時間・フィクスチャの到達数を検証する。`Profile::open` が非 unix で
//!   `Unsupported` のため `AppState` を構築できず、Windows では実行できない（任意の skip ではない。
//!   `windows_profile_is_unsupported` が `Unsupported` を確認し、対応が入った時点で落ちて有効化を促す）。
//! - **アクター層（全 OS）**: 同じフィクスチャ HTML・同じハンドル構成で `PageScriptHandle` を直接
//!   呼び、CDP 経路が捨てる診断（`PageRunOutput` の打ち切り種別・スクリプト別結果・
//!   `net_rejections`）を具体値で検証する。
//!
//! # 未検証の経路
//!
//! 「許可された初回ホップ → 内部アドレスへのリダイレクト」を `src` で拒否する経路は、Fetcher の
//! 内部アドレス許可が全許可 / 全拒否の 2 値のためオフラインでは再現できない（core `tests/fetch.rs`
//! 冒頭に既知の未検証経路として記載済み）。ここでは初回ホップ自体の拒否（既定ハンドル）と、
//! XHR / fetch / `src` のリダイレクト先に接続が出ないこと（許可ハンドル）までを検証する。
//!
//! 期待値は具体値で書く。ケースは時間アサーションの安定を優先して直列に実行する。

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fandhe_browser_core::{
    AbortKind, Config, NavigationState, NetRejectKind, NetRequestKind, PageRunOptions,
    PageRunOutput, PageRunRequest, PageScriptActorOptions, PageScriptError, PageScriptExecutor,
    PageScriptHandle, PageScriptOutcome, PageScriptSkipReason, ScriptDiagnosticKind, ScriptOutcome,
    SrcFetchErrorKind, WallTimeScope, page_script_fetch_options,
};

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

// ---------------------------------------------------------------------------
// フィクスチャサーバー
// ---------------------------------------------------------------------------

/// 受けたリクエストのパス一覧（到達回数の検査用）。
type Hits = Arc<Mutex<Vec<String>>>;

/// 応答 1 件: (ステータス行, 追加ヘッダ（各行 CRLF 終端）, 本文)。
type Reply = (&'static str, String, String);

/// `127.0.0.1` の空きポートで待ち受け、リクエストのパスを記録して `respond(path)` を返す。
/// ヘッダ読み取りは 8 KiB で打ち切る。応答は `Connection: close`。
fn serve(respond: impl Fn(&str) -> Reply + Send + Sync + 'static) -> (u16, Hits) {
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
                let mut data = Vec::new();
                let mut buf = [0u8; 1024];
                while data.len() < 8 * 1024 {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => data.extend_from_slice(buf.get(..n).unwrap_or(&[])),
                    }
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&data).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                if let Ok(mut r) = recorded.lock() {
                    r.push(path.clone());
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

fn hit_paths(hits: &Hits) -> Vec<String> {
    hits.lock().map(|h| h.clone()).unwrap_or_default()
}

fn hit_count(hits: &Hits) -> usize {
    hits.lock().map(|h| h.len()).unwrap_or(usize::MAX)
}

fn html_reply(body: &str) -> Reply {
    (
        "200 OK",
        "Content-Type: text/html\r\n".to_owned(),
        body.to_owned(),
    )
}

fn js_reply(body: &str) -> Reply {
    (
        "200 OK",
        "Content-Type: application/javascript\r\n".to_owned(),
        body.to_owned(),
    )
}

fn not_found() -> Reply {
    ("404 Not Found", String::new(), String::new())
}

/// `routes` の（パス, 種別, 本文）に従うサーバー。種別: `"html"` / `"js"`。
fn serve_routes(routes: Vec<(&'static str, &'static str, String)>) -> (u16, Hits) {
    serve(move |path| {
        routes
            .iter()
            .find(|(p, _, _)| *p == path)
            .map_or_else(not_found, |(_, kind, body)| {
                if *kind == "js" {
                    js_reply(body)
                } else {
                    html_reply(body)
                }
            })
    })
}

// ---------------------------------------------------------------------------
// ハンドル・アクター層の実行
// ---------------------------------------------------------------------------

/// `engine` で有効なハンドル。`allow_loopback` ならページスクリプト用 Fetcher がループバックを
/// 許可する（本番既定は拒否。ローカルフィクスチャから `src` を取るためのテスト専用設定）。
fn handle(engine: &str, allow_loopback: bool) -> PageScriptHandle {
    handle_with_run(engine, allow_loopback, PageRunOptions::default())
}

/// [`handle`] に実行オプションを渡せる版。
fn handle_with_run(engine: &str, allow_loopback: bool, run: PageRunOptions) -> PageScriptHandle {
    let cfg = Config::from_toml_str(&format!(
        "[js]\nengine = \"{engine}\"\nexecute_page_scripts = true\n"
    ))
    .expect("config");
    let mut options = PageScriptActorOptions::default().with_run_options(run);
    if allow_loopback {
        options = options.with_fetch_options(
            page_script_fetch_options().with_allow_private_network_access(true),
        );
    }
    PageScriptHandle::with_options(cfg.js(), Arc::new(TokioExecutor), options)
}

/// タイマー上限（1000 回）を実時間上限と切り離して検証するためのハンドル。V8 は発火ごとに
/// 子プロセスと往復するため、負荷の高い環境では既定の 5 秒に先に当たる。実時間上限だけを緩め、
/// 件数上限（`TimerFires`）は既定のままにする。
fn timer_handle(engine: &str) -> PageScriptHandle {
    handle_with_run(
        engine,
        false,
        PageRunOptions::default()
            .with_script_wall_time(Duration::from_secs(120))
            .with_page_wall_time(Duration::from_secs(120)),
    )
}

fn actor_run(
    rt: &tokio::runtime::Runtime,
    h: &PageScriptHandle,
    html: &str,
    url: &str,
) -> Result<PageScriptOutcome, PageScriptError> {
    let state = NavigationState::new();
    let generation = state.begin_navigation().expect("generation");
    let request = PageRunRequest::new(Arc::from(html), url, state.scope(), generation);
    rt.block_on(h.run_page_scripts(request))
}

fn executed(result: Result<PageScriptOutcome, PageScriptError>) -> Box<PageRunOutput> {
    match result {
        Ok(PageScriptOutcome::Executed(out)) => out,
        other => panic!("expected Executed, got {other:?}"),
    }
}

fn rejections(out: &PageRunOutput) -> Vec<(NetRequestKind, NetRejectKind)> {
    out.bridge_diagnostics()
        .net_rejections
        .iter()
        .map(|r| (r.request, r.kind))
        .collect()
}

fn outcomes(out: &PageRunOutput) -> Vec<&ScriptOutcome> {
    out.scripts().iter().map(|r| r.outcome()).collect()
}

/// 直下の子プロセス数（Linux のみ。`/proc/*/stat` の ppid を走査する。他 OS は 0 固定）。
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

// ---------------------------------------------------------------------------
// フィクスチャのページ
// ---------------------------------------------------------------------------

const SPA_HTML: &str =
    "<html><body><div id=\"app\"></div><script src=\"/app.js\"></script></body></html>";
const SPA_JS: &str = "var app=document.getElementById('app');\
    var h=document.createElement('h1');h.textContent='Hello SPA';app.appendChild(h);\
    for(var i=1;i<=3;i++){var li=document.createElement('li');li.textContent='item-'+i;app.appendChild(li)}void 0";

const FORM_HTML: &str = "<html><body><script src=\"/form.js\"></script></body></html>";
/// `form` と、`name` 付き input / textarea / select 各 1 個・`name` なし input 1 個を生成する。
const FORM_JS: &str = "var f=document.createElement('form');f.setAttribute('id','signup');\
    var a=document.createElement('input');a.setAttribute('name','email');f.appendChild(a);\
    var b=document.createElement('textarea');b.setAttribute('name','note');f.appendChild(b);\
    var c=document.createElement('select');c.setAttribute('name','plan');f.appendChild(c);\
    var d=document.createElement('input');f.appendChild(d);\
    document.body.appendChild(f);void 0";

const EXCEPTION_HTML: &str = "<html><body>\
    <script>document.body.setAttribute('data-a','1')</script>\
    <script>throw new Error('boom')</script>\
    <script>document.body.setAttribute('data-c','1')</script>\
    </body></html>";

/// 65 件の inline スクリプト（`data-s1`..`data-s65` を付ける）と DOMContentLoaded リスナー。
fn many_scripts_html() -> String {
    let mut s = String::from(
        "<html><body><script>document.addEventListener('DOMContentLoaded',function(){\
         document.body.setAttribute('data-dcl','1')})</script>",
    );
    // 先頭のリスナー登録が 1 件目なので、マーカーは 2..=65 件目の `data-s{n}`（n = 通し番号）。
    for n in 2..=65 {
        s.push_str(&format!(
            "<script>document.body.setAttribute('data-s{n}','1')</script>"
        ));
    }
    s.push_str("</body></html>");
    s
}

const TIMER_HTML: &str = "<html><body><script>var n=0;setInterval(function(){n++;\
    document.body.setAttribute('data-ticks',''+n)},0)</script></body></html>";

/// `innerHTML` の一括生成（63,000 バイト = 9,000 ノード）を繰り返してノード数上限に当てる。
/// 例外で 1 件目が終わり、2 件目は続行する。
const NODES_HTML: &str = "<html><body>\
    <script>for(;;){var d=document.createElement('div');d.innerHTML='<i></i>'.repeat(9000);\
    document.body.appendChild(d)}</script>\
    <script>document.body.setAttribute('data-after','1')</script></body></html>";

/// 約 60,000 バイトのテキストを持つ `p` を 150 個（合計約 9 MB > 8 MiB）追加する。
const BIG_HTML: &str = "<html><body><p>base</p><script>var t='x'.repeat(60000);\
    for(var i=0;i<150;i++){var p=document.createElement('p');p.textContent=t;\
    document.body.appendChild(p)}void 0</script></body></html>";

const LOOP_HTML: &str = "<html><body>\
    <script>document.body.setAttribute('data-m1','1')</script>\
    <script>while(true){}</script>\
    <script>document.body.setAttribute('data-m3','1')</script></body></html>";

/// 1.2 秒ずつ空回りする 3 件（各件はスクリプト単位 2 秒未満）の後に無限ループを置く。累積 3.6 秒の
/// 残り約 1.4 秒でページ単位 5 秒に当たり、4 件目（無限ループ）が打ち切られる。
const PAGE_TIME_HTML: &str = "<html><body>\
    <script>var e=Date.now()+1200;while(Date.now()<e){}document.body.setAttribute('data-p1','1')</script>\
    <script>var e=Date.now()+1200;while(Date.now()<e){}document.body.setAttribute('data-p2','1')</script>\
    <script>var e=Date.now()+1200;while(Date.now()<e){}document.body.setAttribute('data-p3','1')</script>\
    <script>while(true){}</script>\
    <script>document.body.setAttribute('data-p5','1')</script>\
    </body></html>";

/// SSRF（既定ハンドル）: `t` はフィクスチャ T のポート。内部宛の `src`・XHR・fetch を並べる。
fn ssrf_html(t: u16) -> String {
    format!(
        "<html><body>\
         <script src=\"http://127.0.0.1:{t}/s1\"></script>\
         <script src=\"http://localhost:{t}/s2\"></script>\
         <script src=\"http://[::1]:{t}/s3\"></script>\
         <script src=\"/own.js\"></script>\
         <script src=\"http://10.0.0.1/x.js\"></script>\
         <script src=\"http://192.168.0.1/x.js\"></script>\
         <script src=\"http://169.254.169.254/x.js\"></script>\
         <script src=\"file:///etc/passwd\"></script>\
         <script>function m(k,v){{document.body.setAttribute('data-'+k,v)}}\
         function x(k,u){{var r=new XMLHttpRequest();r.open('GET',u);\
         r.onload=function(){{m(k,'load')}};r.onerror=function(){{m(k,'error')}};r.send()}}\
         x('own','/x');x('t','http://127.0.0.1:{t}/t');x('file','file:///etc/passwd');\
         fetch('/f');</script></body></html>"
    )
}

/// SSRF（許可ハンドル）: リダイレクト経由で T / `file:` へ向かう XHR・fetch・`src`。
const REDIR_HTML: &str = "<html><body>\
    <script src=\"/to-file\"></script>\
    <script>function m(k,v){document.body.setAttribute('data-'+k,v)}\
    function x(k,u){var r=new XMLHttpRequest();r.open('GET',u);\
    r.onload=function(){m(k,'load')};r.onerror=function(){m(k,'error')};r.send()}\
    x('redir','/redir');x('tofile','/to-file');fetch('/redir');</script></body></html>";

/// リダイレクト先 T と、リダイレクトを返す S を立てる。戻り値: (S ポート, S hits, T ポート, T hits)。
fn serve_redirects() -> (u16, Hits, u16, Hits) {
    let (t, t_hits) = serve(|_| html_reply("secret"));
    let (s, s_hits) = serve(move |path| match path {
        "/page" => html_reply(REDIR_HTML),
        "/redir" => (
            "302 Found",
            format!("Location: http://localhost:{t}/secret\r\n"),
            String::new(),
        ),
        "/to-file" => (
            "302 Found",
            "Location: file:///etc/passwd\r\n".to_owned(),
            String::new(),
        ),
        _ => not_found(),
    });
    (s, s_hits, t, t_hits)
}

/// SSRF（既定ハンドル）用に S（ページ）と T（接続数を数えるだけ）を立てる。
fn serve_ssrf() -> (u16, Hits, u16, Hits) {
    let (t, t_hits) = serve(|_| js_reply(""));
    let page = ssrf_html(t);
    let (s, s_hits) = serve(move |path| {
        if path == "/page" {
            html_reply(&page)
        } else {
            js_reply("")
        }
    });
    (s, s_hits, t, t_hits)
}

// ---------------------------------------------------------------------------
// アクター層のケース（全 OS）
// ---------------------------------------------------------------------------

fn actor_cases(engine: &str) {
    let rt = rt();

    eprintln!("case: JS-1/PERF-7 actor layer starts no worker for disabled or script-less pages");
    {
        let (port, hits) = serve_routes(vec![("/app.js", "js", SPA_JS.to_owned())]);
        let base_children = child_process_count();
        let h = handle(engine, true);
        let url = format!("http://127.0.0.1:{port}/page");
        let skipped = actor_run(&rt, &h, "<p>no script</p>", &url);
        assert!(matches!(
            skipped,
            Ok(PageScriptOutcome::Skipped(
                PageScriptSkipReason::NoScriptElement
            ))
        ));
        let disabled = PageScriptHandle::disabled();
        let skipped = actor_run(&rt, &disabled, SPA_HTML, &url);
        assert!(matches!(
            skipped,
            Ok(PageScriptOutcome::Skipped(PageScriptSkipReason::Disabled))
        ));
        assert_eq!(h.stats().worker_spawns(), 0);
        assert_eq!(disabled.stats().worker_spawns(), 0);
        assert_eq!(child_process_count(), base_children);
        assert_eq!(hit_count(&hits), 0);
    }

    eprintln!("case: JS-4/JS-5 actor layer renders an SPA from a src script");
    {
        let (port, hits) = serve_routes(vec![("/app.js", "js", SPA_JS.to_owned())]);
        let url = format!("http://127.0.0.1:{port}/page");
        let out = executed(actor_run(&rt, &handle(engine, true), SPA_HTML, &url));
        assert!(out.abort().is_none(), "{:?}", out.abort());
        assert_eq!(outcomes(&out), vec![&ScriptOutcome::Succeeded]);
        assert!(out.html().contains("<h1>Hello SPA</h1>"), "{}", out.html());
        assert!(out.html().contains("<li>item-3</li>"), "{}", out.html());
        assert_eq!(hit_paths(&hits), vec!["/app.js".to_owned()]);
    }

    eprintln!("case: JS-4 actor layer keeps running after an exception");
    {
        let out = executed(actor_run(
            &rt,
            &handle(engine, false),
            EXCEPTION_HTML,
            "http://127.0.0.1:1/page",
        ));
        assert!(out.abort().is_none(), "{:?}", out.abort());
        let o = outcomes(&out);
        assert_eq!(o.len(), 3, "{o:?}");
        assert_eq!(o.first(), Some(&&ScriptOutcome::Succeeded));
        match o.get(1) {
            Some(ScriptOutcome::Exception { message, .. }) => {
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("expected Exception, got {other:?}"),
        }
        assert_eq!(o.get(2), Some(&&ScriptOutcome::Succeeded));
    }

    eprintln!("case: JS-6 actor layer records script_limit_exceeded and stops at 64 scripts");
    {
        let out = executed(actor_run(
            &rt,
            &handle(engine, false),
            &many_scripts_html(),
            "http://127.0.0.1:1/page",
        ));
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::ScriptCount { limit: 64 })
        );
        assert_eq!(out.abort().map(|a| a.kind().as_str()), Some("script_count"));
        let skipped = out.scripts().iter().find_map(|r| match r.outcome() {
            ScriptOutcome::Skipped { kind, count } => Some((*kind, *count)),
            _ => None,
        });
        let (kind, _) = skipped.expect("script_limit_exceeded record");
        assert_eq!(kind, ScriptDiagnosticKind::ScriptLimitExceeded);
        assert_eq!(kind.as_str(), "script_limit_exceeded");
        let succeeded = outcomes(&out)
            .iter()
            .filter(|o| matches!(o, ScriptOutcome::Succeeded))
            .count();
        assert_eq!(succeeded, 64);
        assert!(out.html().contains("data-s64=\"1\""));
        assert!(!out.html().contains("data-s65=\"1\""));
        assert!(!out.html().contains("data-dcl=\"1\""));
    }

    eprintln!("case: JS-6/JS-7 actor layer caps interval timers at 1000 fires");
    {
        let out = executed(actor_run(
            &rt,
            &timer_handle(engine),
            TIMER_HTML,
            "http://127.0.0.1:1/page",
        ));
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::TimerFires { limit: 1000 })
        );
        assert_eq!(out.event_loop().fired(), 1000);
        assert!(out.html().contains("data-ticks=\"1000\""), "{}", out.html());
    }

    eprintln!("case: JS-6 actor layer stops node over-generation without aborting the page");
    {
        let out = executed(actor_run(
            &rt,
            &handle(engine, false),
            NODES_HTML,
            "http://127.0.0.1:1/page",
        ));
        assert!(out.abort().is_none(), "{:?}", out.abort());
        match outcomes(&out).first() {
            Some(ScriptOutcome::Exception { message, .. }) => {
                assert!(message.contains("100000"), "{message}");
            }
            other => panic!("expected Exception, got {other:?}"),
        }
        assert!(out.html().contains("data-after=\"1\""));
        // 上限内で止まる（9,000 ノードの塊が 11 個 = 99,000 ノード + 文書の既存ノード）。
        // スクリプト本文のリテラル 1 個を除く。
        let blocks = out.html().matches("<i></i>").count() - 1;
        assert_eq!(blocks, 99_000);
    }

    eprintln!("case: JS-6 actor layer fails closed when the serialized output exceeds 8 MiB");
    {
        let result = actor_run(
            &rt,
            &handle(engine, false),
            BIG_HTML,
            "http://127.0.0.1:1/page",
        );
        match result {
            Err(PageScriptError::Run(e)) => {
                let text = e.to_string();
                assert!(text.contains("8388608"), "{text}");
            }
            other => panic!("expected Err(Run), got {other:?}"),
        }
    }

    eprintln!("case: JS-6 actor layer aborts an infinite loop at the 2s script wall time");
    {
        let started = Instant::now();
        let out = executed(actor_run(
            &rt,
            &handle(engine, false),
            LOOP_HTML,
            "http://127.0.0.1:1/page",
        ));
        let elapsed = started.elapsed();
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::WallTime {
                scope: WallTimeScope::Script,
                limit: Duration::from_secs(2),
            })
        );
        assert!(elapsed <= Duration::from_secs(8), "{elapsed:?}");
        assert!(out.html().contains("data-m1=\"1\""));
        assert!(!out.html().contains("data-m3=\"1\""));
        assert!(matches!(
            outcomes(&out).get(2),
            Some(ScriptOutcome::NotExecuted { .. })
        ));
    }

    // ページ単位の実時間上限は約 5 秒かかるため、CDP 層を実行できない OS（非 unix）だけアクター層で見る。
    #[cfg(not(unix))]
    {
        eprintln!("case: JS-6 actor layer aborts cumulative script time at the 5s page wall time");
        let started = Instant::now();
        let out = executed(actor_run(
            &rt,
            &handle(engine, false),
            PAGE_TIME_HTML,
            "http://127.0.0.1:1/page",
        ));
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::WallTime {
                scope: WallTimeScope::Page,
                limit: Duration::from_secs(5),
            })
        );
        assert!(started.elapsed() <= Duration::from_secs(8));
        assert!(!out.html().contains("data-p5=\"1\""));
    }

    eprintln!("case: JS-8 actor layer rejects internal src/XHR/fetch with the default handle");
    {
        let (s, s_hits, t, t_hits) = serve_ssrf();
        let url = format!("http://127.0.0.1:{s}/page");
        let out = executed(actor_run(&rt, &handle(engine, false), &ssrf_html(t), &url));
        assert_eq!(
            hit_count(&t_hits),
            0,
            "internal fixture must not be reached"
        );
        assert_eq!(
            hit_count(&s_hits),
            0,
            "own origin script must not be fetched"
        );
        let kinds: Vec<SrcFetchErrorKind> = out
            .scripts()
            .iter()
            .filter_map(|r| match r.outcome() {
                ScriptOutcome::SrcFetchFailed { kind } => Some(*kind),
                _ => None,
            })
            .collect();
        let mut expected = vec![SrcFetchErrorKind::DisallowedAddress; 7];
        expected.push(SrcFetchErrorKind::DisallowedScheme);
        assert_eq!(kinds, expected);
        // XHR: 自オリジン（既定では内部アドレス拒否）・T（別オリジン）・`file:`（不正 URL）、
        // fetch: 自オリジン。到達順は問わないため文字列で並べ替えて比べる。
        let mut rej: Vec<String> = rejections(&out)
            .iter()
            .map(|(r, k)| format!("{}:{}", r.as_str(), k.as_str()))
            .collect();
        rej.sort();
        let mut expected = vec![
            "fetch:disallowed_address",
            "xhr:cross_origin",
            "xhr:disallowed_address",
            "xhr:invalid_url",
        ];
        expected.sort();
        assert_eq!(rej, expected);
    }

    eprintln!("case: JS-8 actor layer never follows redirects to internal targets");
    {
        let (s, _s_hits, _t, t_hits) = serve_redirects();
        let url = format!("http://127.0.0.1:{s}/page");
        let out = executed(actor_run(&rt, &handle(engine, true), REDIR_HTML, &url));
        assert_eq!(hit_count(&t_hits), 0, "redirect target must not be reached");
        assert_eq!(
            outcomes(&out).first().map(|o| o.as_str()),
            Some("src_fetch_failed")
        );
        let rej = rejections(&out);
        assert!(!rej.is_empty(), "redirects must be recorded as rejections");
    }
}

// ---------------------------------------------------------------------------
// CDP 層のケース（unix のみ。Profile::open が非 unix で Unsupported）
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod cdp_layer {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use fandhe_browser_cdp::CdpState;
    use fandhe_browser_cdp::test_support::LoopbackDispatcher;
    use fandhe_browser_core::{AppState, PageScriptHandle};
    use fandhe_browser_profile::Profile;
    use serde_json::{Value, json};

    use super::*;

    /// 一時プロファイルディレクトリ。成否に関わらず Drop で削除する。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            Self(base.join(format!(
                "fandhe-cdp-page-js-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            )))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// CDP 入口の 1 セッション分（状態・ディスパッチャ・一時ディレクトリ）。
    struct Cdp {
        st: Arc<CdpState>,
        d: LoopbackDispatcher,
        next_id: i64,
        _dir: TempDir,
    }

    impl Cdp {
        fn new(scripts: Option<PageScriptHandle>) -> Self {
            let dir = TempDir::new();
            let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
            let mut app = AppState::with_disabled_renderer(profile);
            if let Some(h) = scripts {
                app = app.with_page_scripts(h);
            }
            Self {
                st: Arc::new(CdpState::new(Arc::new(app))),
                d: LoopbackDispatcher::new().expect("dispatcher"),
                next_id: 0,
                _dir: dir,
            }
        }

        async fn call(&mut self, method: &str, params: Value) -> Vec<Value> {
            self.next_id += 1;
            let req = json!({"id": self.next_id, "method": method, "params": params});
            self.d
                .dispatch(&self.st, &req.to_string())
                .await
                .iter()
                .map(|f| serde_json::from_str(f).expect("json frame"))
                .collect()
        }

        /// `Page.navigate` を実行し、(フレーム, 所要時間) を返す。応答 → `Page.frameNavigated` →
        /// `Page.loadEventFired` の 3 フレームであることまで検証する（`CDP-1`）。
        async fn navigate(&mut self, url: &str) -> (Vec<Value>, Duration) {
            let started = Instant::now();
            let frames = self.call("Page.navigate", json!({"url": url})).await;
            let elapsed = started.elapsed();
            assert_eq!(frames.len(), 3, "{frames:?}");
            assert_eq!(
                frames.first().and_then(|f| f.get("result")),
                Some(&json!({"frameId": "main", "loaderId": "1"}))
            );
            assert_eq!(
                frames.get(1).and_then(|f| f["method"].as_str()),
                Some("Page.frameNavigated")
            );
            assert_eq!(
                frames.get(2).and_then(|f| f["method"].as_str()),
                Some("Page.loadEventFired")
            );
            (frames, elapsed)
        }

        /// `DOM.getDocument` の応答全体（JSON 文字列）。
        async fn document_text(&mut self, depth: i64) -> String {
            let mut f = self.call("DOM.getDocument", json!({"depth": depth})).await;
            assert_eq!(f.len(), 1, "{f:?}");
            f.remove(0).to_string()
        }

        async fn root_node_id(&mut self) -> i64 {
            let f = self.call("DOM.getDocument", json!({"depth": 1})).await;
            f.first()
                .and_then(|r| r["result"]["root"]["nodeId"].as_i64())
                .expect("root nodeId")
        }

        async fn query(&mut self, root: i64, selector: &str) -> i64 {
            let f = self
                .call(
                    "DOM.querySelector",
                    json!({"nodeId": root, "selector": selector}),
                )
                .await;
            f.first()
                .and_then(|r| r["result"]["nodeId"].as_i64())
                .unwrap_or_else(|| panic!("querySelector failed: {f:?}"))
        }

        /// 共有状態へ確定した最新の HTML。
        fn html(&self) -> String {
            self.st
                .app_state()
                .navigation()
                .latest()
                .expect("latest")
                .html()
                .to_owned()
        }
    }

    /// `name` が空でない input / textarea / select の件数（compat-practical の form 判定と同じ見方）。
    fn named_fields(node: &Value) -> usize {
        let own = {
            let name = node["nodeName"].as_str().unwrap_or("").to_ascii_uppercase();
            let attrs = node["attributes"].as_array().cloned().unwrap_or_default();
            let named = attrs.chunks(2).any(|kv| {
                kv.first().and_then(Value::as_str) == Some("name")
                    && kv
                        .get(1)
                        .and_then(Value::as_str)
                        .is_some_and(|v| !v.is_empty())
            });
            usize::from(matches!(name.as_str(), "INPUT" | "TEXTAREA" | "SELECT") && named)
        };
        let children: usize = node["children"]
            .as_array()
            .map(|c| c.iter().map(named_fields).sum())
            .unwrap_or(0);
        own + children
    }

    pub fn run(engine: &str) {
        let rt = rt();
        rt.block_on(async {
            no_start(engine).await;
            spa(engine).await;
            form(engine).await;
            exception(engine).await;
            many_scripts(engine).await;
            timers(engine).await;
            nodes(engine).await;
            big_output(engine).await;
            infinite_loop(engine).await;
            page_wall_time(engine).await;
            ssrf_default(engine).await;
            ssrf_redirects(engine).await;
        });
    }

    async fn no_start(engine: &str) {
        eprintln!("case: JS-1/PERF-7 no worker is started when scripts are off or absent");
        let (port, hits) = serve_routes(vec![
            ("/spa", "html", SPA_HTML.to_owned()),
            (
                "/plain",
                "html",
                "<html><body><p>plain</p></body></html>".to_owned(),
            ),
            ("/app.js", "js", SPA_JS.to_owned()),
        ]);
        let base_children = child_process_count();
        // (a) 既定状態（ページ JS 実行なし）: ハンドルは無効で、src も取得しない。
        let mut off = Cdp::new(None);
        off.navigate(&format!("http://127.0.0.1:{port}/spa")).await;
        assert!(!off.st.app_state().page_scripts().is_enabled());
        assert_eq!(off.st.app_state().page_scripts().stats().worker_spawns(), 0);
        assert!(!hit_paths(&hits).contains(&"/app.js".to_owned()));
        // (b) 有効だが `<script>` のないページ: ワーカーを起動しない。
        let h = handle(engine, true);
        let mut on = Cdp::new(Some(h.clone()));
        on.navigate(&format!("http://127.0.0.1:{port}/plain")).await;
        assert!(h.is_enabled());
        assert_eq!(h.stats().worker_spawns(), 0);
        assert_eq!(child_process_count(), base_children);
    }

    async fn spa(engine: &str) {
        eprintln!("case: JS-4/JS-5 SPA page is rendered when enabled and left empty when disabled");
        let (port, hits) = serve_routes(vec![
            ("/spa", "html", SPA_HTML.to_owned()),
            ("/app.js", "js", SPA_JS.to_owned()),
        ]);
        let url = format!("http://127.0.0.1:{port}/spa");
        let mut on = Cdp::new(Some(handle(engine, true)));
        on.navigate(&url).await;
        let doc = on.document_text(-1).await;
        assert!(doc.contains("Hello SPA"), "{doc}");
        assert_eq!(doc.matches("item-").count(), 3, "{doc}");
        assert!(on.html().contains("<h1>Hello SPA</h1>"), "{}", on.html());
        assert_eq!(
            hit_paths(&hits).iter().filter(|p| *p == "/app.js").count(),
            1
        );

        let before = hit_count(&hits);
        let mut off = Cdp::new(None);
        off.navigate(&url).await;
        let doc = off.document_text(-1).await;
        assert!(!doc.contains("Hello SPA"), "{doc}");
        assert!(
            off.html().contains("<div id=\"app\"></div>"),
            "{}",
            off.html()
        );
        // 無効時はページ本体の取得 1 件だけで、/app.js へは行かない。
        assert_eq!(hit_count(&hits) - before, 1);
    }

    async fn form(engine: &str) {
        eprintln!("case: JS-4 a script-generated form is found with the compat-practical steps");
        let (port, _hits) = serve_routes(vec![
            ("/form", "html", FORM_HTML.to_owned()),
            ("/form.js", "js", FORM_JS.to_owned()),
        ]);
        let url = format!("http://127.0.0.1:{port}/form");
        let mut on = Cdp::new(Some(handle(engine, true)));
        on.navigate(&url).await;
        let root = on.root_node_id().await;
        let form = on.query(root, "form").await;
        assert_ne!(form, 0);
        let frames = on
            .call(
                "DOM.requestChildNodes",
                json!({"nodeId": form, "depth": -1}),
            )
            .await;
        let event = frames
            .iter()
            .find(|f| f["method"] == json!("DOM.setChildNodes"))
            .expect("setChildNodes");
        let nodes = event["params"]["nodes"].as_array().expect("nodes");
        let fields: usize = nodes.iter().map(named_fields).sum();
        assert_eq!(fields, 3, "{event}");

        // 対照: 実行無効なら form は存在しない（nodeId 0）。
        let mut off = Cdp::new(None);
        off.navigate(&url).await;
        let root = off.root_node_id().await;
        let f = off
            .call(
                "DOM.querySelector",
                json!({"nodeId": root, "selector": "form"}),
            )
            .await;
        assert_eq!(
            f.first().map(|r| r["result"].clone()),
            Some(json!({"nodeId": 0}))
        );
    }

    async fn exception(engine: &str) {
        eprintln!("case: JS-4 an exception does not stop later scripts or the navigation");
        let (port, _hits) = serve_routes(vec![("/ex", "html", EXCEPTION_HTML.to_owned())]);
        let mut on = Cdp::new(Some(handle(engine, false)));
        on.navigate(&format!("http://127.0.0.1:{port}/ex")).await;
        let html = on.html();
        assert!(html.contains("data-a=\"1\""), "{html}");
        assert!(html.contains("data-c=\"1\""), "{html}");
    }

    async fn many_scripts(engine: &str) {
        eprintln!("case: JS-6 only 64 of 65 scripts run and DOMContentLoaded is not fired");
        let (port, _hits) = serve_routes(vec![("/many", "html", many_scripts_html())]);
        let mut on = Cdp::new(Some(handle(engine, false)));
        on.navigate(&format!("http://127.0.0.1:{port}/many")).await;
        let html = on.html();
        assert!(html.contains("data-s64=\"1\""), "{html}");
        assert!(!html.contains("data-s65=\"1\""), "{html}");
        assert!(!html.contains("data-dcl=\"1\""), "{html}");
    }

    async fn timers(engine: &str) {
        eprintln!("case: JS-6/JS-7 a setInterval(0) runaway ends at 1000 fires with the DOM fixed");
        let (port, _hits) = serve_routes(vec![("/timer", "html", TIMER_HTML.to_owned())]);
        let mut on = Cdp::new(Some(timer_handle(engine)));
        on.navigate(&format!("http://127.0.0.1:{port}/timer")).await;
        let root = on.root_node_id().await;
        let hit = on.query(root, "body[data-ticks=\"1000\"]").await;
        assert_ne!(hit, 0);
    }

    async fn nodes(engine: &str) {
        eprintln!("case: JS-6 node over-generation stops at the node limit and later scripts run");
        let (port, _hits) = serve_routes(vec![("/nodes", "html", NODES_HTML.to_owned())]);
        let mut on = Cdp::new(Some(handle(engine, false)));
        on.navigate(&format!("http://127.0.0.1:{port}/nodes")).await;
        let html = on.html();
        assert!(html.contains("data-after=\"1\""), "{html}");
        // スクリプト本文のリテラル 1 個を除く。
        let blocks = html.matches("<i></i>").count() - 1;
        assert_eq!(blocks, 99_000);
        // getDocument は 1 応答 10,000 ノードまでのため、depth 1 で読む（読み出し側の上限）。
        let doc = on.document_text(1).await;
        assert!(doc.contains("nodeId"), "{doc}");
    }

    async fn big_output(engine: &str) {
        eprintln!("case: JS-6 over-8MiB output keeps the fetched HTML and the navigation succeeds");
        let (port, _hits) = serve_routes(vec![("/big", "html", BIG_HTML.to_owned())]);
        let mut on = Cdp::new(Some(handle(engine, false)));
        on.navigate(&format!("http://127.0.0.1:{port}/big")).await;
        // 実行結果は破棄され、取得時の HTML で確定する（途中の DOM の確定ではない）。
        assert_eq!(
            on.html().matches("<p>").count(),
            1,
            "len={}",
            on.html().len()
        );
        assert!(on.html().contains("<p>base</p>"));
    }

    async fn infinite_loop(engine: &str) {
        eprintln!("case: JS-6 an infinite loop is cut by the wall time and navigation responds");
        let (port, _hits) = serve_routes(vec![("/loop", "html", LOOP_HTML.to_owned())]);
        let mut on = Cdp::new(Some(handle(engine, false)));
        let (_, elapsed) = on.navigate(&format!("http://127.0.0.1:{port}/loop")).await;
        assert!(elapsed >= Duration::from_millis(1900), "{elapsed:?}");
        assert!(elapsed <= Duration::from_secs(8), "{elapsed:?}");
        let html = on.html();
        assert!(html.contains("data-m1=\"1\""), "{html}");
        assert!(!html.contains("data-m3=\"1\""), "{html}");
    }

    async fn page_wall_time(engine: &str) {
        eprintln!("case: JS-6 cumulative script time is cut at the 5s page wall time");
        let (port, _hits) = serve_routes(vec![("/slow", "html", PAGE_TIME_HTML.to_owned())]);
        let mut on = Cdp::new(Some(handle(engine, false)));
        let (_, elapsed) = on.navigate(&format!("http://127.0.0.1:{port}/slow")).await;
        assert!(elapsed >= Duration::from_millis(4500), "{elapsed:?}");
        assert!(elapsed <= Duration::from_secs(8), "{elapsed:?}");
        let html = on.html();
        for ok in ["data-p1=\"1\"", "data-p2=\"1\"", "data-p3=\"1\""] {
            assert!(html.contains(ok), "{ok}: {html}");
        }
        assert!(!html.contains("data-p5=\"1\""), "{html}");
    }

    async fn ssrf_default(engine: &str) {
        eprintln!("case: JS-8/JS-4 internal src/XHR/fetch never connect with the default handle");
        let (s, s_hits, t, t_hits) = serve_ssrf();
        let _ = t;
        let mut on = Cdp::new(Some(handle(engine, false)));
        on.navigate(&format!("http://127.0.0.1:{s}/page")).await;
        assert_eq!(
            hit_count(&t_hits),
            0,
            "internal fixture must not be reached"
        );
        // S への接続はページ本体の取得 1 件だけ（自オリジンのループバックも既定では拒否）。
        assert_eq!(hit_paths(&s_hits), vec!["/page".to_owned()]);
        let html = on.html();
        assert!(html.contains("data-t=\"error\""), "{html}");
        assert!(html.contains("data-file=\"error\""), "{html}");
    }

    async fn ssrf_redirects(engine: &str) {
        eprintln!("case: JS-8 redirects to internal targets are not followed");
        let (s, _s_hits, _t, t_hits) = serve_redirects();
        let mut on = Cdp::new(Some(handle(engine, true)));
        on.navigate(&format!("http://127.0.0.1:{s}/page")).await;
        assert_eq!(hit_count(&t_hits), 0, "redirect target must not be reached");
        let html = on.html();
        assert!(html.contains("data-redir=\"error\""), "{html}");
        assert!(html.contains("data-tofile=\"error\""), "{html}");
    }
}

/// Windows など非 unix: `Profile::open` が `Unsupported` のため CDP 層を実行できないことを確認する。
/// 対応（XOS-7〜10）が入ると失敗し、CDP 層ケースの有効化を促す。
#[cfg(not(unix))]
fn windows_profile_is_unsupported() {
    let dir = std::env::temp_dir().join(format!("fandhe-cdp-page-js-{}", std::process::id()));
    let result = fandhe_browser_profile::Profile::open(&dir);
    assert!(
        result.is_err(),
        "Profile::open is expected to be unsupported"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn run(engine: &str) {
    actor_cases(engine);
    #[cfg(unix)]
    cdp_layer::run(engine);
    #[cfg(not(unix))]
    windows_profile_is_unsupported();
}

/// macOS では boa のランタイム生成が失敗し、アクターは `Err(Run(_))` を返す（成功を装わない）。
#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub fn run_boa_fail_closed() {
    let rt = rt();
    let result = actor_run(
        &rt,
        &handle("boa", false),
        "<body><script>1</script></body>",
        "https://example.test/",
    );
    assert!(
        matches!(result, Err(PageScriptError::Run(_))),
        "boa must be disabled on macOS: {result:?}"
    );
}

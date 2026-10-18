//! `module_graph` の単体テスト（fake エンジン + ローカル HTTP。Issue #1116・`JS-4`・`JS-6`・`JS-8`）。
//!
//! 実エンジン（V8）での結合は `tests/support/page_runner_cases.rs` の `run_module_graph` が担う。

use super::*;
use crate::fetch::FetchOptions;
use crate::page_runner::{
    PageRunInput, PageRunOptions, PageRunOutput, run_with_fetcher_and_factory,
};
use crate::page_script::ScriptCollectionOptions;
use fandhe_browser_js::{EngineKind, JsEngine, JsValue, ModuleRecord, ModuleRequest, NativeFn};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

// ---- fake エンジン ----

#[derive(Default)]
struct EngineLog {
    /// `compile:<id>:<url>` / `link:<referrer>:<specifier>=<target>` / `eval:<id>`。
    events: Vec<String>,
    sources: HashMap<u32, String>,
}

/// module API を持つテスト用エンジン。`import "<spec>"` の文から依存を作り、ソース中の印
/// （`syntax-error`・`tla-pending`・`reject:<msg>`・`unsupported`・`compile-unsupported`）で
/// 結果を分岐する。classic の評価はすべて成功させる。
struct FakeModuleEngine {
    log: Arc<Mutex<EngineLog>>,
    next_id: u32,
}

impl JsEngine for FakeModuleEngine {
    fn evaluate_script(
        &mut self,
        _script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        Ok(JsValue::Undefined)
    }

    fn inject_global_function(
        &mut self,
        _name: &str,
        _func: NativeFn,
    ) -> Result<(), JsEngineError> {
        Ok(())
    }

    fn bind_dom_like_object(
        &mut self,
        _name: &str,
        _methods: Vec<(String, NativeFn)>,
    ) -> Result<(), JsEngineError> {
        Ok(())
    }

    fn compile_module(&mut self, url: &str, source: &str) -> Result<ModuleRecord, JsEngineError> {
        if source.contains("syntax-error") {
            return Err(JsEngineError::EvaluationFailed("SyntaxError".to_string()));
        }
        if source.contains("compile-unsupported") {
            return Err(JsEngineError::NotYetImplemented("module".to_string()));
        }
        self.next_id += 1;
        let id = self.next_id;
        let mut requests: Vec<ModuleRequest> = Vec::new();
        for stmt in source.split([';', '\n']) {
            if let Some(spec) = stmt
                .trim()
                .strip_prefix("import \"")
                .and_then(|s| s.strip_suffix('"'))
                && !requests.iter().any(|r| r.specifier() == spec)
            {
                requests.push(ModuleRequest::new(spec.to_string()));
            }
        }
        let mut log = self.log.lock().expect("log");
        log.events.push(format!("compile:{id}:{url}"));
        log.sources.insert(id, source.to_string());
        Ok(ModuleRecord::new(ModuleId::from_raw(id), requests))
    }

    fn link_module(
        &mut self,
        referrer: ModuleId,
        resolutions: &[ModuleResolution],
    ) -> Result<(), JsEngineError> {
        let mut log = self.log.lock().expect("log");
        for r in resolutions {
            log.events.push(format!(
                "link:{}:{}={}",
                referrer.raw(),
                r.specifier(),
                r.target().raw()
            ));
        }
        Ok(())
    }

    fn evaluate_module(
        &mut self,
        root: ModuleId,
        _options: &EvaluateOptions,
    ) -> Result<ModuleEvaluation, JsEngineError> {
        let mut log = self.log.lock().expect("log");
        log.events.push(format!("eval:{}", root.raw()));
        let src = log.sources.get(&root.raw()).cloned().unwrap_or_default();
        if src.contains("unsupported") {
            return Err(JsEngineError::NotYetImplemented("module".to_string()));
        }
        if src.contains("tla-pending") {
            return Ok(ModuleEvaluation::Pending);
        }
        if let Some(m) = src.strip_prefix("reject:") {
            return Ok(ModuleEvaluation::rejected(m.to_string()));
        }
        Ok(ModuleEvaluation::Fulfilled)
    }
}

fn fake_factory(log: Arc<Mutex<EngineLog>>) -> impl FnOnce() -> crate::Result<JsRuntime> {
    move || {
        Ok(JsRuntime::from_engine_for_test(
            EngineKind::V8,
            Box::new(FakeModuleEngine { log, next_id: 0 }),
        ))
    }
}

// ---- ローカル HTTP ----

type Hits = Arc<Mutex<HashMap<String, usize>>>;

/// `127.0.0.1` の空きポートで、パス（クエリ除く）から応答を返す最小サーバー。ヒット数を数える。
fn spawn_server(respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> (u16, Hits) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let hits: Hits = Arc::new(Mutex::new(HashMap::new()));
    let respond = Arc::new(respond);
    let hits_for_thread = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (respond, hits) = (respond.clone(), hits_for_thread.clone());
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buf = [0u8; 1024];
                let mut data = Vec::new();
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            data.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                            if data.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let head = String::from_utf8_lossy(&data).into_owned();
                let target = head.split_whitespace().nth(1).unwrap_or("/");
                let path = target.split('?').next().unwrap_or("/").to_string();
                if let Ok(mut h) = hits.lock() {
                    *h.entry(path.clone()).or_insert(0) += 1;
                }
                let _ = stream.write_all(&respond(&path));
                let _ = stream.flush();
            });
        }
    });
    (port, hits)
}

fn response(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn js(body: &str) -> Vec<u8> {
    response("200 OK", "Content-Type: text/javascript\r\n", body)
}

fn hit_count(hits: &Hits, path: &str) -> usize {
    hits.lock()
        .ok()
        .and_then(|h| h.get(path).copied())
        .unwrap_or(0)
}

fn total_hits(hits: &Hits) -> usize {
    hits.lock().map(|h| h.values().sum()).unwrap_or(0)
}

fn loopback_fetcher() -> Fetcher {
    Fetcher::new(FetchOptions::new().with_allow_private_network_access(true)).expect("fetcher")
}

fn module_options() -> PageRunOptions {
    PageRunOptions::default()
        .with_collection(ScriptCollectionOptions::default().with_execute_modules(true))
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

/// 実 Fetcher と fake エンジンで HTML を実行する。
fn run(html: &str, port: u16, options: &PageRunOptions) -> (PageRunOutput, Arc<Mutex<EngineLog>>) {
    let log = Arc::new(Mutex::new(EngineLog::default()));
    let base = format!("http://127.0.0.1:{port}/");
    let input = PageRunInput::new(html, &base);
    let fetcher = loopback_fetcher();
    let out = block_on(run_with_fetcher_and_factory(
        &input,
        options,
        &fetcher,
        fake_factory(log.clone()),
    ))
    .expect("run");
    (out, log)
}

fn outcomes(out: &PageRunOutput) -> Vec<ScriptOutcome> {
    out.scripts().iter().map(|r| r.outcome().clone()).collect()
}

fn events(log: &Arc<Mutex<EngineLog>>) -> Vec<String> {
    log.lock().map(|l| l.events.clone()).unwrap_or_default()
}

fn failed(kind: SrcFetchErrorKind) -> ScriptOutcome {
    ScriptOutcome::SrcFetchFailed { kind }
}

/// 全パスに同じ JS 応答を返すサーバー。
fn leaf_server() -> (u16, Hits) {
    spawn_server(|_| js("export const x = 1;"))
}

// ---- 表テスト ----

/// `JS-8`: specifier の解決（相対は join、絶対は解析、それ以外は bare）。
#[test]
fn js_8_resolve_specifier_table() {
    let referrer = Url::parse("http://h.test/a/b.js").expect("url");
    let ok = |s: &str| {
        resolve_specifier(&referrer, s)
            .map(|u| u.to_string())
            .map_err(|k| k.as_str())
    };
    assert_eq!(ok("./c.js"), Ok("http://h.test/a/c.js".to_string()));
    assert_eq!(ok("../c.js"), Ok("http://h.test/c.js".to_string()));
    assert_eq!(ok("/c.js"), Ok("http://h.test/c.js".to_string()));
    assert_eq!(
        ok("http://o.test/c.js"),
        Ok("http://o.test/c.js".to_string())
    );
    assert_eq!(ok("foo"), Err("bare_specifier"));
    assert_eq!(ok("foo/bar.js"), Err("bare_specifier"));
    assert_eq!(ok(""), Err("bare_specifier"));
}

/// `JS-8`: JavaScript MIME type の essence だけを受け付け、ヘッダ無しは拒否する。
#[test]
fn js_8_is_javascript_mime_table() {
    for ok in [
        "text/javascript",
        "TEXT/JavaScript",
        "text/javascript; charset=utf-8",
        " application/x-javascript ;q=1",
        "application/ecmascript",
        "text/javascript1.5",
        "text/jscript",
    ] {
        assert!(is_javascript_mime(Some(ok)), "{ok}");
    }
    for ng in [
        "text/plain",
        "application/json",
        "text/html",
        "text/javascriptx",
        "application/javascript+json",
        "",
    ] {
        assert!(!is_javascript_mime(Some(ng)), "{ng}");
    }
    assert!(!is_javascript_mime(None));
}

/// `JS-8`・`JS-6`: 期限 0 の module 取得は送信せず `Timeout`。
#[test]
fn js_6_get_for_module_zero_deadline_does_not_send() {
    let (port, hits) = leaf_server();
    let fetcher = loopback_fetcher();
    let err = block_on(fetcher.get_for_module(
        &format!("http://127.0.0.1:{port}/a.js"),
        1024,
        Duration::ZERO,
    ))
    .expect_err("timeout");
    assert!(matches!(err, Error::Timeout { .. }), "{err}");
    assert_eq!(total_hits(&hits), 0);
}

// ---- グラフの取得 ----

/// `JS-4`: 2 段の依存と循環（entry → a → b → a）を解決し、各 URL は 1 回だけ取得する。
#[test]
fn js_4_module_graph_two_levels_with_cycle() {
    let (port, hits) = spawn_server(|p| match p {
        "/a.js" => js("import \"./b.js\";"),
        "/b.js" => js("import \"./a.js\";"),
        _ => response("404 Not Found", "", ""),
    });
    let (out, log) = run(
        "<script type=module>import \"./a.js\";</script>",
        port,
        &module_options(),
    );
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
    assert_eq!(hit_count(&hits, "/a.js"), 1);
    assert_eq!(hit_count(&hits, "/b.js"), 1);
    let ev = events(&log);
    assert!(ev.contains(&"link:2:./b.js=3".to_string()), "{ev:?}");
    assert!(ev.contains(&"link:3:./a.js=2".to_string()), "{ev:?}");
    assert!(ev.contains(&"link:1:./a.js=2".to_string()), "{ev:?}");
    assert_eq!(
        ev.iter().filter(|e| e.starts_with("eval:")).count(),
        1,
        "{ev:?}"
    );
    assert!(out.abort().is_none());
}

/// `JS-8`: 同じ URL は経路が違っても 1 回しか取得せず、evaluate も 1 回。
#[test]
fn js_8_duplicate_import_fetched_once() {
    let (port, hits) = leaf_server();
    let html = "<script type=module src=\"/s.js\"></script>\
                <script type=module>import \"/s.js\";</script>\
                <script type=module>import \"/s.js\";</script>\
                <script type=module src=\"/s.js\"></script>";
    let (out, log) = run(html, port, &module_options());
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded; 4]);
    assert_eq!(hit_count(&hits, "/s.js"), 1);
    let evals = events(&log)
        .into_iter()
        .filter(|e| e.starts_with("eval:"))
        .count();
    // s.js（1 回）と 2 つの inline の root。最後の `src` の再指定は evaluate しない。
    assert_eq!(evals, 3);
}

/// `JS-4`: 同じ `src` の再指定は、最初の結果（例外・保留）をそのまま返す。成功に化けない。
#[test]
fn js_4_repeated_src_reports_first_result() {
    let (port, hits) = spawn_server(|path| match path {
        "/r.js" => js("reject:boom"),
        _ => js("tla-pending"),
    });
    let html = "<script type=module src=\"/r.js\"></script>\
                <script type=module src=\"/r.js\"></script>\
                <script type=module src=\"/p.js\"></script>\
                <script type=module src=\"/p.js\"></script>";
    let (out, _) = run(html, port, &module_options());
    let exception = ScriptOutcome::Exception {
        message: "boom".to_string(),
        truncated: false,
    };
    assert_eq!(
        outcomes(&out),
        vec![
            exception.clone(),
            exception,
            ScriptOutcome::ModulePending,
            ScriptOutcome::ModulePending
        ]
    );
    assert_eq!(hit_count(&hits, "/r.js"), 1);
}

/// `JS-4`: フラグメントだけが違う URL は別モジュール。URL にはフラグメントが残り、取得時は除く。
#[test]
fn js_4_fragment_distinguishes_modules() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>import \"/m.js#a\"; import \"/m.js#b\";</script>";
    let (out, log) = run(html, port, &module_options());
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
    let ev = events(&log);
    assert!(ev.iter().any(|e| e.ends_with("/m.js#a")), "{ev:?}");
    assert!(ev.iter().any(|e| e.ends_with("/m.js#b")), "{ev:?}");
    assert_eq!(hit_count(&hits, "/m.js"), 2);
}

/// `JS-6`: 総量は文書順に数える。後続の classic の分で先行の module を打ち切らない。
#[test]
fn js_6_total_bytes_follow_document_order() {
    let (port, _hits) = spawn_server(|_| js(&"a".repeat(10)));
    // module 15 B + 依存 10 B = 25 B。後続の classic 10 B は module の実行後に加算される。
    let html = "<script type=module>import \"/m.js\";</script><script>0123456789</script>";
    let (out, _) = run(
        html,
        port,
        &module_options().with_max_total_script_bytes(25),
    );
    assert_eq!(
        outcomes(&out),
        vec![
            ScriptOutcome::Succeeded,
            ScriptOutcome::Aborted {
                kind: AbortKind::ScriptBytes {
                    scope: ScriptBytesScope::Total,
                    limit: 25
                },
                message: None
            }
        ]
    );
}

/// `JS-8`・`SEC-2`: 拒否される specifier は取得せず、種別を残して後続を続ける。
#[test]
fn js_8_module_rejections_are_not_fetched() {
    let (port, hits) = leaf_server();
    let (other, other_hits) = leaf_server();
    let cases = [
        ("foo".to_string(), SrcFetchErrorKind::BareSpecifier),
        (
            "data:text/javascript,1".to_string(),
            SrcFetchErrorKind::DisallowedScheme,
        ),
        (
            "file:///x.js".to_string(),
            SrcFetchErrorKind::DisallowedScheme,
        ),
        (
            format!("http://127.0.0.1:{other}/x.js"),
            SrcFetchErrorKind::CrossOrigin,
        ),
        (
            "blob:http://127.0.0.1/x".to_string(),
            SrcFetchErrorKind::DisallowedScheme,
        ),
    ];
    for (spec, kind) in cases {
        let html = format!("<script type=module>import \"{spec}\";</script><script>1</script>");
        let (out, _) = run(&html, port, &module_options());
        assert_eq!(
            outcomes(&out),
            vec![failed(kind), ScriptOutcome::Succeeded],
            "{spec}"
        );
    }
    assert_eq!(total_hits(&hits), 0);
    assert_eq!(total_hits(&other_hits), 0);
}

/// `JS-8`: JavaScript MIME type 以外（ヘッダ無しを含む）は評価しない。
#[test]
fn js_8_non_javascript_mime_rejected() {
    let (port, _hits) = spawn_server(|p| match p {
        "/plain.js" => response("200 OK", "Content-Type: text/plain\r\n", "x"),
        "/nomime.js" => response("200 OK", "", "x"),
        "/json.js" => response("200 OK", "Content-Type: application/json\r\n", "x"),
        "/ok1.js" => response(
            "200 OK",
            "Content-Type: text/javascript; charset=utf-8\r\n",
            "x",
        ),
        "/ok2.js" => response("200 OK", "Content-Type: application/x-javascript\r\n", "x"),
        _ => response("404 Not Found", "", ""),
    });
    for path in ["plain", "nomime", "json"] {
        let html = format!("<script type=module>import \"/{path}.js\";</script>");
        let (out, log) = run(&html, port, &module_options());
        assert_eq!(
            outcomes(&out),
            vec![failed(SrcFetchErrorKind::UnsupportedMimeType)],
            "{path}"
        );
        assert!(
            !events(&log).iter().any(|e| e.starts_with("eval:")),
            "{path}"
        );
    }
    for path in ["ok1", "ok2"] {
        let html = format!("<script type=module>import \"/{path}.js\";</script>");
        let (out, _) = run(&html, port, &module_options());
        assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded], "{path}");
    }
}

/// `JS-8`・`SEC-2`: 別オリジンへのリダイレクトは送信前に止まり、遷移先は取得されない。
#[test]
fn js_8_cross_origin_redirect_rejected() {
    let (target, target_hits) = leaf_server();
    let (port, _hits) = spawn_server(move |_| {
        response(
            "302 Found",
            &format!("Location: http://127.0.0.1:{target}/t.js\r\n"),
            "",
        )
    });
    let (out, _) = run(
        "<script type=module>import \"/r.js\";</script>",
        port,
        &module_options(),
    );
    assert_eq!(outcomes(&out), vec![failed(SrcFetchErrorKind::CrossOrigin)]);
    assert_eq!(total_hits(&target_hits), 0);
}

/// `JS-8`: 非 2xx は `HttpStatus`、構文エラーは例外で、どちらも後続を続ける。
#[test]
fn js_8_status_and_syntax_errors_continue() {
    let (port, _hits) = spawn_server(|p| match p {
        "/bad.js" => js("syntax-error"),
        _ => response("404 Not Found", "Content-Type: text/html\r\n", "nope"),
    });
    let html = "<script type=module>import \"/missing.js\";</script>\
                <script type=module>import \"/bad.js\";</script>\
                <script>1</script>";
    let (out, _) = run(html, port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![
            failed(SrcFetchErrorKind::HttpStatus { status: 404 }),
            ScriptOutcome::Exception {
                message: "SyntaxError".to_string(),
                truncated: false
            },
            ScriptOutcome::Succeeded,
        ]
    );
}

// ---- 上限 ----

fn many_imports(n: usize) -> String {
    (0..n)
        .map(|i| format!("import \"./m{i}.js\";"))
        .collect::<String>()
}

/// `JS-6`: 件数の上限（既定 64。inline の entry を含む）の境界。
#[test]
fn js_6_max_modules_boundary() {
    let (port, hits) = leaf_server();
    let html = |n: usize| {
        format!(
            "<script type=module>{}</script><script>1</script>",
            many_imports(n)
        )
    };
    // entry 1 + 依存 63 = 64 件は成功。
    let (out, _) = run(&html(63), port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Succeeded, ScriptOutcome::Succeeded]
    );
    // 依存 64 件は 65 件目で打ち切る。
    let before = total_hits(&hits);
    let (out, _) = run(&html(64), port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![
            ScriptOutcome::Aborted {
                kind: AbortKind::ScriptCount { limit: 64 },
                message: None
            },
            ScriptOutcome::NotExecuted {
                kind: AbortKind::ScriptCount { limit: 64 }
            },
        ]
    );
    assert!(
        out.lifecycle().iter().all(|l| matches!(
            l.outcome(),
            crate::page_runner::LifecycleOutcome::NotFired { .. }
        )),
        "{:?}",
        out.lifecycle()
    );
    // 63 件までしか取得しない（64 番目の依存 `m63.js` は取得しない）。
    assert_eq!(total_hits(&hits) - before, 63);
    assert_eq!(hit_count(&hits, "/m63.js"), 0);
}

/// `n` 段目が `n+1` 段目を import し続けるサーバー（`last` 以降は葉）。
fn chain_server(last: usize) -> (u16, Hits) {
    spawn_server(move |p| {
        let n: usize = p
            .trim_start_matches("/d")
            .trim_end_matches(".js")
            .parse()
            .unwrap_or(0);
        if n >= last {
            js("export {};")
        } else {
            js(&format!("import \"./d{}.js\";", n + 1))
        }
    })
}

/// `JS-6`: グラフの深さの上限（32）の境界。33 段目は取得しない。
#[test]
fn js_6_module_graph_depth_boundary() {
    let html = "<script type=module>import \"./d1.js\";</script>";
    // entry + d1..d32（深さ 32）は成功する。
    let (port, hits) = chain_server(32);
    let (out, _) = run(html, port, &module_options());
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
    assert_eq!(hit_count(&hits, "/d32.js"), 1);
    // d32 がさらに d33 を import すると深さ 33 で打ち切り、d33 は取得しない。
    let (port, hits) = chain_server(33);
    let (out, _) = run(html, port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Aborted {
            kind: AbortKind::ResourceLimit,
            message: Some("module graph depth limit exceeded".to_string())
        }]
    );
    assert_eq!(hit_count(&hits, "/d32.js"), 1);
    assert_eq!(hit_count(&hits, "/d33.js"), 0);
}

/// `JS-6`: `src` 1 件のバイト上限の境界（16 B は成功、17 B は打ち切り）。
#[test]
fn js_6_module_per_script_bytes_boundary() {
    let (port, _hits) = spawn_server(|p| match p {
        "/m16.js" => js(&"a".repeat(16)),
        _ => js(&"a".repeat(17)),
    });
    let opts = module_options().with_max_src_script_bytes(16);
    let (out, _) = run(
        "<script type=module>import \"/m16.js\";</script>",
        port,
        &opts,
    );
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
    let (out, _) = run(
        "<script type=module>import \"/m17.js\";</script>",
        port,
        &opts,
    );
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Aborted {
            kind: AbortKind::ScriptBytes {
                scope: ScriptBytesScope::PerScript,
                limit: 16
            },
            message: None
        }]
    );
}

/// `JS-6`: 総量の上限は classic・inline module・依存で共有する（27 B は成功、26 B は打ち切り）。
#[test]
fn js_6_module_total_bytes_boundary() {
    let (port, _hits) = spawn_server(|_| js(&"a".repeat(10)));
    // classic 2 B + inline module 15 B + 依存 10 B = 27 B。
    let html = "<script>ab</script><script type=module>import \"/m.js\";</script>";
    let (out, _) = run(
        html,
        port,
        &module_options().with_max_total_script_bytes(27),
    );
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Succeeded, ScriptOutcome::Succeeded]
    );
    let (out, _) = run(
        html,
        port,
        &module_options().with_max_total_script_bytes(26),
    );
    assert_eq!(
        outcomes(&out),
        vec![
            ScriptOutcome::Succeeded,
            ScriptOutcome::Aborted {
                kind: AbortKind::ScriptBytes {
                    scope: ScriptBytesScope::Total,
                    limit: 26
                },
                message: None
            },
        ]
    );
}

/// `JS-6`: 取得待ちはページの実時間で打ち切られる（応答しないサーバー）。
#[test]
fn js_6_module_fetch_bounded_by_page_wall_time() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let held: Arc<Mutex<Vec<std::net::TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    let held_for_thread = held.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if let Ok(mut h) = held_for_thread.lock() {
                h.push(stream);
            }
        }
    });
    let limit = Duration::from_millis(400);
    let opts = module_options().with_page_wall_time(limit);
    let started = std::time::Instant::now();
    let (out, _) = run(
        "<script type=module>import \"/hang.js\";</script>",
        port,
        &opts,
    );
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Aborted {
            kind: AbortKind::WallTime {
                scope: WallTimeScope::Page,
                limit
            },
            message: None
        }]
    );
    assert!(started.elapsed() < Duration::from_secs(4));
}

// ---- 評価結果 ----

/// `JS-4`: 評価の結果（例外・保留・未対応）を成功と区別し、後続を続ける。
#[test]
fn js_4_module_evaluation_outcomes() {
    let (port, _hits) = leaf_server();
    let long = format!("reject:{}", "x".repeat(5000));
    let html = format!(
        "<script type=module>reject:boom</script>\
         <script type=module>tla-pending</script>\
         <script type=module>unsupported</script>\
         <script type=module>compile-unsupported</script>\
         <script type=module>{long}</script>\
         <script>1</script>"
    );
    let (out, _) = run(&html, port, &module_options());
    let got = outcomes(&out);
    assert_eq!(
        got.get(..4),
        Some(
            &[
                ScriptOutcome::Exception {
                    message: "boom".to_string(),
                    truncated: false
                },
                ScriptOutcome::ModulePending,
                ScriptOutcome::ModuleUnsupported,
                ScriptOutcome::ModuleUnsupported,
            ][..]
        )
    );
    match got.get(4) {
        Some(ScriptOutcome::Exception { message, truncated }) => {
            assert_eq!(message.len(), 4096);
            assert!(truncated);
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(got.get(5), Some(&ScriptOutcome::Succeeded));
    assert!(out.abort().is_none());
}

/// `JS-8`・`SEC-2`: 診断（Debug 出力を含む）に URL・クエリ・ホストを残さない。
#[test]
fn js_8_module_diagnostics_have_no_url() {
    let (port, _hits) = spawn_server(|p| match p {
        "/bad.js" => response("200 OK", "Content-Type: text/plain\r\n", "x"),
        _ => js("export {};"),
    });
    let html = "<script type=module>import \"/bad.js?token=secret\";</script>\
                <script type=module>import \"/ok.js?token=secret\";</script>";
    let (out, _) = run(html, port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![
            failed(SrcFetchErrorKind::UnsupportedMimeType),
            ScriptOutcome::Succeeded
        ]
    );
    // html には入力のスクリプト本文がそのまま残るため、診断の部分だけを検査する。
    let dump = format!(
        "{:?}{:?}{:?}{:?}{:?}{:?}",
        out.scripts(),
        out.abort(),
        out.lifecycle(),
        out.bridge_diagnostics(),
        out.event_loop(),
        out.net()
    );
    for banned in ["secret", "127.0.0.1", "token"] {
        assert!(!dump.contains(banned), "{banned} in {dump}");
    }
}

/// `JS-4`: gate 無効（既定）では module は従来どおり Skipped で、取得もしない。
#[test]
fn js_4_gate_off_keeps_classic_behavior() {
    let (port, hits) = leaf_server();
    let (out, log) = run(
        "<script type=module src=\"/m.js\"></script><script type=module>import \"/x.js\";</script>",
        port,
        &PageRunOptions::default(),
    );
    assert_eq!(
        out.scripts()
            .iter()
            .map(|r| r.outcome().as_str())
            .collect::<Vec<_>>(),
        vec!["skipped", "skipped"]
    );
    assert_eq!(total_hits(&hits), 0);
    assert!(events(&log).is_empty());
}

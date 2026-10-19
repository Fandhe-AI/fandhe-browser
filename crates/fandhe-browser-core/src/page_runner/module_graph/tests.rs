//! `module_graph` の単体テスト（fake エンジン + ローカル HTTP。Issue #1116・`JS-4`・`JS-6`・`JS-8`）。
//!
//! 実エンジン（V8）での結合は `tests/support/page_runner_cases.rs` の `run_module_graph` が担う。

use super::*;
use crate::event_loop::EventLoopLimits;
use crate::fetch::FetchOptions;
use crate::page_runner::{
    PageRunInput, PageRunOptions, PageRunOutput, run_with_fetcher_and_factory,
};
use crate::page_script::ScriptCollectionOptions;
use fandhe_browser_js::{
    DynamicImportId, DynamicImportRequest, EngineKind, JsEngine, JsValue, ModuleRecord,
    ModuleRequest, NativeFn,
};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

// ---- fake エンジン ----

#[derive(Default)]
struct EngineLog {
    /// `compile:<id>:<url>` / `link:<referrer>:<specifier>=<target>` / `eval:<id>`。
    events: Vec<String>,
    sources: HashMap<u32, String>,
    /// `finish_dynamic_import` に渡された残り時間（`options.timeout()`。呼び出し順）。
    finish_timeouts: Vec<Duration>,
}

/// module API を持つテスト用エンジン。`import "<spec>"` の文から依存を作り、ソース中の印
/// （`syntax-error`・`tla-pending`・`reject:<msg>`・`unsupported`・`compile-unsupported`）で
/// 結果を分岐する。classic の評価はすべて成功させる。
///
/// 動的 `import()` の模擬（Issue #1117）: module の本文の `dynamic "<spec>"` 文は evaluate 時に
/// 保留へ積む（referrer はその module）。`dynamic-unknown "<spec>"` は未知の referrer、classic の
/// `dynamic-doc "<spec>"` は `referrer=None`。`no-dyn-support` を含む module を compile すると
/// `take` が `NotYetImplemented` を返す。
struct FakeModuleEngine {
    log: Arc<Mutex<EngineLog>>,
    next_id: u32,
    pending: Vec<DynamicImportRequest>,
    next_dyn: u32,
    take_unsupported: bool,
    /// `slow-take` を含む module を compile すると、要求を返す `take` がこの時間ブロックする。
    take_sleep: Option<Duration>,
    /// `__dom.op` の束縛（checkpoint でタイマーを登録して上限超過を再現する用）。
    op: Option<NativeFn>,
    /// `checkpoint-timers` を含む module を compile すると、checkpoint の評価でタイマーを複数登録する。
    checkpoint_timers: bool,
}

impl FakeModuleEngine {
    /// `tag "<spec>"` 形式の文から specifier を抜き出す。
    fn scan(src: &str, tag: &str) -> Vec<String> {
        src.split([';', '\n'])
            .filter_map(|stmt| {
                stmt.trim()
                    .strip_prefix(tag)
                    .and_then(|s| s.strip_prefix(" \""))
                    .and_then(|s| s.strip_suffix('"'))
                    .map(str::to_string)
            })
            .collect()
    }

    fn push_dynamic(&mut self, referrer: Option<ModuleId>, specs: Vec<String>) {
        for spec in specs {
            self.next_dyn += 1;
            self.pending.push(DynamicImportRequest::new(
                DynamicImportId::from_raw(self.next_dyn),
                referrer,
                spec,
            ));
        }
    }
}

impl JsEngine for FakeModuleEngine {
    fn evaluate_script(
        &mut self,
        script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        if script == "__fandheRunMicrotasks()" && self.checkpoint_timers {
            if let Some(op) = self.op.as_mut() {
                for _ in 0..3 {
                    let _ = op(&[
                        JsValue::String("timerSet".to_string()),
                        JsValue::Number(1.0),
                        JsValue::Bool(false),
                    ]);
                }
            }
            return Ok(JsValue::Undefined);
        }
        let specs = Self::scan(script, "dynamic-doc");
        self.push_dynamic(None, specs);
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
        methods: Vec<(String, NativeFn)>,
    ) -> Result<(), JsEngineError> {
        self.op = methods.into_iter().find(|(n, _)| n == "op").map(|(_, f)| f);
        Ok(())
    }

    fn compile_module(&mut self, url: &str, source: &str) -> Result<ModuleRecord, JsEngineError> {
        if source.contains("syntax-error") {
            return Err(JsEngineError::EvaluationFailed("SyntaxError".to_string()));
        }
        if source.contains("compile-unsupported") {
            return Err(JsEngineError::NotYetImplemented("module".to_string()));
        }
        if source.contains("no-dyn-support") {
            self.take_unsupported = true;
        }
        if source.contains("checkpoint-timers") {
            self.checkpoint_timers = true;
        }
        if source.contains("slow-take") {
            self.take_sleep = Some(Duration::from_millis(400));
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
        // `dynamic-on-compile`: 評価に入れない module（依存解決失敗）でも referrer として引けるかの検査用。
        let early = Self::scan(source, "dynamic-on-compile");
        self.push_dynamic(Some(ModuleId::from_raw(id)), early);
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
        // `slow-link`: link で時間を使い、取得・compile の後でページ時計を尽きさせる。
        if log
            .sources
            .get(&referrer.raw())
            .is_some_and(|src| src.contains("slow-link"))
        {
            std::thread::sleep(Duration::from_millis(500));
        }
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
        drop(log);
        self.push_dynamic(Some(root), Self::scan(&src, "dynamic"));
        self.push_dynamic(
            Some(ModuleId::from_raw(9999)),
            Self::scan(&src, "dynamic-unknown"),
        );
        let log = self.log.lock().expect("log");
        if src.contains("unsupported") {
            return Err(JsEngineError::NotYetImplemented("module".to_string()));
        }
        if src.contains("eval-too-large") {
            return Err(JsEngineError::EvaluationFailed("x".repeat(16)));
        }
        if src.contains("tla-pending") {
            return Ok(ModuleEvaluation::Pending);
        }
        // 1 回目は保留、再 evaluate では確定する（後続の classic が待ちを解いた状況の模擬）。
        let tag = format!("eval:{}", root.raw());
        let seen = log.events.iter().filter(|e| **e == tag).count();
        if src.contains("tla-later-fulfil") {
            return Ok(if seen <= 1 {
                ModuleEvaluation::Pending
            } else {
                ModuleEvaluation::Fulfilled
            });
        }
        if src.contains("tla-later-reject") {
            return Ok(if seen <= 1 {
                ModuleEvaluation::Pending
            } else {
                ModuleEvaluation::rejected("late".to_string())
            });
        }
        if let Some(m) = src.strip_prefix("reject:") {
            return Ok(ModuleEvaluation::rejected(m.to_string()));
        }
        Ok(ModuleEvaluation::Fulfilled)
    }

    fn take_dynamic_import_requests(&mut self) -> Result<Vec<DynamicImportRequest>, JsEngineError> {
        self.log
            .lock()
            .expect("log")
            .events
            .push("take".to_string());
        if self.take_unsupported {
            return Err(JsEngineError::NotYetImplemented("take".to_string()));
        }
        if let Some(sleep) = self.take_sleep
            && !self.pending.is_empty()
        {
            std::thread::sleep(sleep);
        }
        Ok(std::mem::take(&mut self.pending))
    }

    fn finish_dynamic_import(
        &mut self,
        id: DynamicImportId,
        result: Result<ModuleId, String>,
        options: &EvaluateOptions,
    ) -> Result<ModuleEvaluation, JsEngineError> {
        let mut log = self.log.lock().expect("log");
        log.finish_timeouts.push(options.timeout());
        // 対象の本文の印で `import()` の Promise の確定結果を分岐する（V8 の挙動の模擬）。
        let (line, settlement) = match result {
            Ok(target) => {
                let src = log.sources.get(&target.raw()).cloned().unwrap_or_default();
                if src.contains("settle-timeout") {
                    // 評価に入った後の時間切れ（エンジンが reject 済みで打ち切る）の模擬。
                    log.events.push(format!("finish:{}:timeout", id.raw()));
                    return Err(JsEngineError::Timeout("finish timeout".to_string()));
                }
                if src.contains("finish-too-large") {
                    // 拒否メッセージが結果サイズ上限を超えたエンジン側の失敗（Promise は reject 済み）。
                    log.events.push(format!("finish:{}:too-large", id.raw()));
                    return Err(JsEngineError::EvaluationFailed("x".repeat(16)));
                }
                let settlement = if src.contains("settle-throw-ref") {
                    ModuleEvaluation::rejected(
                        "Uncaught ReferenceError: Foo is not defined (line 1)".to_string(),
                    )
                } else if src.contains("settle-throw") {
                    ModuleEvaluation::rejected("boom".to_string())
                } else if src.contains("settle-pending") {
                    ModuleEvaluation::Pending
                } else {
                    ModuleEvaluation::Fulfilled
                };
                (
                    format!("finish:{}:ok:{}", id.raw(), target.raw()),
                    settlement,
                )
            }
            Err(msg) => (
                format!("finish:{}:err:{msg}", id.raw()),
                ModuleEvaluation::rejected(msg),
            ),
        };
        log.events.push(line);
        Ok(settlement)
    }
}

fn fake_factory(log: Arc<Mutex<EngineLog>>) -> impl FnOnce() -> crate::Result<JsRuntime> {
    move || {
        Ok(JsRuntime::from_engine_for_test(
            EngineKind::V8,
            Box::new(FakeModuleEngine {
                log,
                next_id: 0,
                pending: Vec::new(),
                next_dyn: 0,
                take_unsupported: false,
                take_sleep: None,
                op: None,
                checkpoint_timers: false,
            }),
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
        message: "module evaluation failed".to_string(),
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

/// `JS-4`: 保留の module は再参照でエンジンの現在の状態を取り直し、確定した結果へ更新する。
#[test]
fn js_4_pending_module_is_rechecked_on_rereference() {
    let (port, hits) = spawn_server(|path| match path {
        "/f.js" => js("tla-later-fulfil"),
        _ => js("tla-later-reject"),
    });
    let html = "<script type=module src=\"/f.js\"></script>\
                <script type=module src=\"/f.js\"></script>\
                <script type=module src=\"/f.js\"></script>\
                <script type=module src=\"/x.js\"></script>\
                <script type=module src=\"/x.js\"></script>\
                <script type=module src=\"/x.js\"></script>";
    let (out, _) = run(html, port, &module_options());
    let exception = ScriptOutcome::Exception {
        message: "module evaluation failed".to_string(),
        truncated: false,
    };
    assert_eq!(
        outcomes(&out),
        vec![
            ScriptOutcome::ModulePending,
            ScriptOutcome::Succeeded,
            ScriptOutcome::Succeeded,
            ScriptOutcome::ModulePending,
            exception.clone(),
            exception
        ]
    );
    assert_eq!(hit_count(&hits, "/f.js"), 1);
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
                message: "module compile failed".to_string(),
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

/// `JS-6`: 近道の経路があっても、最長の依存経路で深さの上限を検査する。
/// entry が d1 と d2 を import し、d1→d2→…→d33 と続く（d2 への最短は深さ 1）。
#[test]
fn js_6_module_graph_depth_not_bypassed_by_shortcut() {
    let (port, _hits) = chain_server(33);
    let html = "<script type=module>import \"./d1.js\";import \"./d2.js\";</script>";
    let (out, _) = run(html, port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Aborted {
            kind: AbortKind::ResourceLimit,
            message: Some("module graph depth limit exceeded".to_string())
        }]
    );
    // 深さ 32 まで（d1..d32 が chain、d2 への近道あり）は成功する。
    let (port, _hits) = chain_server(32);
    let (out, _) = run(html, port, &module_options());
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
}

/// `JS-8`・`SEC-2`: 失敗の診断にエンジンの生メッセージ（specifier・クエリ）を残さない。
#[test]
fn js_8_module_failure_messages_do_not_leak_engine_text() {
    let (port, _hits) = spawn_server(|p| match p {
        "/bad.js" => js("syntax-error ./dep.js?token=secret"),
        _ => js("export {};"),
    });
    let html = "<script type=module>reject:SyntaxError ./dep.js?token=secret</script>\
                <script type=module>import \"/bad.js?token=secret\";</script>";
    let (out, _) = run(html, port, &module_options());
    let dump = format!("{:?}", out.scripts());
    assert!(!dump.contains("secret"), "{dump}");
    assert!(!dump.contains("token"), "{dump}");
    assert_eq!(outcomes(&out).len(), 2);
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
                    message: "module evaluation failed".to_string(),
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
            assert_eq!(message, "module evaluation failed");
            assert!(!truncated);
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

/// `JS-6`: 循環の祖先を除いて求めた高さを別経路へ再利用せず、最長経路で深さを検査する。
/// entry→X→A、entry→Y→B、A→B、A→E1→…→E30、B→A。
/// entry→Y→B→A→E1→…→E30 は深さ 34 で上限（32）を超える。
#[test]
fn js_6_module_graph_depth_not_underestimated_by_cycle_memo() {
    let (port, _hits) = spawn_server(|p| match p {
        "/x.js" => js("import \"./a.js\";"),
        "/y.js" => js("import \"./b.js\";"),
        "/a.js" => js("import \"./b.js\";import \"./e1.js\";"),
        "/b.js" => js("import \"./a.js\";"),
        _ => {
            let n: usize = p
                .trim_start_matches("/e")
                .trim_end_matches(".js")
                .parse()
                .unwrap_or(0);
            if n >= 30 {
                js("export {};")
            } else {
                js(&format!("import \"./e{}.js\";", n + 1))
            }
        }
    });
    let html = "<script type=module>import \"./x.js\";import \"./y.js\";</script>";
    let (out, _) = run(html, port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![ScriptOutcome::Aborted {
            kind: AbortKind::ResourceLimit,
            message: Some("module graph depth limit exceeded".to_string())
        }]
    );
}

/// 評価の `EvaluationFailed`（拒否メッセージの上限超過等）はページを打ち切らず、
/// 固定メッセージの例外として後続を続ける。
#[test]
fn js_4_module_evaluation_failed_continues() {
    let (port, _hits) = leaf_server();
    let html = "<script type=module>eval-too-large</script><script>1</script>";
    let (out, _) = run(html, port, &module_options());
    assert_eq!(
        outcomes(&out),
        vec![
            ScriptOutcome::Exception {
                message: "module evaluation failed".to_string(),
                truncated: false
            },
            ScriptOutcome::Succeeded
        ]
    );
    assert!(out.abort().is_none());
}

/// `JS-6`: 20 module の完全相互 import でも深さ検査は多項式時間で終わり、成功する
/// （循環は 1 つの塊として扱い、単純経路を列挙しない）。
#[test]
fn js_6_module_graph_complete_mutual_imports_terminate() {
    let (port, _hits) = spawn_server(|_| {
        let body: String = (0..20).map(|i| format!("import \"./c{i}.js\";")).collect();
        js(&body)
    });
    let html = "<script type=module>import \"./c0.js\";</script>";
    let started = std::time::Instant::now();
    let (out, _) = run(html, port, &module_options().with_max_modules(64));
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
}

// ---- 動的 import（Issue #1117） ----

fn finishes(log: &Arc<Mutex<EngineLog>>) -> Vec<String> {
    events(log)
        .into_iter()
        .filter(|e| e.starts_with("finish:"))
        .collect()
}

/// `JS-4`: gate 無効（既定）では `take` を呼ばず、サーバーにも到達しない。
#[test]
fn js_4_dynamic_import_gate_off_never_takes() {
    let (port, hits) = leaf_server();
    let html = "<script>1</script><script type=module>dynamic \"/c.js\"</script>";
    let (out, log) = run(html, port, &PageRunOptions::default());
    assert!(!events(&log).contains(&"take".to_string()));
    assert_eq!(total_hits(&hits), 0);
    assert_eq!(out.dynamic_imports().requested(), 0);
    assert_eq!(out.dynamic_imports().fulfilled(), 0);
    assert!(out.dynamic_imports().failures().is_empty());
}

/// `JS-4`・`JS-8`: 動的 import の対象を 1 回だけ取得し、compile して `ok` で完了させる。
#[test]
fn js_4_dynamic_import_fetches_and_finishes_ok() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>dynamic \"/c.js\"</script>";
    let (out, log) = run(html, port, &module_options());
    assert_eq!(outcomes(&out), vec![ScriptOutcome::Succeeded]);
    assert_eq!(hit_count(&hits, "/c.js"), 1);
    // inline の root が id 1、動的に取得した対象が id 2。
    assert_eq!(finishes(&log), vec!["finish:1:ok:2".to_string()]);
    let r = out.dynamic_imports();
    assert_eq!((r.requested(), r.fulfilled(), r.rejected()), (1, 1, 0));
    assert!(out.abort().is_none());
}

/// `JS-4`: classic スクリプトからの `import()`（`referrer=None`）は文書の base URL で解決する。
#[test]
fn js_4_dynamic_import_from_classic_uses_document_base() {
    let (port, hits) = leaf_server();
    let html = "<script>dynamic-doc \"./c.js\"</script>";
    let (out, log) = run(html, port, &module_options());
    assert_eq!(hit_count(&hits, "/c.js"), 1);
    assert_eq!(finishes(&log), vec!["finish:1:ok:1".to_string()]);
    assert_eq!(out.dynamic_imports().fulfilled(), 1);
}

/// `JS-8`・`SEC-2`: 拒否は種別だけを `Err` と診断に残す（URL・ポート・パスを含めない）。
#[test]
fn js_8_dynamic_import_rejections_have_kind_only() {
    let (other_port, other_hits) = leaf_server();
    let (port, _hits) = spawn_server(|p| match p {
        "/text.js" => response("200 OK", "Content-Type: text/plain\r\n", "export {};"),
        "/bad.js" => js("syntax-error"),
        _ => response("404 Not Found", "", ""),
    });
    let html = format!(
        "<script type=module>dynamic \"foo\";dynamic \"http://127.0.0.1:{other_port}/secret.js?t=1\";\
         dynamic \"/text.js\";dynamic \"/missing.js\";dynamic \"/bad.js\"</script>"
    );
    let (out, log) = run(&html, port, &module_options());
    assert_eq!(
        finishes(&log),
        vec![
            "finish:1:err:dynamic import failed: bare_specifier",
            "finish:2:err:dynamic import failed: cross_origin",
            "finish:3:err:dynamic import failed: unsupported_mime_type",
            "finish:4:err:dynamic import failed: http_status",
            "finish:5:err:dynamic import failed: module_compile_failed",
        ]
    );
    for line in finishes(&log) {
        assert!(
            !line.contains("127.0.0.1") && !line.contains("secret"),
            "{line}"
        );
    }
    assert_eq!(total_hits(&other_hits), 0);
    let r = out.dynamic_imports();
    assert_eq!((r.requested(), r.fulfilled(), r.rejected()), (5, 0, 5));
    assert_eq!(
        r.failures(),
        &[
            DynamicImportFailureKind::Fetch(SrcFetchErrorKind::BareSpecifier),
            DynamicImportFailureKind::Fetch(SrcFetchErrorKind::CrossOrigin),
            DynamicImportFailureKind::Fetch(SrcFetchErrorKind::UnsupportedMimeType),
            DynamicImportFailureKind::Fetch(SrcFetchErrorKind::HttpStatus { status: 404 }),
            DynamicImportFailureKind::CompileFailed,
        ]
    );
    assert!(out.abort().is_none());
}

/// `JS-8`: 静的 import 済みの URL は再取得せず、その id で完了させる。
#[test]
fn js_8_dynamic_import_reuses_static_module() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>import \"/s.js\";dynamic \"/s.js\"</script>";
    let (out, log) = run(html, port, &module_options());
    assert_eq!(hit_count(&hits, "/s.js"), 1);
    assert_eq!(finishes(&log), vec!["finish:1:ok:2".to_string()]);
    assert_eq!(out.dynamic_imports().fulfilled(), 1);
}

/// `JS-6`: 動的 import も `max_modules` の枠を使う。静的との合計が上限以内なら成功、超えたら打ち切り。
#[test]
fn js_6_dynamic_import_counts_toward_max_modules() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>import \"/a.js\";dynamic \"/d.js\"</script>";
    // inline + a.js + d.js = 3 件。
    let (out, _) = run(html, port, &module_options().with_max_modules(3));
    assert_eq!(hit_count(&hits, "/d.js"), 1);
    assert_eq!(out.dynamic_imports().fulfilled(), 1);
    assert!(out.abort().is_none());

    let (port, hits) = leaf_server();
    let (out, log) = run(html, port, &module_options().with_max_modules(2));
    assert_eq!(hit_count(&hits, "/d.js"), 0);
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ScriptCount { limit: 2 })
    );
    // reject してから打ち切る。
    assert_eq!(
        finishes(&log),
        vec!["finish:1:err:dynamic import failed: script_count".to_string()]
    );
    assert_eq!(out.dynamic_imports().rejected(), 1);
}

/// `JS-6`・Issue #1117: 打ち切りで取り出し済みの未処理要求が残っても、全件を requested と rejected に計上する。
#[test]
fn js_6_dynamic_import_abort_counts_remaining_queue() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>dynamic \"/d1.js\";dynamic \"/d2.js\"</script>";
    // inline のみで枠が埋まり、最初の動的 import で max_modules 超過になる。
    let (out, _) = run(html, port, &module_options().with_max_modules(1));
    assert_eq!(hit_count(&hits, "/d1.js"), 0);
    assert_eq!(hit_count(&hits, "/d2.js"), 0);
    let r = out.dynamic_imports();
    assert_eq!((r.requested(), r.fulfilled(), r.rejected()), (2, 0, 2));
    assert_eq!(
        r.failures(),
        &[
            DynamicImportFailureKind::Limit(AbortKind::ScriptCount { limit: 1 }),
            DynamicImportFailureKind::Limit(AbortKind::ScriptCount { limit: 1 }),
        ]
    );
}

/// `JS-6`: キャッシュに当たる繰り返しでも、要求数の上限を超えたら打ち切る。
#[test]
fn js_6_dynamic_import_request_limit() {
    let (port, hits) = leaf_server();
    let body = "dynamic \"/s.js\";".repeat(MAX_PAGE_DYNAMIC_IMPORTS + 1);
    let html = format!("<script type=module>{body}</script>");
    let (out, _) = run(&html, port, &module_options());
    assert_eq!(hit_count(&hits, "/s.js"), 1);
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ResourceLimit)
    );
    let r = out.dynamic_imports();
    assert_eq!(r.requested(), MAX_PAGE_DYNAMIC_IMPORTS + 1);
    assert_eq!(r.fulfilled(), MAX_PAGE_DYNAMIC_IMPORTS);
    assert_eq!(r.rejected(), 1);
}

/// `JS-6`: 動的に取得する本文も総バイト数の枠に含める（境界は「ちょうどなら成功」）。
#[test]
fn js_6_dynamic_import_total_bytes() {
    let inline = "dynamic \"/d.js\"";
    let (port, _hits) = spawn_server(|_| js(&"a".repeat(10)));
    let html = format!("<script type=module>{inline}</script>");
    let exact = inline.len() + 10;
    let (out, _) = run(
        &html,
        port,
        &module_options().with_max_total_script_bytes(exact),
    );
    assert!(out.abort().is_none());
    assert_eq!(out.dynamic_imports().fulfilled(), 1);

    let (out, log) = run(
        &html,
        port,
        &module_options().with_max_total_script_bytes(exact - 1),
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::ScriptBytes {
            scope: ScriptBytesScope::Total,
            limit: exact - 1
        })
    );
    assert_eq!(
        finishes(&log),
        vec!["finish:1:err:dynamic import failed: script_bytes".to_string()]
    );
}

/// `JS-6`: 動的 import の取得待ちもページの実時間で打ち切る（reject せず打ち切る）。
#[test]
fn js_6_dynamic_import_fetch_bounded_by_page_wall_time() {
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
    let (out, log) = run(
        "<script type=module>dynamic \"/hang.js\"</script>",
        port,
        &opts,
    );
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit
        })
    );
    assert!(finishes(&log).is_empty());
    assert!(started.elapsed() < Duration::from_secs(4));
}

/// `JS-4`: 依存解決に失敗した module も referrer として登録される（後続 module の動的 import に影響しない）。
#[test]
fn js_4_dynamic_import_referrer_registered_even_if_deps_fail() {
    let (port, _hits) = spawn_server(|p| match p {
        "/ok.js" => js("export {};"),
        _ => response("404 Not Found", "", ""),
    });
    let html = "<script type=module>import \"bare\";dynamic-on-compile \"/ok.js\"</script>\
                <script type=module>dynamic \"/ok.js\"</script>";
    let (out, log) = run(html, port, &module_options());
    assert!(
        !finishes(&log)
            .iter()
            .any(|l| l.contains("unknown_referrer")),
        "{:?}",
        finishes(&log)
    );
    assert!(
        !out.dynamic_imports()
            .failures()
            .contains(&DynamicImportFailureKind::UnknownReferrer)
    );
    assert!(out.dynamic_imports().fulfilled() >= 1);
}

/// `JS-4`: 未知の referrer は fail-closed で reject する（取得しない）。
#[test]
fn js_4_dynamic_import_unknown_referrer_rejected() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>dynamic-unknown \"/c.js\"</script>";
    let (out, log) = run(html, port, &module_options());
    assert_eq!(total_hits(&hits), 0);
    assert_eq!(
        finishes(&log),
        vec!["finish:1:err:dynamic import failed: unknown_referrer".to_string()]
    );
    assert_eq!(
        out.dynamic_imports().failures(),
        &[DynamicImportFailureKind::UnknownReferrer]
    );
    assert!(out.abort().is_none());
}

/// `JS-4`: `take` が未対応のエンジン（boa 等）は「要求なし」とし、以降は呼ばない。
#[test]
fn js_4_dynamic_import_take_not_yet_implemented_is_noop() {
    let (port, hits) = leaf_server();
    let html = "<script type=module>no-dyn-support;dynamic \"/c.js\"</script>";
    let (out, log) = run(html, port, &module_options());
    let takes = events(&log).iter().filter(|e| *e == "take").count();
    assert_eq!(takes, 1);
    assert_eq!(total_hits(&hits), 0);
    assert_eq!(out.dynamic_imports().requested(), 0);
    assert!(out.abort().is_none());
}

/// `JS-4`・Issue #1117: 対象が評価で throw して `import()` が reject された場合は、完了の通知が
/// 受理されても resolve として数えず、`EvaluationFailed` として記録する。
#[test]
fn js_4_dynamic_import_rejected_by_evaluation_is_not_counted_fulfilled() {
    let (port, _hits) = spawn_server(|_| js("settle-throw"));
    let html = "<script type=module>dynamic \"/t.js\"</script>";
    let (out, _) = run(html, port, &module_options());
    let r = out.dynamic_imports();
    assert_eq!(
        (r.requested(), r.fulfilled(), r.rejected(), r.pending()),
        (1, 0, 1, 0)
    );
    assert_eq!(r.failures(), &[DynamicImportFailureKind::EvaluationFailed]);
    assert!(out.abort().is_none());
}

/// `JS-5`・Issue #1133: 動的 import の評価失敗は、種別と同数・同順で例外の要約を残す。
#[test]
fn js_5_dynamic_import_evaluation_error_is_summarized() {
    let (port, _hits) = spawn_server(|path| match path {
        "/r.js" => js("settle-throw-ref"),
        "/b.js" => js("settle-throw"),
        _ => js("finish-too-large"),
    });
    let html = "<script type=module>dynamic \"/r.js\"</script>\
                <script type=module>dynamic \"/b.js\"</script>\
                <script type=module>dynamic \"/l.js\"</script>";
    let (out, _) = run(html, port, &module_options());
    let r = out.dynamic_imports();
    assert_eq!(r.failures().len(), 3);
    let got: Vec<_> = r
        .evaluation_errors()
        .iter()
        .map(|e| {
            (
                e.kind().as_str(),
                e.message_class().as_str(),
                e.identifier(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("ReferenceError", "not_defined", Some("Foo")),
            ("other", "other", None),
            ("other", "other", None),
        ]
    );
}

/// `JS-5`・Issue #1133: module 評価失敗は固定文の `outcome` を保ったまま、要約を記録に載せる。
/// 再参照でも同じ要約を返し、メッセージ中の URL・クエリは `Debug` にも出ない。
#[test]
fn js_5_module_evaluation_failure_carries_exception_summary() {
    let (port, _hits) = spawn_server(|path| match path {
        "/r.js" => {
            js("reject:Uncaught ReferenceError: IntersectionObserver is not defined (line 3)")
        }
        "/u.js" => js("reject:Uncaught Error: https://h.example/p?token=SECRET"),
        _ => js("1"),
    });
    let html = "<script type=module src=\"/r.js\"></script>\
                <script type=module src=\"/r.js\"></script>\
                <script type=module src=\"/u.js\"></script>\
                <script type=module>1</script>";
    let (out, _) = run(html, port, &module_options());
    let fixed = ScriptOutcome::Exception {
        message: "module evaluation failed".to_string(),
        truncated: false,
    };
    assert_eq!(
        outcomes(&out),
        vec![
            fixed.clone(),
            fixed.clone(),
            fixed,
            ScriptOutcome::Succeeded
        ]
    );
    let sums: Vec<_> = out
        .scripts()
        .iter()
        .map(|r| {
            r.exception_summary().map(|e| {
                (
                    e.kind().as_str(),
                    e.message_class().as_str(),
                    e.identifier(),
                )
            })
        })
        .collect();
    assert_eq!(
        sums,
        vec![
            Some((
                "ReferenceError",
                "not_defined",
                Some("IntersectionObserver")
            )),
            Some((
                "ReferenceError",
                "not_defined",
                Some("IntersectionObserver")
            )),
            Some(("Error", "other", None)),
            None,
        ]
    );
    let dbg = format!("{:?}", out.scripts());
    assert!(!dbg.contains("SECRET") && !dbg.contains("https"));
}

/// `JS-5`・`REPAIR-4`・Issue #1140: `in` 演算子の TypeError は分類とプロパティ名が
/// `exception_summary()` まで届き、`identifier`（不足 API 名）には入らない。
#[test]
fn js_5_in_operator_type_error_carries_property() {
    let (port, _hits) = spawn_server(|path| match path {
        "/i.js" => js(
            "reject:Uncaught TypeError: Cannot use 'in' operator to search for 'WebkitAnimation' in undefined (line 1)",
        ),
        _ => js("1"),
    });
    let html = "<script type=module src=\"/i.js\"></script>";
    let (out, _) = run(html, port, &module_options());
    let sum = out
        .scripts()
        .first()
        .and_then(|r| r.exception_summary())
        .map(|e| {
            (
                e.kind().as_str(),
                e.message_class().as_str(),
                e.identifier(),
                e.property(),
            )
        });
    assert_eq!(
        sum,
        Some((
            "TypeError",
            "cannot_use_in_operator",
            None,
            Some("WebkitAnimation")
        ))
    );
}

/// `JS-5`・`REPAIR-4`・Issue #1145: Minified React error は分類と番号だけが
/// `exception_summary()` まで届き、案内 URL・クエリ・トークンは届かない。
#[test]
fn js_5_minified_react_error_carries_code_only() {
    let (port, _hits) = spawn_server(|path| match path {
        "/r.js" => js(
            "reject:Uncaught Error: Minified React error #299; visit https://h.example/errors/299?token=SECRET for the full message. (line 9)",
        ),
        _ => js("1"),
    });
    let html = "<script type=module src=\"/r.js\"></script>";
    let (out, _) = run(html, port, &module_options());
    let e = out
        .scripts()
        .first()
        .and_then(|r| r.exception_summary())
        .expect("summary");
    assert_eq!(e.kind().as_str(), "Error");
    assert_eq!(e.message_class().as_str(), "minified_react_error");
    assert_eq!(e.code(), Some("299"));
    assert_eq!(e.identifier(), None);
    assert_eq!(e.property(), None);
    let dbg = format!("{e:?}");
    assert!(!dbg.contains("SECRET") && !dbg.contains("https") && !dbg.contains("token"));
}

/// `JS-4`・Issue #1117: top-level await 等で未確定のままの `import()` は resolve にも reject にも
/// 数えず、`pending` に数える。
#[test]
fn js_4_dynamic_import_pending_target_is_counted_pending() {
    let (port, _hits) = spawn_server(|_| js("settle-pending"));
    let html = "<script type=module>dynamic \"/p.js\"</script>";
    let (out, _) = run(html, port, &module_options());
    let r = out.dynamic_imports();
    assert_eq!(
        (r.requested(), r.fulfilled(), r.rejected(), r.pending()),
        (1, 0, 0, 1)
    );
    assert!(r.failures().is_empty());
    assert!(out.abort().is_none());
}

/// `JS-6`・Issue #1117: 完了の評価・checkpoint には、固定値ではなくページの残り時間
/// （スクリプト単位の上限とページの残りの小さい方）を渡す。
#[test]
fn js_6_dynamic_import_finish_receives_remaining_budget() {
    let (port, _hits) = leaf_server();
    let html = "<script type=module>dynamic \"/c.js\"</script>";
    let script_limit = Duration::from_millis(1500);
    let opts = module_options()
        .with_script_wall_time(script_limit)
        .with_page_wall_time(Duration::from_secs(60));
    let (_, log) = run(html, port, &opts);
    let timeouts = log.lock().expect("log").finish_timeouts.clone();
    assert_eq!(timeouts, vec![script_limit]);

    // ページの残りの方が小さければ、その残りで丸める。
    let (port, _hits) = leaf_server();
    let page_limit = Duration::from_secs(3);
    let opts = module_options()
        .with_script_wall_time(Duration::from_secs(60))
        .with_page_wall_time(page_limit);
    let (_, log) = run(html, port, &opts);
    let timeouts = log.lock().expect("log").finish_timeouts.clone();
    assert_eq!(timeouts.len(), 1);
    assert!(
        timeouts.iter().all(|t| *t <= page_limit && !t.is_zero()),
        "{timeouts:?}"
    );
}

/// `JS-6`・Issue #1117: 取り出した時点でページの実時間が尽きていても、残った `import()` を数えて
/// reject 扱いにし、ページを打ち切る（成功終了にしない）。
#[test]
fn js_6_dynamic_import_requests_left_when_clock_expired_are_counted_and_aborted() {
    let (port, hits) = leaf_server();
    let limit = Duration::from_millis(200);
    let html = "<script type=module>slow-take;dynamic \"/c.js\"</script>";
    let (out, log) = run(html, port, &module_options().with_page_wall_time(limit));
    assert_eq!(
        out.abort().map(|a| a.kind().clone()),
        Some(AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit
        })
    );
    assert_eq!(total_hits(&hits), 0);
    assert!(finishes(&log).is_empty());
    let r = out.dynamic_imports();
    assert_eq!((r.requested(), r.fulfilled(), r.rejected()), (1, 0, 1));
    assert_eq!(
        r.failures(),
        &[DynamicImportFailureKind::Limit(AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit
        })]
    );
}

/// `JS-6`・Issue #1117: 残りのページ時間がスクリプト上限を下回る状態で、完了の評価が実際に
/// タイムアウトした場合は reject 済みのため、`rejected` と `Limit(WallTime)` に計上して打ち切る。
#[test]
fn js_6_dynamic_import_page_scope_eval_timeout_is_counted_rejected() {
    let (port, _hits) = spawn_server(|_| js("settle-timeout"));
    let html = "<script type=module>dynamic \"/c.js\"</script>";
    let limit = Duration::from_secs(3);
    let opts = module_options()
        .with_script_wall_time(Duration::from_secs(60))
        .with_page_wall_time(limit);
    let (out, _log) = run(html, port, &opts);
    let kind = AbortKind::WallTime {
        scope: WallTimeScope::Page,
        limit,
    };
    assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind.clone()));
    let r = out.dynamic_imports();
    assert_eq!((r.requested(), r.fulfilled(), r.rejected()), (1, 0, 1));
    assert_eq!(r.failures(), &[DynamicImportFailureKind::Limit(kind)]);
}

/// `JS-6`・Issue #1117: 取得・link の後でページ時計が尽きて完了を呼べなくても、要求は rejected に計上する
/// （requested = fulfilled + rejected + pending）。
#[test]
fn js_6_dynamic_import_clock_expired_before_finish_is_counted_rejected() {
    let (port, _hits) = spawn_server(|_| js("slow-link"));
    let html = "<script type=module>dynamic \"/s.js\"</script>";
    let limit = Duration::from_millis(300);
    let opts = module_options().with_page_wall_time(limit);
    let (out, _) = run(html, port, &opts);
    let kind = AbortKind::WallTime {
        scope: WallTimeScope::Page,
        limit,
    };
    assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind.clone()));
    let r = out.dynamic_imports();
    assert_eq!(
        (r.requested(), r.fulfilled(), r.rejected(), r.pending()),
        (1, 0, 1, 0)
    );
    assert_eq!(r.failures(), &[DynamicImportFailureKind::Limit(kind)]);
}

/// `JS-6`・Issue #1117: 結果サイズ上限を超える拒否は、その要求だけの reject として数え、ページは継続する
/// （静的 evaluate_root・classic 評価と同じ扱い）。
#[test]
fn js_6_dynamic_import_oversized_rejection_continues_page() {
    let (port, _hits) = spawn_server(|path| {
        if path == "/big.js" {
            js("finish-too-large")
        } else {
            js("1")
        }
    });
    let html = "<script type=module>dynamic \"/big.js\";dynamic \"/ok.js\"</script>";
    let (out, _) = run(html, port, &module_options());
    assert!(out.abort().is_none());
    let r = out.dynamic_imports();
    assert_eq!(
        (r.requested(), r.fulfilled(), r.rejected(), r.pending()),
        (2, 1, 1, 0)
    );
    assert_eq!(r.failures(), &[DynamicImportFailureKind::EvaluationFailed]);
}

/// `JS-6`・Issue #1117: checkpoint で資源上限に達して打ち切るとき、取り出し済みの残り要求を rejected に
/// 計上する（requested = fulfilled + rejected + pending）。
#[test]
fn js_6_dynamic_import_checkpoint_limit_counts_remaining_queue() {
    let (port, _hits) = spawn_server(|path| {
        if path == "/a.js" {
            js("checkpoint-timers")
        } else {
            js("1")
        }
    });
    let html = "<script type=module>dynamic \"/a.js\";dynamic \"/b.js\"</script>";
    let opts = module_options()
        .with_event_loop_limits(EventLoopLimits::default().with_max_registered_timers(1));
    let (out, _) = run(html, port, &opts);
    let kind = AbortKind::RegisteredTimers { limit: 1 };
    assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind.clone()));
    let r = out.dynamic_imports();
    assert_eq!(
        (r.requested(), r.fulfilled(), r.rejected(), r.pending()),
        (2, 1, 1, 0)
    );
    assert_eq!(r.failures(), &[DynamicImportFailureKind::Limit(kind)]);
}
/// `JS-5`・`REPAIR-4`・`SEC-2`・Issue #1152: 既定（opt-in なし）では生メッセージを一切保持しない。
/// 固定文の `outcome` と要約は従来どおりで、`Debug` にも本文は出ない。
#[test]
fn js_5_raw_exception_message_absent_by_default() {
    let (port, _hits) = spawn_server(|path| match path {
        "/u.js" => js("reject:Uncaught Error: https://h.example/p?token=SECRET"),
        _ => js("settle-throw"),
    });
    let html = "<script type=module src=\"/u.js\"></script>\
                <script type=module>dynamic \"/t.js\"</script>";
    let (out, _) = run(html, port, &module_options());
    assert!(!module_options().retains_raw_exception_messages());
    assert!(
        out.scripts()
            .iter()
            .all(|r| r.raw_exception_message().is_none())
    );
    assert!(out.dynamic_imports().raw_evaluation_messages().is_empty());
    assert_eq!(out.dynamic_imports().evaluation_errors().len(), 1);
    let dbg = format!("{:?}{:?}", out.scripts(), out.dynamic_imports());
    assert!(!dbg.contains("SECRET") && !dbg.contains("https"));
}

/// `JS-5`・`REPAIR-4`・`SEC-2`・Issue #1152: opt-in 時は 300 文字（char 境界）に切った生メッセージを
/// 静的 root（再参照を含む）に保持し、`Debug` には本文を出さない。
#[test]
fn js_5_raw_exception_message_retained_when_opted_in() {
    let long = format!("Uncaught Error: {}", "あ".repeat(400));
    let long_js = format!("reject:{long}");
    let (port, _hits) = spawn_server(move |path| match path {
        "/u.js" => js("reject:Uncaught Error: https://h.example/p?token=SECRET"),
        "/l.js" => js(&long_js),
        _ => js("1"),
    });
    let html = "<script type=module src=\"/u.js\"></script>\
                <script type=module src=\"/u.js\"></script>\
                <script type=module src=\"/l.js\"></script>";
    let opts = module_options().with_retain_raw_exception_messages(true);
    let (out, _) = run(html, port, &opts);
    let raws: Vec<_> = out
        .scripts()
        .iter()
        .map(|r| r.raw_exception_message().map(str::to_owned))
        .collect();
    assert_eq!(
        raws.first().cloned().flatten().as_deref(),
        Some("Uncaught Error: https://h.example/p?token=SECRET")
    );
    assert_eq!(raws.first(), raws.get(1));
    let clipped = raws.get(2).cloned().flatten().unwrap_or_default();
    assert_eq!(clipped.chars().count(), 300);
    assert!(clipped.starts_with("Uncaught Error: あ"));
    let dbg = format!("{:?}", out.scripts());
    assert!(!dbg.contains("SECRET") && dbg.contains("redacted"));
}

/// `JS-5`・`SEC-2`・Issue #1152: 動的 import の生メッセージは `evaluation_errors` と同数・同順。
#[test]
fn js_5_dynamic_import_raw_messages_align_with_evaluation_errors() {
    let (port, _hits) = spawn_server(|path| match path {
        "/r.js" => js("settle-throw-ref"),
        "/b.js" => js("settle-throw"),
        _ => js("finish-too-large"),
    });
    let html = "<script type=module>dynamic \"/r.js\"</script>\
                <script type=module>dynamic \"/b.js\"</script>\
                <script type=module>dynamic \"/l.js\"</script>";
    let opts = module_options().with_retain_raw_exception_messages(true);
    let (out, _) = run(html, port, &opts);
    let r = out.dynamic_imports();
    assert_eq!(r.evaluation_errors().len(), 3);
    assert_eq!(
        r.raw_evaluation_messages(),
        vec![
            Some("Uncaught ReferenceError: Foo is not defined (line 1)"),
            Some("boom"),
            Some(""),
        ]
    );
}

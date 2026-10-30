//! V8 / boa 共通の `Runtime.evaluate`・`Runtime.releaseObject`・`Runtime.callFunctionOn`・`awaitPromise` の
//! 結合テストケース（TASK-44.7・#1218・TASK-44.8・#1219・`DOM.describeNode`/`resolveNode`（#1348）・
//! ビヘイビア `CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・関連 `JS-6`・`PERF-7`）。
//!
//! `tests/runtime_evaluate_v8.rs` と `tests/runtime_evaluate_boa.rs` が `#[path]` で取り込む。実エンジンの
//! 子プロセスを使うため、`main` の先頭で `run_js_worker_if_requested` を呼ぶ独自 `main` が前提
//! （`harness = false`）。外部ネットワークは使わず、`127.0.0.1` のローカル HTTP フィクスチャだけを相手にする。
//! 期待値は具体値で書き、時間アサーションの安定を優先してケースは直列に実行する。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fandhe_browser_cdp::test_support::RuntimeDispatcher;
use fandhe_browser_cdp::{CdpState, SessionId, TargetId, TargetKind};
use fandhe_browser_core::{AppState, Config, ScriptContextHandle};
use fandhe_browser_profile::Profile;
use serde_json::{Value, json};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// 一時プロファイルディレクトリ。成否に関わらず Drop で削除する。
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let base = fandhe_browser_profile::test_base_dir();
        Self(base.join(format!(
            "fandhe-cdp-runtime-eval-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `127.0.0.1` の空きポートで固定の HTML を返す。ヘッダ読み取りは 8 KiB で打ち切る。
fn serve(body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
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
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    port
}

const PAGE_HTML: &str = "<html><body><p id=\"x\">hello</p></body></html>";

/// 評価器つきの CDP 状態と、ディスパッチャ・フィクスチャを束ねたもの。
struct Cdp {
    st: Arc<CdpState>,
    d: RuntimeDispatcher,
    port: u16,
    next_id: i64,
    _dir: TempDir,
}

/// 1 つのターゲットとそのセッション。
struct Page {
    tid: TargetId,
    sid: SessionId,
}

impl Cdp {
    fn new(engine: &str) -> Self {
        let cfg = Config::from_toml_str(&format!("[js]\nengine = \"{engine}\"\n")).expect("config");
        let handle = ScriptContextHandle::new(cfg.js());
        assert!(
            handle.is_enabled(),
            "evaluator must be enabled for {engine}"
        );
        Self::with(handle, RuntimeDispatcher::new().expect("dispatcher"))
    }

    fn with(handle: ScriptContextHandle, d: RuntimeDispatcher) -> Self {
        let dir = TempDir::new();
        let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
        let app = AppState::with_disabled_renderer(profile).with_script_contexts(handle);
        Self {
            st: Arc::new(CdpState::new(Arc::new(app))),
            d,
            port: serve(PAGE_HTML),
            next_id: 0,
            _dir: dir,
        }
    }

    /// 配信する HTML を差し替える（`PageScriptHandle` は持たないので、ページの `<script>` は実行されない）。
    fn with_html(mut self, body: &'static str) -> Self {
        self.port = serve(body);
        self
    }

    fn worker_spawns(&self) -> u64 {
        self.st
            .app_state()
            .script_contexts()
            .stats()
            .worker_spawns()
    }

    /// 新しいページターゲットとセッション（`Runtime.enable` 済み）。
    async fn page(&mut self) -> Page {
        let reg = self.st.registry();
        let tid = reg
            .create_target(TargetKind::Page, "about:blank")
            .expect("create target");
        let sid = reg.attach(&tid).expect("attach");
        let p = Page { tid, sid };
        let f = self.call(&p, "Runtime.enable", json!({})).await;
        assert_eq!(f.len(), 1, "no events before the first document: {f:?}");
        p
    }

    async fn call(&mut self, p: &Page, method: &str, params: Value) -> Vec<Value> {
        self.next_id += 1;
        let req = json!({
            "id": self.next_id,
            "method": method,
            "sessionId": p.sid.as_str(),
            "params": params,
        });
        self.d
            .dispatch(&self.st, &req.to_string())
            .await
            .iter()
            .map(|f| serde_json::from_str(f).expect("json frame"))
            .collect()
    }

    /// フィクスチャへ遷移し、フレーム列を返す。
    async fn navigate(&mut self, p: &Page) -> Vec<Value> {
        let url = format!("http://127.0.0.1:{}/", self.port);
        self.call(p, "Page.navigate", json!({"url": url})).await
    }

    /// 名前付き isolated world を作り、contextId を返す。
    async fn isolated(&mut self, p: &Page, name: &str) -> u64 {
        let params = json!({"frameId": p.tid.as_str(), "worldName": name});
        let f = self.call(p, "Page.createIsolatedWorld", params).await;
        f.last()
            .and_then(|r| r["result"]["executionContextId"].as_u64())
            .unwrap_or_else(|| panic!("createIsolatedWorld failed: {f:?}"))
    }

    /// `Runtime.evaluate` の応答フレーム（最後のフレーム）。
    async fn eval(&mut self, p: &Page, ctx: u64, expr: &str, by_value: bool) -> Value {
        let params = json!({"expression": expr, "contextId": ctx, "returnByValue": by_value});
        self.call(p, "Runtime.evaluate", params)
            .await
            .pop()
            .expect("response frame")
    }

    /// `Runtime.callFunctionOn` の応答フレーム（最後のフレーム）。
    async fn callf(&mut self, p: &Page, params: Value) -> Value {
        self.call(p, "Runtime.callFunctionOn", params)
            .await
            .pop()
            .expect("response frame")
    }

    /// 式を評価して `objectId` を得る。
    async fn handle(&mut self, p: &Page, ctx: u64, expr: &str) -> String {
        let f = self.eval(p, ctx, expr, false).await;
        ro(&f)["objectId"].as_str().expect("objectId").to_owned()
    }

    async fn release(&mut self, p: &Page, object_id: &str) -> Value {
        self.call(p, "Runtime.releaseObject", json!({"objectId": object_id}))
            .await
            .pop()
            .expect("response frame")
    }

    /// 遷移済みのページと isolated world（contextId）。
    async fn ready(&mut self) -> (Page, u64) {
        let p = self.page().await;
        self.navigate(&p).await;
        let ctx = self.isolated(&p, "u").await;
        (p, ctx)
    }
}

/// 応答の `result.result`（成功を要求する）。
fn ro(frame: &Value) -> &Value {
    assert!(frame.get("error").is_none(), "unexpected error: {frame}");
    &frame["result"]["result"]
}

/// 応答のエラー `(code, message)`。
fn err(frame: &Value) -> (i64, String) {
    let e = frame
        .get("error")
        .unwrap_or_else(|| panic!("expected error: {frame}"));
    (
        e["code"].as_i64().expect("code"),
        e["message"].as_str().expect("message").to_owned(),
    )
}

fn is_err(frame: &Value, code: i64, message: &str) -> bool {
    err(frame) == (code, message.to_owned())
}

/// 全ケースを直列に実行する。
pub fn run(engine: &str) {
    let rt = rt();
    eprintln!("case: primitives and unserializable values");
    rt.block_on(primitives(engine));
    eprintln!("case: injection safety");
    rt.block_on(injection(engine));
    eprintln!("case: objects and nodes");
    rt.block_on(objects(engine));
    eprintln!("case: returnByValue");
    rt.block_on(by_value(engine));
    eprintln!("case: exceptions");
    rt.block_on(exceptions(engine));
    eprintln!("case: main world, stale and unsupported params");
    rt.block_on(gating(engine));
    eprintln!("case: main world (stage 1)");
    rt.block_on(main_world(engine));
    eprintln!("case: main world sees the original HTML (JS-4)");
    rt.block_on(main_world_js4(engine));
    eprintln!("case: envelope hardening");
    rt.block_on(hardening(engine));
    eprintln!("case: lone surrogates");
    rt.block_on(lone_surrogates(engine));
    eprintln!("case: releaseObject");
    rt.block_on(release_object(engine));
    eprintln!("case: handle limit");
    rt.block_on(handle_limit(engine));
    eprintln!("case: result size limit");
    rt.block_on(result_size(engine));
    eprintln!("case: contexts per scope limit");
    rt.block_on(contexts_per_scope(engine));
    eprintln!("case: timeout");
    rt.block_on(timeout(engine));
    eprintln!("case: callFunctionOn");
    rt.block_on(call_function_on(engine));
    eprintln!("case: callFunctionOn rejections");
    rt.block_on(call_function_on_rejections(engine));
    eprintln!("case: callFunctionOn executionContextId");
    rt.block_on(call_function_on_execution_context(engine));
    eprintln!("case: awaitPromise");
    rt.block_on(await_promise(engine));
    eprintln!("case: DOM.describeNode / DOM.resolveNode");
    rt.block_on(dom_node(engine));
}

async fn primitives(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let cases: [(&str, Value); 12] = [
        ("1+1", json!({"type":"number","value":2})),
        ("\"a\"+\"b\"", json!({"type":"string","value":"ab"})),
        ("undefined", json!({"type":"undefined"})),
        (
            "null",
            json!({"type":"object","subtype":"null","value":null}),
        ),
        ("true", json!({"type":"boolean","value":true})),
        ("NaN", json!({"type":"number","unserializableValue":"NaN"})),
        ("-0", json!({"type":"number","unserializableValue":"-0"})),
        (
            "1/0",
            json!({"type":"number","unserializableValue":"Infinity"}),
        ),
        (
            "-1/0",
            json!({"type":"number","unserializableValue":"-Infinity"}),
        ),
        ("12n", json!({"type":"bigint","unserializableValue":"12n"})),
        ("0", json!({"type":"number","value":0})),
        ("1.5", json!({"type":"number","value":1.5})),
    ];
    for (expr, want) in cases {
        let f = c.eval(&p, ctx, expr, false).await;
        assert_eq!(ro(&f), &want, "{expr}");
    }
    // returnByValue でもプリミティブは同じ表現。
    let f = c.eval(&p, ctx, "-0", true).await;
    assert_eq!(ro(&f), &json!({"type":"number","unserializableValue":"-0"}));
    // 評価の応答は sessionId を伴う。
    assert_eq!(f["sessionId"], p.sid.as_str());
}

async fn injection(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    // 式の中の引用符・括弧・バッククォートが評価器のソースを壊さない。
    let f = c.eval(&p, ctx, "'a\")+(1'", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"a\")+(1"}));
    let f = c.eval(&p, ctx, "`x`+'\\''+\"y\"", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"x'y"}));
    // 行区切り（U+2028）を含む式も評価できる（リテラルでエスケープされる）。
    let f = c.eval(&p, ctx, "1\u{2028}+1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":2}));
    let f = c.eval(&p, ctx, "1\n+\r\n2", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":3}));
    // 評価器の包みを閉じようとする式は、ただの式として評価される。
    let f = c.eval(&p, ctx, "\"); throw 1; (\"", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"); throw 1; ("}));
    // 閉じていない式は構文エラーの exceptionDetails で、評価器自体は壊れない。
    let f = c.eval(&p, ctx, "1); throw 1; (2", false).await;
    assert_eq!(
        f["result"]["exceptionDetails"]["exception"]["className"],
        "SyntaxError"
    );
    // 退避した組込みは書き換えられない（同じ world のユーザーコードが JSON.parse を壊しても動く）。
    let f = c
        .eval(
            &p,
            ctx,
            "JSON.parse = function () { throw 9; }; 'tampered'",
            false,
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"tampered"}));
    let f = c.eval(&p, ctx, "'still works'", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"still works"}));
    // ヘルパー自体は再定義・削除できない。
    let f = c
        .eval(
            &p,
            ctx,
            "var r = []; try { Object.defineProperty(globalThis, '__fandheCdpHelper', {value: 1}); r.push('def'); } catch (e) { r.push('blocked'); } r.join()",
            false,
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"blocked"}));
    let f = c.eval(&p, ctx, "typeof __fandheCdpHelper", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"object"}));
}

async fn objects(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let f = c.eval(&p, ctx, "({a:1})", false).await;
    assert_eq!(
        ro(&f),
        &json!({
            "type":"object","className":"Object","description":"Object",
            "objectId": format!("{ctx}.1"),
        })
    );
    let f = c.eval(&p, ctx, "({a:1})", false).await;
    assert_eq!(ro(&f)["objectId"], format!("{ctx}.2"));
    let f = c.eval(&p, ctx, "[1,2]", false).await;
    assert_eq!(
        ro(&f),
        &json!({
            "type":"object","subtype":"array","className":"Array","description":"Array(2)",
            "objectId": format!("{ctx}.3"),
        })
    );
    let f = c.eval(&p, ctx, "document.body", false).await;
    assert_eq!(ro(&f)["type"], "object");
    assert_eq!(ro(&f)["subtype"], "node");
    // 説明は nodeName（無ければ localName・tagName）を小文字化したもの。shim の Element はこれらを
    // 持たないため、現状は className（"Element"）に代わる。
    assert!(
        ro(&f)["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty())
    );
    assert_eq!(ro(&f)["objectId"], format!("{ctx}.4"));
    let f = c.eval(&p, ctx, "(function f(){ return 1; })", false).await;
    assert_eq!(ro(&f)["type"], "function");
    assert!(ro(&f).get("subtype").is_none());
    assert_eq!(ro(&f)["className"], "Function");
    assert_eq!(ro(&f)["objectId"], format!("{ctx}.5"));
    assert!(
        ro(&f)["description"]
            .as_str()
            .is_some_and(|d| d.contains("function f"))
    );
    let f = c.eval(&p, ctx, "new Date(0)", false).await;
    assert_eq!(ro(&f)["subtype"], "date");
    let f = c.eval(&p, ctx, "/a/", false).await;
    assert_eq!(ro(&f)["subtype"], "regexp");
}

/// 封筒の JSON 化がユーザー定義の `toJSON` に影響されないこと、説明文・className の切り詰めが
/// サロゲートペアを分断しないこと（`SEC-2`・`CDP-2`）。
/// 単独サロゲートは封筒を壊さず U+FFFD に置換される（キー・値・入れ子・例外の説明を含む）。
async fn lone_surrogates(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let f = c.eval(&p, ctx, "'\\ud800'", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"\u{FFFD}"}));
    let f = c.eval(&p, ctx, "'a\\udc00b'", false).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"a\u{FFFD}b"}));
    // 正しいペアは保たれる。
    let f = c.eval(&p, ctx, "'\\ud83d\\ude00'", false).await;
    assert_eq!(ro(&f)["value"], "\u{1F600}");
    // 文字列中の「バックスラッシュ + ud800」という文字列はそのまま。
    let f = c.eval(&p, ctx, "'\\\\ud800'", false).await;
    assert_eq!(ro(&f)["value"], "\\ud800");
    let f = c.eval(&p, ctx, "new Error('\\ud800')", false).await;
    assert_eq!(ro(&f)["description"], "Error: \u{FFFD}");
    let f = c.eval(&p, ctx, "throw new Error('\\ud800')", false).await;
    assert_eq!(
        f["result"]["exceptionDetails"]["exception"]["description"],
        "Error: \u{FFFD}"
    );
    let f = c
        .eval(
            &p,
            ctx,
            "({'\\ud800k': '\\udc00', n: [{x: 'z\\ud800'}]})",
            true,
        )
        .await;
    assert_eq!(
        ro(&f)["value"],
        json!({"\u{FFFD}k": "\u{FFFD}", "n": [{"x": "z\u{FFFD}"}]})
    );
    // 直後の評価も正常（ヘルパーの状態が壊れていない）。
    let f = c.eval(&p, ctx, "1+1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":2}));
}

async fn hardening(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let hijack = "Object.prototype.toJSON = function () { if (this.kind) \
        return {kind:'value',result:{type:'number',value:999}}; return this; }; 1";
    let f = c.eval(&p, ctx, hijack, false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    let f = c.eval(&p, ctx, "2", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":2}));
    let f = c.eval(&p, ctx, "({a:1})", false).await;
    assert_eq!(ro(&f)["type"], "object");
    assert_eq!(ro(&f)["className"], "Object");
    let f = c.eval(&p, ctx, "({a:[1]})", true).await;
    assert_eq!(ro(&f)["value"], json!({"a":[1]}));
    let f = c.eval(&p, ctx, "throw 5", false).await;
    assert!(f.get("result").is_some(), "{f}");
    assert_eq!(f["result"]["exceptionDetails"]["exception"]["value"], 5);

    // Array.prototype の index setter でハンドルの追跡記録を妨げられない（件数・上限が保たれる）。
    let f = c
        .eval(
            &p,
            ctx,
            "Object.defineProperty(Array.prototype, '0', {configurable:true, set:function(){ throw 1; }}); 1",
            false,
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    let f = c.eval(&p, ctx, "({})", false).await;
    assert_eq!(ro(&f)["type"], "object");
    let f = c.eval(&p, ctx, "[{}, {}]", false).await;
    assert_eq!(ro(&f)["subtype"], "array");
    // bigint は Rust 側の 1024 バイト上限ちょうどまで通り、超過は明示的なサイズ上限エラー。
    let f = c.eval(&p, ctx, "10n ** 1022n", false).await;
    assert_eq!(
        ro(&f)["unserializableValue"].as_str().map(str::len),
        Some(1024)
    );
    let f = c.eval(&p, ctx, "10n ** 1023n", false).await;
    assert!(is_err(&f, -32000, "evaluation result too large"), "{f}");

    // 説明文は "Error: " + message が 4096 単位の境界で絵文字を分断する。
    let f = c
        .eval(&p, ctx, "new Error('a'.repeat(4088) + '\\u{1F600}')", false)
        .await;
    let d = ro(&f)["description"].as_str().expect("description");
    assert_eq!(d, format!("Error: {}", "a".repeat(4088)));
    // className（256 単位）も同様。
    let f = c
        .eval(
            &p,
            ctx,
            "({constructor: Object.defineProperty(function(){}, 'name', \
              {value: 'a'.repeat(255) + '\\u{1F600}'})})",
            false,
        )
        .await;
    assert_eq!(ro(&f)["className"], "a".repeat(255));
}

async fn by_value(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let f = c.eval(&p, ctx, "({a:[1]})", true).await;
    assert_eq!(
        ro(&f),
        &json!({"type":"object","className":"Object","value":{"a":[1]}})
    );
    assert!(ro(&f).get("objectId").is_none());
    let f = c.eval(&p, ctx, "[1,'x',null]", true).await;
    assert_eq!(
        ro(&f),
        &json!({"type":"object","subtype":"array","className":"Array","value":[1,"x",null]})
    );
    // 循環参照は値にできない（例外を装わず固定文言のエラー）。
    let f = c.eval(&p, ctx, "var o = {}; o.o = o; o", true).await;
    assert!(
        is_err(&f, -32000, "value could not be returned by value"),
        "{f}"
    );
    // 関数は値にできない。
    let f = c.eval(&p, ctx, "(function(){})", true).await;
    assert!(
        is_err(&f, -32000, "value could not be returned by value"),
        "{f}"
    );
    // by-value ではハンドルを払い出さない（次の通常評価が 1 番）。
    let f = c.eval(&p, ctx, "({})", false).await;
    assert_eq!(ro(&f)["objectId"], format!("{ctx}.1"));
}

async fn exceptions(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let f = c.eval(&p, ctx, "throw new Error(\"x\")", false).await;
    assert!(
        f.get("error").is_none(),
        "JS の例外は成功応答の中で返す: {f}"
    );
    let r = &f["result"];
    let d = &r["exceptionDetails"];
    assert_eq!(d["exceptionId"], 1);
    assert_eq!(d["text"], "uncaught exception");
    assert_eq!(d["lineNumber"], 0);
    assert_eq!(d["columnNumber"], 0);
    assert_eq!(d["exception"]["type"], "object");
    assert_eq!(d["exception"]["subtype"], "error");
    assert_eq!(d["exception"]["className"], "Error");
    assert_eq!(d["exception"]["objectId"], format!("{ctx}.1"));
    assert!(
        d["exception"]["description"]
            .as_str()
            .is_some_and(|s| s.contains("Error") && s.contains('x'))
    );
    assert_eq!(&r["result"], &d["exception"]);
    // 構文エラーも exceptionDetails（連番 2）。
    let f = c.eval(&p, ctx, "(", false).await;
    let d = &f["result"]["exceptionDetails"];
    assert_eq!(d["exceptionId"], 2);
    assert_eq!(d["exception"]["subtype"], "error");
    assert_eq!(d["exception"]["className"], "SyntaxError");
    // プリミティブの throw。
    let f = c.eval(&p, ctx, "throw 1", false).await;
    let d = &f["result"]["exceptionDetails"];
    assert_eq!(d["exceptionId"], 3);
    assert_eq!(d["exception"], json!({"type":"number","value":1}));
    // 例外の後も同じコンテキストで評価を続けられる。
    let f = c.eval(&p, ctx, "1+1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":2}));
}

async fn gating(engine: &str) {
    let mut c = Cdp::new(engine);
    let p = c.page().await;
    let nav = c.navigate(&p).await;
    // Runtime.enable 後の遷移で告知された main world の contextId。
    let main_id = nav
        .iter()
        .find(|f| {
            f["method"] == "Runtime.executionContextCreated"
                && f["params"]["context"]["auxData"]["isDefault"] == true
        })
        .and_then(|f| f["params"]["context"]["id"].as_u64())
        .expect("main context announced");
    // main world も確定 DOM の再パースで評価できる（段階 1。#1347）。
    let f = c.eval(&p, main_id, "1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    // contextId 省略は現在の文書の main world。
    let f = c
        .call(&p, "Runtime.evaluate", json!({"expression": "1"}))
        .await
        .pop()
        .expect("frame");
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));

    // 未対応・未知のパラメータは成功を装わず拒否する。
    let iso = c.isolated(&p, "u").await;
    let spawns = c.worker_spawns();
    for params in [
        json!({"expression":"1","contextId":iso,"objectGroup":"g"}),
        json!({"expression":"1","contextId":iso,"generatePreview":true}),
        json!({"expression":"1","contextId":iso,"includeCommandLineAPI":true}),
    ] {
        let f = c
            .call(&p, "Runtime.evaluate", params)
            .await
            .pop()
            .expect("frame");
        assert!(is_err(&f, -32602, "unsupported params"), "{f}");
    }
    assert_eq!(c.worker_spawns(), spawns);

    // getProperties・releaseObjectGroup は未実装のまま。
    for m in ["Runtime.getProperties", "Runtime.releaseObjectGroup"] {
        let f = c.call(&p, m, json!({})).await.pop().expect("frame");
        assert!(is_err(&f, -32601, "method not implemented"), "{m}: {f}");
    }

    // 遷移後の古い contextId は表から刈り込まれ、未知として拒否される。
    let f = c.eval(&p, iso, "1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    c.navigate(&p).await;
    let f = c.eval(&p, iso, "1", false).await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
}

/// Runtime.enable 後の遷移で告知された main world の contextId。
fn main_context_id(nav: &[Value]) -> u64 {
    nav.iter()
        .find(|f| {
            f["method"] == "Runtime.executionContextCreated"
                && f["params"]["context"]["auxData"]["isDefault"] == true
        })
        .and_then(|f| f["params"]["context"]["id"].as_u64())
        .expect("main context announced")
}

/// `contextId` 省略の `Runtime.evaluate`（現在の文書の main world）。
async fn eval_default(c: &mut Cdp, p: &Page, expr: &str, by_value: bool) -> Value {
    c.call(
        p,
        "Runtime.evaluate",
        json!({"expression": expr, "returnByValue": by_value}),
    )
    .await
    .pop()
    .expect("response frame")
}

/// main world の評価（段階 1。#1347・`CDP-2`・`CDP-7`・`SEC-2`・`REPAIR-3`）。
async fn main_world(engine: &str) {
    let mut c = Cdp::new(engine);
    let p = c.page().await;
    let nav = c.navigate(&p).await;
    let main_id = main_context_id(&nav);

    // (a) 確定 DOM。明示 id と省略の両方。
    let f = c
        .eval(
            &p,
            main_id,
            "document.getElementById('x').textContent",
            true,
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"hello"}));
    let f = eval_default(&mut c, &p, "document.getElementById('x').textContent", true).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"hello"}));

    // (b) ページのグローバル参照は ReferenceError（偽の値を返さない）。
    let f = c
        .eval(&p, main_id, "pageGlobalThatDoesNotExist", false)
        .await;
    assert!(f.get("error").is_none(), "{f}");
    let d = &f["result"]["exceptionDetails"];
    assert_eq!(d["exception"]["className"], "ReferenceError");
    assert_eq!(d["exception"]["subtype"], "error");

    // (c) ハンドルと releaseObject。
    let f = c.eval(&p, main_id, "({})", false).await;
    let id = ro(&f)["objectId"].as_str().expect("objectId").to_owned();
    // (b) の例外オブジェクトが 1 番を使っているので、次の番号になる。
    assert_eq!(id, format!("{main_id}.2"));
    let f = c.release(&p, &id).await;
    assert_eq!(f["result"], json!({}));
    let f = c.release(&p, &id).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");

    // (d) callFunctionOn の objectId 経路。
    let doc = c.handle(&p, main_id, "document").await;
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "function () { return this.querySelector('#x') !== null; }",
                "objectId": doc,
                "returnByValue": true,
            }),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"boolean","value":true}));

    // (e) callFunctionOn の executionContextId 経路（Puppeteer の Frame.#document() 型）。
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "function () { 'use strict'; return this === undefined; }",
                "executionContextId": main_id,
                "returnByValue": true,
            }),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"boolean","value":true}));
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "function () { return document; }",
                "executionContextId": main_id,
            }),
        )
        .await;
    assert_eq!(ro(&f)["subtype"], "node");
    let oid = ro(&f)["objectId"].as_str().expect("objectId");
    assert!(oid.starts_with(&format!("{main_id}.")), "{oid}");

    // (f) world の分離。
    let f = c.eval(&p, main_id, "globalThis.__m = 1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    let iso = c.isolated(&p, "u").await;
    let f = c.eval(&p, iso, "typeof __m", true).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"undefined"}));
    c.eval(&p, iso, "globalThis.__i = 1", false).await;
    let f = c.eval(&p, main_id, "typeof __i", true).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"undefined"}));

    // (g) main のハンドルを isolated の呼び出しへ渡すと contextId の不一致。
    let h = c.handle(&p, main_id, "({})").await;
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "function (a) { return 1; }",
                "executionContextId": iso,
                "arguments": [{"objectId": h}],
            }),
        )
        .await;
    assert!(
        is_err(&f, -32000, "argument belongs to another execution context"),
        "{f}"
    );

    // (h) 他セッションの main の contextId・objectId は存在を区別せず拒否する。
    let other = c.page().await;
    c.navigate(&other).await;
    let f = c.eval(&other, main_id, "1", false).await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let live = c.handle(&p, main_id, "({})").await;
    let f = c.release(&other, &live).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");

    // (i) 遷移後、古い main の contextId は未知。省略なら新しい文書の main で評価できる。
    c.navigate(&p).await;
    let f = c.eval(&p, main_id, "1", false).await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let f = eval_default(&mut c, &p, "document.getElementById('x').textContent", true).await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"hello"}));
}

/// ページスクリプトを持つ HTML（`PageScriptHandle` なし = `execute_page_scripts` は実効で false）。
const SCRIPT_PAGE_HTML: &str = "<html><body><p id=\"x\">hello</p><script>var pageGlobal=1;var q=document.createElement('p');q.id='added';document.body.appendChild(q)</script></body></html>";

/// `JS-4`: `execute_page_scripts = false` では main world は元の HTML を見る（スクリプトの効果は無い）。
async fn main_world_js4(engine: &str) {
    let mut c = Cdp::new(engine).with_html(SCRIPT_PAGE_HTML);
    let p = c.page().await;
    c.navigate(&p).await;
    let f = eval_default(
        &mut c,
        &p,
        "document.getElementById('added') === null",
        true,
    )
    .await;
    assert_eq!(ro(&f), &json!({"type":"boolean","value":true}));
    let f = eval_default(&mut c, &p, "document.querySelectorAll('p').length", true).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    let f = eval_default(&mut c, &p, "pageGlobal", false).await;
    assert_eq!(
        f["result"]["exceptionDetails"]["exception"]["className"],
        "ReferenceError"
    );
}

async fn release_object(engine: &str) {
    let mut c = Cdp::new(engine);
    let p = c.page().await;
    c.navigate(&p).await;
    let ctx = c.isolated(&p, "u").await;
    // ヘルパー未注入のコンテキストは評価せずに「無い」。
    let spawns = c.worker_spawns();
    let f = c.release(&p, &format!("{ctx}.1")).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    assert_eq!(c.worker_spawns(), spawns);

    let f = c.eval(&p, ctx, "({})", false).await;
    let id = ro(&f)["objectId"].as_str().expect("objectId").to_owned();
    assert_eq!(id, format!("{ctx}.1"));
    let f = c.release(&p, &id).await;
    assert_eq!(f["result"], json!({}));
    assert_eq!(f["sessionId"], p.sid.as_str());
    let f = c.release(&p, &id).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");

    // 他セッションの objectId は存在を区別せず「無い」。
    let f = c.eval(&p, ctx, "({})", false).await;
    let live = ro(&f)["objectId"].as_str().expect("objectId").to_owned();
    let other = c.page().await;
    c.navigate(&other).await;
    let f = c.release(&other, &live).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    // 自分のハンドルは影響を受けていない。
    let f = c.release(&p, &live).await;
    assert_eq!(f["result"], json!({}));

    // 書式の不正は INVALID_PARAMS、未知のキーは UNSUPPORTED_PARAMS。
    let f = c.release(&p, "01.1").await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let f = c
        .call(
            &p,
            "Runtime.releaseObject",
            json!({"objectId": live, "x": 1}),
        )
        .await
        .pop()
        .expect("frame");
    assert!(is_err(&f, -32602, "unsupported params"), "{f}");
}

async fn handle_limit(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    for i in 1..=4096u32 {
        let f = c.eval(&p, ctx, "({})", false).await;
        assert_eq!(ro(&f)["objectId"], format!("{ctx}.{i}"), "evaluation {i}");
    }
    let f = c.eval(&p, ctx, "({})", false).await;
    assert!(is_err(&f, -32000, "too many remote objects"), "{f}");
    // プリミティブは影響を受けない。
    let f = c.eval(&p, ctx, "1", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    // 1 件解放すると、次は成功する（番号は再利用しない）。
    let f = c.release(&p, &format!("{ctx}.1")).await;
    assert_eq!(f["result"], json!({}));
    let f = c.eval(&p, ctx, "({})", false).await;
    assert_eq!(ro(&f)["objectId"], format!("{ctx}.4097"));
}

async fn result_size(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let f = c.eval(&p, ctx, "'x'.repeat(999000)", true).await;
    let v = ro(&f);
    assert_eq!(v["type"], "string");
    assert_eq!(v["value"].as_str().map(str::len), Some(999_000));
    let f = c.eval(&p, ctx, "'x'.repeat(1000001)", true).await;
    assert!(is_err(&f, -32000, "evaluation result too large"), "{f}");
    // 式の上限（1 MiB 超）は評価器を介さず拒否する。
    let spawns = c.worker_spawns();
    let big = "1".repeat(1_048_577);
    let f = c.eval(&p, ctx, &big, false).await;
    assert!(is_err(&f, -32000, "expression too large"), "{f}");
    assert_eq!(c.worker_spawns(), spawns);
    // エスケープで包んだソースが 1 MiB を超える式も同じエラー。
    let quotes = "\"".repeat(400_000);
    let f = c.eval(&p, ctx, &format!("'{quotes}'.length"), false).await;
    assert!(is_err(&f, -32000, "expression too large"), "{f}");
}

async fn contexts_per_scope(engine: &str) {
    let mut c = Cdp::new(engine);
    let p = c.page().await;
    c.navigate(&p).await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(c.isolated(&p, &format!("w{i}")).await);
    }
    for id in ids.iter().take(4) {
        let f = c.eval(&p, *id, "1", false).await;
        assert_eq!(ro(&f), &json!({"type":"number","value":1}));
    }
    // 5 つ目の world の初回評価は scope あたりのコンテキスト数の上限に当たる。
    let f = c.eval(&p, ids[4], "1", false).await;
    assert!(
        is_err(&f, -32000, "evaluation resource limit exceeded"),
        "{f}"
    );
}

async fn timeout(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let f = c.eval(&p, ctx, "({keep:1})", false).await;
    assert_eq!(ro(&f)["objectId"], format!("{ctx}.1"));
    let started = Instant::now();
    let f = c.eval(&p, ctx, "while (true) {}", false).await;
    let elapsed = started.elapsed();
    assert!(is_err(&f, -32000, "evaluation timed out"), "{f}");
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(20),
        "elapsed {elapsed:?}"
    );
    let next = c.eval(&p, ctx, "({})", false).await;
    if engine == "v8" {
        // V8 はコンテキストが残る（ハンドル表も残る。次の番号は 2）。
        assert_eq!(ro(&next)["objectId"], format!("{ctx}.2"));
        // メタデータ取得（constructor の getter）で応答できなかった評価はハンドルを残さない
        // （ハンドルは確保前なので番号も消費しない。次の番号は 3）。
        let f = c
            .eval(
                &p,
                ctx,
                "({ get constructor() { while (true) {} } })",
                false,
            )
            .await;
        assert!(is_err(&f, -32000, "evaluation timed out"), "{f}");
        let after = c.eval(&p, ctx, "({})", false).await;
        assert_eq!(ro(&after)["objectId"], format!("{ctx}.3"));
    } else {
        // boa は子プロセスごと終了し状態が失われる。contextId は失効して再利用されない。
        assert!(is_err(&next, -32602, "invalid params"), "{next}");
    }
}

/// 評価器が無効（macOS の boa は fail-closed）のとき、`Runtime.evaluate` は偽の成功を返さず
/// 固定文言で拒否し、子プロセスも起動しない。
#[allow(dead_code)]
pub fn run_disabled() {
    let rt = rt();
    rt.block_on(async {
        let mut c = Cdp::with(
            ScriptContextHandle::disabled(),
            RuntimeDispatcher::new().expect("dispatcher"),
        );
        let p = c.page().await;
        c.navigate(&p).await;
        let f = c.eval(&p, 1, "1", false).await;
        assert!(is_err(&f, -32000, "execution contexts unavailable"), "{f}");
        let f = c.describe(&p, "1.1").await;
        assert!(is_err(&f, -32000, "execution contexts unavailable"), "{f}");
        let f = c.resolve(&p, json!({"backendNodeId": 1})).await;
        assert!(is_err(&f, -32000, "execution contexts unavailable"), "{f}");
        assert_eq!(c.worker_spawns(), 0);
    });
}
/// Playwright の utility script 相当（`evaluate(isFunction, returnByValue, expression, argCount, ...args)`）。
const UTILITY_SCRIPT: &str = "({ tag: 'utility', evaluate: function (isFunction, returnByValue, expression, argCount) { \
     var fn = (0, eval)(expression); return isFunction ? fn.apply(null, Array.prototype.slice.call(arguments, 4)) : fn; } })";

async fn call_function_on(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let h = c.handle(&p, ctx, UTILITY_SCRIPT).await;

    // Playwright 型: this は utility script、arguments は {objectId} と value 4 件。
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "(utilityScript, ...args) => utilityScript.evaluate(...args)",
                "objectId": h,
                "arguments": [
                    {"objectId": h},
                    {"value": true},
                    {"value": true},
                    {"value": "() => 'hello' + 1"},
                    {"value": 0}
                ],
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"hello1"}), "{f}");
    assert_eq!(f["sessionId"], p.sid.as_str());

    // this の束縛（具体値）。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return this.tag; }", "objectId": h, "returnByValue": true}),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"utility"}));

    // userGesture の有無で結果は変わらない（REPAIR-3: 効果なし）。
    for ug in [false, true] {
        let f = c
            .callf(
                &p,
                json!({"functionDeclaration": "function () { return 7; }", "objectId": h, "userGesture": ug, "returnByValue": true}),
            )
            .await;
        assert_eq!(ro(&f), &json!({"type":"number","value":7}));
    }

    // unserializableValue の往復。
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "function (a, b, c, d, e, f) { return [typeof a + String(a), 1 / b === -Infinity, c, d, typeof e + String(e), f === undefined].join('|'); }",
                "objectId": h,
                "arguments": [
                    {"unserializableValue": "NaN"},
                    {"unserializableValue": "-0"},
                    {"unserializableValue": "Infinity"},
                    {"unserializableValue": "-Infinity"},
                    {"unserializableValue": "-12n"},
                    {}
                ],
                "returnByValue": true,
            }),
        )
        .await;
    assert_eq!(
        ro(&f),
        &json!({"type":"string","value":"numberNaN|true|Infinity|-Infinity|bigint-12|true"}),
        "{f}"
    );

    // ハンドル返し。戻りのオブジェクトは objectId を持つ。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return { n: 1 }; }", "objectId": h}),
        )
        .await;
    assert_eq!(ro(&f)["type"], "object");
    assert!(ro(&f)["objectId"].as_str().is_some(), "{f}");

    // 関数でない・構文エラー・throw。
    let f = c
        .callf(&p, json!({"functionDeclaration": "42", "objectId": h}))
        .await;
    assert!(
        is_err(
            &f,
            -32000,
            "function declaration did not evaluate to a function"
        ),
        "{f}"
    );
    for decl in ["function (", "function () { throw new Error('boom'); }"] {
        let f = c
            .callf(&p, json!({"functionDeclaration": decl, "objectId": h}))
            .await;
        assert_eq!(
            f["result"]["exceptionDetails"]["text"], "uncaught exception",
            "{decl}: {f}"
        );
        assert_eq!(f["result"]["result"]["type"], "object", "{decl}: {f}");
    }

    // 64 件ちょうどは成功する。
    let args: Vec<Value> = (0..64).map(|i| json!({"value": i})).collect();
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return arguments.length; }", "objectId": h, "arguments": args, "returnByValue": true}),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"number","value":64}));
}

async fn call_function_on_rejections(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let h = c.handle(&p, ctx, "({})").await;
    let other_ctx = c.isolated(&p, "other").await;
    let foreign = c.handle(&p, other_ctx, "({})").await;
    let decl = "function () { return 1; }";

    // 65 件は拒否される。
    let args: Vec<Value> = (0..65).map(|i| json!({"value": i})).collect();
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": decl, "objectId": h, "arguments": args}),
        )
        .await;
    assert!(is_err(&f, -32000, "too many call arguments"), "{f}");
    // 書式不正の objectId。
    for bad in ["01.1", "x", ""] {
        let f = c
            .callf(&p, json!({"functionDeclaration": decl, "objectId": bad}))
            .await;
        assert!(is_err(&f, -32602, "invalid params"), "{bad}: {f}");
    }
    // 引数要素が 2 キー。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": decl, "objectId": h, "arguments": [{"value": 1, "objectId": h}]}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    // 他コンテキストの引数。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": decl, "objectId": h, "arguments": [{"objectId": foreign}]}),
        )
        .await;
    assert!(
        is_err(&f, -32000, "argument belongs to another execution context"),
        "{f}"
    );
    // 未対応キー・型の誤り。
    for params in [
        json!({"functionDeclaration": decl, "objectId": h, "objectGroup": "g"}),
        json!({"functionDeclaration": decl, "objectId": h, "uniqueContextId": "x"}),
    ] {
        let f = c.callf(&p, params).await;
        assert!(is_err(&f, -32602, "unsupported params"), "{f}");
    }
    // objectId と executionContextId は排他（#1345）。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": decl, "objectId": h, "executionContextId": ctx}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": decl, "objectId": h, "userGesture": 1}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    // 解放済みのハンドルは this でも引数でも「無い」。ユーザーコードは実行しない。
    let gone = c.handle(&p, ctx, "({})").await;
    assert_eq!(c.release(&p, &gone).await["result"], json!({}));
    let f = c
        .callf(&p, json!({"functionDeclaration": decl, "objectId": gone}))
        .await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    let f = c.eval(&p, ctx, "globalThis.__ran = 0", false).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":0}));
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { globalThis.__ran = 1; }", "objectId": h, "arguments": [{"objectId": gone.clone()}]}),
        )
        .await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    let f = c.eval(&p, ctx, "globalThis.__ran", true).await;
    assert_eq!(
        ro(&f),
        &json!({"type":"number","value":0}),
        "引数が無ければ実行しない"
    );
    // 他セッションの objectId。
    let other = c.page().await;
    c.navigate(&other).await;
    let f = c
        .callf(&other, json!({"functionDeclaration": decl, "objectId": h}))
        .await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    // ヘルパー未注入の contextId。
    let fresh = c.isolated(&p, "fresh").await;
    let spawns = c.worker_spawns();
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": decl, "objectId": format!("{fresh}.1")}),
        )
        .await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    assert_eq!(
        c.worker_spawns(),
        spawns,
        "ヘルパー未注入では評価器を起動しない"
    );
}

async fn await_promise(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;
    let h = c.handle(&p, ctx, "({})").await;

    // 非 Promise の値はそのまま返る（evaluate・callFunctionOn とも）。
    let f = c
        .call(
            &p,
            "Runtime.evaluate",
            json!({"expression": "40 + 2", "contextId": ctx, "awaitPromise": true, "returnByValue": true}),
        )
        .await
        .pop()
        .expect("frame");
    assert_eq!(ro(&f), &json!({"type":"number","value":42}), "{f}");
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return 'v'; }", "objectId": h, "awaitPromise": true, "returnByValue": true}),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"v"}), "{f}");

    // microtask checkpoint の後も pending なら成功を装わず -32000。
    let pending = "function () { return new Promise(function () {}); }";
    for _ in 0..100 {
        let f = c
            .callf(
                &p,
                json!({"functionDeclaration": pending, "objectId": h, "awaitPromise": true}),
            )
            .await;
        assert!(is_err(&f, -32000, "promise was not settled"), "{f}");
    }
    let f = c
        .call(
            &p,
            "Runtime.evaluate",
            json!({"expression": "new Promise(function () {})", "contextId": ctx, "awaitPromise": true}),
        )
        .await
        .pop()
        .expect("frame");
    assert!(is_err(&f, -32000, "promise was not settled"), "{f}");
    // 100 回繰り返してもスロット・ハンドルが枯渇しない（毎回削除される）。
    let f = c.eval(&p, ctx, "({})", false).await;
    assert!(ro(&f)["objectId"].as_str().is_some(), "{f}");

    // awaitPromise なしの Promise は RemoteObject（subtype promise）のまま返る。
    let f = c
        .callf(&p, json!({"functionDeclaration": pending, "objectId": h}))
        .await;
    assert_eq!(ro(&f)["subtype"], "promise", "{f}");

    // Promise と判定できるのに then の反応登録が投げる場合（constructor = null で species 解決が
    // TypeError）は、成功を装わず exceptionDetails 付きのエラー応答にする（CDP-6・REPAIR-3）。
    let broken = "function () { var p = Promise.resolve(42); p.constructor = null; return p; }";
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": broken, "objectId": h, "awaitPromise": true}),
        )
        .await;
    assert_eq!(
        f["result"]["exceptionDetails"]["text"], "uncaught exception",
        "{f}"
    );
    assert_eq!(f["result"]["result"]["subtype"], "error", "{f}");
    assert!(
        f["result"]["result"]["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("TypeError: ")),
        "{f}"
    );
    let f = c
        .call(
            &p,
            "Runtime.evaluate",
            json!({"expression": "(() => { const p = Promise.resolve(42); p.constructor = null; return p; })()", "contextId": ctx, "awaitPromise": true}),
        )
        .await
        .pop()
        .expect("frame");
    assert_eq!(
        f["result"]["exceptionDetails"]["text"], "uncaught exception",
        "{f}"
    );
    assert_eq!(f["result"]["result"]["subtype"], "error", "{f}");

    // native Promise の決着（エンジンごとの実測値。REPAIR-3: call_function_on.rs の差異節に記録）。
    for (name, decl) in [
        ("resolved", "function () { return Promise.resolve(42); }"),
        (
            "rejected",
            "function () { return Promise.reject(new Error('no')); }",
        ),
        (
            "queued",
            "function () { return new Promise(function (r) { Promise.resolve().then(function () { r(7); }); }); }",
        ),
    ] {
        let f = c
            .callf(
                &p,
                json!({"functionDeclaration": decl, "objectId": h, "awaitPromise": true, "returnByValue": true}),
            )
            .await;
        if engine == "v8" {
            // V8 は評価の終わりに microtask が流れ、反応ジョブが決着を記録する。
            match name {
                "resolved" => assert_eq!(ro(&f), &json!({"type":"number","value":42}), "{f}"),
                "queued" => assert_eq!(ro(&f), &json!({"type":"number","value":7}), "{f}"),
                _ => {
                    assert_eq!(
                        f["result"]["exceptionDetails"]["text"],
                        "uncaught exception"
                    );
                    assert_eq!(f["result"]["result"]["description"], "Error: no", "{f}");
                }
            }
        } else {
            // boa は js crate が native Promise のジョブを流さないため決着せず、成功を装わない（REPAIR-3）。
            assert!(is_err(&f, -32000, "promise was not settled"), "{name}: {f}");
        }
    }
}

/// #1345: `executionContextId` で world を指す `Runtime.callFunctionOn`（Puppeteer の `queryOne` の経路）。
async fn call_function_on_execution_context(engine: &str) {
    let mut c = Cdp::new(engine);
    let (p, ctx) = c.ready().await;

    // ヘルパー未注入の新しい world でも、事前の evaluate なしで評価できる。
    let fresh = c.isolated(&p, "fresh").await;
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return 6 * 7; }", "executionContextId": fresh, "returnByValue": true}),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"number","value":42}), "{f}");
    // 未注入の world で引数にハンドルがあれば、評価器を起動せず「無い」。
    let fresh2 = c.isolated(&p, "fresh2").await;
    let spawns = c.worker_spawns();
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return 1; }", "executionContextId": fresh2,
                   "arguments": [{"objectId": format!("{fresh2}.1")}]}),
        )
        .await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    assert_eq!(c.worker_spawns(), spawns);

    // this は undefined（strict）。sloppy 関数は JS の仕様どおり globalThis になる。
    for (decl, want) in [
        (
            "function () { 'use strict'; return this === undefined; }",
            true,
        ),
        ("function () { return this === globalThis; }", true),
    ] {
        let f = c
            .callf(
                &p,
                json!({"functionDeclaration": decl, "executionContextId": ctx, "returnByValue": true}),
            )
            .await;
        assert_eq!(
            ro(&f),
            &json!({"type":"boolean","value":want}),
            "{decl}: {f}"
        );
    }

    // Puppeteer の queryOne 型。
    let el = c.handle(&p, ctx, "document").await;
    let util = c.handle(&p, ctx, "({ tag: 'util' })").await;
    let f = c
        .callf(
            &p,
            json!({
                "functionDeclaration": "function (node, selector, util) { return util.tag + ':' + (node.querySelector(selector) !== null); }",
                "executionContextId": ctx,
                "arguments": [{"objectId": el}, {"value": "body"}, {"objectId": util}],
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"util:true"}), "{f}");

    // ハンドル返し。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return { n: 1 }; }", "executionContextId": ctx}),
        )
        .await;
    assert_eq!(ro(&f)["type"], "object", "{f}");
    let oid = ro(&f)["objectId"].as_str().expect("objectId");
    assert!(oid.starts_with(&format!("{ctx}.")), "{oid}");

    // 排他。
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () {}", "executionContextId": ctx, "objectId": el}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let f = c
        .callf(&p, json!({"functionDeclaration": "function () {}"}))
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");

    // 未対応キー。
    for extra in [
        json!({"uniqueContextId": "x"}),
        json!({"serializationOptions": {}}),
        json!({"objectGroup": "g"}),
    ] {
        let mut params =
            json!({"functionDeclaration": "function () {}", "executionContextId": ctx});
        params
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let f = c.callf(&p, params).await;
        assert!(is_err(&f, -32602, "unsupported params"), "{f}");
    }

    // 他セッションの contextId は未知と区別しない。
    let other = c.page().await;
    c.navigate(&other).await;
    let f = c
        .callf(
            &other,
            json!({"functionDeclaration": "function () { return 1; }", "executionContextId": ctx}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");

    // 引数の objectId の contextId 不一致。ユーザーコードは実行しない。
    let other_ctx = c.isolated(&p, "other").await;
    let foreign = c.handle(&p, other_ctx, "({})").await;
    c.eval(&p, ctx, "globalThis.__ran = 0", false).await;
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { globalThis.__ran = 1; }", "executionContextId": ctx,
                   "arguments": [{"objectId": foreign}]}),
        )
        .await;
    assert!(
        is_err(&f, -32000, "argument belongs to another execution context"),
        "{f}"
    );
    let f = c.eval(&p, ctx, "globalThis.__ran", true).await;
    assert_eq!(ro(&f), &json!({"type":"number","value":0}));

    // 遷移後の古い contextId は未知。
    c.navigate(&p).await;
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return 1; }", "executionContextId": ctx}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");

    // main world も評価できる（#1347）。
    let mut c = Cdp::new(engine);
    let p = c.page().await;
    let nav = c.navigate(&p).await;
    let main_id = nav
        .iter()
        .find(|f| {
            f["method"] == "Runtime.executionContextCreated"
                && f["params"]["context"]["auxData"]["isDefault"] == true
        })
        .and_then(|f| f["params"]["context"]["id"].as_u64())
        .expect("main context announced");
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return 1; }", "executionContextId": main_id}),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"number","value":1}));
}
/// `objectId` の `<contextId>.<n>` の `n`。
fn handle_no(object_id: &str) -> u64 {
    object_id
        .split_once('.')
        .and_then(|(_, n)| n.parse().ok())
        .expect("objectId")
}

impl Cdp {
    /// `DOM.describeNode {objectId}` の応答フレーム。
    async fn describe(&mut self, p: &Page, object_id: &str) -> Value {
        self.call(p, "DOM.describeNode", json!({"objectId": object_id}))
            .await
            .pop()
            .expect("response frame")
    }

    /// `DOM.resolveNode` の応答フレーム。
    async fn resolve(&mut self, p: &Page, params: Value) -> Value {
        self.call(p, "DOM.resolveNode", params)
            .await
            .pop()
            .expect("response frame")
    }
}

/// 応答の `result.node.backendNodeId`（成功を要求する）。
fn backend_of(frame: &Value) -> i64 {
    assert!(frame.get("error").is_none(), "unexpected error: {frame}");
    let node = &frame["result"]["node"];
    assert_eq!(node["nodeId"], node["backendNodeId"], "{frame}");
    node["backendNodeId"].as_i64().expect("backendNodeId")
}

/// 応答の `result.object`（成功を要求する）。
fn object_of(frame: &Value) -> &Value {
    assert!(frame.get("error").is_none(), "unexpected error: {frame}");
    &frame["result"]["object"]
}

/// `DOM.describeNode` / `DOM.resolveNode`（world をまたぐノード同一性。#1348・`CDP-2`・`CDP-3`・`CDP-6`・`SEC-2`・`JS-4`）。
async fn dom_node(engine: &str) {
    let mut c = Cdp::new(engine);
    let p = c.page().await;
    let nav = c.navigate(&p).await;
    let main_id = main_context_id(&nav);
    let iso = c.isolated(&p, "u").await;

    // 前提: 両 world に初期ノード同一性の窓口がある。
    for ctx in [main_id, iso] {
        let f = c.eval(&p, ctx, "typeof __fandheDomIdentity", true).await;
        assert_eq!(ro(&f), &json!({"type":"string","value":"object"}), "{f}");
        let f = c
            .eval(&p, ctx, "__fandheDomIdentity.orderOf(document)", true)
            .await;
        assert_eq!(ro(&f), &json!({"type":"number","value":1}), "{f}");
    }

    // (a) Puppeteer の第 1 手: DOM.getDocument なしで main の document を describe する。
    let doc_handle = c.handle(&p, main_id, "document").await;
    let f = c.describe(&p, &doc_handle).await;
    assert_eq!(backend_of(&f), 1, "{f}");
    assert_eq!(f["result"]["node"]["nodeName"], "#document");
    assert_eq!(f["result"]["node"]["nodeType"], 9);
    let f = c
        .call(&p, "DOM.getDocument", json!({}))
        .await
        .pop()
        .expect("frame");
    assert_eq!(f["result"]["root"]["nodeId"], 1, "{f}");

    // (b) 往復（AC1）: isolated → DOM ドメインの ID → main。
    let x_iso = c.handle(&p, iso, "document.getElementById('x')").await;
    let f = c.describe(&p, &x_iso).await;
    let x_backend = backend_of(&f);
    assert_eq!(x_backend, 5, "{f}");
    assert_eq!(f["result"]["node"]["nodeName"], "P");
    assert_eq!(f["result"]["node"]["attributes"], json!(["id", "x"]));
    assert_eq!(f["result"]["node"]["parentId"], 4);
    let f = c
        .call(
            &p,
            "DOM.querySelector",
            json!({"nodeId": 1, "selector": "#x"}),
        )
        .await
        .pop()
        .expect("frame");
    assert_eq!(f["result"]["nodeId"], x_backend, "{f}");
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": x_backend, "executionContextId": main_id}),
        )
        .await;
    let obj = object_of(&f);
    assert_eq!(obj["type"], "object");
    assert_eq!(obj["subtype"], "node");
    assert_eq!(obj["description"], "p");
    let x_main = obj["objectId"].as_str().expect("objectId").to_owned();
    assert!(x_main.starts_with(&format!("{main_id}.")), "{x_main}");
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return this.id; }", "objectId": x_main, "returnByValue": true}),
        )
        .await;
    assert_eq!(ro(&f), &json!({"type":"string","value":"x"}), "{f}");
    // 逆向き（main → isolated）も同じ backendNodeId。
    let f = c.describe(&p, &x_main).await;
    assert_eq!(backend_of(&f), x_backend, "{f}");
    let f = c
        .resolve(&p, json!({"nodeId": x_backend, "executionContextId": iso}))
        .await;
    let x_back = object_of(&f)["objectId"]
        .as_str()
        .expect("objectId")
        .to_owned();
    assert!(x_back.starts_with(&format!("{iso}.")), "{x_back}");

    // (c) 1 回の resolve で払い出すハンドルは 1 つだけ。
    let r1 = c
        .resolve(
            &p,
            json!({"backendNodeId": x_backend, "executionContextId": main_id}),
        )
        .await;
    let r2 = c
        .resolve(
            &p,
            json!({"backendNodeId": x_backend, "executionContextId": main_id}),
        )
        .await;
    let (id1, id2) = (
        object_of(&r1)["objectId"].as_str().expect("id").to_owned(),
        object_of(&r2)["objectId"].as_str().expect("id").to_owned(),
    );
    assert_ne!(id1, id2);
    assert_eq!(handle_no(&id2), handle_no(&id1) + 1);
    let next = c.handle(&p, main_id, "({})").await;
    assert_eq!(handle_no(&next), handle_no(&id2) + 1);
    let f = c.release(&p, &id1).await;
    assert_eq!(f["result"], json!({}));
    let f = c.release(&p, &id1).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");

    // (d) 他セッション。
    let sid2 = c.st.registry().attach(&p.tid).expect("attach 2");
    let p2 = Page {
        tid: p.tid.clone(),
        sid: sid2,
    };
    let f = c.describe(&p2, &x_iso).await;
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    let f = c.resolve(&p2, json!({"backendNodeId": x_backend})).await;
    assert!(is_err(&f, -32000, "node not found"), "{f}");
    let doc2 = eval_default(&mut c, &p2, "document", false).await;
    let doc2_handle = ro(&doc2)["objectId"].as_str().expect("objectId").to_owned();
    let f = c.describe(&p2, &doc2_handle).await;
    let doc2_backend = backend_of(&f);
    assert!(doc2_backend > 6, "second session gets its own range: {f}");
    // 2 つ目のセッションが払い出した後でも、1 つ目の範囲の ID と逆向きの ID は拒否される。
    let f = c.resolve(&p2, json!({"backendNodeId": x_backend})).await;
    assert!(is_err(&f, -32000, "node not found"), "{f}");
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": doc2_backend, "executionContextId": main_id}),
        )
        .await;
    assert!(is_err(&f, -32000, "node not found"), "{f}");
    let f = c
        .resolve(
            &p2,
            json!({"backendNodeId": doc2_backend, "executionContextId": main_id}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let f = c.resolve(&p2, json!({"backendNodeId": doc2_backend})).await;
    assert_eq!(object_of(&f)["subtype"], "node", "{f}");

    // (e) 世代違い。
    let nav2 = c.navigate(&p).await;
    let main2 = main_context_id(&nav2);
    assert_ne!(main2, main_id);
    let f = c.describe(&p, &x_iso).await;
    // 遷移で旧世代の contextId は表から落ちる（存在を区別しない固定の拒否）。
    assert!(is_err(&f, -32000, "remote object not found"), "{f}");
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": x_backend, "executionContextId": iso}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": x_backend, "executionContextId": main2}),
        )
        .await;
    assert!(is_err(&f, -32000, "node not found"), "{f}");

    // (f) 確定後に作られたノードと非ノード。
    let created = c.handle(&p, main2, "document.createElement('div')").await;
    let f = c.describe(&p, &created).await;
    assert!(is_err(&f, -32000, "node has no backend node id"), "{f}");
    let appended = c
        .handle(
            &p,
            main2,
            "(function () { var d = document.createElement('div'); document.body.appendChild(d); return d; })()",
        )
        .await;
    let f = c.describe(&p, &appended).await;
    assert!(is_err(&f, -32000, "node has no backend node id"), "{f}");
    let plain = c.handle(&p, main2, "({})").await;
    let f = c.describe(&p, &plain).await;
    assert!(is_err(&f, -32000, "object is not a node"), "{f}");
    // 未払い出しの文書では、どの backendNodeId も解決しない。
    let f = c
        .resolve(&p, json!({"backendNodeId": 5, "executionContextId": main2}))
        .await;
    assert!(is_err(&f, -32000, "node not found"), "{f}");

    // (g) 範囲外。
    let doc_h = c.handle(&p, main2, "document").await;
    let base = backend_of(&c.describe(&p, &doc_h).await) - 1;
    for bad in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("5"),
        json!(9_007_199_254_740_992_i64),
    ] {
        let f = c
            .resolve(
                &p,
                json!({"backendNodeId": bad, "executionContextId": main2}),
            )
            .await;
        assert!(is_err(&f, -32602, "invalid params"), "{bad}: {f}");
    }
    for out in [base, base + 1000, base + 4_294_967_296] {
        let f = c
            .resolve(
                &p,
                json!({"backendNodeId": out, "executionContextId": main2}),
            )
            .await;
        assert!(is_err(&f, -32000, "node not found"), "{out}: {f}");
    }

    // (h) world 間で DOM の変更は共有されない（R6）。
    let iso2 = c.isolated(&p, "w").await;
    let x2 = c.handle(&p, iso2, "document.getElementById('x')").await;
    let f = c
        .eval(
            &p,
            iso2,
            "document.getElementById('x').setAttribute('data-k', '1')",
            true,
        )
        .await;
    assert!(f.get("error").is_none(), "{f}");
    let f = c.describe(&p, &x2).await;
    assert_eq!(f["result"]["node"]["attributes"], json!(["id", "x"]), "{f}");
    let x2_backend = backend_of(&f);
    assert_eq!(x2_backend, base + 5);
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": x2_backend, "executionContextId": main2}),
        )
        .await;
    let x2_main = object_of(&f)["objectId"].as_str().expect("id").to_owned();
    let f = c
        .callf(
            &p,
            json!({"functionDeclaration": "function () { return this.getAttribute('data-k'); }", "objectId": x2_main, "returnByValue": true}),
        )
        .await;
    assert_eq!(
        ro(&f),
        &json!({"type":"object","subtype":"null","value":null}),
        "{f}"
    );

    // (i) その他のキー。
    let f = c
        .call(
            &p,
            "DOM.describeNode",
            json!({"objectId": x2, "pierce": true}),
        )
        .await
        .pop()
        .expect("frame");
    assert!(is_err(&f, -32602, "unsupported params"), "{f}");
    let f = c
        .call(&p, "DOM.describeNode", json!({"nodeId": 1}))
        .await
        .pop()
        .expect("frame");
    assert!(is_err(&f, -32602, "unsupported params"), "{f}");
    let f = c
        .call(
            &p,
            "DOM.describeNode",
            json!({"objectId": doc_h, "depth": 0}),
        )
        .await
        .pop()
        .expect("frame");
    let node = &f["result"]["node"];
    assert!(node.get("children").is_none(), "{f}");
    assert_eq!(node["childNodeCount"], 1, "{f}");
    let f = c.describe(&p, &doc_h).await;
    assert_eq!(
        f["result"]["node"]["children"][0]["nodeName"], "HTML",
        "{f}"
    );
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": x2_backend, "executionContextId": main2, "objectGroup": "g"}),
        )
        .await;
    assert!(is_err(&f, -32602, "unsupported params"), "{f}");
    let f = c.resolve(&p, json!({"backendNodeId": x2_backend})).await;
    assert!(
        object_of(&f)["objectId"]
            .as_str()
            .expect("id")
            .starts_with(&format!("{main2}.")),
        "default is the main world: {f}"
    );
    // 未知・他セッションの contextId は存在を区別しない。
    let f = c
        .resolve(
            &p,
            json!({"backendNodeId": x2_backend, "executionContextId": 999_999}),
        )
        .await;
    assert!(is_err(&f, -32602, "invalid params"), "{f}");

    // (j) sessionId なし。
    let req = json!({"id": 9001, "method": "DOM.describeNode", "params": {"objectId": "1.1"}});
    let frames = c.d.dispatch(&c.st, &req.to_string()).await;
    let v: Value = serde_json::from_str(frames.last().expect("frame")).expect("json");
    assert!(is_err(&v, -32602, "unsupported params"), "{v}");

    // Runtime.addBinding は未実装のまま（成功を装わない。CDP-6）。
    let f = c
        .call(&p, "Runtime.addBinding", json!({"name": "b"}))
        .await
        .pop()
        .expect("frame");
    assert!(is_err(&f, -32601, "method not implemented"), "{f}");
}

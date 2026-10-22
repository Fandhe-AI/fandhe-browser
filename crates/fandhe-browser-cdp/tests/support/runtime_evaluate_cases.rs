//! V8 / boa 共通の `Runtime.evaluate`・`Runtime.releaseObject` の結合テストケース（TASK-44.7・#1218・
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
    eprintln!("case: envelope hardening");
    rt.block_on(hardening(engine));
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
    let f = c.eval(&p, main_id, "1", false).await;
    assert!(
        is_err(&f, -32000, "evaluation in the main world is not supported"),
        "{f}"
    );
    // contextId 省略は既定（main）world 扱い。
    let f = c
        .call(&p, "Runtime.evaluate", json!({"expression": "1"}))
        .await
        .pop()
        .expect("frame");
    assert!(
        is_err(&f, -32000, "evaluation in the main world is not supported"),
        "{f}"
    );
    assert_eq!(c.worker_spawns(), 0, "main world 評価は評価器を起動しない");

    // 未対応・未知のパラメータは成功を装わず拒否する。
    let iso = c.isolated(&p, "u").await;
    for params in [
        json!({"expression":"1","contextId":iso,"awaitPromise":true}),
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
    assert_eq!(c.worker_spawns(), 0);

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
        assert_eq!(c.worker_spawns(), 0);
    });
}

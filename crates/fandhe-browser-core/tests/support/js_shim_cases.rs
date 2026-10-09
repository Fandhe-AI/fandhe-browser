//! V8 / boa 共通の JS shim テストケース（TASK-108・Issue #778・ビヘイビア `JS-5` / `JS-6`）。
//!
//! `tests/js_shim_v8.rs` と `tests/js_shim_boa.rs` が `#[path]` で取り込み、同一の shim・
//! 同一のスニペットが両エンジンで同じ結果になることを確認する。各ケースは
//! 「パース → `DomBridge::attach` → `JsRuntime::install_dom_shim` → 評価 → DOM の
//! シリアライズ結果の比較」の順で、期待値は具体値で書く。

use fandhe_browser_core::js_stub::{JsRuntime, execute_js_stub};
use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{
    Config, DocumentReadyState, DomBridge, DomBridgeLimits, DomLimits, Error, LocationChangeKind,
    NodeId, ParseOptions, parse_document, query_selector,
};

const FIXTURE: &str =
    "<html><head></head><body><div id=\"a\"><p>1</p><p>2</p></div><script>x</script></body></html>";
const FIXTURE_BODY: &str = "<div id=\"a\"><p>1</p><p>2</p></div><script>x</script>";

struct Page {
    bridge: DomBridge,
    rt: JsRuntime,
    script: NodeId,
}

fn setup(engine: &str, limit_extra_nodes: Option<usize>) -> Page {
    let mut doc = parse_document(FIXTURE, &ParseOptions::default())
        .expect("fixture parses")
        .document;
    let list = parse_selector_list("script").expect("selector");
    let script = query_selector(&doc, doc.root(), &list)
        .expect("query")
        .expect("script exists");
    if let Some(extra) = limit_extra_nodes {
        let limits = DomLimits::default().with_max_nodes(doc.node_count() + extra);
        doc.set_limits(limits);
    }
    let bridge = DomBridge::new(DomBridgeLimits::default());
    bridge.attach(doc).expect("attach");
    let cfg = Config::from_toml_str(&format!("[js]\nengine = \"{engine}\"\n")).expect("config");
    let mut rt = JsRuntime::from_config(cfg.js()).expect("runtime");
    let installed = rt.install_dom_shim(&bridge).expect("install shim");
    assert_eq!(installed.evaluated, vec!["dom.js", "window.js"]);
    Page { bridge, rt, script }
}

impl Page {
    fn eval(&mut self, script: &str) -> String {
        match execute_js_stub(&mut self.rt, script) {
            Ok(out) => out.value,
            Err(e) => panic!("script failed: {e}\n{script}"),
        }
    }

    fn html(&self) -> String {
        self.bridge
            .with_document(|d| d.serialize_html().expect("serialize").into_html())
            .expect("lock")
            .expect("attached")
    }
}

fn check(page: &mut Page, script: &str, expected: &str) {
    eprintln!("  js: {script}");
    assert_eq!(page.eval(script), expected, "script: {script}");
}

/// 全ケースを `engine`（`"v8"` / `"boa"`）で実行する。
pub fn run(engine: &str) {
    eprintln!("case: JS-5 document basics (readyState / currentScript)");
    let mut p = setup(engine, None);
    check(&mut p, "typeof document", "object");
    check(&mut p, "document.readyState", "loading");
    p.bridge
        .set_ready_state(DocumentReadyState::Interactive)
        .expect("set");
    check(&mut p, "document.readyState", "interactive");
    check(&mut p, "document.currentScript === null", "true");
    p.bridge.set_current_script(Some(p.script)).expect("set");
    check(
        &mut p,
        "document.currentScript === document.querySelector('script')",
        "true",
    );
    check(&mut p, "document.currentScript.textContent", "x");
    // ページ JS から書き換えても Rust 側の状態は変わらない。
    check(
        &mut p,
        "try { document.readyState = 'complete'; } catch (e) {} document.readyState",
        "interactive",
    );

    eprintln!("case: JS-5 createElement / appendChild / body / head");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('DIV'); d.setAttribute('class', 'k'); document.body.appendChild(d) === d",
        "true",
    );
    check(
        &mut p,
        "document.body.innerHTML",
        &format!("{FIXTURE_BODY}<div class=\"k\"></div>"),
    );
    check(
        &mut p,
        "document.head !== null && document.head !== document.body",
        "true",
    );

    eprintln!("case: JS-5 getElementById / querySelector(All)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "document.getElementById('a') === document.getElementById('a')",
        "true",
    );
    check(&mut p, "document.getElementById('nope') === null", "true");
    check(&mut p, "document.querySelectorAll('p').length", "2");
    check(
        &mut p,
        "document.querySelectorAll('p').map(function (e) { return e.textContent; }).join('|')",
        "1|2",
    );
    check(
        &mut p,
        "document.getElementById('a').querySelector('p').textContent",
        "1",
    );
    check(&mut p, "document.querySelectorAll('table').length", "0");
    check(&mut p, "document.querySelector('table') === null", "true");

    eprintln!("case: JS-5 insertBefore / removeChild");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var a = document.getElementById('a'); var n = document.createElement('span'); \
         a.insertBefore(n, null); var f = document.createElement('b'); \
         a.insertBefore(f, a.querySelector('p')); var r = a.removeChild(n); \
         (r === n) + ':' + a.innerHTML",
        "true:<b></b><p>1</p><p>2</p>",
    );

    eprintln!("case: JS-5 setAttribute / removeAttribute");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var a = document.getElementById('a'); a.setAttribute('data-x', 1); a.removeAttribute('id'); document.body.innerHTML",
        "<div data-x=\"1\"><p>1</p><p>2</p></div><script>x</script>",
    );
    assert_eq!(
        p.html(),
        "<html><head></head><body><div data-x=\"1\"><p>1</p><p>2</p></div><script>x</script></body></html>"
    );

    eprintln!("case: JS-5 textContent get / set");
    let mut p = setup(engine, None);
    check(&mut p, "document.querySelector('p').textContent", "1");
    check(
        &mut p,
        "var a = document.getElementById('a'); a.textContent = null; a.innerHTML",
        "",
    );
    check(&mut p, "a.textContent = '<i>'; a.innerHTML", "&lt;i&gt;");
    check(&mut p, "a.textContent", "<i>");

    eprintln!("case: JS-5 innerHTML setter (core parser)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var a = document.getElementById('a'); a.innerHTML = '<p id=\"x\">a</p><b>b</b>'; \
         document.getElementById('x').textContent",
        "a",
    );
    assert_eq!(
        p.html(),
        "<html><head></head><body><div id=\"a\"><p id=\"x\">a</p><b>b</b></div><script>x</script></body></html>"
    );
    check(&mut p, "a.innerHTML", "<p id=\"x\">a</p><b>b</b>");

    eprintln!("case: JS-6 innerHTML honours the node limit");
    let mut p = setup(engine, Some(2));
    let before = p.html();
    check(
        &mut p,
        "try { document.getElementById('a').innerHTML = '<i></i><i></i><i></i>'; 'ok' } catch (e) { 'threw' }",
        "threw",
    );
    assert_eq!(p.html(), before);
    check(
        &mut p,
        "try { document.getElementById('a').innerHTML = '<i></i>'; 'ok' } catch (e) { 'threw' }",
        "ok",
    );

    eprintln!("case: JS-5 bypassing the shim cannot corrupt the DOM");
    let mut p = setup(engine, None);
    let before = p.html();
    for call in [
        "__dom.op('appendChild', NaN, 1)",
        "__dom.op('appendChild', 1, 2)",
        "__dom.op('appendChild', 4294967296 * 99, 1)",
        "__dom.op('removeChild', -1, 0)",
        "__dom.op('nope')",
        "__dom.op('createElement')",
        "__dom.op('createElement', 5)",
        "__dom.op('setInnerHTML', 1, 2)",
        "__dom.op({}, 1)",
        "__dom.op()",
    ] {
        check(
            &mut p,
            &format!("try {{ {call}; 'ok' }} catch (e) {{ 'threw' }}"),
            "threw",
        );
    }
    assert_eq!(p.html(), before);
    // ページが __dom を潰しても shim は捕捉済みの関数で動き続ける。
    check(
        &mut p,
        "globalThis.__dom = null; document.getElementById('a') !== null",
        "true",
    );

    eprintln!("case: JS-5 shim argument checks");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "try { new Node(); 'ok' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "try { document.body.appendChild({}); 'ok' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "document.body instanceof Element && document instanceof Node",
        "true",
    );
    check(
        &mut p,
        "document.createTextNode('t') instanceof Text",
        "true",
    );

    eprintln!("case: JS-5 reinstall and bridge identity");
    let mut p = setup(engine, None);
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(&mut p, "document.readyState", "loading");
    let other = DomBridge::new(DomBridgeLimits::default());
    let err = p.rt.install_dom_shim(&other).expect_err("different bridge");
    assert!(matches!(err, Error::Unsupported { .. }), "{err}");

    eprintln!("case: JS-5 reinstall survives page tampering with __dom");
    let mut p = setup(engine, None);
    p.eval(
        "try { globalThis.__dom = null; } catch (e) {} \
         try { delete globalThis.__dom; } catch (e) {} \
         try { globalThis.__dom = { op: function () { return 'fake'; } }; } catch (e) {} \
         'tampered'",
    );
    p.rt.install_dom_shim(&p.bridge)
        .expect("reinstall after tamper");
    check(&mut p, "document.readyState", "loading");
    check(&mut p, "typeof document.createElement('p')", "object");

    window_cases(engine);

    eprintln!("case: JS-5 disabled runtime does not pretend to install");
    let mut disabled = JsRuntime::disabled();
    let err = disabled.install_dom_shim(&p.bridge).expect_err("disabled");
    assert!(matches!(err, Error::JsExecutionUnavailable { .. }), "{err}");
}
/// `window` / `location` / `navigator` / `console`（#779・`JS-5`・`SEC-2`）のケース。
fn window_cases(engine: &str) {
    const URL: &str = "https://example.com:8443/a/b?q=1#f";

    eprintln!("case: JS-5 window / self / globalThis");
    let mut p = setup(engine, None);
    check(&mut p, "window === self && self === globalThis", "true");
    check(&mut p, "window.document === document", "true");
    check(&mut p, "typeof window", "object");

    eprintln!("case: JS-5 location defaults to about:blank");
    check(&mut p, "location.href", "about:blank");
    check(&mut p, "location.origin", "null");

    eprintln!("case: JS-5 location reflects the navigated URL");
    p.bridge
        .set_location("https://user:pw@example.com:8443/a/b?q=1#f")
        .expect("set_location");
    check(&mut p, "location.href", URL);
    check(&mut p, "location.origin", "https://example.com:8443");
    check(&mut p, "location.protocol", "https:");
    check(&mut p, "location.host", "example.com:8443");
    check(&mut p, "location.hostname", "example.com");
    check(&mut p, "location.port", "8443");
    check(&mut p, "location.pathname", "/a/b");
    check(&mut p, "location.search", "?q=1");
    check(&mut p, "location.hash", "#f");
    check(&mut p, "String(location)", URL);
    check(&mut p, "window.location === location", "true");
    assert!(p.bridge.set_location("file:///etc/passwd").is_err());

    eprintln!("case: JS-5 navigation attempts are recorded and ignored");
    for script in [
        "location.href = 'https://evil.test/'",
        "location = 'https://evil.test/'",
        "location.pathname = '/x'",
        "location.search = '?z'",
        "location.assign('https://evil.test/a')",
        "location.replace('https://evil.test/b')",
        "location.reload()",
    ] {
        check(
            &mut p,
            &format!("try {{ {script}; 'ok' }} catch (e) {{ 'threw' }}"),
            "ok",
        );
    }
    check(&mut p, "location.href", URL);
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    let got: Vec<_> = diag
        .location_changes_ignored
        .iter()
        .map(|c| (c.kind, c.value_len))
        .collect();
    assert_eq!(
        got,
        vec![
            (LocationChangeKind::Href, 18),
            (LocationChangeKind::AssignLocation, 18),
            (LocationChangeKind::Pathname, 2),
            (LocationChangeKind::Search, 2),
            (LocationChangeKind::Assign, 19),
            (LocationChangeKind::Replace, 19),
            (LocationChangeKind::Reload, 0),
        ]
    );
    assert!(!format!("{diag:?}").contains("evil.test"));
    assert_eq!(p.bridge.take_diagnostics().expect("empty").dropped, 0);

    eprintln!("case: JS-5 location.hash is the only writable part");
    check(
        &mut p,
        "location.hash = 'n'; location.href",
        "https://example.com:8443/a/b?q=1#n",
    );
    check(&mut p, "location.hash = '#m'; location.hash", "#m");
    check(
        &mut p,
        "location.hash = ''; location.href",
        "https://example.com:8443/a/b?q=1",
    );

    eprintln!("case: SEC-2 navigator is honest about being fandhe-browser");
    let mut p = setup(engine, None);
    let ua = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
    check(&mut p, "navigator.userAgent", &ua);
    check(&mut p, "navigator.webdriver === true", "true");
    check(
        &mut p,
        "/Chrome|Mozilla|Safari|Headless|Gecko/.test(navigator.userAgent)",
        "false",
    );
    check(&mut p, "typeof window.chrome", "undefined");
    check(
        &mut p,
        "try { navigator.webdriver = false; } catch (e) {} navigator.webdriver",
        "true",
    );
    check(
        &mut p,
        "try { new Navigator(); 'ok' } catch (e) { e instanceof TypeError }",
        "true",
    );

    eprintln!("case: JS-5 console is a non-throwing recorder");
    check(&mut p, "typeof console.log", "function");
    check(&mut p, "String(console.log('a', 1))", "undefined");
    check(
        &mut p,
        "console.log({ toString: function () { throw new Error('x'); } }); 'ok'",
        "ok",
    );
    check(
        &mut p,
        "console.group(); console.time('t'); console.timeEnd('t'); console.clear(); \
         console.assert(true); console.trace('t'); 'ok'",
        "ok",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.console_messages.len(), 3);
    assert_eq!(diag.console_messages[0].text, "a 1");
    assert_eq!(diag.console_messages[1].text, "[unprintable]");
    assert_eq!(diag.console_messages[2].text, "t");

    eprintln!("case: JS-5 console truncates page strings and caps forwarding");
    check(&mut p, "console.log('x'.repeat(100000)); 'ok'", "ok");
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.console_messages.len(), 1);
    assert_eq!(diag.console_messages[0].text.len(), 4096);
    assert!(diag.console_messages[0].truncated);
    // shim の転送上限（64 回）は install ごとに数えるため、新しいページで確かめる。
    let mut p = setup(engine, None);
    check(
        &mut p,
        "for (var i = 0; i < 100; i++) { console.log(i); } document.getElementById('a') !== null",
        "true",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.console_messages.len(), 64);

    eprintln!("case: JS-5 reinstall restores the window globals after tampering");
    let mut p = setup(engine, None);
    p.bridge.set_location("https://example.com/x").expect("set");
    p.eval(
        "try { delete globalThis.location; } catch (e) {} \
         globalThis.navigator = { userAgent: 'Chrome' }; \
         globalThis.console = null; \
         globalThis.window = 1; 'tampered'",
    );
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(&mut p, "navigator.userAgent", &ua);
    check(&mut p, "typeof console.log", "function");
    check(&mut p, "location.href", "https://example.com/x");
    check(&mut p, "window === globalThis", "true");
}

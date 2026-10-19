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
    assert_eq!(
        installed.evaluated,
        vec![
            "dom.js",
            "window.js",
            "events.js",
            "timers.js",
            "net.js",
            "observers.js",
            "style.js",
            "url.js",
        ]
    );
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
    observer_cases(engine);
    style_cases(engine);
    url_cases(engine);

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

    eprintln!("case: JS-4 reinstall fires only the new page's lifecycle listeners");
    let mut p = setup(engine, None);
    p.eval(
        "globalThis.log = []; \
         document.addEventListener('DOMContentLoaded', function () { log.push('old'); }); 'ok'",
    );
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    p.eval(
        "globalThis.log = []; \
         document.addEventListener('DOMContentLoaded', function () { log.push('new'); }); \
         window.addEventListener('load', function () { log.push('load'); }); 'ok'",
    );
    p.eval("__fandheLifecycle('DOMContentLoaded'); __fandheLifecycle('load'); 'fired'");
    check(&mut p, "log.join(',')", "new,load");

    eprintln!("case: JS-4 a frozen event does not stop later listeners");
    let mut p = setup(engine, None);
    p.eval(
        "globalThis.log = []; \
         document.addEventListener('DOMContentLoaded', function (e) { Object.freeze(e); log.push('a'); }); \
         window.addEventListener('DOMContentLoaded', function (e) { log.push('b:' + (e.currentTarget === window)); }); 'ok'",
    );
    p.eval("__fandheLifecycle('DOMContentLoaded'); 'fired'");
    check(&mut p, "log.join(',')", "a,b:true");

    eprintln!("case: JS-4 page cannot replace the lifecycle dispatcher");
    let mut p = setup(engine, None);
    p.eval(
        "globalThis.log = []; \
         document.addEventListener('DOMContentLoaded', function () { log.push('kept'); }); \
         __fandheLifecycle.setImpl(function () { log.push('hijacked'); }); 'ok'",
    );
    p.eval("__fandheLifecycle('DOMContentLoaded'); 'fired'");
    check(&mut p, "log.join(',')", "kept");

    eprintln!("case: JS-4 own `call` on __dom.op cannot unlock the dispatcher");
    let mut p = setup(engine, None);
    p.eval(
        "globalThis.log = []; \
         document.addEventListener('DOMContentLoaded', function () { log.push('kept'); }); \
         try { __dom.op.call = function () { return true; }; } catch (e) {} \
         __fandheLifecycle.setImpl(function () { log.push('hijacked'); }); 'ok'",
    );
    p.eval("__fandheLifecycle('DOMContentLoaded'); 'fired'");
    check(&mut p, "log.join(',')", "kept");

    eprintln!("case: JS-4 replacing Object.defineProperty does not stop listeners");
    let mut p = setup(engine, None);
    p.eval(
        "globalThis.log = []; \
         document.addEventListener('DOMContentLoaded', function () { log.push('ran'); }); \
         Object.defineProperty = function () { throw new Error('x'); }; 'ok'",
    );
    p.eval("__fandheLifecycle('DOMContentLoaded'); 'fired'");
    check(&mut p, "log.join(',')", "ran");

    eprintln!("case: JS-4 saved event currentTarget is null after dispatch");
    let mut p = setup(engine, None);
    p.eval(
        "globalThis.saved = null; \
         document.addEventListener('DOMContentLoaded', function (e) { saved = e; }); 'ok'",
    );
    p.eval("__fandheLifecycle('DOMContentLoaded'); 'fired'");
    check(&mut p, "String(saved.currentTarget)", "null");

    eprintln!("case: JS-6 listener count is capped per target and flagged to the runner");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "globalThis.n = 0; \
         for (var i = 0; i < 1100; i++) { \
           document.addEventListener('DOMContentLoaded', function () { n++; }); \
         } \
         window.addEventListener('load', function () { n += 1000; }); 'ok'",
        "ok",
    );
    assert!(p.bridge.listener_limit_exceeded().expect("flag"));
    p.eval("__fandheLifecycle('DOMContentLoaded'); __fandheLifecycle('load'); 'fired'");
    // document は 1024 件で打ち止め、window は別枠で登録できる。
    check(&mut p, "String(n)", "2024");
    let mut p = setup(engine, None);
    p.eval("document.addEventListener('load', function () {}); 'ok'");
    assert!(!p.bridge.listener_limit_exceeded().expect("flag"));
}
/// `MutationObserver` / `MutationRecord`（#1138・`JS-5`・`JS-7`・`JS-6`・`REPAIR-3`・`SEC-2`）のケース。
///
/// 配信は shim 内の microtask キューで行われるため、ランナーの checkpoint に相当する
/// `__fandheRunMicrotasks()` を評価して drain する。
fn observer_cases(engine: &str) {
    eprintln!("case: JS-5 MutationObserver API surface and argument validation");
    let mut p = setup(engine, None);
    check(&mut p, "typeof MutationObserver", "function");
    check(&mut p, "typeof MutationRecord", "function");
    check(&mut p, "typeof WebKitMutationObserver", "undefined");
    for (script, label) in [
        ("new MutationObserver(1)", "callback"),
        ("MutationObserver(function () {})", "new"),
        (
            "new MutationObserver(function () {}).observe(document.body, {})",
            "empty options",
        ),
        (
            "new MutationObserver(function () {}).observe({}, { childList: true })",
            "target",
        ),
        (
            "new MutationObserver(function () {}).observe(document.body, { attributes: false, attributeOldValue: true })",
            "oldValue without attributes",
        ),
        ("new MutationRecord()", "illegal constructor"),
    ] {
        eprintln!("  rejected: {label}");
        check(
            &mut p,
            &format!(
                "try {{ {script}; 'no error' }} catch (e) {{ e instanceof TypeError ? 'TypeError' : 'other' }}"
            ),
            "TypeError",
        );
    }
    check(
        &mut p,
        "typeof globalThis.__fandheDomMutationHandoff",
        "undefined",
    );

    eprintln!("case: JS-7 callback runs once in a microtask with batched records");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var calls = 0, got = null; \
         var mo = new MutationObserver(function (r, o) { calls++; got = r.length + ':' + r[0].type + ':' + (o === mo); }); \
         mo.observe(document.body, { childList: true }); \
         document.body.appendChild(document.createElement('i')); calls",
        "0",
    );
    check(
        &mut p,
        "__fandheRunMicrotasks(); calls + '|' + got",
        "1|1:childList:true",
    );
    check(&mut p, "__fandheRunMicrotasks(); calls", "1");
    check(
        &mut p,
        "calls = 0; document.body.appendChild(document.createElement('b')); \
         document.body.appendChild(document.createElement('u')); \
         __fandheRunMicrotasks(); calls + '|' + got",
        "1|2:childList:true",
    );

    eprintln!("case: JS-5 record fields for attributes / removeChild / insertBefore");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var recs = []; var el = document.getElementById('a'); \
         var mo = new MutationObserver(function (r) { for (var i = 0; i < r.length; i++) recs.push(r[i]); }); \
         mo.observe(el, { attributes: true, childList: true }); \
         el.setAttribute('data-x', '1'); el.removeAttribute('data-x'); \
         var first = el.querySelector('p'); var extra = document.createElement('i'); \
         el.insertBefore(extra, first); el.removeChild(extra); __fandheRunMicrotasks(); recs.length",
        "4",
    );
    check(
        &mut p,
        "recs[0].type + ',' + recs[0].attributeName + ',' + recs[0].oldValue + ',' + (recs[0].target === el)",
        "attributes,data-x,null,true",
    );
    check(
        &mut p,
        "recs[1].type + ',' + recs[1].attributeName",
        "attributes,data-x",
    );
    check(
        &mut p,
        "recs[2].addedNodes.length + ',' + (recs[2].addedNodes[0] === extra) + ',' + (recs[2].nextSibling === first) + ',' + recs[2].removedNodes.length",
        "1,true,true,0",
    );
    check(
        &mut p,
        "recs[3].removedNodes.length + ',' + (recs[3].removedNodes[0] === extra) + ',' + recs[3].previousSibling",
        "1,true,null",
    );

    eprintln!("case: JS-5 removeAttribute of a missing attribute makes no record");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var n = 0; var el = document.getElementById('a'); \
         var mo = new MutationObserver(function (r) { n += r.length; }); \
         mo.observe(el, { attributes: true }); \
         el.removeAttribute('data-none'); __fandheRunMicrotasks(); var none = n; \
         el.setAttribute('data-x', '1'); el.removeAttribute('data-x'); __fandheRunMicrotasks(); \
         none + ',' + n",
        "0,2",
    );

    eprintln!("case: JS-5 false oldValue options and iterable attributeFilter");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var ok = true; var el = document.getElementById('a'); var mo = new MutationObserver(function () {}); \
         try { mo.observe(el, { childList: true, attributes: false, attributeOldValue: false, characterData: false, characterDataOldValue: false }); } catch (e) { ok = false; } \
         var thrown = false; \
         try { mo.observe(el, { childList: true, attributes: false, attributeOldValue: true }); } catch (e) { thrown = e instanceof TypeError; } \
         ok + ',' + thrown",
        "true,true",
    );
    check(
        &mut p,
        "var got = []; var mo2 = new MutationObserver(function (r) { for (var i = 0; i < r.length; i++) got.push(r[i].attributeName); }); \
         mo2.observe(el, { attributeFilter: new Set(['data-x']) }); \
         el.setAttribute('data-y', '1'); el.setAttribute('data-x', '1'); __fandheRunMicrotasks(); got.join('|')",
        "data-x",
    );

    eprintln!("case: JS-5 attribute names are normalized for HTML elements");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var got = []; var el = document.getElementById('a'); \
         var mo = new MutationObserver(function (r) { for (var i = 0; i < r.length; i++) got.push(r[i].attributeName); }); \
         mo.observe(el, { attributeFilter: ['data-x'] }); \
         el.setAttribute('DATA-X', '1'); el.removeAttribute('Data-X'); __fandheRunMicrotasks(); got.join('|')",
        "data-x|data-x",
    );

    eprintln!("case: JS-6 disconnect and takeRecords leave nothing queued for delivery");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var calls = 0; var el = document.getElementById('a'); \
         var mo = new MutationObserver(function () { calls++; }); \
         mo.observe(el, { attributes: true }); el.setAttribute('data-x', '1'); mo.disconnect(); \
         mo.observe(el, { attributes: true }); el.setAttribute('data-y', '1'); mo.takeRecords(); \
         __fandheRunMicrotasks(); calls",
        "0",
    );

    eprintln!("case: JS-5 characterData / textContent / innerHTML");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var types = []; var mo = new MutationObserver(function (r) { for (var i = 0; i < r.length; i++) types.push(r[i].type + ':' + r[i].addedNodes.length); }); \
         var t = document.createTextNode('a'); document.body.appendChild(t); \
         mo.observe(t, { characterData: true }); mo.observe(document.getElementById('a'), { childList: true }); \
         t.textContent = 'b'; document.getElementById('a').textContent = 'c'; \
         document.getElementById('a').innerHTML = '<b></b>'; __fandheRunMicrotasks(); types.join(',')",
        "characterData:0,childList:0,childList:0",
    );

    eprintln!("case: JS-5 subtree requires the current ancestor relation");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var n = 0, tgt = null; var a = document.getElementById('a'); \
         var mo = new MutationObserver(function (r) { n += r.length; tgt = r[0].target; }); \
         mo.observe(document.body, { childList: true, subtree: true }); \
         a.appendChild(document.createElement('i')); __fandheRunMicrotasks(); n + '|' + (tgt === a)",
        "1|true",
    );
    check(
        &mut p,
        "n = 0; mo.disconnect(); mo.observe(document.body, { childList: true }); \
         a.appendChild(document.createElement('i')); __fandheRunMicrotasks(); n",
        "0",
    );

    eprintln!("case: JS-7 disconnect drops pending records and later changes");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var calls = 0; var mo = new MutationObserver(function () { calls++; }); \
         mo.observe(document.body, { childList: true }); \
         document.body.appendChild(document.createElement('i')); mo.disconnect(); \
         __fandheRunMicrotasks(); document.body.appendChild(document.createElement('i')); \
         __fandheRunMicrotasks(); calls",
        "0",
    );

    eprintln!("case: JS-7 takeRecords returns pending records and empties the queue");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var calls = 0; var mo = new MutationObserver(function () { calls++; }); \
         mo.observe(document.body, { childList: true }); \
         document.body.appendChild(document.createElement('i')); \
         var first = mo.takeRecords().length; var second = mo.takeRecords().length; \
         __fandheRunMicrotasks(); first + ',' + second + ',' + calls",
        "1,0,0",
    );

    eprintln!("case: JS-5 attributeFilter restricts attribute records");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var names = []; var el = document.getElementById('a'); \
         var mo = new MutationObserver(function (r) { for (var i = 0; i < r.length; i++) names.push(r[i].attributeName); }); \
         mo.observe(el, { attributeFilter: ['data-b'] }); \
         el.setAttribute('data-a', '1'); el.setAttribute('data-b', '1'); __fandheRunMicrotasks(); names.join(',')",
        "data-b",
    );
    assert_eq!(
        p.bridge
            .take_diagnostics()
            .expect("diagnostics")
            .mutation_options_ignored,
        0
    );

    eprintln!("case: REPAIR-3 unsupported oldValue options are accepted and counted");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var el = document.getElementById('a'); var mo = new MutationObserver(function () {}); \
         mo.observe(el, { attributes: true, attributeOldValue: true }); 'ok'",
        "ok",
    );
    assert_eq!(
        p.bridge
            .take_diagnostics()
            .expect("diagnostics")
            .mutation_options_ignored,
        1
    );

    eprintln!("case: JS-7 a throwing callback does not stop other observers");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var second = 0; \
         var m1 = new MutationObserver(function () { throw new Error('boom'); }); \
         var m2 = new MutationObserver(function () { second++; }); \
         m1.observe(document.body, { childList: true }); m2.observe(document.body, { childList: true }); \
         document.body.appendChild(document.createElement('i')); __fandheRunMicrotasks(); second",
        "1",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.timer_errors.len(), 1);
    assert_eq!(diag.timer_errors[0].message, "boom");

    eprintln!("case: JS-5 delivery survives page tampering with queueMicrotask");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var calls = 0; var mo = new MutationObserver(function () { calls++; }); \
         mo.observe(document.body, { childList: true }); globalThis.queueMicrotask = null; \
         document.body.appendChild(document.createElement('i')); __fandheRunMicrotasks(); calls",
        "1",
    );

    eprintln!("case: JS-6 pending record limit is reported to the runner");
    let mut p = setup(engine, None);
    check(
        &mut p,
        // op は worker プロセス越しで遅いため、observer 100 個 × 変更 101 回（op 数を抑えて）で上限を超える。
        "var mos = []; var total = 0; \
         for (var k = 0; k < 100; k++) { var m = new MutationObserver(function () {}); m.observe(document.body, { attributes: true }); mos.push(m); } \
         for (var i = 0; i < 101; i++) { document.body.setAttribute('data-n', String(i)); } \
         for (var k = 0; k < mos.length; k++) { total += mos[k].takeRecords().length; } total",
        "10000",
    );
    assert!(
        p.bridge
            .take_diagnostics()
            .expect("diagnostics")
            .mutation_observer_limit_exceeded
    );

    eprintln!("case: JS-6 registration limit is reported to the runner");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var el = document.createElement('i'); \
         for (var i = 0; i < 1001; i++) { new MutationObserver(function () {}).observe(el, { childList: true }); } 'ok'",
        "ok",
    );
    assert!(
        p.bridge
            .take_diagnostics()
            .expect("diagnostics")
            .mutation_observer_limit_exceeded
    );
}

/// 要素の `style`（#1141・`JS-5`・`JS-6`・`SEC-2`・`REPAIR-3`）のケース。
fn style_cases(engine: &str) {
    eprintln!("case: JS-5 style in-check (React-style feature detection)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var s = document.createElement('div').style; typeof s",
        "object",
    );
    check(&mut p, "s instanceof CSSStyleDeclaration", "true");
    check(&mut p, "'display' in s", "true");
    check(&mut p, "'cssFloat' in s", "true");
    check(&mut p, "'animation' in s", "true");
    check(
        &mut p,
        "'backgroundColor' in s && 'background-color' in s",
        "true",
    );
    check(&mut p, "'WebkitAnimation' in s", "false");
    check(&mut p, "'MozAnimation' in s", "false");
    check(
        &mut p,
        "var e = document.createElement('div'); e.style === e.style",
        "true",
    );
    check(
        &mut p,
        "try { new CSSStyleDeclaration(); 'no error' } catch (x) { x instanceof TypeError ? 'TypeError' : 'other' }",
        "TypeError",
    );
    check(&mut p, "typeof document.style", "undefined");

    eprintln!("case: JS-5 style camelCase read/write syncs the style attribute");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); d.style.backgroundColor = 'red'; \
         d.style.backgroundColor + '|' + d.style.getPropertyValue('background-color')",
        "red|red",
    );
    check(&mut p, "document.body.appendChild(d) === d", "true");
    assert!(
        p.html()
            .contains("<div style=\"background-color: red;\"></div>"),
        "html: {}",
        p.html()
    );
    check(
        &mut p,
        "d.style.cssFloat = 'left'; d.style.getPropertyValue('float')",
        "left",
    );

    eprintln!("case: JS-5 setProperty / getPropertyValue / removeProperty / priority");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var s = document.createElement('div').style; s.setProperty('color', 'blue', 'important'); \
         s.getPropertyPriority('color') + '|' + s.cssText",
        "important|color: blue !important;",
    );
    check(&mut p, "s.removeProperty('color')", "blue");
    check(&mut p, "s.getPropertyValue('color')", "");
    check(&mut p, "s.removeProperty('color')", "");
    check(
        &mut p,
        "s.setProperty('--MyVar', '1px'); s.setProperty('COLOR', 'red'); s.cssText",
        "--MyVar: 1px; color: red;",
    );
    check(
        &mut p,
        "s.getPropertyValue('--MyVar') + s.getPropertyValue('--myvar')",
        "1px",
    );
    check(&mut p, "s.setProperty('color', ''); s.length", "1");
    check(
        &mut p,
        "s.setProperty('1bad', 'x'); s.setProperty('a', 'b', 'bogus'); s.length",
        "1",
    );

    eprintln!("case: JS-5 style cssText parse / serialize");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); document.body.appendChild(d); \
         d.style.cssText = 'display: none; color: red'; \
         d.style.display + '|' + d.style.length + '|' + d.style.item(0) + '|' + d.style.item(5) + '|' + d.style.item(-1)",
        "none|2|display||",
    );
    assert!(
        p.html()
            .contains("<div style=\"display: none; color: red;\"></div>"),
        "html: {}",
        p.html()
    );
    check(
        &mut p,
        "d.style = 'top: 1px'; d.style.cssText + '|' + d.style.length",
        "top: 1px;|1",
    );
    check(
        &mut p,
        "d.style.cssText = 'background: url(\"a;b\") no-repeat !important; bad; : x; left: 2px;; '; d.style.cssText",
        "background: url(\"a;b\") no-repeat !important; left: 2px;",
    );
    check(
        &mut p,
        "d.style.cssText = ''; d.style.cssText + '|' + d.style.length",
        "|0",
    );
    assert!(
        p.html().contains("<div style=\"\"></div>"),
        "html: {}",
        p.html()
    );

    eprintln!("case: JS-5 style invalid value does not smuggle declarations");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var s = document.createElement('div').style; s.color = 'red'; \
         s.color = 'blue; position: fixed'; s.setProperty('width', '1px} x{'); s.cssText",
        "color: red;",
    );
    // コメント内の括弧で深さをずらして別宣言を紛れ込ませる試みは拒否する。
    check(
        &mut p,
        "s.color = 'red/*(*/; position: fixed; /*)*/'; s.setProperty('top', '1px/*'); \
         s.setProperty('left', 'red\\\\'); s.cssText",
        "color: red;",
    );
    // cssText ではコメントを除去し、コメント内の `;` と括弧を境界・深さに数えない。
    check(
        &mut p,
        "s.cssText = 'color: red/*(*/; top: 1px; /*)*/ left: 2px; /* a; b: c */ width: 3px'; \
         s.cssText",
        "color: red; top: 1px; left: 2px; width: 3px;",
    );

    // 引用符内の未エスケープ改行は文字列を終わらせるため、setProperty では拒否し、cssText では
    // 改行以降を別宣言として扱う（実属性の解釈と一致させる）。
    check(
        &mut p,
        "s.cssText = ''; s.setProperty('--x', '\"a\\n; color: red; x:\"'); s.cssText",
        "",
    );
    check(
        &mut p,
        "s.cssText = '--x: \"a\\n; color: red'; s.cssText",
        "color: red;",
    );
    // 名前の文字列化で style が更新されても、getPropertyPriority は最新のモデルを参照する。
    check(
        &mut p,
        "s.cssText = ''; s.getPropertyPriority({ toString: function () { \
         s.setProperty('color', 'red', 'important'); return 'color'; } })",
        "important",
    );

    let mut q = setup(engine, None);
    // 角括弧も丸括弧と同様に追跡する。未閉鎖の `[` は拒否し、cssText の `[a;b]` は 1 値として扱う。
    check(
        &mut q,
        "var s = document.createElement('div').style; s.setProperty('--x', '['); s.color = 'red'; s.cssText",
        "color: red;",
    );
    check(
        &mut q,
        "s.cssText = '--x: [a;b]; color: red'; s.length + '|' + s.getPropertyValue('--x') + '|' + s.color",
        "2|[a;b]|red",
    );
    // 既存名の更新は宣言の位置を保つ（新規名だけ末尾）。
    check(
        &mut q,
        "s.cssText = 'color: red; display: block'; s.color = 'blue'; s.item(0) + '|' + s.cssText",
        "color|color: blue; display: block;",
    );

    // 優先度の区切りは引用符・括弧の外の未エスケープの `!` だけ（core の parse_declarations と同じ）。
    check(
        &mut q,
        r#"s.cssText = '--x: \\!important; color: red'; s.length + '|' + s.getPropertyValue('--x') + '|[' + s.getPropertyPriority('--x') + ']|' + s.cssText"#,
        r#"2|\!important|[]|--x: \!important; color: red;"#,
    );
    check(
        &mut q,
        r#"s.cssText = '--y: "a!important"; --z: f(b!important); --w: a!b; color: red !important'; s.getPropertyValue('--y') + '|' + s.getPropertyValue('--z') + '|' + s.getPropertyValue('--w') + '|' + s.getPropertyPriority('--y') + s.getPropertyPriority('--z') + s.getPropertyPriority('--w') + '|' + s.getPropertyPriority('color') + '|' + s.length"#,
        r#""a!important"|f(b!important)|a!b||important|4"#,
    );

    // 上限超過の値は走査せずに拒否し、モデルと属性は変わらない（JS-6。二次時間の走査を避ける）。
    check(
        &mut q,
        "var big = '!A'.repeat(1000000); s.cssText = 'color: red'; s.setProperty('--x', big); s.color = big; s.cssText = big; s.cssText",
        "color: red;",
    );
    check(
        &mut q,
        "var e = document.createElement('div'); e.style.color = 'blue'; e.style.setProperty('--x', big); e.setAttribute('style', 'color: red'); e.style.setProperty('--y', '!A'.repeat(2049)); e.style.cssText + '|' + e.style.length",
        "color: red;|1",
    );

    eprintln!(
        "case: JS-5 style unbalanced paren / oversize cssText / removeProperty arg side effects"
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); document.body.appendChild(d); var s = d.style; \
         s.background = 'url('; s.color = 'red'; s.setProperty('top', 'calc(1px'); s.cssText",
        "color: red;",
    );
    check(
        &mut p,
        "s.cssText = 'x'.repeat(32769); s.cssText + '|' + s.length",
        "color: red;|1",
    );
    assert!(
        p.html().contains("style=\"color: red;\""),
        "attribute must be kept: {}",
        p.html()
    );
    check(&mut p, "s.cssText = ''; s.cssText + '|' + s.length", "|0");
    check(
        &mut p,
        "s.color = 'red'; s.removeProperty({ toString: function () { s.backgroundColor = 'blue'; return 'color'; } }); \
         s.cssText",
        "background-color: blue;",
    );

    eprintln!("case: JS-6 style limits are enforced and warned once");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var s = document.createElement('div').style; \
         for (var i = 0; i < 300; i++) { s.setProperty('p' + i, '1'); } \
         s.setProperty('q', 'x'.repeat(5000)); s.length",
        "256",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.console_messages.len(), 1);
    assert_eq!(
        diag.console_messages[0].text,
        "style shim limit exceeded: declaration ignored"
    );

    eprintln!("case: JS-5 style attribute sync and MutationObserver");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); d.setAttribute('STYLE', 'left: 2px'); \
         d.style.left + '|' + d.style.length",
        "2px|1",
    );
    check(&mut p, "d.removeAttribute('style'); d.style.length", "0");
    check(
        &mut p,
        "var seen = []; var mo = new MutationObserver(function (r) { \
           for (var i = 0; i < r.length; i++) seen.push(r[i].type + ':' + r[i].attributeName); }); \
         mo.observe(d, { attributes: true }); d.style.top = '1px'; __fandheRunMicrotasks(); seen.join(',')",
        "attributes:style",
    );

    eprintln!("case: REPAIR-3 style baseline of parser-created elements is unknown");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "document.body.innerHTML = '<p id=\"sp\" style=\"margin: 0\">x</p>'; \
         var sp = document.getElementById('sp'); sp.style.margin + '|' + sp.style.length",
        "|0",
    );
    check(
        &mut p,
        "sp.style.color = 'red'; sp.style.cssText",
        "color: red;",
    );
    assert!(
        p.html()
            .contains("<p id=\"sp\" style=\"color: red;\">x</p>"),
        "html: {}",
        p.html()
    );

    eprintln!("case: REPAIR-3 / SEC-2 no computed style stub and no vendor-prefixed names");
    let mut p = setup(engine, None);
    check(&mut p, "typeof getComputedStyle", "undefined");
    check(
        &mut p,
        "var bad = 0; var names = Object.getOwnPropertyNames(CSSStyleDeclaration.prototype); \
         for (var i = 0; i < names.length; i++) { \
           if (/^(webkit|moz|ms|-webkit-|-moz-|-ms-)/i.test(names[i])) bad++; } bad",
        "0",
    );
    check(&mut p, "names.length > 100", "true");

    eprintln!("case: JS-5 style model is committed only after the attribute write succeeds");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); document.body.appendChild(d); var s = d.style; var big = ''; \
         for (var i = 0; i < 4096; i++) big += '\\u3042'; \
         var thrown = 0; \
         for (var k = 0; k < 6; k++) { try { s.setProperty('--v' + k, big); } catch (e) { thrown++; } } \
         thrown + '|' + s.length + '|' + (s.getPropertyValue('--v5') === '')",
        "1|5|true",
    );
    let html = p.html();
    assert!(
        html.contains("--v4:") && !html.contains("--v5:"),
        "attribute must match model"
    );
    check(
        &mut p,
        "var before = s.cssText; var t2 = 0; \
         try { s.cssText = '--a: ' + big + '; --b: ' + big + '; --c: ' + big + '; \
                --d: ' + big + '; --e: ' + big + '; --f: ' + big; } catch (e) { t2++; } \
         t2 + '|' + (s.cssText === before)",
        "1|true",
    );
    check(&mut p, "s.removeProperty('--v0'); s.length", "4");
    let html = p.html();
    assert!(
        !html.contains("--v0:") && html.contains("--v1:"),
        "attribute must match model"
    );

    eprintln!("case: JS-5 style setAttribute / removeAttribute stringify arguments once");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); document.body.appendChild(d); var n = 0; \
         var v = { toString: function () { n++; return n === 1 ? 'color: red' : 'color: blue'; } }; \
         d.setAttribute('style', v); d.style.color + '|' + n",
        "red|1",
    );
    assert!(
        p.html().contains("color: red"),
        "attribute must hold the first stringification"
    );
    check(
        &mut p,
        "var m = 0; var nm = { toString: function () { m++; return m === 1 ? 'style' : 'x'; } }; \
         d.removeAttribute(nm); d.style.length + '|' + m",
        "0|1",
    );

    eprintln!(
        "case: JS-5 style cssText keeps an earlier !important against a later normal declaration"
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var d = document.createElement('div'); var s = d.style; \
         s.cssText = 'color: red !important; color: blue'; \
         s.color + '|' + s.getPropertyPriority('color') + '|' + s.cssText",
        "red|important|color: red !important;",
    );
    check(
        &mut p,
        "s.cssText = 'color: red !important; color: blue !important'; s.color",
        "blue",
    );
    check(
        &mut p,
        "s.cssText = 'color: red !important'; s.setProperty('color', 'green'); s.color + '|' + s.getPropertyPriority('color')",
        "green|",
    );
    // 値内のトップレベル `!important` は拒否し、重要宣言は priority 引数でのみ指定できる。
    check(
        &mut p,
        "s.cssText = ''; s.setProperty('color', 'red !important'); s.color = 'red !important'; \
         s.getPropertyValue('color') + '|' + s.getPropertyPriority('color') + '|' + s.cssText",
        "||",
    );
    check(
        &mut p,
        "s.setProperty('color', 'red', 'important'); \
         s.getPropertyValue('color') + '|' + s.getPropertyPriority('color') + '|' + s.cssText",
        "red|important|color: red !important;",
    );

    eprintln!("case: JS-5 style survives shim re-install without double wrapping");
    let mut p = setup(engine, None);
    p.rt.install_dom_shim(&p.bridge).expect("reinstall shim");
    check(
        &mut p,
        "var d = document.createElement('div'); d.style.color = 'red'; d.setAttribute('data-x', '1'); \
         d.style.cssText + '|' + (d.style instanceof CSSStyleDeclaration)",
        "color: red;|true",
    );
}

/// `URL` / `URLSearchParams`（#1144・`JS-5`・`JS-6`・`SEC-2`・`REPAIR-3`）のケース。
fn url_cases(engine: &str) {
    eprintln!("case: JS-5 URL globals and unsupported APIs stay undefined");
    let mut p = setup(engine, None);
    check(&mut p, "typeof URL", "function");
    check(&mut p, "typeof URLSearchParams", "function");
    check(&mut p, "typeof URL.createObjectURL", "undefined");
    check(&mut p, "typeof URL.revokeObjectURL", "undefined");
    check(&mut p, "typeof URLPattern", "undefined");

    eprintln!("case: JS-5 URL getters / toString / toJSON / failures");
    check(
        &mut p,
        "var u = new URL('https://user:pw@example.com:8080/a/b?x=1#h'); \
         [u.href, u.origin, u.protocol, u.username, u.password, u.host, u.hostname, u.port, \
          u.pathname, u.search, u.hash].join('|')",
        "https://user:pw@example.com:8080/a/b?x=1#h|https://example.com:8080|https:|user|pw|example.com:8080|example.com|8080|/a/b|?x=1|#h",
    );
    check(
        &mut p,
        "new URL('/p?a=1', 'https://example.com/x').href",
        "https://example.com/p?a=1",
    );
    check(
        &mut p,
        "var v = new URL('https://a/p'); String(v) + '|' + JSON.stringify({ u: v })",
        "https://a/p|{\"u\":\"https://a/p\"}",
    );
    check(&mut p, "new URL(v).href", "https://a/p");
    check(
        &mut p,
        "try { new URL('bad'); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "try { URL('https://a/'); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "[URL.canParse('https://a/'), URL.canParse('bad'), URL.canParse('/x', 'https://a/'), \
         URL.canParse('/x', 'bad')].join()",
        "true,false,true,false",
    );
    check(
        &mut p,
        "try { new URL('https://a/' + new Array(70000).join('x')); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );

    eprintln!("case: JS-5 URLSearchParams encoding / decoding");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var q = new URLSearchParams(); q.set('q', 'a b&c'); q.toString()",
        "q=a+b%26c",
    );
    check(
        &mut p,
        "q.append('k', '+%'); q.toString()",
        "q=a+b%26c&k=%2B%25",
    );
    check(
        &mut p,
        "var d = new URLSearchParams('a=b+c&d=%2B&e=%zz&f=%E3%81%82&g'); \
         [d.get('a'), d.get('d'), d.get('e'), d.get('f'), d.get('g'), d.get('none')].join('|')",
        "b c|+|%zz|あ||",
    );
    check(&mut p, "d.get('none') === null", "true");
    check(
        &mut p,
        "new URLSearchParams({ k: '*-._', t: '~!' }).toString()",
        "k=*-._&t=%7E%21",
    );
    check(
        &mut p,
        "new URLSearchParams([['n', '\\u3042\\ud83d\\ude00']]).toString()",
        "n=%E3%81%82%F0%9F%98%80",
    );
    check(
        &mut p,
        "new URLSearchParams([['n', '\\ud800']]).toString()",
        "n=%EF%BF%BD",
    );
    check(
        &mut p,
        "new URLSearchParams('x=%ff%41').get('x').charCodeAt(0)",
        "65533",
    );

    eprintln!("case: JS-5 URLSearchParams init forms and methods");
    check(
        &mut p,
        "new URLSearchParams([['a', '1'], ['a', '2']]).getAll('a').join(',')",
        "1,2",
    );
    check(&mut p, "new URLSearchParams({ x: 1 }).toString()", "x=1");
    check(
        &mut p,
        "new URLSearchParams('?a=1').get('a') + new URLSearchParams(new URLSearchParams('b=2')).get('b')",
        "12",
    );
    check(
        &mut p,
        "try { new URLSearchParams([['a']]); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "var m = new URLSearchParams('a=1&b=2&a=3'); \
         m.has('a') + '|' + m.has('z') + '|' + m.getAll('a').join(',')",
        "true|false|1,3",
    );
    check(&mut p, "m.set('a', '9'); m.toString()", "a=9&b=2");
    check(&mut p, "m.delete('a'); m.toString()", "b=2");
    check(&mut p, "m.set('c', '3'); m.toString()", "b=2&c=3");
    check(
        &mut p,
        "var acc = []; for (var kv of m) { acc.push(kv[0] + kv[1]); } acc.join()",
        "b2,c3",
    );
    check(
        &mut p,
        "var ks = []; var it = m.keys(); var r; while (!(r = it.next()).done) { ks.push(r.value); } \
         ks.join() + '|' + Array.from(m.values()).join() + '|' + Array.from(m.entries()).join(';')",
        "b,c|2,3|b,2;c,3",
    );
    check(
        &mut p,
        "var fe = []; m.forEach(function (v, k, o) { fe.push(k + v + (o === m)); }); fe.join()",
        "b2true,c3true",
    );

    eprintln!("case: JS-5 URL searchParams and search stay in sync");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var u = new URL('https://a/p?x=1#h'); u.searchParams.append('y', 'a b'); u.href",
        "https://a/p?x=1&y=a+b#h",
    );
    check(
        &mut p,
        "u.searchParams.delete('x'); u.searchParams.delete('y'); u.href",
        "https://a/p#h",
    );
    check(
        &mut p,
        "u.search = '?k=v'; u.searchParams.get('k') + '|' + u.href",
        "v|https://a/p?k=v#h",
    );
    check(&mut p, "u.searchParams === u.searchParams", "true");
    check(&mut p, "u.searchParams.set('k', 'w'); u.search", "?k=w");

    eprintln!("case: JS-5 URL href / search / hash setters");
    check(
        &mut p,
        "u.hash = 'z'; u.hash + '|' + u.href",
        "#z|https://a/p?k=w#z",
    );
    check(&mut p, "u.hash = ''; u.href", "https://a/p?k=w");
    check(&mut p, "u.search = 'a#b'; u.search", "?a%23b");
    check(&mut p, "u.search = ''; u.href", "https://a/p");
    check(
        &mut p,
        "u.href = 'https://b/?q=1'; u.host + '|' + u.searchParams.get('q')",
        "b|1",
    );
    check(
        &mut p,
        "try { u.href = 'bad'; 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "var w = new URL('https://a/p?x?y#f?g'); w.search + '|' + w.hash + '|' + w.pathname",
        "?x?y|#f?g|/p",
    );
    check(
        &mut p,
        "w.hash = 'n'; w.href + '|' + (new URL('http://a/?')).search + '|' + (new URL('http://a/#')).hash",
        "https://a/p?x?y#n||",
    );
    check(
        &mut p,
        "var e1 = new URL('http://a/?'); e1.search = 'z=1'; e1.href",
        "http://a/?z=1",
    );

    eprintln!(
        "case: JS-5 review fixes (empty-vs-delimiter, trailing space, null, USVString, arity, sync failure)"
    );
    check(
        &mut p,
        "var r1 = new URL('https://a/p'); r1.search = '?'; r1.hash = '#'; r1.href",
        "https://a/p?#",
    );
    check(
        &mut p,
        "var r2 = new URL('https://a/p'); r2.hash = 'a '; r2.search = 'b '; r2.href",
        "https://a/p?b%20#a%20",
    );
    check(&mut p, "new URLSearchParams(null).toString()", "");
    check(
        &mut p,
        "var r3 = new URLSearchParams(); r3.append('\\ud800', 'x'); r3.get('\\ufffd') + '|' + r3.has('\\ud800')",
        "x|true",
    );
    check(
        &mut p,
        "try { new URLSearchParams().append('x'); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    // JS-6: 終端のない iterable の pair は 3 個目で拒否し、確保を続けない。
    check(
        &mut p,
        "var cnt = 0; var inf = { [Symbol.iterator]() { return { next() { cnt++; if (cnt > 1000) { throw new Error('runaway'); } return { done: false, value: 'x' }; } }; } }; \
         try { new URLSearchParams([inf]); 'no' } catch (e) { (e instanceof TypeError) + '|' + cnt }",
        "true|3",
    );
    // JS-6: 外側の iterable が件数上限を越えたら TypeError。
    check(
        &mut p,
        "var oc = 0; var outer = { [Symbol.iterator]() { return { next() { oc++; return { done: oc > 70000, value: ['a', 'b'] }; } }; } }; \
         try { new URLSearchParams(outer); 'no' } catch (e) { (e instanceof TypeError) + '|' + (oc < 70000) }",
        "true|true",
    );
    // JS-6: 文字列・append の文字数上限。
    check(
        &mut p,
        "var big = 'a'.repeat(1048577); var q = new URLSearchParams(); \
         var r1 = 0; try { new URLSearchParams(big) } catch (e) { r1 = e instanceof TypeError } \
         var r2 = 0; try { q.append('k', big) } catch (e) { r2 = e instanceof TypeError } \
         r1 + '|' + r2 + '|' + q.toString()",
        "true|true|",
    );
    // append の引数変換中に一覧が変わっても、変換後の最新の一覧に追加する。
    check(
        &mut p,
        "var pa = new URLSearchParams(); pa.append('a', { toString() { pa.append('b', '2'); return '1' } }); pa.toString()",
        "b=2&a=1",
    );
    check(
        &mut p,
        "try { new URLSearchParams().get(); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    check(
        &mut p,
        "var r4 = new URL('https://a/'); var big = new Array(70001).join('a'); \
         var thrown = false; try { r4.searchParams.set('k', big); } catch (e) { thrown = e instanceof TypeError; } \
         thrown + '|' + r4.searchParams.has('k') + '|' + r4.href",
        "true|false|https://a/",
    );

    // JS-6: 文字列からの解析も確保前に件数上限を検査する。
    check(
        &mut p,
        "var r5 = 0; try { new URLSearchParams('a&'.repeat(100000)) } catch (e) { r5 = e instanceof TypeError } \
         r5 + '|' + new URLSearchParams('a&'.repeat(1000)).toString().length",
        "true|2999",
    );
    // JS-6: Symbol は TypeError、canParse は引数省略で TypeError、例外時は iterator.return を呼ぶ。
    check(
        &mut p,
        "var t1 = 'x'; try { new URLSearchParams([[Symbol('s'), 'v']]) } catch (e) { t1 = e instanceof TypeError } \
         var t2 = 'x'; try { URL.canParse() } catch (e) { t2 = e instanceof TypeError } \
         var closed = 0; var it = { [Symbol.iterator]() { return { next() { return { done: false, value: 1 } }, return() { closed++; return {} } } } }; \
         try { new URLSearchParams(it) } catch (e) {} \
         t1 + '|' + t2 + '|' + closed",
        "true|true|1",
    );
    // JS-6: set は置換で消える項目を差し引いた最終状態で予算を検査する。
    check(
        &mut p,
        "var q6 = new URLSearchParams(); q6.set('k', 'a'.repeat(600000)); q6.set('k', 'b'.repeat(600000)); \
         var q7 = new URLSearchParams('k=1&'.repeat(3000)); q7.set('k', '2'); \
         q6.get('k').length + '|' + q6.getAll('k').length + '|' + q7.toString()",
        "600000|1|k=2",
    );

    eprintln!("case: REPAIR-3 unimplemented setters throw in strict mode and do not change state");
    check(
        &mut p,
        "(function () { 'use strict'; try { w.port = '1'; return 'no'; } catch (e) { return e instanceof TypeError; } })()",
        "true",
    );
    check(&mut p, "w.port === ''", "true");
}

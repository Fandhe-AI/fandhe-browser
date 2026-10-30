//! V8 / boa 共通の JS shim テストケース（TASK-108・Issue #778・ビヘイビア `JS-5` / `JS-6`）。
//!
//! `tests/js_shim_v8.rs` と `tests/js_shim_boa.rs` が `#[path]` で取り込み、同一の shim・
//! 同一のスニペットが両エンジンで同じ結果になることを確認する。各ケースは
//! 「パース → `DomBridge::attach` → `JsRuntime::install_dom_shim` → 評価 → DOM の
//! シリアライズ結果の比較」の順で、期待値は具体値で書く。

use fandhe_browser_core::js_stub::{JsRuntime, execute_js_stub};
use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{
    Config, DocumentReadyState, DomBridge, DomBridgeLimits, DomLimits, Error, ExceptionKind,
    LocationChangeKind, NodeId, ParseOptions, parse_document, query_selector,
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
    setup_html(engine, FIXTURE, limit_extra_nodes)
}

/// 任意の HTML（`<script>` を 1 つ含むこと）で始める setup。パーサー由来の属性を検証する。
fn setup_html(engine: &str, html: &str, limit_extra_nodes: Option<usize>) -> Page {
    let mut doc = parse_document(html, &ParseOptions::default())
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
            "timers.js",
            "abort.js",
            "events.js",
            "net.js",
            "observers.js",
            "style.js",
            "stylesheet.js",
            "url.js",
            "storage.js",
            "history.js",
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
    eprintln!("case: CDP-2 initial order identity (__fandheDomIdentity. #1346)");
    let mut p = setup(engine, None);
    check(&mut p, "typeof __fandheDomIdentity", "object");
    check(&mut p, "Object.isFrozen(__fandheDomIdentity)", "true");
    check(&mut p, "__fandheDomIdentity.orderOf(document)", "1");
    check(
        &mut p,
        "var el = document.querySelector('#a'); __fandheDomIdentity.nodeAt(__fandheDomIdentity.orderOf(el)) === el",
        "true",
    );
    // 移動後も同一性が保たれる。
    check(
        &mut p,
        "var n = __fandheDomIdentity.orderOf(el); document.body.appendChild(el); document.head.appendChild(el); __fandheDomIdentity.nodeAt(n) === el && __fandheDomIdentity.orderOf(el) === n",
        "true",
    );
    check(
        &mut p,
        "try { __fandheDomIdentity.orderOf(document.createElement('div')); 'no' } catch (e) { String(e.message).indexOf('created after attach') >= 0 }",
        "true",
    );
    for bad in ["0", "-1", "1.5", "99999"] {
        check(
            &mut p,
            &format!("try {{ __fandheDomIdentity.nodeAt({bad}); 'no' }} catch (e) {{ 'threw' }}"),
            "threw",
        );
    }
    check(
        &mut p,
        "try { __fandheDomIdentity.orderOf({}); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );

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

    eprintln!("case: JS-5 document.activeElement defaults to body (#1173・#1185)");
    let mut p = setup(engine, None);
    check(&mut p, "typeof document.activeElement", "object");
    check(&mut p, "document.activeElement === document.body", "true");
    check(&mut p, "document.activeElement instanceof Element", "true");
    check(
        &mut p,
        "try { document.activeElement = null; } catch (e) {} document.activeElement === document.body",
        "true",
    );

    eprintln!("case: JS-5 document.defaultView is window (#1173)");
    let mut p = setup(engine, None);
    check(&mut p, "document.defaultView === window", "true");
    check(&mut p, "typeof document.defaultView.history", "object");

    eprintln!(
        "case: JS-5 name and type reflect to attributes so react-dom 19 inputs keep them (#1176)"
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var i = document.createElement('input'); i.type = 'text'; i.name = 'user'; \
         i.getAttribute('type') + '|' + i.getAttribute('name') + '|' + i.name + '|' + i.type",
        "text|user|user|text",
    );
    check(
        &mut p,
        "document.createElement('input').name === '' && document.createElement('input').type === 'text'",
        "true",
    );
    check(
        &mut p,
        "var j = document.createElement('input'); j.setAttribute('name', 'q'); \
         document.body.appendChild(j); document.querySelectorAll('input[name]').length",
        "1",
    );

    eprintln!("case: JS-5 type defaults and enum normalization per element kind (#1186)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var ti = document.createElement('input'); ti.type + '|' + ti.hasAttribute('type')",
        "text|false",
    );
    check(
        &mut p,
        "ti.setAttribute('type', 'CHECKBOX'); ti.type + '|' + ti.getAttribute('type')",
        "checkbox|CHECKBOX",
    );
    check(&mut p, "ti.setAttribute('type', 'foo'); ti.type", "text");
    check(
        &mut p,
        "var tb = document.createElement('button'); var r1 = tb.type; tb.type = 'RESET'; \
         r1 + '|' + tb.type + '|' + tb.getAttribute('type')",
        "submit|reset|RESET",
    );
    check(&mut p, "tb.setAttribute('type', 'x'); tb.type", "submit");
    check(&mut p, "document.createElement('div').type", "");
    check(
        &mut p,
        "document.body.innerHTML = '<svg><a id=\"sa\"></a></svg>'; document.getElementById('sa').type",
        "",
    );

    eprintln!("case: JS-5 id and className reflect to attributes (#1186)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var e = document.createElement('div'); var r0 = e.id + '|' + e.className; \
         e.id = 'x'; e.className = 'a b'; document.body.appendChild(e); \
         r0 + '|' + e.getAttribute('id') + '|' + e.getAttribute('class') + '|' + \
         (document.getElementById('x') === e) + '|' + document.querySelectorAll('.a').length",
        "||x|a b|true|1",
    );

    eprintln!("case: JS-5 value reflects on form controls and falls back on others (#1186)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var vi = document.createElement('input'); var v0 = vi.value; vi.value = 'v'; \
         var v1 = vi.getAttribute('value'); vi.value = null; \
         v0 + '|' + v1 + '|' + vi.getAttribute('value') + '|' + vi.value",
        "|v||",
    );
    check(
        &mut p,
        "var op1 = document.createElement('option'); op1.textContent = 'T'; var o0 = op1.value; \
         op1.value = 'ov'; o0 + '|' + op1.value + '|' + op1.getAttribute('value')",
        "T|ov|ov",
    );
    check(
        &mut p,
        "var dv = document.createElement('div'); var d0 = String(dv.value); dv.value = 'z'; \
         d0 + '|' + dv.value + '|' + dv.hasAttribute('value')",
        "undefined|z|false",
    );
    check(
        &mut p,
        "var dn = document.createElement('div'); dn.value = Object.create(null); \
         var calls = 0; dn.value = { toString: function () { calls++; return 'x'; } }; \
         (typeof dn.value) + '|' + calls + '|' + dn.hasAttribute('value')",
        "object|0|false",
    );
    check(
        &mut p,
        "var ob = document.createElement('option'); ob.value = null; \
         var bt = document.createElement('button'); bt.value = null; \
         ob.value + '|' + bt.value + '|' + ob.getAttribute('value')",
        "null|null|null",
    );
    check(
        &mut p,
        "var de = document.createElement('div'); de.value = 'x'; \
         var h1 = de.hasOwnProperty('value'); var k = Object.keys(de).indexOf('value') >= 0; \
         var ok = delete de.value; \
         h1 + '|' + k + '|' + ok + '|' + de.hasOwnProperty('value') + '|' + String(de.value)",
        "true|true|true|false|undefined",
    );
    check(
        &mut p,
        "var op2 = document.createElement('option'); op2.innerHTML = '\\n   a \\t  b\\n  '; op2.value",
        "a b",
    );
    check(
        &mut p,
        "var op3 = document.createElement('option'); op3.innerHTML = 'A<script>B</script>C'; op3.value",
        "AC",
    );

    eprintln!("case: JS-5 boolean attributes reflect presence (#1186)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var bi = document.createElement('input'); document.body.appendChild(bi); \
         bi.disabled = true; bi.checked = true; bi.readOnly = true; bi.required = true; bi.multiple = true; \
         bi.placeholder = 'p'; \
         [bi.disabled, bi.hasAttribute('disabled'), bi.hasAttribute('readonly'), bi.getAttribute('placeholder'), \
          document.querySelectorAll('[disabled]').length, document.querySelectorAll('[checked]').length, \
          bi.required, bi.multiple].join(',')",
        "true,true,true,p,1,1,true,true",
    );
    check(
        &mut p,
        "bi.disabled = false; bi.checked = 0; bi.readOnly = ''; \
         [bi.disabled, bi.hasAttribute('disabled'), bi.checked, bi.hasAttribute('readonly')].join(',')",
        "false,false,false,false",
    );
    check(
        &mut p,
        "var bd = document.createElement('div'); var c0 = String(bd.checked); bd.checked = true; \
         c0 + '|' + bd.checked + '|' + bd.hasAttribute('checked')",
        "undefined|true|false",
    );

    eprintln!(
        "case: JS-5 React value tracking pattern works on Element.prototype accessors (#1186)"
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var ri = document.createElement('input'); \
         var rd = Object.getOwnPropertyDescriptor(ri.constructor.prototype, 'value'); \
         Object.defineProperty(ri, 'value', { configurable: true, get: function () { return rd.get.call(this); }, \
           set: function (v) { rd.set.call(this, v); } }); \
         ri.value = 'tracked'; (typeof rd.get) + '|' + (typeof rd.set) + '|' + ri.value + '|' + ri.getAttribute('value')",
        "function|function|tracked|tracked",
    );

    eprintln!("case: JS-5 reflection setters notify MutationObserver only on real change (#1186)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var me = document.createElement('input'); document.body.appendChild(me); var seen = []; \
         var mo = new MutationObserver(function (r) { \
           for (var i = 0; i < r.length; i++) seen.push(r[i].attributeName); }); \
         mo.observe(me, { attributes: true }); me.id = 'i'; me.className = 'c'; me.disabled = true; \
         me.checked = false; __fandheRunMicrotasks(); seen.join(',')",
        "id,class,disabled",
    );

    eprintln!("case: JS-5 reflection ignores page-replaced setAttribute and toLowerCase (#1186)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var ei = document.createElement('input'); \
         Element.prototype.setAttribute = function () { throw new Error('hijacked'); }; \
         String.prototype.toLowerCase = function () { return 'hijacked'; }; \
         ei.id = 'h'; ei.disabled = true; ei.setAttribute === undefined; \
         var ok = ei.id + '|' + ei.disabled; \
         ok",
        "h|true",
    );
    check(
        &mut p,
        "var ti2 = document.createElement('input'); ti2.type = 'RADIO'; ti2.type",
        "radio",
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "String.prototype.charCodeAt = function () { return 0; }; \
         String.prototype.charAt = function () { return 'X'; }; \
         String.fromCharCode = function () { return 'Y'; }; \
         var ti3 = document.createElement('input'); ti3.setAttribute('type', 'CHECKBOX'); ti3.type",
        "checkbox",
    );

    eprintln!("case: JS-5 HTMLIFrameElement exists so instanceof does not throw (#1168)");
    let mut p = setup(engine, None);
    check(&mut p, "typeof HTMLIFrameElement", "function");
    check(
        &mut p,
        "document.body instanceof HTMLIFrameElement",
        "false",
    );
    check(&mut p, "typeof HTMLElement", "function");
    check(&mut p, "document.body instanceof HTMLElement", "false");
    check(
        &mut p,
        "Object.getPrototypeOf(HTMLIFrameElement.prototype) === HTMLElement.prototype",
        "true",
    );
    check(
        &mut p,
        "try { new HTMLElement(); 'no' } catch (e) { e.name }",
        "TypeError",
    );
    check(&mut p, "document.body instanceof Element", "true");
    check(
        &mut p,
        "var s1 = document.createElement('style'); s1.setAttribute('data-emotion', 'css'); \
         var s2 = document.createElement('style'); s2.setAttribute('data-emotion', 'css'); \
         s2.setAttribute('data-s', ''); \
         document.head.appendChild(s1); document.head.appendChild(s2); \
         document.querySelectorAll('style[data-emotion]:not([data-s])').length",
        "1",
    );
    check(
        &mut p,
        "try { new HTMLIFrameElement(); 'no' } catch (e) { e.name }",
        "TypeError",
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
    node_cases(engine);
    abort_cases(engine);
    storage_cases(engine);
    history_cases(engine);
    event_target_cases(engine);
    history_traversal_cases(engine);
    focus_cases(engine);
    create_element_ns_cases(engine);
    get_elements_by_tag_name_cases(engine);
    parent_node_cases(engine);
    child_traversal_cases(engine);
    attributes_cases(engine);
    is_equal_node_cases(engine);
    element_name_cases(engine);
    document_element_cases(engine);
    class_list_cases(engine);
    style_sheet_cases(engine);

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

    eprintln!("case: JS-5 matchMedia is a fixed-value stub (#1227・SEC-2)");
    check(&mut p, "typeof window.matchMedia", "function");
    check(
        &mut p,
        "matchMedia('(prefers-color-scheme: dark)').matches",
        "false",
    );
    // 環境で評価していない証拠: 実ブラウザで常に真になるクエリも false。
    check(&mut p, "matchMedia('all').matches", "false");
    check(&mut p, "matchMedia('(min-width: 1px)').matches", "false");
    check(
        &mut p,
        "matchMedia('(max-width: 600px)').media",
        "(max-width: 600px)",
    );
    check(&mut p, "matchMedia(1).media", "1");
    check(
        &mut p,
        "var called = 0; var mq = matchMedia('x'); var fn1 = function () { called++; }; \
         var r = [mq.addListener(fn1), mq.addEventListener('change', fn1), mq.removeListener(fn1)]; \
         r.map(String).join(',') + '|' + called + '|' + mq.onchange",
        "undefined,undefined,undefined|0|null",
    );
    check(
        &mut p,
        "try { matchMedia(); 'ok' } catch (e) { 'threw' }",
        "threw",
    );
    check(
        &mut p,
        "try { new MediaQueryList(); 'ok' } catch (e) { 'threw' }",
        "threw",
    );
    check(
        &mut p,
        "typeof window.innerWidth + '|' + typeof screen",
        "undefined|undefined",
    );

    eprintln!("case: JS-5 IntersectionObserver is inert and performance.now is monotonic (#1227)");
    check(
        &mut p,
        "var io = 0; var o = new IntersectionObserver(function () { io++; }); \
         o.observe(document.body); o.disconnect(); \
         String(o.root) + '|' + o.rootMargin + '|' + o.thresholds.join(',') + '|' + o.takeRecords().length + '|' + io",
        "null|0px 0px 0px 0px|0|0|0",
    );
    check(
        &mut p,
        "try { new IntersectionObserver(1); 'ok' } catch (e) { 'threw' }",
        "threw",
    );
    check(
        &mut p,
        "var t0 = performance.now(); typeof t0 + '|' + (t0 >= 0) + '|' + (performance.now() >= t0)",
        "number|true|true",
    );
    // 壁時計（Date.now）を差し替えても performance.now は影響を受けない。
    check(
        &mut p,
        "var t1 = performance.now(); Date.now = function () { return -1e12; }; var t2 = performance.now(); String(t2 >= t1 && t2 >= 0)",
        "true",
    );

    eprintln!("case: JS-5 location defaults to about:blank");
    check(&mut p, "document.URL", "about:blank");
    check(&mut p, "document.location === location", "true");
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
    eprintln!("case: JS-5 document.location / document.URL (#1158)");
    check(&mut p, "document.location.port", "8443");
    check(&mut p, "document.location.protocol", "https:");
    check(&mut p, "document.URL", URL);
    check(&mut p, "String(document.location)", URL);
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
    // document.location / document.URL の代入も無視される。
    check(
        &mut p,
        "try { document.location = 'https://evil.test/'; 'ok' } catch (e) { 'threw' }",
        "ok",
    );
    check(
        &mut p,
        "try { document.URL = 'x'; document.URL } catch (e) { 'threw' }",
        URL,
    );
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
            (LocationChangeKind::AssignLocation, 18),
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

    eprintln!("case: JS-5 console never keeps an Error stack without opt-in (#1183)");
    check(
        &mut p,
        "console.error(new TypeError('x')); \
         console.error({ get stack() { throw new Error('boom'); } }); 'ok'",
        "ok",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.console_messages.len(), 2);
    assert_eq!(diag.console_messages[0].text, "TypeError: x");
    assert_eq!(diag.console_messages[0].raw_stack(), None);
    assert_eq!(diag.console_messages[1].raw_stack(), None);

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
    assert_eq!(diag.timer_errors[0].summary.kind(), ExceptionKind::Error);

    eprintln!("case: JS-7 throwing error.name getter keeps reporting (#1156)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var m1 = new MutationObserver(function () { \
           var e = { message: 'x is not defined' }; \
           Object.defineProperty(e, 'name', { get: function () { throw 1; } }); throw e; }); \
         m1.observe(document.body, { childList: true }); \
         document.body.appendChild(document.createElement('i')); __fandheRunMicrotasks(); 1",
        "1",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.timer_errors.len(), 1);
    // name が読めなくても本文から ReferenceError と推定される。
    assert_eq!(
        diag.timer_errors[0].summary.kind(),
        ExceptionKind::ReferenceError
    );

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

    eprintln!("case: JS-5 style reads the original style attribute of innerHTML-created elements");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "document.body.innerHTML = '<p id=\"sp\" style=\"margin: 0\">x</p>'; \
         var sp = document.getElementById('sp'); sp.style.margin + '|' + sp.style.length",
        "0|1",
    );
    check(
        &mut p,
        "sp.style.color = 'red'; sp.style.cssText",
        "margin: 0; color: red;",
    );
    assert!(
        p.html()
            .contains("<p id=\"sp\" style=\"margin: 0; color: red;\">x</p>"),
        "html: {}",
        p.html()
    );

    eprintln!("case: JS-5 style keeps declarations of a parser-created style attribute");
    let mut p = setup_html(
        engine,
        "<html><head></head><body><div id=\"h\" style=\"color: red\"></div>\
         <div id=\"n\"></div><script>x</script></body></html>",
        None,
    );
    check(
        &mut p,
        "var h = document.getElementById('h'); h.style.color",
        "red",
    );
    check(
        &mut p,
        "h.style.display = 'none'; h.getAttribute('style')",
        "color: red; display: none;",
    );
    assert!(
        p.html().contains("style=\"color: red; display: none;\""),
        "html: {}",
        p.html()
    );
    check(
        &mut p,
        "var nn = document.getElementById('n'); nn.style.length",
        "0",
    );
    check(
        &mut p,
        "nn.style.top = '1px'; nn.getAttribute('style')",
        "top: 1px;",
    );

    eprintln!("case: JS-5 element getAttribute / hasAttribute");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var e = document.getElementById('a'); e.getAttribute('id') + '|' + e.getAttribute('ID')",
        "a|a",
    );
    check(&mut p, "String(e.getAttribute('nope') === null)", "true");
    check(
        &mut p,
        "e.hasAttribute('id') + '|' + e.hasAttribute('nope')",
        "true|false",
    );
    check(
        &mut p,
        "e.setAttribute('data-x', '1'); e.getAttribute('data-x')",
        "1",
    );
    check(
        &mut p,
        "try { String(e.getAttribute('a b')) + '|' + e.hasAttribute('a b') } catch (err) { 'threw' }",
        "null|false",
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
    // JS-6: 外側が配列で件数上限を越えるなら、反復の前に TypeError で拒否する。
    check(
        &mut p,
        "var pr = ['a', 'b']; var big = new Array(70000).fill(pr); try { new URLSearchParams(big); 'no' } catch (e) { e instanceof TypeError }",
        "true",
    );
    // JS-5: delete / has の第 2 引数は値指定として扱う。
    check(
        &mut p,
        "var dv = new URLSearchParams('a=1&a=2&b=3'); dv.delete('a', '1'); var hv = new URLSearchParams('a=1&a=2'); dv.toString() + '|' + hv.has('a', '2') + '|' + hv.has('a', '3') + '|' + hv.has('a', undefined)",
        "a=2&b=3|true|false|true",
    );
    // JS-5: Symbol.iterator が関数でない値は record 扱いにせず TypeError にする。
    check(
        &mut p,
        "var rec = new URLSearchParams({ a: 2 }); var ri; try { new URLSearchParams({ [Symbol.iterator]: 1, a: 2 }); ri = 'no' } catch (e) { ri = e instanceof TypeError } rec.toString() + '|' + ri",
        "a=2|true",
    );
    // JS-5: 通常呼び出しは既存インスタンスを再初期化せず TypeError にする。
    check(
        &mut p,
        "var cu = new URL('https://a/x'); var cs = new URLSearchParams('a=1'); var e1 = 0, e2 = 0; try { URL.call(cu, 'https://b/') } catch (e) { e1 = e instanceof TypeError } try { URLSearchParams.call(cs, 'b=2') } catch (e) { e2 = e instanceof TypeError } e1 + '|' + e2 + '|' + cu.href + '|' + cs.toString()",
        "true|true|https://a/x|a=1",
    );
    // JS-5: iterator.next() の戻りがオブジェクトでなければ TypeError にする。
    check(
        &mut p,
        "var bad = { [Symbol.iterator]() { return { next() { return 1 } } } }; try { new URLSearchParams(bad); 'no' } catch (e) { e instanceof TypeError }",
        "true",
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
        "var r5 = 0; try { new URLSearchParams('a&'.repeat(65537)) } catch (e) { r5 = e instanceof TypeError } \
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

    // JS-6: search・hash setter は上限超過（入力長・符号化後の長さ）を符号化の前後で拒否し、状態を変えない。
    check(
        &mut p,
        "var lu = new URL('https://a/p?k=v#h'); \
         lu.search = 'a'.repeat(65537); var r1 = lu.href; \
         lu.search = ' '.repeat(30000); var r2 = lu.href; \
         lu.hash = 'x'.repeat(65537); var r3 = lu.href; \
         lu.hash = ' '.repeat(30000); var r4 = lu.href; \
         lu.search = 'z'; \
         [r1, r2, r3, r4, lu.href, lu.searchParams.get('z')].join('|')",
        "https://a/p?k=v#h|https://a/p?k=v#h|https://a/p?k=v#h|https://a/p?k=v#h|https://a/p?z#h|",
    );
    // JS-6: 検索系メソッドの名前が予算を超えるなら変換の前に TypeError で拒否する。
    check(
        &mut p,
        "var lk = 'x'; try { new URLSearchParams('a=1').get('x'.repeat(1048577)) } catch (e) { lk = e instanceof TypeError } \
         lk + '|' + new URLSearchParams('a=1').get('a')",
        "true|1",
    );
    // JS-5: record 初期化では、正規化後に同名になるキーを後の値で上書きし、最初の位置を保つ。
    check(
        &mut p,
        "new URLSearchParams({'\\ud800': 'a', x: '1', '\\ud801': 'b'}).toString()",
        "%EF%BF%BD=b&x=1",
    );

    // JS-5: record 初期化の列挙可能な Symbol キーは TypeError、列挙不可の Symbol キーは無視する。
    check(
        &mut p,
        "var sy = 'x'; try { new URLSearchParams({ [Symbol('key')]: 'v' }) } catch (e) { sy = e instanceof TypeError } \
         var hid = {}; Object.defineProperty(hid, Symbol('h'), { value: 1, enumerable: false }); hid.a = 'b'; \
         sy + '|' + new URLSearchParams(hid).toString()",
        "true|a=b",
    );

    eprintln!("case: REPAIR-3 unimplemented setters throw in strict mode and do not change state");
    check(
        &mut p,
        "(function () { 'use strict'; try { w.port = '1'; return 'no'; } catch (e) { return e instanceof TypeError; } })()",
        "true",
    );
    check(&mut p, "w.port === ''", "true");
}

/// Node の基本プロパティ（`nodeType`・定数・`nodeName`・`ownerDocument`。#1151・`JS-5`）。
/// 要素の `tagName` / `parentNode` は未実装（REPAIR-3）で、未定義のままであることも固定する。
fn node_cases(engine: &str) {
    eprintln!("case: JS-5 Node basic properties");
    let mut p = setup(engine, None);
    check(&mut p, "String(document.getElementById('a').nodeType)", "1");
    check(&mut p, "String(document.nodeType)", "9");
    check(&mut p, "String(document.createTextNode('t').nodeType)", "3");
    check(
        &mut p,
        "String(document.createElement('div').nodeType)",
        "1",
    );
    check(
        &mut p,
        "[Node.ELEMENT_NODE, Node.TEXT_NODE, Node.DOCUMENT_NODE, Node.DOCUMENT_FRAGMENT_NODE, Node.COMMENT_NODE].join(',')",
        "1,3,9,11,8",
    );
    check(&mut p, "String(Node.prototype.DOCUMENT_NODE)", "9");
    check(&mut p, "String(document.body.ELEMENT_NODE)", "1");
    check(
        &mut p,
        "Node.ELEMENT_NODE = 5; String(Node.ELEMENT_NODE)",
        "1",
    );
    check(&mut p, "document.nodeName", "#document");
    check(&mut p, "document.createTextNode('t').nodeName", "#text");
    check(
        &mut p,
        "String(document.getElementById('a').ownerDocument === document)",
        "true",
    );
    check(
        &mut p,
        "String(document.createElement('p').ownerDocument === document)",
        "true",
    );
    check(&mut p, "String(document.ownerDocument)", "null");
    check(
        &mut p,
        "String('localName' in document.body) + '|' + String('childNodes' in document.body)",
        "true|true",
    );
    check(
        &mut p,
        "try { Object.getOwnPropertyDescriptor(Element.prototype, 'nodeType').get.call({}); 'no' } \
         catch (e) { e instanceof TypeError ? 'TypeError' : 'other' }",
        "TypeError",
    );
    check(
        &mut p,
        "var n = document.getElementById('a'); String(!!(n && (n.nodeType === 1 || n.nodeType === 9 || n.nodeType === 11)))",
        "true",
    );
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(&mut p, "String(document.body.nodeType)", "1");

    eprintln!(
        "case: JS-5 element dispatchEvent delivers on the target only, no propagation (#1242)"
    );
    check(
        &mut p,
        "var el = document.getElementById('a'); var hit = 0; var fn = function () { hit++; }; \
         el.addEventListener('click', fn, { capture: true, passive: true }); \
         el.addEventListener('click', fn); el.removeEventListener('click', fn); \
         typeof el.addEventListener + '|' + typeof el.dispatchEvent + '|' + hit",
        "function|function|0",
    );
    check(
        &mut p,
        "var el2 = document.getElementById('a'); var o = []; var up = 0; \
         document.addEventListener('k', function () { up++; }); window.addEventListener('k', function () { up++; }); \
         var l = function (e) { o.push(e.detail + ':' + (e.target === el2) + ':' + (e.currentTarget === el2) + ':' + e.eventPhase); e.preventDefault(); }; \
         el2.addEventListener('k', l); \
         var r1 = el2.dispatchEvent(new CustomEvent('k', { detail: 5, cancelable: true, bubbles: true })); \
         var r2 = el2.dispatchEvent(new CustomEvent('k', { detail: 6 })); \
         el2.removeEventListener('k', l); el2.dispatchEvent(new CustomEvent('k', { detail: 7 })); \
         var bad = 'none'; try { Node.prototype.dispatchEvent.call({}, new Event('k')); } catch (x) { bad = x.name; } \
         o.join(',') + '|' + r1 + '|' + r2 + '|' + up + '|' + bad",
        "5:true:true:2,6:true:true:2|false|true|0|TypeError",
    );
    check(
        &mut p,
        "for (var i = 0; i < 1100; i++) { document.body.addEventListener('e' + i, function () {}); } 'ok'",
        "ok",
    );
}

/// `DOMException` / `AbortSignal` / `AbortController`（#1150・`JS-5`・`JS-6`・`REPAIR-3`・`SEC-2`）のケース。
fn abort_cases(engine: &str) {
    eprintln!("case: JS-5 abort globals exist and EventTarget / Event are generic (#1177)");
    let mut p = setup(engine, None);
    check(&mut p, "typeof AbortController", "function");
    check(&mut p, "typeof AbortSignal", "function");
    check(&mut p, "typeof DOMException", "function");
    check(&mut p, "typeof EventTarget", "function");
    check(&mut p, "typeof Event", "function");

    eprintln!("case: JS-5 AbortController abort() state and reason");
    check(
        &mut p,
        "var c = new AbortController(); \
         var before = [c.signal.aborted, String(c.signal.reason), c.signal === c.signal].join('|'); \
         c.abort(); \
         var r = c.signal.reason; \
         before + '/' + [c.signal.aborted, r instanceof DOMException, r instanceof Error, r.name, r.code, r.message].join('|')",
        "false|undefined|true/true|true|true|AbortError|20|signal is aborted without reason",
    );
    check(
        &mut p,
        "var c2 = new AbortController(); c2.abort('x'); c2.abort('y'); c2.signal.reason",
        "x",
    );

    eprintln!("case: JS-5 abort event via onabort and addEventListener");
    check(
        &mut p,
        "var log = []; var c3 = new AbortController(); var s = c3.signal; \
         s.onabort = function (e) { log.push('on:' + e.type + ':' + (e.target === s) + ':' + (e.currentTarget === s)); }; \
         s.addEventListener('abort', function (e) { log.push('add'); }); \
         var once = function () { log.push('once'); }; \
         s.addEventListener('abort', once, { once: true }); \
         s.addEventListener('abort', once, { once: true }); \
         var removed = function () { log.push('removed'); }; \
         s.addEventListener('abort', removed); s.removeEventListener('abort', removed); \
         s.addEventListener('abort', { handleEvent: function (e) { log.push('obj'); } }); \
         c3.abort(); c3.abort(); log.join(',')",
        "on:abort:true:true,add,once,obj",
    );

    eprintln!("case: JS-5 throwIfAborted / illegal constructor / brand check");
    check(
        &mut p,
        "var c4 = new AbortController(); c4.signal.throwIfAborted(); \
         c4.abort('why'); var got; try { c4.signal.throwIfAborted(); } catch (e) { got = e; } String(got)",
        "why",
    );
    check(
        &mut p,
        "var res = []; \
         try { new AbortSignal(); } catch (e) { res.push(e instanceof TypeError); } \
         try { Object.getOwnPropertyDescriptor(AbortSignal.prototype, 'aborted').get.call({}); } \
         catch (e) { res.push(e instanceof TypeError); } \
         try { AbortController(); } catch (e) { res.push(e instanceof TypeError); } \
         res.join('|')",
        "true|true|true",
    );

    eprintln!("case: JS-5 AbortSignal.abort / any");
    check(
        &mut p,
        "[AbortSignal.abort().aborted, AbortSignal.abort('r').reason, AbortSignal.abort().reason.name].join('|')",
        "true|r|AbortError",
    );
    check(
        &mut p,
        "var a = new AbortController(); var b = new AbortController(); \
         var any = AbortSignal.any([a.signal, b.signal]); var pre = any.aborted; \
         b.abort('z'); \
         var pre2 = AbortSignal.any([a.signal, AbortSignal.abort('q')]); \
         [pre, any.aborted, any.reason, pre2.aborted, pre2.reason].join('|')",
        "false|true|z|true|q",
    );
    check(
        &mut p,
        "var bad; try { AbortSignal.any([{}]); } catch (e) { bad = e instanceof TypeError; } String(bad)",
        "true",
    );

    eprintln!("case: JS-5 AbortSignal.timeout validates its delay");
    check(
        &mut p,
        "var t = AbortSignal.timeout(10); var bads = []; \
         [-1, NaN, Infinity].forEach(function (v) { \
           try { AbortSignal.timeout(v); bads.push('no'); } catch (e) { bads.push(e instanceof TypeError); } \
         }); [t.aborted, bads.join(',')].join('|')",
        "false|true,true,true",
    );

    eprintln!("case: JS-5 DOMException basics");
    check(
        &mut p,
        "var d = new DOMException('m', 'TimeoutError'); var e0 = new DOMException(); \
         [d.code, d.name, d.message, e0.name, e0.code, e0.message === '', DOMException.ABORT_ERR, \
          DOMException.prototype.TIMEOUT_ERR, String(d)].join('|')",
        "23|TimeoutError|m|Error|0|true|20|23|TimeoutError: m",
    );

    eprintln!("case: JS-5 a throwing listener does not stop later listeners or abort() caller");
    check(
        &mut p,
        "var seen = []; var c5 = new AbortController(); \
         c5.signal.addEventListener('abort', function () { throw new Error('boom'); }); \
         c5.signal.addEventListener('abort', function () { seen.push('second'); }); \
         var threw = false; try { c5.abort(); } catch (e) { threw = true; } \
         seen.join(',') + '|' + threw",
        "second|false",
    );

    eprintln!("case: JS-5 explicit undefined reason yields AbortError");
    check(
        &mut p,
        "var cu = new AbortController(); cu.abort(undefined); \
         [cu.signal.reason.name, AbortSignal.abort(undefined).reason.name].join('|')",
        "AbortError|AbortError",
    );

    eprintln!("case: JS-5 listeners added during dispatch are not called in that dispatch");
    check(
        &mut p,
        "var ld = []; var cd = new AbortController(); \
         var late = function () { ld.push('late'); }; \
         cd.signal.onabort = function () { ld.push('on'); cd.signal.addEventListener('abort', late); }; \
         cd.signal.addEventListener('abort', function () { ld.push('first'); \
           cd.signal.addEventListener('abort', function () { ld.push('late2'); }); }); \
         cd.abort(); ld.join(',')",
        "on,first",
    );

    eprintln!("case: JS-5 dependents are settled before page abort algorithms run");
    check(
        &mut p,
        "var fa = new AbortController(); var fb = new AbortController(); \
         __fandheAbortFollow(fa.signal, function () { fb.abort('second'); }); \
         var fs = AbortSignal.any([fa.signal, fb.signal]); \
         fa.abort('first'); \
         [fa.signal.reason, fs.reason, fb.signal.reason].join('|')",
        "first|first|second",
    );

    eprintln!("case: JS-6 abort listener count is capped and flagged");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var c6 = new AbortController(); var cnt = 0; \
         for (var i = 0; i < 1025; i++) { c6.signal.addEventListener('abort', function () { cnt++; }); } \
         c6.abort(); String(cnt)",
        "1024",
    );
    assert!(p.bridge.listener_limit_exceeded().expect("flag"));

    eprintln!(
        "case: JS-6/JS-8 AbortSignal.any releases follow registrations from long-lived inputs"
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var root = new AbortController(); var ok = 0; \
         for (var k = 0; k < 1100; k++) { \
           var req = new AbortController(); \
           AbortSignal.any([root.signal, req.signal]); \
           req.abort(); ok++; \
         } \
         var last = AbortSignal.any([root.signal]); root.abort('r'); \
         [ok, last.aborted, last.reason].join('|')",
        "1100|true|r",
    );
    eprintln!(
        "case: JS-6 AbortSignal.any rolls back partial registrations when a dependent cap is hit"
    );
    check(
        &mut p,
        "var full = new AbortController(); var keep = []; \
         for (var m = 0; m < 1024; m++) { keep.push(AbortSignal.any([full.signal])); } \
         var other = new AbortController(); var thrown; \
         try { AbortSignal.any([other.signal, full.signal]); } catch (e) { thrown = e instanceof TypeError; } \
         var again = AbortSignal.any([other.signal]); other.abort('o'); \
         [thrown, again.aborted].join('|')",
        "true|true",
    );
    eprintln!("case: JS-6 addEventListener reads options before the listener cap check");
    check(
        &mut p,
        "var c7 = new AbortController(); var s7 = c7.signal; var cnt7 = 0; var reentered = false; \
         for (var q = 0; q < 1023; q++) { s7.addEventListener('abort', function () { cnt7++; }); } \
         s7.addEventListener('abort', function () { cnt7++; }, { get once() { \
           if (!reentered) { reentered = true; s7.addEventListener('abort', function () { cnt7++; }); } \
           return false; } }); \
         c7.abort(); String(cnt7)",
        "1024",
    );
    eprintln!("case: JS-5 AbortSignal.any propagates through a very deep chain without recursion");
    check(
        &mut p,
        "var root8 = new AbortController(); var tail = root8.signal; \
         for (var z = 0; z < 3000; z++) { tail = AbortSignal.any([tail]); } \
         var fired8 = 0; tail.addEventListener('abort', function () { fired8++; }); \
         root8.abort('deep'); \
         [tail.aborted, tail.reason, fired8].join('|')",
        "true|deep|1",
    );

    eprintln!("case: JS-5 AbortSignal.any multi-level dispatch follows creation order");
    check(
        &mut p,
        "var r12 = new AbortController(); var log12 = []; \
         var a12 = AbortSignal.any([r12.signal]); var b12 = AbortSignal.any([a12]); \
         var c12 = AbortSignal.any([r12.signal]); \
         r12.signal.addEventListener('abort', function () { log12[log12.length] = 'root'; }); \
         a12.addEventListener('abort', function () { log12[log12.length] = 'a'; }); \
         b12.addEventListener('abort', function () { log12[log12.length] = 'b'; }); \
         c12.addEventListener('abort', function () { log12[log12.length] = 'c'; }); \
         r12.abort('x'); log12.join(',')",
        "root,a,b,c",
    );
    eprintln!("case: JS-5 abort(undefined) and any() follow the default AbortError reason");
    check(
        &mut p,
        "var c13 = new AbortController(); c13.abort(undefined); \
         var c14 = new AbortController(); c14.abort(); \
         var d13 = AbortSignal.abort(undefined); var e13 = AbortSignal.any([d13]); \
         [c13.signal.reason.name, c14.signal.reason.name, d13.reason.name, \
          AbortSignal.abort().reason.name, e13.reason.name].join('|')",
        "AbortError|AbortError|AbortError|AbortError|AbortError",
    );

    eprintln!("case: JS-4 tampered Array.prototype push/pop does not break abort() or any");
    check(
        &mut p,
        "var tp = Array.prototype.push, tq = Array.prototype.pop; \
         Array.prototype.push = function () { throw new Error('tampered'); }; \
         Array.prototype.pop = function () { throw new Error('tampered'); }; \
         var c9 = new AbortController(); var any9 = AbortSignal.any([c9.signal]); \
         var log9 = []; any9.addEventListener('abort', function () { log9[log9.length] = 'any'; }); \
         c9.abort('t'); \
         Array.prototype.push = tp; Array.prototype.pop = tq; \
         [c9.signal.aborted, any9.aborted, any9.reason, log9.join(',')].join('|')",
        "true|true|t|any",
    );

    eprintln!("case: JS-4 AbortSignal.any reads iterator value once");
    check(
        &mut p,
        "var c10 = new AbortController(); var reads10 = 0; \
         var calls10 = 0; \
         var iter10 = {}; iter10[Symbol.iterator] = function () { return { next: function () { \
           calls10++; return calls10 === 1 ? { done: false, get value() { reads10++; return reads10 === 1 ? c10.signal : {}; } } : { done: true }; } }; }; \
         var r10 = AbortSignal.any(iter10); c10.abort('v'); [reads10, r10.aborted, r10.reason].join('|')",
        "1|true|v",
    );

    eprintln!("case: JS-4 AbortSignal.any dependents receive abort in registration order");
    check(
        &mut p,
        "var rootO = new AbortController(); var log11 = []; \
         var sa = AbortSignal.any([rootO.signal]); var sb = AbortSignal.any([rootO.signal]); \
         rootO.signal.addEventListener('abort', function () { log11.push('root'); }); \
         sa.addEventListener('abort', function () { log11.push('a'); }); \
         sb.addEventListener('abort', function () { log11.push('b'); }); \
         rootO.abort('x'); log11.join(',')",
        "root,a,b",
    );

    eprintln!("case: JS-4 AbortSignal.any closes the input iterator when validation fails");
    check(
        &mut p,
        "var closed12 = 0; var iter12 = {}; \
         iter12[Symbol.iterator] = function () { return { next: function () { return { done: false, value: {} }; }, \
           'return': function () { closed12++; return {}; } }; }; \
         var bad12; try { AbortSignal.any(iter12); } catch (e) { bad12 = e instanceof TypeError; } \
         [bad12, closed12].join('|')",
        "true|1",
    );

    eprintln!("case: JS-4 followed algorithm does not receive the internal container as this");
    check(
        &mut p,
        "var c13 = new AbortController(); var seen13 = 'none'; \
         __fandheAbortFollow(c13.signal, function () { \
           seen13 = String(this !== null && this !== undefined && (Object.prototype.hasOwnProperty.call(this, 0) || Object.prototype.hasOwnProperty.call(this, 1))); \
           try { delete this[1]; } catch (e) {} \
         }); \
         var s13 = AbortSignal.any([c13.signal]); c13.abort('z'); \
         [seen13, s13.aborted, s13.reason].join('|')",
        "false|true|z",
    );
}

/// `storage.js`（#1167・`JS-5`・`JS-6`・`SEC-2`）の in-memory Web Storage。
fn storage_cases(engine: &str) {
    eprintln!("case: JS-5 storage globals");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "[typeof Storage, typeof localStorage, typeof sessionStorage, window.localStorage === localStorage, \
         localStorage !== sessionStorage, localStorage instanceof Storage, \
         Object.prototype.toString.call(localStorage)].join('|')",
        "function|object|object|true|true|true|[object Storage]",
    );

    eprintln!("case: JS-5 storage round trip, overwrite, remove, clear");
    check(
        &mut p,
        "localStorage.setItem('a','1'); localStorage.setItem('b','2'); \
         [localStorage.length, localStorage.getItem('a'), localStorage.key(0), localStorage.key(1), \
         localStorage.key(2) === null, localStorage.getItem('zz') === null].join('|')",
        "2|1|a|b|true|true",
    );
    check(
        &mut p,
        "localStorage.setItem('a','9'); \
         [localStorage.length, localStorage.getItem('a'), localStorage.key(0)].join('|')",
        "2|9|a",
    );
    check(
        &mut p,
        "localStorage.removeItem('nope'); localStorage.removeItem('a'); \
         [localStorage.length, localStorage.key(0), localStorage.getItem('a') === null].join('|')",
        "1|b|true",
    );
    check(&mut p, "localStorage.clear(); localStorage.length", "0");

    eprintln!("case: JS-5 storage stringification and special keys");
    check(
        &mut p,
        "localStorage.setItem('n', 1); localStorage.setItem('x', null); localStorage.setItem(1, 'v'); \
         localStorage.setItem('__proto__', 'p'); \
         [typeof localStorage.getItem('n'), localStorage.getItem('x'), localStorage.getItem('1'), \
         localStorage.getItem('__proto__'), localStorage.length].join('|')",
        "string|null|v|p|4",
    );
    check(
        &mut p,
        "var r = []; try { localStorage.setItem(Symbol('s'), 'v'); } catch (e) { r.push(e instanceof TypeError); } \
         try { localStorage.getItem(); } catch (e) { r.push(e instanceof TypeError); } r.join('|')",
        "true|true",
    );
    eprintln!("case: JS-5 storage key() rejects BigInt even if Number is replaced");
    check(
        &mut p,
        "var N = Number; Number = function () { return 0; }; var r = []; \
         try { localStorage.key(0n); r.push('no-throw'); } catch (e) { r.push(e instanceof TypeError); } \
         r.push(localStorage.key('1')); Number = N; r.join('|')",
        "true|x",
    );

    eprintln!("case: JS-5 storage instances are independent");
    check(
        &mut p,
        "localStorage.setItem('k','L'); String(sessionStorage.getItem('k'))",
        "null",
    );

    eprintln!("case: JS-6 storage quota rejects without changing state");
    check(
        &mut p,
        "localStorage.clear(); localStorage.setItem('keep','1'); var big = 'x'.repeat(5 * 1024 * 1024 + 1); \
         var q = []; try { localStorage.setItem('big', big); } catch (e) { \
         q.push(e instanceof DOMException, e.name, e.code); } \
         q.push(localStorage.length, localStorage.getItem('keep'), localStorage.getItem('big') === null); q.join('|')",
        "true|QuotaExceededError|22|1|1|true",
    );
    check(
        &mut p,
        "var n = 0, err = ''; \
         try { for (; n < 70000; n++) { sessionStorage.setItem('k' + n, ''); } } catch (e) { err = e.name; } \
         [n, err, sessionStorage.length].join('|')",
        "65536|QuotaExceededError|65536",
    );

    eprintln!("case: JS-6 storage internal key array is unreachable from Array.prototype setters");
    check(
        &mut p,
        "localStorage.clear(); var leaked = 0; \
         Object.defineProperty(Array.prototype, '0', { configurable: true, set: function () { leaked++; this.length = 0; } }); \
         var r; try { for (var i = 0; i < 5; i++) { localStorage.setItem('p' + i, 'v'); } \
         localStorage.removeItem('p1'); r = leaked + '|' + localStorage.length + '|' + localStorage.key(0) + '|' + localStorage.key(3) + '|' + localStorage.getItem('p4'); } \
         finally { delete Array.prototype[0]; } localStorage.clear(); r",
        "0|4|p0|p4|v",
    );

    eprintln!("case: JS-5 storage brand check and SEC-2 identity surface");
    check(
        &mut p,
        "var b = []; try { Storage.prototype.getItem.call({}, 'a'); } catch (e) { b.push(e instanceof TypeError); } \
         try { new Storage(); } catch (e) { b.push(e instanceof TypeError); } b.join('|')",
        "true|true",
    );
    check(
        &mut p,
        "[typeof StorageEvent, 'remainingSpace' in localStorage, typeof navigator.storage].join('|')",
        "undefined|false|undefined",
    );

    eprintln!("case: JS-5 storage resets on reinstall");
    check(
        &mut p,
        "localStorage.clear(); localStorage.setItem('r','1'); localStorage.length",
        "1",
    );
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(&mut p, "localStorage.length", "0");
}

/// `history.js`（#1171・`JS-5`・`JS-6`・`SEC-2`・`REPAIR-3`）の最小 `window.history`。
fn history_cases(engine: &str) {
    const BASE: &str = "https://example.com:8443/a/b?q=1#f";
    eprintln!("case: JS-5 history globals and initial values");
    let mut p = setup(engine, None);
    p.bridge.set_location(BASE).expect("set_location");
    check(
        &mut p,
        "[typeof History, typeof history, window.history === history, history instanceof History, \
         Object.prototype.toString.call(history), history.length, history.state === null, \
         history.scrollRestoration].join('|')",
        "function|object|true|true|[object History]|1|true|auto",
    );
    check(
        &mut p,
        "var b = []; try { new History(); } catch (e) { b.push(e instanceof TypeError); } \
         try { History.prototype.pushState.call({}, null, ''); } catch (e) { b.push(e instanceof TypeError); } b.join('|')",
        "true|true",
    );

    eprintln!("case: JS-5 history same-origin URL rewrite updates location views");
    let views = "[location.href, location.pathname, location.search, location.hash, document.URL, \
                 document.location.href].join(' ')";
    for (url, want) in [
        (
            "c?x=1",
            "https://example.com:8443/a/c?x=1 /a/c ?x=1  https://example.com:8443/a/c?x=1 https://example.com:8443/a/c?x=1",
        ),
        (
            "/p/q#h",
            "https://example.com:8443/p/q#h /p/q  #h https://example.com:8443/p/q#h https://example.com:8443/p/q#h",
        ),
        (
            "?z=2",
            "https://example.com:8443/p/q?z=2 /p/q ?z=2  https://example.com:8443/p/q?z=2 https://example.com:8443/p/q?z=2",
        ),
        (
            "#only",
            "https://example.com:8443/p/q?z=2#only /p/q ?z=2 #only https://example.com:8443/p/q?z=2#only https://example.com:8443/p/q?z=2#only",
        ),
        (
            "https://example.com:8443/abs",
            "https://example.com:8443/abs /abs   https://example.com:8443/abs https://example.com:8443/abs",
        ),
    ] {
        check(
            &mut p,
            &format!("history.pushState(null, '', '{url}'); {views}"),
            want,
        );
    }

    eprintln!("case: JS-5 history push grows length, replace does not, capped at 50");
    check(
        &mut p,
        "var l0 = history.length; history.replaceState(null, '', '/r'); var l1 = history.length; \
         history.pushState(null, '', '/s'); [l0, l1, history.length].join('|')",
        "6|6|7",
    );
    check(
        &mut p,
        "for (var i = 0; i < 60; i++) { history.pushState(null, ''); } history.length",
        "50",
    );

    eprintln!("case: JS-5 history rejects cross-origin URLs without changing state (SEC-2)");
    check(
        &mut p,
        "history.pushState({k: 1}, '', '/keep'); history.length",
        "50",
    );
    for bad in [
        "https://evil.test/x",
        "https://example.com:9000/",
        "http://example.com:8443/",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "https://user:pw@example.com:8443/",
    ] {
        check(
            &mut p,
            &format!(
                "var r; try {{ history.pushState({{k: 2}}, '', '{bad}'); r = 'no-throw'; }} catch (e) {{ \
                 r = (e instanceof DOMException) + ',' + e.name + ',' + e.code; }} \
                 [r, location.pathname, history.state.k, history.length].join('|')"
            ),
            "true,SecurityError,18|/keep|1|50",
        );
    }
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert!(
        !format!("{diag:?}").contains("evil.test"),
        "診断に URL 本文が残る"
    );

    eprintln!(
        "case: JS-6 history rejects over-long URLs with SecurityError before the bridge limit (#1175)"
    );
    for (label, url_expr) in [
        ("over 64KiB", "'/' + 'a'.repeat(64 * 1024 + 1)"),
        ("just over 8192 units", "'/' + 'a'.repeat(8192)"),
        (
            "multibyte within unit limit",
            "'/' + '\\u3042'.repeat(8191)",
        ),
    ] {
        for method in ["pushState", "replaceState"] {
            check(
                &mut p,
                &format!(
                    "var r; try {{ history.{method}({{k: 2}}, '', {url_expr}); r = 'no-throw'; }} catch (e) {{ \
                     r = (e instanceof DOMException) + ',' + e.name + ',' + e.code; }} \
                     [r, location.pathname, history.state.k, history.length].join('|')"
                ),
                "true,SecurityError,18|/keep|1|50",
            );
            eprintln!("  rejected: {label} via {method}");
        }
    }
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert!(
        !format!("{diag:?}").contains("aaaaaaaaaaaaaaaa"),
        "診断に長大な URL 本文が残る"
    );

    eprintln!("case: JS-5 history omitted or null URL updates only state");
    check(
        &mut p,
        "history.replaceState({n: 1}, '', null); history.replaceState({n: 2}, ''); [location.pathname, history.state.n].join('|')",
        "/keep|2",
    );

    eprintln!("case: JS-6 history state clone, identity and limits");
    check(
        &mut p,
        "var src = {a: [1, 2]}; history.replaceState(src, ''); src.a.push(3); \
         [history.state === history.state, history.state !== src, history.state.a.length].join('|')",
        "true|true|2",
    );
    check(
        &mut p,
        "history.replaceState(undefined, ''); var u = history.state === undefined; \
         history.replaceState(null, ''); u + '|' + (history.state === null)",
        "true|true",
    );
    check(
        &mut p,
        "history.replaceState({keep: 1}, ''); var out = []; var cyc = {}; cyc.self = cyc; \
         var cases = [function () {}, cyc, 'x'.repeat(2 * 1024 * 1024 + 1)]; \
         for (var i = 0; i < cases.length; i++) { try { history.pushState(cases[i], ''); out.push('no-throw'); } \
         catch (e) { out.push(e instanceof DOMException && e.name); } } \
         out.push(history.state.keep, history.length); out.join('|')",
        "DataCloneError|DataCloneError|DataCloneError|1|50",
    );
    check(
        &mut p,
        "history.replaceState('x'.repeat(2 * 1024 * 1024 - 2), ''); history.state.length",
        "2097150",
    );
    // 共有参照グラフ（27 段の [a, a]）は予算で拒否され、状態は変わらない。
    check(
        &mut p,
        "history.replaceState({keep: 7}, ''); var a = 'x'; for (var i = 0; i < 27; i++) { a = [a, a]; } \
         var r; try { history.pushState(a, ''); r = 'no-throw'; } catch (e) { r = e.name; } r + '|' + history.state.keep",
        "DataCloneError|7",
    );
    // 空文字列の URL 引数は現在の URL（フラグメント含む）を保持する。
    check(
        &mut p,
        "history.replaceState({}, '', '#route'); history.replaceState({}, '', ''); history.pushState({}, '', ''); location.hash",
        "#route",
    );

    // url の toString 内で pushState が再入しても、内側のエントリの URL を上書きしない（push・replace 共通）。
    check(
        &mut p,
        "history.replaceState(0, '', '/base'); \
         history.pushState(2, '', {toString: function () { history.pushState(1, '', '/inner'); return '/outer'; }}); \
         var a = history.state + ':' + location.pathname; \
         history.back(); __fandheRunMicrotasks(); a + '|' + history.state + ':' + location.pathname",
        "2:/outer|1:/inner",
    );
    eprintln!("case: history reentrant toString keeps inner entry url");
    eprintln!("case: JS-6 history state clone budget counts every array slot (shared refs)");
    check(
        &mut p,
        "history.replaceState({keep: 9}, ''); var a = new Array(50000); var r; \
         try { history.pushState(new Array(40000).fill(a), ''); r = 'no-throw'; } catch (e) { r = e.name; } \
         var b = new Array(100001).fill(null); var r2; \
         try { history.pushState(b, ''); r2 = 'no-throw'; } catch (e) { r2 = e.name; } \
         [r, r2, history.state.keep].join('|')",
        "DataCloneError|DataCloneError|9",
    );

    eprintln!(
        "case: JS-6 history clone ignores page-modified Array.prototype.push and budgets many keys"
    );
    check(
        &mut p,
        "var op = Array.prototype.push; Array.prototype.push = function () {}; \
         history.replaceState([1, 2], ''); var l = history.state.length; Array.prototype.push = op; \
         var big = {}; for (var i = 0; i < 100001; i++) { big['k' + i] = 1; } var r; \
         try { history.replaceState(big, ''); r = 'no-throw'; } catch (e) { r = e.name; } l + '|' + r",
        "2|DataCloneError",
    );

    eprintln!("case: JS-5 history methods can be wrapped (Backtrace SDK pattern)");
    check(
        &mut p,
        "var o = history.pushState; history.pushState = function () { return o.apply(this, arguments); }; \
         history.pushState({a: 1}, '', '/w'); location.pathname",
        "/w",
    );
    check(
        &mut p,
        "var t = []; try { history.replaceState({}); } catch (e) { t.push(e instanceof TypeError); } t.join('|')",
        "true",
    );

    eprintln!("case: JS-5 history go(0) and go() are recorded as reloads, not executed (REPAIR-3)");
    let _ = p.bridge.take_diagnostics();
    check(
        &mut p,
        "history.go(0); history.go(); \
         var t = ''; try { history.go(1n); t = 'no-throw'; } catch (e) { t = e instanceof TypeError; } \
         __fandheRunMicrotasks(); [location.pathname, t].join('|')",
        "/w|true",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    let got: Vec<_> = diag
        .location_changes_ignored
        .iter()
        .map(|c| (c.kind, c.value_len))
        .collect();
    assert_eq!(
        got,
        vec![
            (LocationChangeKind::HistoryGo, 1),
            (LocationChangeKind::HistoryGo, 1),
        ]
    );

    eprintln!("case: JS-5 history scrollRestoration accepts only auto/manual");
    check(
        &mut p,
        "history.scrollRestoration = 'manual'; var a = history.scrollRestoration; \
         history.scrollRestoration = 'bogus'; a + '|' + history.scrollRestoration",
        "manual|manual",
    );

    eprintln!("case: SEC-2 history adds no identity surface");
    check(
        &mut p,
        "[typeof PopStateEvent, 'onpopstate' in window, typeof navigation].join('|')",
        "function|true|undefined",
    );

    eprintln!("case: JS-5 history resets on reinstall");
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(
        &mut p,
        "[history.length, history.state === null].join('|')",
        "1|true",
    );

    eprintln!("case: JS-5 history on about:blank rejects URLs but accepts state-only updates");
    let mut b = setup(engine, None);
    check(
        &mut b,
        "var r; try { history.pushState(null, '', '/x'); r = 'no-throw'; } catch (e) { r = e.name; } \
         history.pushState({z: 1}, ''); [r, history.length, history.state.z, location.href].join('|')",
        "SecurityError|2|1|about:blank",
    );
}
/// 汎用 `Event` / `EventTarget`（#1177・`JS-4`・`JS-5`・`REPAIR-3`・`SEC-2`）のケース。
///
/// リスナーの例外は microtask 経由で報告されるため、`__fandheRunMicrotasks()` で drain して診断を確認する。
fn event_target_cases(engine: &str) {
    eprintln!("case: JS-5 Event / EventTarget globals exist");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "[typeof Event, typeof EventTarget, typeof PopStateEvent, typeof CustomEvent].join('|')",
        "function|function|function|function",
    );

    eprintln!("case: JS-5 CustomEvent detail / instanceof / initCustomEvent (#1242)");
    check(
        &mut p,
        "var o = {k: 1}; var ce = new CustomEvent('x', {detail: o, bubbles: true}); \
         [ce.detail === o, ce.type, ce.bubbles, ce.cancelable, ce instanceof Event, ce instanceof CustomEvent, \
          Object.getPrototypeOf(CustomEvent) === Event, Object.prototype.toString.call(ce), ce.isTrusted].join('|')",
        "true|x|true|false|true|true|true|[object CustomEvent]|false",
    );
    check(
        &mut p,
        "[new CustomEvent('x').detail, new CustomEvent('x', {detail: undefined}).detail, new CustomEvent('x', {detail: 0}).detail].map(String).join('|')",
        "null|null|0",
    );
    check(
        &mut p,
        "var r = []; \
         try { CustomEvent('x'); } catch (e) { r.push(e instanceof TypeError); } \
         try { new CustomEvent(); } catch (e) { r.push(e instanceof TypeError); } \
         try { Object.getOwnPropertyDescriptor(CustomEvent.prototype, 'detail').get.call(new Event('x')); } catch (e) { r.push(e instanceof TypeError); } \
         r.join('|')",
        "true|true|true",
    );
    check(
        &mut p,
        "var log = []; var init = { get bubbles() { log.push('bubbles'); return false; }, get cancelable() { log.push('cancelable'); return false; }, \
         get composed() { log.push('composed'); return false; }, get detail() { log.push('detail'); return 1; } }; \
         new CustomEvent('x', init); log.join(',')",
        "bubbles,cancelable,composed,detail",
    );
    check(
        &mut p,
        "var ce = new CustomEvent('a', {detail: 1}); var seen = []; \
         ce.initCustomEvent('b', true, true, 9); seen.push([ce.type, ce.bubbles, ce.cancelable, ce.detail].join(':')); \
         var t = new EventTarget(); t.addEventListener('b', function (e) { e.initCustomEvent('c', false, false, 0); seen.push(e.type + ':' + e.detail); }); \
         t.dispatchEvent(ce); seen.push(ce.type); \
         try { ce.initCustomEvent(); } catch (e) { seen.push(e instanceof TypeError); } \
         seen.join('|')",
        "b:true:true:9|b:9|b|true",
    );
    check(
        &mut p,
        "var t = new EventTarget(); var d = {n: 1}; var got = []; \
         t.addEventListener('q', function (e) { got.push(e.detail === d); }); \
         window.addEventListener('q', function (e) { got.push(e.detail === d && e instanceof CustomEvent); }); \
         t.dispatchEvent(new CustomEvent('q', {detail: d})); window.dispatchEvent(new CustomEvent('q', {detail: d})); got.join('|')",
        "true|true",
    );

    eprintln!(
        "case: JS-5 EventTarget converts type to DOMString and rejects invalid receivers (#1177)"
    );
    check(
        &mut p,
        "var tc = new EventTarget(); var nc = 0; function fc() { nc++; } \
         tc.addEventListener(123, fc); tc.dispatchEvent(new Event(123)); \
         tc.removeEventListener(123, fc); tc.dispatchEvent(new Event('123')); \
         var sym = ''; try { tc.addEventListener(Symbol('s'), fc); } catch (e) { sym = e.name; } \
         var bad = []; \
         ['dispatchEvent', 'addEventListener', 'removeEventListener'].forEach(function (m) { \
           try { EventTarget.prototype[m].call({}, m === 'dispatchEvent' ? new Event('x') : 'x', function () {}); bad.push('ok'); } \
           catch (e) { bad.push(e.name); } }); \
         [nc, sym, bad.join(',')].join('|')",
        "1|TypeError|TypeError,TypeError,TypeError",
    );

    eprintln!("case: JS-5 EventTarget basics (dedupe, capture order, once, handleEvent)");
    check(
        &mut p,
        "var t = new EventTarget(); var log = []; \
         function f(e) { log.push('f:' + e.type + ':' + (e.target === t) + ':' + (e.currentTarget === t) + ':' + e.eventPhase + ':' + e.isTrusted); } \
         t.addEventListener('x', f); t.addEventListener('x', f); t.addEventListener('x', f, true); \
         t.addEventListener('x', function () { log.push('once'); }, { once: true }); \
         t.addEventListener('x', { handleEvent: function (e) { log.push('h:' + (this !== t)); } }); \
         var ev = new Event('x'); var r1 = t.dispatchEvent(ev); var r2 = t.dispatchEvent(new Event('x')); \
         log.join('|') + '#' + r1 + r2 + '#' + ev.currentTarget + ev.eventPhase",
        "f:x:true:true:2:false|f:x:true:true:2:false|once|h:true|f:x:true:true:2:false|f:x:true:true:2:false|h:true#truetrue#null0",
    );
    check(
        &mut p,
        "var t2 = new EventTarget(); var n = 0; function g() { n++; } \
         t2.addEventListener('y', g); t2.removeEventListener('y', g, true); t2.dispatchEvent(new Event('y')); \
         t2.removeEventListener('y', g); t2.dispatchEvent(new Event('y')); n",
        "1",
    );
    check(
        &mut p,
        "var t3 = new EventTarget(); var o = []; \
         t3.addEventListener('s', function () { o.push(1); arguments[0].stopImmediatePropagation(); }); \
         t3.addEventListener('s', function () { o.push(2); }); t3.dispatchEvent(new Event('s')); \
         var c = new Event('c', { cancelable: true }); t3.addEventListener('c', function (e) { e.preventDefault(); }); \
         var rc = t3.dispatchEvent(c); \
         var nc = new Event('nc'); t3.addEventListener('nc', function (e) { e.preventDefault(); }); \
         var rn = t3.dispatchEvent(nc); \
         var pe = new Event('p', { cancelable: true }); t3.addEventListener('p', function (e) { e.preventDefault(); }, { passive: true }); \
         var rp = t3.dispatchEvent(pe); \
         [o.join(), rc, c.defaultPrevented, rn, nc.defaultPrevented, rp, pe.defaultPrevented].join('|')",
        "1|false|true|true|false|true|false",
    );
    check(
        &mut p,
        "var t4 = new EventTarget(); var r = []; var e4 = new Event('z'); \
         t4.addEventListener('z', function () { try { t4.dispatchEvent(e4); r.push('no'); } catch (x) { r.push(x.name + ':' + (x instanceof DOMException)); } }); \
         t4.dispatchEvent(e4); \
         try { t4.dispatchEvent({}); } catch (x) { r.push(x instanceof TypeError); } \
         try { t4.dispatchEvent(); } catch (x) { r.push(x instanceof TypeError); } \
         r.join('|')",
        "InvalidStateError:true|true|true",
    );

    eprintln!("case: JS-5 EventTarget subclassing (ES5 and class)");
    check(
        &mut p,
        "function Sub() { EventTarget.call(this); } Sub.prototype = Object.create(EventTarget.prototype); \
         Sub.prototype.constructor = Sub; var s = new Sub(); var n5 = 0; \
         s.addEventListener('a', function () { n5++; }); s.dispatchEvent(new Event('a')); \
         class K extends EventTarget { constructor() { super(); this.v = 7; } } \
         var k = new K(); var n6 = 0; k.addEventListener('a', function () { n6++; }); k.dispatchEvent(new Event('a')); \
         [n5, n6, k.v, k instanceof EventTarget, Object.prototype.toString.call(k)].join('|')",
        "1|1|7|true|[object EventTarget]",
    );

    eprintln!("case: JS-5 Event constructor");
    check(
        &mut p,
        "var e = new Event('foo', { bubbles: true, cancelable: true, composed: true }); \
         [e.type, e.bubbles, e.cancelable, e.composed, e.defaultPrevented, e.target, e.eventPhase, e.isTrusted, \
          Event.AT_TARGET, e.BUBBLING_PHASE, Object.prototype.toString.call(e)].join('|')",
        "foo|true|true|true|false||0|false|2|3|[object Event]",
    );
    check(
        &mut p,
        "var b = []; try { Event('x'); } catch (x) { b.push(x instanceof TypeError); } \
         try { new Event(); } catch (x) { b.push(x instanceof TypeError); } \
         try { Object.getOwnPropertyDescriptor(Event.prototype, 'type').get.call({}); } catch (x) { b.push(x instanceof TypeError); } \
         b.join('|')",
        "true|true|true",
    );

    eprintln!("case: JS-4 dispatchEvent listener error is reported later and does not stop others");
    let mut p = setup(engine, None);
    let _ = p.bridge.take_diagnostics();
    check(
        &mut p,
        "var t = new EventTarget(); var o = []; \
         t.addEventListener('e', function () { throw new Error('boom'); }); \
         t.addEventListener('e', function () { o.push('after'); }); \
         var thrown = 'no'; try { t.dispatchEvent(new Event('e')); } catch (x) { thrown = 'yes'; } \
         thrown + o.join()",
        "noafter",
    );
    p.eval("__fandheRunMicrotasks(); 'ok'");
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert_eq!(diag.timer_errors.len(), 1);
    assert_eq!(diag.timer_errors[0].message, "boom");
    assert_eq!(
        diag.timer_errors[0].source,
        fandhe_browser_core::dom_bridge::TimerCallbackSource::Microtask
    );

    eprintln!("case: JS-5 window and document dispatchEvent (bubbles document to window)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var o = []; \
         document.addEventListener('d', function (e) { o.push('doc:' + e.eventPhase + ':' + (e.currentTarget === document)); }); \
         window.addEventListener('d', function (e) { o.push('win:' + e.eventPhase + ':' + (e.currentTarget === window) + ':' + (e.target === document)); }); \
         document.dispatchEvent(new Event('d', { bubbles: true })); document.dispatchEvent(new Event('d')); \
         window.dispatchEvent(new Event('d')); \
         document.addEventListener('d', function (e) { e.stopPropagation(); }); \
         document.dispatchEvent(new Event('d', { bubbles: true })); \
         o.join(',')",
        "doc:2:true,win:3:true:true,doc:2:true,win:2:true:false,doc:2:true",
    );
    check(
        &mut p,
        "var lc = []; document.addEventListener('DOMContentLoaded', function (e) { lc.push('dcl:' + e.type); }); \
         window.addEventListener('load', function (e) { lc.push('load:' + e.type); }); \
         __fandheLifecycle('DOMContentLoaded'); __fandheLifecycle('load'); lc.join(',')",
        "dcl:DOMContentLoaded,load:load",
    );

    eprintln!("case: JS-5 AbortSignal stays independent of EventTarget (REPAIR-3)");
    check(
        &mut p,
        "var ac = new AbortController(); var an = 0; ac.signal.addEventListener('abort', function () { an++; }); \
         window.dispatchEvent(new Event('abort')); \
         [an, ac.signal instanceof EventTarget, typeof ac.signal.dispatchEvent, window instanceof EventTarget, \
          document instanceof EventTarget].join('|')",
        "0|false|undefined|false|false",
    );

    eprintln!(
        "case: JS-5 lifecycle events are real Events (instanceof, prototype methods) (#1177)"
    );
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var lr = []; \
         document.addEventListener('DOMContentLoaded', function (e) { \
           lr.push(e instanceof Event); \
           Event.prototype.stopPropagation.call(e); \
           lr.push(e.type + ':' + e.eventPhase + ':' + e.bubbles + ':' + (e.target === document) + ':' + e.isTrusted); \
         }); \
         window.addEventListener('DOMContentLoaded', function () { lr.push('win-ran'); }); \
         window.addEventListener('load', function (e) { \
           Event.prototype.preventDefault.call(e); Event.prototype.stopImmediatePropagation.call(e); \
           lr.push('load:' + e.eventPhase + ':' + (e.target === window) + ':' + e.defaultPrevented); \
         }); \
         __fandheLifecycle('DOMContentLoaded'); __fandheLifecycle('load'); lr.join(',')",
        "true,DOMContentLoaded:2:true:true:false,load:2:true:false",
    );

    eprintln!("case: JS-5 re-initializing an existing Event is rejected (#1177)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var t = new EventTarget(); var e = new Event('x'); var o = []; \
         t.addEventListener('x', function () { \
           try { Event.call(e, 'y'); o.push('reinit-ok'); } catch (x) { o.push(x.name); } \
           try { t.dispatchEvent(e); o.push('redispatch-ok'); } catch (x) { o.push(x.name); } \
           o.push(e.type + ':' + e.eventPhase); \
         }); \
         t.dispatchEvent(e); \
         var pe = new PopStateEvent('popstate', { state: 3 }); \
         o.join() + '|' + pe.state + ':' + (pe instanceof Event) + ':' + new Event('z').type",
        "TypeError,InvalidStateError,x:2|3:true:z",
    );

    eprintln!("case: JS-4 element brand check ignores page rewrites of Node (#1177)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var el = document.createElement('div'); var n = 0; var NodeOrig = window.Node; \
         window.Node = function () {}; \
         Object.defineProperty(NodeOrig, Symbol.hasInstance, { value: function () { return false; }, configurable: true }); \
         el.addEventListener('c', function () { n++; }); el.removeEventListener('c', function () {}); \
         var rej = []; \
         ['addEventListener', 'removeEventListener'].forEach(function (m) { \
           try { NodeOrig.prototype[m].call({}, 'c', function () {}); rej.push('ok'); } catch (x) { rej.push(x.name); } \
         }); \
         Object.defineProperty(NodeOrig, Symbol.hasInstance, { value: function () { return true; }, configurable: true }); \
         try { NodeOrig.prototype.addEventListener.call({}, 'c', function () {}); rej.push('ok'); } catch (x) { rej.push(x.name); } \
         rej.join()",
        "TypeError,TypeError,TypeError",
    );
}

/// `history` の back / forward / go と popstate（#1177・`JS-5`・`JS-6`・`REPAIR-3`・`SEC-2`）のケース。
///
/// 移動は microtask で実行されるため、`__fandheRunMicrotasks()` で drain してから結果を確認する。
fn history_traversal_cases(engine: &str) {
    const BASE: &str = "https://example.com:8443/a/b?q=1#f";
    let based = |engine: &str| {
        let p = setup(engine, None);
        p.bridge.set_location(BASE).expect("set_location");
        let _ = p.bridge.take_diagnostics();
        p
    };

    eprintln!("case: JS-5 history traversal moves URL and state and delivers popstate");
    let mut p = based(engine);
    check(
        &mut p,
        "var log = []; \
         window.addEventListener('popstate', function (e) { \
           log.push(location.pathname + ':' + (e.state && e.state.n) + ':' + (e.state === history.state) + ':' + (e instanceof PopStateEvent) + ':' + (e instanceof Event) + ':' + e.isTrusted); }); \
         history.pushState({n: 1}, '', '/a'); history.pushState({n: 2}, '', '/b'); history.pushState({n: 3}, '', '/c'); \
         history.back(); location.pathname",
        "/c",
    );
    check(
        &mut p,
        "__fandheRunMicrotasks(); log.join(',') + '|' + location.pathname + '|' + history.state.n + '|' + history.length",
        "/b:2:true:true:true:false|/b|2|4",
    );
    check(
        &mut p,
        "history.forward(); __fandheRunMicrotasks(); location.pathname + history.state.n",
        "/c3",
    );
    check(
        &mut p,
        "history.go(-2); __fandheRunMicrotasks(); location.pathname + history.state.n + log.length",
        "/a13",
    );
    check(
        &mut p,
        "history.go(-1); __fandheRunMicrotasks(); location.href + '|' + (history.state === null)",
        "https://example.com:8443/a/b?q=1#f|true",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert!(diag.location_changes_ignored.is_empty());

    eprintln!("case: JS-5 history.state reflects pushState and replaceState values");
    let mut p = based(engine);
    check(
        &mut p,
        "history.replaceState({r: 1}, ''); var a = history.state.r; history.pushState({n: 1}, '', '/p'); \
         [a, history.state.n, history.state !== null].join('|')",
        "1|1|true",
    );

    eprintln!("case: JS-6 history public state mutation does not leak into stored state");
    let mut p = based(engine);
    check(
        &mut p,
        "history.pushState({n: 1}, '', '/a'); history.state.n = 9; history.pushState({n: 2}, '', '/b'); \
         history.back(); __fandheRunMicrotasks(); var r = [history.state.n]; \
         history.state.n = 7; history.forward(); __fandheRunMicrotasks(); history.back(); __fandheRunMicrotasks(); \
         r.push(history.state.n); r.join('|')",
        "1|1",
    );

    eprintln!("case: DOM capture stopPropagation on target skips non-capture listeners");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var t = new EventTarget(), o = []; \
         t.addEventListener('x', function (e) { o.push('cap'); e.stopPropagation(); }, true); \
         t.addEventListener('x', function () { o.push('bub'); }); \
         t.dispatchEvent(new Event('x')); o.join(',')",
        "cap",
    );

    eprintln!("case: JS-5 history out-of-range traversal and go(0)");
    let mut p = based(engine);
    check(
        &mut p,
        "var n = 0; window.addEventListener('popstate', function () { n++; }); history.pushState({k: 1}, '', '/k'); \
         history.go(5); history.back(); history.back(); history.forward(); __fandheRunMicrotasks(); \
         [location.pathname, history.state === null, n, history.length].join('|')",
        "/k|false|2|2",
    );
    check(
        &mut p,
        "history.go(-9); history.go(9); __fandheRunMicrotasks(); [location.pathname, n].join('|')",
        "/k|2",
    );
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    assert!(diag.location_changes_ignored.is_empty());
    check(&mut p, "history.go(0); history.go(); 'ok'", "ok");
    let diag = p.bridge.take_diagnostics().expect("diagnostics");
    let got: Vec<_> = diag
        .location_changes_ignored
        .iter()
        .map(|c| (c.kind, c.value_len))
        .collect();
    assert_eq!(
        got,
        vec![
            (LocationChangeKind::HistoryGo, 1),
            (LocationChangeKind::HistoryGo, 1)
        ]
    );
    assert!(!format!("{diag:?}").contains("example.com"));

    eprintln!("case: JS-5 history pushState after back truncates forward entries");
    let mut p = based(engine);
    check(
        &mut p,
        "var n = 0; window.addEventListener('popstate', function () { n++; }); \
         history.pushState({n: 1}, '', '/a'); history.pushState({n: 2}, '', '/b'); history.pushState({n: 3}, '', '/c'); \
         history.back(); __fandheRunMicrotasks(); history.pushState({n: 9}, '', '/d'); \
         var len = history.length; history.forward(); __fandheRunMicrotasks(); \
         [len, location.pathname, history.state.n, n].join('|')",
        "4|/d|9|1",
    );
    check(
        &mut p,
        "history.back(); __fandheRunMicrotasks(); location.pathname + history.state.n",
        "/b2",
    );

    eprintln!("case: JS-6 history releases the public state of entries that are left");
    let mut p = based(engine);
    check(
        &mut p,
        "history.pushState({payload: {k: 1}}, '', '/a'); var old = history.state; \
         old.payload.extra = 'mutated'; history.pushState({n: 2}, '', '/b'); \
         history.back(); __fandheRunMicrotasks(); var back = history.state; \
         [back !== old, back.payload.k, back.payload.extra === undefined, old.payload.extra, history.state === history.state].join('|')",
        "true|1|true|mutated|true",
    );
    check(
        &mut p,
        "history.replaceState({r: 1}, ''); var r1 = history.state; history.forward(); __fandheRunMicrotasks(); \
         history.back(); __fandheRunMicrotasks(); [history.state !== r1, history.state.r].join('|')",
        "true|1",
    );

    eprintln!("case: JS-5 history cap 50 evicts the oldest entry");
    let mut p = based(engine);
    check(
        &mut p,
        "for (var i = 0; i < 60; i++) { history.pushState({i: i}, '', '/p' + i); } \
         history.go(-50); __fandheRunMicrotasks(); var a = history.state.i; \
         history.go(-49); __fandheRunMicrotasks(); [history.length, a, history.state.i, location.pathname].join('|')",
        "50|59|10|/p10",
    );

    eprintln!("case: JS-6 history cumulative state budget drops the oldest entries");
    let mut p = based(engine);
    check(
        &mut p,
        "var big = 'x'.repeat(2 * 1024 * 1024 - 2); \
         for (var i = 0; i < 5; i++) { history.pushState(big + '', '', '/s' + i); } \
         var len = history.length; var cur = history.state.length; \
         history.go(-3); __fandheRunMicrotasks(); var p3 = location.pathname; \
         history.go(-1); __fandheRunMicrotasks(); \
         [len, cur, p3, location.pathname].join('|')",
        "4|2097150|/s1|/s1",
    );

    eprintln!("case: JS-5 history hash interplay is folded into the entry URL");
    let mut p = based(engine);
    check(
        &mut p,
        "location.hash = '#x'; history.pushState(null, '', '/c'); history.back(); __fandheRunMicrotasks(); location.href",
        "https://example.com:8443/a/b?q=1#x",
    );

    eprintln!("case: JS-5 onpopstate order, coercion and reset");
    let mut p = based(engine);
    check(
        &mut p,
        "var o = []; window.addEventListener('popstate', function () { o.push('A'); }); \
         window.onpopstate = function () { o.push('B:' + (this === window)); }; \
         window.addEventListener('popstate', function () { o.push('C'); }); \
         history.pushState(1, '', '/a'); history.back(); __fandheRunMicrotasks(); o.join('')",
        "AB:trueC",
    );
    check(
        &mut p,
        "window.onpopstate = 5; var z = String(window.onpopstate); history.forward(); __fandheRunMicrotasks(); \
         window.onpopstate = function () { o.push('D'); }; history.back(); __fandheRunMicrotasks(); z + '|' + o.join('')",
        "null|AB:trueCACACD",
    );
    check(
        &mut p,
        "window.onpopstate = null; history.forward(); __fandheRunMicrotasks(); o.join('').length",
        "15",
    );
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(
        &mut p,
        "[window.onpopstate, history.length, 'onpopstate' in window].join('|')",
        "|1|true",
    );

    eprintln!("case: JS-5 history traversal on about:blank moves state only");
    let mut b = setup(engine, None);
    check(
        &mut b,
        "var n = []; window.onpopstate = function (e) { n.push(e.state && e.state.z); }; \
         history.pushState({z: 1}, ''); history.pushState({z: 2}, ''); history.back(); __fandheRunMicrotasks(); \
         [location.href, history.state.z, n.join(), history.length].join('|')",
        "about:blank|1|1|3",
    );

    eprintln!("case: JS-5 PopStateEvent constructor");
    check(
        &mut b,
        "var pe = new PopStateEvent('popstate', { state: { q: 1 }, bubbles: true }); \
         var d = []; try { PopStateEvent('x'); } catch (x) { d.push(x instanceof TypeError); } \
         [pe.state.q, pe.type, pe.bubbles, pe instanceof Event, Object.prototype.toString.call(pe), \
          new PopStateEvent('popstate').state === null, d.join()].join('|')",
        "1|popstate|true|true|[object PopStateEvent]|true|true",
    );
    eprintln!("case: JS-5 PopStateEvent reads init members once (#1189)");
    check(
        &mut b,
        "var n = 0; var e = new PopStateEvent('x', { get state() { return ++n; } }); \
         [e.state, e.state, n].join('|')",
        "1|1|1",
    );
    check(
        &mut b,
        "var c = 0; var r; \
         try { r = new PopStateEvent('x', { get state() { if (c++) { throw new Error('twice'); } return 'ok'; } }).state; } \
         catch (x) { r = 'threw'; } r + '|' + c",
        "ok|1",
    );
    check(
        &mut b,
        "var log = []; new PopStateEvent('x', { get bubbles() { log.push('bubbles'); }, \
         get cancelable() { log.push('cancelable'); }, get composed() { log.push('composed'); }, \
         get state() { log.push('state'); } }); log.join()",
        "bubbles,cancelable,composed,state",
    );
    check(
        &mut b,
        "var k = 0; var e2 = new PopStateEvent('x', { get state() { k++; return undefined; } }); \
         [e2.state === null, k].join('|')",
        "true|1",
    );
    check(
        &mut b,
        "var t = []; try { new PopStateEvent(); } catch (x) { t.push(x instanceof TypeError); } \
         var u = []; window.onpopstate = function (e) { u.push(String(e.state)); }; \
         history.pushState(undefined, ''); history.pushState(1, ''); history.back(); __fandheRunMicrotasks(); \
         t.join() + '|' + u.join() + '|' + String(history.state)",
        "true|undefined|undefined",
    );
}

/// フォーカス追跡（#1185・`JS-5`・`JS-6`・`REPAIR-3`）: `focus()` / `blur()` と `document.activeElement`、
/// 対象要素への focus / blur の配送。focusin / focusout は未実装なので 0 件のまま固定する。
fn focus_cases(engine: &str) {
    const HTML: &str = "<html><head></head><body><input id=i><input id=h type=hidden>\
        <button id=b></button><button id=bd disabled tabindex=0></button><a id=ah href=\"#x\"></a>\
        <a id=an></a><div id=d></div><div id=dt tabindex=-1></div><textarea id=t></textarea>\
        <script>x</script></body></html>";
    let id_of = "document.activeElement.getAttribute('id')";

    eprintln!("case: JS-5 focus() updates document.activeElement for focusable elements only");
    let mut p = setup_html(engine, HTML, None);
    check(
        &mut p,
        "var $ = function (x) { return document.getElementById(x); }; \
         [typeof Element.prototype.focus, typeof Element.prototype.blur, typeof document.hasFocus].join('|')",
        "function|function|undefined",
    );
    check(&mut p, "document.activeElement === document.body", "true");
    check(&mut p, &format!("$('i').focus(); {id_of}"), "i");
    for target in ["d", "h", "bd", "an"] {
        check(&mut p, &format!("$('{target}').focus(); {id_of}"), "i");
    }
    check(&mut p, &format!("$('ah').focus(); {id_of}"), "ah");
    check(&mut p, &format!("$('dt').focus(); {id_of}"), "dt");
    check(&mut p, &format!("$('b').blur(); {id_of}"), "dt");
    check(
        &mut p,
        "$('dt').blur(); document.activeElement === document.body",
        "true",
    );
    check(
        &mut p,
        "document.createElement('input').focus(); document.activeElement === document.body",
        "true",
    );
    check(
        &mut p,
        "try { document.activeElement = $('d'); } catch (e) {} document.activeElement === document.body",
        "true",
    );

    eprintln!("case: JS-5 focus loss on detach / disabled / tabindex removal does not fire blur");
    check(
        &mut p,
        "var bl = 0; $('t').addEventListener('blur', function () { bl++; }); \
         $('t').focus(); var f1 = document.activeElement.getAttribute('id'); \
         document.body.removeChild($('t')); \
         [f1, document.activeElement === document.body, bl].join('|')",
        "t|true|0",
    );
    check(
        &mut p,
        "$('b').focus(); $('b').setAttribute('disabled', ''); document.activeElement === document.body",
        "true",
    );
    check(
        &mut p,
        "$('dt').focus(); $('dt').removeAttribute('tabindex'); document.activeElement === document.body",
        "true",
    );

    eprintln!("case: JS-5 focus loss is kept after reattach / disabled removal / tabindex restore");
    check(
        &mut p,
        "$('i').focus(); var ii = $('i'); document.body.removeChild(ii); document.body.appendChild(ii); \
         document.activeElement === document.body",
        "true",
    );
    check(
        &mut p,
        "$('b').removeAttribute('disabled'); $('b').focus(); \
         $('b').setAttribute('disabled', ''); $('b').removeAttribute('disabled'); \
         document.activeElement === document.body",
        "true",
    );
    check(
        &mut p,
        "var tv = $('dt').getAttribute('tabindex'); if (tv === null) { tv = '0'; $('dt').setAttribute('tabindex', tv); } \
         $('dt').focus(); $('dt').removeAttribute('tabindex'); $('dt').setAttribute('tabindex', tv); \
         document.activeElement === document.body",
        "true",
    );

    eprintln!("case: JS-5 invalid receivers are rejected");
    check(
        &mut p,
        "var r = []; ['focus', 'blur'].forEach(function (m) { \
           try { Element.prototype[m].call({}); r.push('ok'); } catch (e) { r.push(e.name + ':' + /Illegal invocation/.test(e.message)); } }); \
         r.join(',')",
        "TypeError:true,TypeError:true",
    );

    eprintln!("case: JS-5 focus and blur events are delivered to the target only");
    let mut p = setup_html(engine, HTML, None);
    check(
        &mut p,
        "var $ = function (x) { return document.getElementById(x); }; var log = []; var evs = []; \
         ['i', 'b'].forEach(function (k) { \
           $(k).addEventListener('blur', function (e) { log.push('blur:' + k + ':' + (document.activeElement === document.body)); }); \
           $(k).addEventListener('focus', function (e) { evs.push(e); log.push('focus:' + k + ':' + (document.activeElement === $(k))); }); }); \
         var fi = 0; document.addEventListener('focusin', function () { fi++; }); \
         $('i').focus(); $('b').focus(); $('b').focus(); \
         log.join('|') + '#' + fi",
        "focus:i:true|blur:i:true|focus:b:true#0",
    );
    check(
        &mut p,
        "var e = evs[1]; [e.type, e.target === $('b'), e.currentTarget === null, e.bubbles, e.cancelable, e.composed, e.isTrusted, e instanceof Event].join('|')",
        "focus|true|true|false|false|true|false|true",
    );
    check(
        &mut p,
        "log.length = 0; $('b').blur(); $('b').blur(); log.join('|')",
        "blur:b:true",
    );

    eprintln!("case: JS-5 a throwing focus listener does not stop the focus update");
    check(
        &mut p,
        "$('i').addEventListener('focus', function () { throw new Error('boom'); }); \
         $('i').focus(); document.activeElement === $('i')",
        "true",
    );

    eprintln!("case: JS-5 a blur listener that focuses another element aborts the outer focus()");
    let mut p = setup_html(engine, HTML, None);
    check(
        &mut p,
        "var $ = function (x) { return document.getElementById(x); }; var log = []; \
         $('i').addEventListener('blur', function () { $('t').focus(); }); \
         $('t').addEventListener('focus', function () { log.push('t'); }); \
         $('b').addEventListener('focus', function () { log.push('b'); }); \
         $('i').focus(); $('b').focus(); log.join(',') + '|' + document.activeElement.getAttribute('id')",
        "t|t",
    );

    eprintln!("case: JS-5 a blur listener that disables the next target aborts focus delivery");
    let mut p = setup_html(engine, HTML, None);
    check(
        &mut p,
        "var $ = function (x) { return document.getElementById(x); }; var log = []; \
         $('i').addEventListener('blur', function () { $('b').setAttribute('disabled', ''); }); \
         $('b').addEventListener('focus', function () { log.push('b'); }); \
         $('i').focus(); $('b').focus(); \
         log.length + '|' + (document.activeElement === document.body)",
        "0|true",
    );

    eprintln!("case: JS-6 mutually re-entrant focus listeners terminate");
    let mut p = setup_html(engine, HTML, None);
    check(
        &mut p,
        "var $ = function (x) { return document.getElementById(x); }; var n = 0; \
         $('i').addEventListener('focus', function () { n++; $('b').focus(); }); \
         $('b').addEventListener('focus', function () { n++; $('i').focus(); }); \
         $('i').focus(); [n <= 32, ['i', 'b'].indexOf(document.activeElement.getAttribute('id')) >= 0].join('|')",
        "true|true",
    );

    eprintln!("case: JS-5 failed moves keep focus; moving a focused subtree drops it");
    let mut p = setup_html(engine, HTML, None);
    check(
        &mut p,
        "var $ = function (x) { return document.getElementById(x); }; $('i').focus(); \
         try { $('i').appendChild($('i')); } catch (e) {} \
         try { document.body.insertBefore($('b'), $('i').firstChild || $('d').appendChild(document.createElement('p'))); } catch (e) {} \
         document.activeElement === $('i')",
        "true",
    );
    check(
        &mut p,
        "var w = document.createElement('div'); var inner = document.createElement('input'); \
         w.appendChild(inner); document.body.appendChild(w); inner.focus(); \
         var okf = document.activeElement === inner; document.body.appendChild(w); \
         [okf, document.activeElement === document.body].join('|')",
        "true|true",
    );
    check(
        &mut p,
        "var w2 = document.createElement('div'); var in2 = document.createElement('input'); \
         w2.appendChild(in2); document.body.appendChild(w2); in2.focus(); \
         document.body.insertBefore(w2, document.body.firstChild); \
         document.activeElement === document.body",
        "true",
    );
    // node === ref は DOM が変わらず成功するのでフォーカスを保持する（祖先でも同様）。
    check(
        &mut p,
        "var w3 = document.createElement('div'); var in3 = document.createElement('input'); \
         w3.appendChild(in3); document.body.appendChild(w3); in3.focus(); \
         document.body.insertBefore(w3, w3); \
         document.activeElement === in3",
        "true",
    );

    eprintln!("case: JS-5 the focus handoff global is removed after install");
    check(
        &mut p,
        "typeof globalThis.__fandheDomFocusHandoff",
        "undefined",
    );
}

/// JS-5 / REPAIR-3 (#1192): `document.createElementNS` は HTML・SVG の名前空間だけを扱う簡略版。
fn create_element_ns_cases(engine: &str) {
    const SVG: &str = "http://www.w3.org/2000/svg";
    eprintln!(
        "case: JS-5 createElementNS builds SVG (case preserved) and HTML (lowercased) elements"
    );
    let mut p = setup_html(
        engine,
        "<html><head></head><body><script>x</script></body></html>",
        None,
    );
    check(&mut p, "typeof document.createElementNS", "function");
    check(
        &mut p,
        &format!(
            "var ns = '{SVG}'; var sv = document.createElementNS(ns, 'svg'); \
             var r = document.createElementNS(ns, 'clipPath'); sv.appendChild(r); \
             document.body.appendChild(sv); document.body.innerHTML"
        ),
        "<script>x</script><svg><clipPath></clipPath></svg>",
    );
    check(
        &mut p,
        "document.querySelector('svg clipPath') === r",
        "true",
    );
    check(
        &mut p,
        "document.querySelector('svg clippath') === null",
        "true",
    );
    check(
        &mut p,
        "var h = document.createElementNS('http://www.w3.org/1999/xhtml', 'DIV'); \
         document.body.appendChild(h); (h instanceof Element) + '|' + (document.querySelector('div') === h)",
        "true|true",
    );

    eprintln!("case: JS-5 createElementNS rejects unsupported namespaces and prefixed names");
    for args in [
        "null, 'rect'",
        "undefined, 'rect'",
        "'http://www.w3.org/1998/Math/MathML', 'mi'",
        &format!("'{SVG}', 'svg:rect'"),
        &format!("'{SVG}', ''"),
    ] {
        check(
            &mut p,
            &format!("try {{ document.createElementNS({args}); 'ok' }} catch (e) {{ 'threw' }}"),
            "threw",
        );
    }

    eprintln!(
        "case: JS-5 createElementNS result supports style (baseline) and stays a plain Element"
    );
    check(
        &mut p,
        &format!(
            "var sr = document.createElementNS('{SVG}', 'rect'); sr.style.fill = 'red'; \
             sr.getAttribute('style') + '|' + (sr instanceof HTMLElement)"
        ),
        "fill: red;|false",
    );
}

/// JS-5 / JS-6 / REPAIR-3 (#1200): `getElementsByTagName` は live でない静的な Array を返す簡略版。
fn get_elements_by_tag_name_cases(engine: &str) {
    const SVG: &str = "http://www.w3.org/2000/svg";
    eprintln!("case: JS-5 getElementsByTagName matches tag names (HTML case-insensitive, '*')");
    let mut p = setup_html(
        engine,
        "<html><head></head><body><div id=\"a\"><div id=\"b\"></div></div><p></p><script>x</script></body></html>",
        None,
    );
    check(
        &mut p,
        "typeof document.getElementsByTagName + '|' + typeof document.body.getElementsByTagName",
        "function|function",
    );
    check(&mut p, "document.getElementsByTagName('div').length", "2");
    check(
        &mut p,
        "document.getElementsByTagName('script').length",
        "1",
    );
    check(&mut p, "document.getElementsByTagName('DIV').length", "2");
    check(
        &mut p,
        "document.getElementsByTagName('div')[0] === document.querySelector('div')",
        "true",
    );
    check(&mut p, "document.getElementsByTagName('*').length", "7");
    check(
        &mut p,
        "document.getElementsByTagName('span').length + '|' + document.getElementsByTagName('').length",
        "0|0",
    );
    eprintln!("case: JS-5 getElementsByTagName excludes the scope itself and is not live");
    check(
        &mut p,
        "document.body.getElementsByTagName('body').length",
        "0",
    );
    check(
        &mut p,
        "document.getElementById('a').getElementsByTagName('div').length",
        "1",
    );
    check(
        &mut p,
        "var live = document.getElementsByTagName('p'); \
         document.body.appendChild(document.createElement('p')); \
         live.length + '|' + document.getElementsByTagName('p').length",
        "1|2",
    );
    eprintln!("case: JS-5 getElementsByTagName compares SVG names case-sensitively");
    check(
        &mut p,
        &format!(
            "document.body.appendChild(document.createElementNS('{SVG}', 'clipPath')); \
             document.getElementsByTagName('clipPath').length + '|' + \
             document.getElementsByTagName('clippath').length"
        ),
        "1|0",
    );
    check(
        &mut p,
        "try { Document.prototype.getElementsByTagName.call({}, 'div'); 'ok' } catch (e) { 'threw' }",
        "threw",
    );
}

/// JS-5・REPAIR-3・#1203: `parentNode` / `parentElement`。d3 の失敗は `[0]` は取れているのに
/// `[0].parentNode` が未定義だったことが原因（タグ管理系の定型コード）。
fn parent_node_cases(engine: &str) {
    eprintln!("case: JS-5 parentNode of a parsed <script> (#1203)");
    let mut p = setup_html(
        engine,
        "<html><head><script>x</script></head><body></body></html>",
        None,
    );
    check(
        &mut p,
        "var f = document.getElementsByTagName('script')[0]; typeof f + '|' + typeof f.parentNode",
        "object|object",
    );
    check(
        &mut p,
        "document.getElementsByTagName('script')[0].parentNode === document.head",
        "true",
    );
    eprintln!("case: JS-5 tag-manager boilerplate inserts before the first script");
    check(
        &mut p,
        "var s = document.createElement('script'); \
         var f2 = document.getElementsByTagName('script')[0]; \
         f2.parentNode.insertBefore(s, f2); \
         document.getElementsByTagName('script').length + '|' + (s.parentNode === f2.parentNode)",
        "2|true",
    );
    eprintln!("case: JS-5 parentNode up to document, parentElement, detached");
    check(
        &mut p,
        "document.body.parentNode.parentNode === document",
        "true",
    );
    check(
        &mut p,
        "document.parentNode === null && document.body.parentNode.parentElement === null",
        "true",
    );
    check(
        &mut p,
        "document.body.parentElement === document.body.parentNode",
        "true",
    );
    check(
        &mut p,
        "var d = document.createElement('div'); var r0 = d.parentNode === null; \
         document.body.appendChild(d); var r1 = d.parentNode === document.body; \
         document.body.removeChild(d); r0 + '|' + r1 + '|' + (d.parentNode === null)",
        "true|true|true",
    );
    check(
        &mut p,
        "try { Object.getOwnPropertyDescriptor(Node.prototype, 'parentNode').get.call({}); 'ok' } catch (e) { 'threw' }",
        "threw",
    );
}

/// `Node.prototype.isEqualNode`（#1227・`JS-5`）。
fn is_equal_node_cases(engine: &str) {
    eprintln!("case: JS-5 isEqualNode (#1227)");
    let mut p = setup(engine, None);
    let pre = "var a = document.createElement('div'); var b = document.createElement('div'); ";
    let cases: [(&str, &str); 11] = [
        (
            "a.setAttribute('x','1'); a.setAttribute('y','2'); b.setAttribute('y','2'); b.setAttribute('x','1'); a.isEqualNode(b)",
            "true",
        ),
        (
            "a.setAttribute('x','1'); b.setAttribute('x','2'); a.isEqualNode(b)",
            "false",
        ),
        ("a.setAttribute('x','1'); a.isEqualNode(b)", "false"),
        (
            "a.textContent = 'p'; b.textContent = 'q'; a.isEqualNode(b)",
            "false",
        ),
        (
            "a.innerHTML = '<i></i><i></i>'; b.innerHTML = '<i></i>'; a.isEqualNode(b)",
            "false",
        ),
        (
            "a.innerHTML = '<p id=k>t<b>u</b></p>'; b.innerHTML = '<p id=k>t<b>u</b></p>'; a.isEqualNode(b)",
            "true",
        ),
        ("a.isEqualNode(document.createElement('span'))", "false"),
        (
            "document.isEqualNode(document) + '|' + a.isEqualNode(a) + '|' + a.isEqualNode(null)",
            "true|true|false",
        ),
        (
            "try { String(a.isEqualNode()) } catch (e) { 'threw' }",
            "false",
        ),
        (
            "try { a.isEqualNode({}); 'ok' } catch (e) { 'threw' }",
            "threw",
        ),
        (
            "try { Node.prototype.isEqualNode.call({}, a); 'ok' } catch (e) { 'threw' }",
            "threw",
        ),
    ];
    for (body, want) in cases {
        check(&mut p, &format!("{pre}{body}"), want);
    }
}

/// `tagName` / `localName` / `nodeName`（#1236・`JS-5`・`REPAIR-3`）。
fn element_name_cases(engine: &str) {
    const SVG: &str = "http://www.w3.org/2000/svg";
    eprintln!("case: JS-5 tagName / localName / nodeName (#1236)");
    let mut p = setup(engine, None);
    let cases: [(String, &str); 8] = [
        (
            "var e = document.createElement('INPUT'); e.tagName + '|' + e.localName + '|' + e.nodeName".to_string(),
            "INPUT|input|INPUT",
        ),
        ("document.querySelector('div').tagName".to_string(), "DIV"),
        (
            format!("var c = document.createElementNS('{SVG}','clipPath'); c.tagName + '|' + c.localName + '|' + c.nodeName"),
            "clipPath|clipPath|clipPath",
        ),
        (
            "document.body.innerHTML = '<svg><foreignObject id=f></foreignObject></svg>'; var f = document.getElementById('f'); f.tagName + '|' + f.localName".to_string(),
            "foreignObject|foreignObject",
        ),
        (
            "document.createTextNode('t').nodeName + '|' + document.nodeName".to_string(),
            "#text|#document",
        ),
        (
            "String.prototype.toUpperCase = function(){return 'x'}; document.createElement('p').tagName".to_string(),
            "P",
        ),
        (
            "try { Object.getOwnPropertyDescriptor(Element.prototype,'tagName').get.call({}); 'ok' } catch (e) { 'threw' }".to_string(),
            "threw",
        ),
        (
            "String(document.createElement('meta').tagName.toLowerCase() === 'meta')".to_string(),
            "true",
        ),
    ];
    for (body, want) in &cases {
        check(&mut p, body, want);
    }
}

/// `document.documentElement`（#1236・`JS-5`）。
fn document_element_cases(engine: &str) {
    eprintln!("case: JS-5 documentElement (#1236)");
    let mut p = setup(engine, None);
    let cases: [(&str, &str); 4] = [
        (
            "String(document.documentElement === document.querySelector('html'))",
            "true",
        ),
        ("document.documentElement.tagName", "HTML"),
        (
            "String(document.documentElement.parentNode === document) + '|' + String(document.documentElement === document.documentElement)",
            "true|true",
        ),
        (
            "var before = document.documentElement; try { document.documentElement = null } catch (e) {} String(document.documentElement === before)",
            "true",
        ),
    ];
    for (body, want) in cases {
        check(&mut p, body, want);
    }
}

/// `Element.prototype.classList`（#1236・`JS-5`・`REPAIR-3`）。
fn class_list_cases(engine: &str) {
    eprintln!("case: JS-5 classList (#1236)");
    let mut p = setup(engine, None);
    let pre = "var e = document.createElement('div'); e.setAttribute('class','a a b'); var L = e.classList; ";
    let cases: [(&str, &str); 20] = [
        ("String(e.classList === e.classList)", "true"),
        // BigInt は unsigned long 変換で TypeError（Number() では受理されてしまう）
        (
            "var r; try { L.item(1n); r = 'no-throw' } catch (x) { r = x.name } r",
            "TypeError",
        ),
        // 引数変換が class を変更する場合は変換後の集合で判定する（receiver 検証 → 引数変換 → 属性読み取り）
        (
            "e.className = 'a'; String(L.contains({ toString: function () { e.className = 'b'; return 'b'; } }))",
            "true",
        ),
        (
            "e.className = 'a'; String(L.item({ valueOf: function () { e.className = 'x y'; return 1; } }))",
            "y",
        ),
        (
            "L.length + '|' + L.item(0) + L.item(1) + '|' + L.item(5)",
            "2|ab|null",
        ),
        ("L.add('c','a'); e.getAttribute('class')", "a b c"),
        ("L.add('c'); L.remove('a'); e.getAttribute('class')", "b c"),
        ("L.contains('b') + '|' + L.contains('z')", "true|false"),
        (
            "L.toggle('b') + '|' + e.getAttribute('class') + '|' + L.toggle('d') + '|' + e.getAttribute('class')",
            "false|a|true|a d",
        ),
        (
            "L.toggle('a', true) + '|' + L.toggle('x', false) + '|' + e.getAttribute('class')",
            "true|false|a a b",
        ),
        (
            "var r = ''; try { L.toggle() } catch (x) { r += x.name } try { L.contains() } catch (x) { r += x.name } try { L.item() } catch (x) { r += x.name } r + '|' + e.getAttribute('class') + '|' + L.toggle(undefined) + '|' + e.getAttribute('class')",
            "TypeErrorTypeErrorTypeError|a a b|true|a b undefined",
        ),
        // 全引数の DOMString 変換が済んでからトークン検証する（後続引数の toString が class を変更する）
        (
            "var r = ''; try { L.add('', { toString: function () { e.className = 'changed'; return 'x'; } }) } catch (x) { r += x.name } r + '|' + e.getAttribute('class')",
            "TypeError|changed",
        ),
        // Symbol は TypeError（文字列化しない）
        (
            "var r = ''; try { L.add(Symbol('x')) } catch (x) { r += x.name } try { L.contains(Symbol('x')) } catch (x) { r += x.name } try { L.toggle(Symbol('x')) } catch (x) { r += x.name } try { L.remove(Symbol('x')) } catch (x) { r += x.name } r + '|' + e.getAttribute('class')",
            "TypeErrorTypeErrorTypeErrorTypeError|a a b",
        ),
        ("e.className = 'p q'; String(L.length)", "2"),
        (
            "var n = document.createElement('p'); n.classList.remove('x'); String(n.hasAttribute('class'))",
            "false",
        ),
        (
            "var r = ''; try { L.add('') } catch (x) { r += x.name } try { L.add('a b') } catch (x) { r += x.name } r + '|' + e.getAttribute('class')",
            "TypeErrorTypeError|a a b",
        ),
        (
            "var h = document.documentElement; h.classList.add('dark'); h.classList.remove('dark'); h.getAttribute('class')",
            "",
        ),
        (
            "var recs = 0; var mo = new MutationObserver(function(r){ recs += r.length }); mo.observe(e, {attributes:true}); L.add('x'); var q = mo.takeRecords(); q.length + '|' + q[0].attributeName",
            "1|class",
        ),
        (
            "try { Object.getOwnPropertyDescriptor(Element.prototype,'classList').get.call({}); 'ok' } catch (x) { 'threw' }",
            "threw",
        ),
        ("typeof navigator.platform", "undefined"),
    ];
    for (body, want) in cases {
        check(&mut p, &format!("{pre}{body}"), want);
    }
}

/// `Element.prototype.sheet` / `document.styleSheets`（#1202・`JS-5`・`JS-6`・`REPAIR-3`）。
fn style_sheet_cases(engine: &str) {
    const SVG: &str = "http://www.w3.org/2000/svg";
    eprintln!("case: JS-5 style element sheet: null when detached, stable object when connected");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var el = document.createElement('style'); String(el.sheet)",
        "null",
    );
    check(
        &mut p,
        "document.head.appendChild(el); var s = el.sheet; \
         (typeof s) + '|' + (el.sheet === s) + '|' + (s.ownerNode === el) + '|' + s.type + '|' + s.href + '|' + s.disabled",
        "object|true|true|text/css|null|false",
    );
    check(
        &mut p,
        "document.head.removeChild(el); String(el.sheet)",
        "null",
    );
    eprintln!("case: JS-5 sheet is undefined for non-style and SVG style elements");
    check(
        &mut p,
        &format!(
            "var sv = document.createElementNS('{SVG}', 'style'); document.body.appendChild(sv); \
             String(document.createElement('div').sheet) + '|' + String(sv.sheet)"
        ),
        "undefined|undefined",
    );
    check(
        &mut p,
        "try { Object.getOwnPropertyDescriptor(Element.prototype, 'sheet').get.call({}); 'ok' } catch (e) { e instanceof TypeError ? 'TypeError' : 'other' }",
        "TypeError",
    );

    eprintln!("case: JS-5 insertRule / deleteRule keep a persistent cssRules list");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var t = document.createElement('style'); document.head.appendChild(t); var s = t.sheet; var rules = s.cssRules; \
         rules.length + '|' + String(rules.item(0)) + '|' + (s.cssRules === rules)",
        "0|null|true",
    );
    check(
        &mut p,
        "var r0 = s.insertRule('.a{color:red}', 0); var r1 = s.insertRule('  .b{color:blue}  ', s.cssRules.length); \
         var r2 = s.insertRule('.c{color:green}'); \
         [r0, r1, r2, rules.length, rules[0].cssText, rules[1].cssText, rules[2].cssText, rules.item(1) === rules[1], \
          rules[0].parentStyleSheet === s, s.cssRules === rules].join('|')",
        "0|1|0|3|.c{color:green}|.a{color:red}|.b{color:blue}|true|true|true",
    );
    check(
        &mut p,
        "s.deleteRule(0); [rules.length, rules[0].cssText, String(rules[2]), String(rules.item(2))].join('|')",
        "2|.a{color:red}|undefined|null",
    );
    check(
        &mut p,
        "var names = []; \
         try { s.deleteRule(5); } catch (e) { names.push(e.name); } \
         try { s.insertRule('.z{}', 9); } catch (e) { names.push(e.name); } \
         names.join('|') + '|' + rules.length",
        "IndexSizeError|IndexSizeError|2",
    );

    eprintln!("case: JS-5 insertRule accepts one rule and rejects malformed input (SyntaxError)");
    check(
        &mut p,
        "var ok = []; var bad = []; \
         ['@media (min-width:1px){.a{color:red}}', '@font-face{font-family:x;src:url(\"a;b}.woff\")}', '@import url(x.css);', \
          '@import url(x.css)', '.a{content:\"}\"}', '.a{/* } */color:red}', '.a{content:\"\\\\\"}\"}', '/*c*/ .a{} /*d*/'].forEach(function (r) { \
            try { s.insertRule(r, 0); ok.push(1); } catch (e) { ok.push(e.name); } }); \
         ['', '   ', '.a{', '.a{}.b{}', '{}', '.a{} x', '}', '.a{color:\"red}', '.a;'].forEach(function (r) { \
            try { s.insertRule(r, 0); bad.push('ok'); } catch (e) { bad.push(e.name); } }); \
         ok.join(',') + '|' + bad.join(',')",
        "1,1,1,1,1,1,1,1|SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError",
    );

    eprintln!("case: JS-5 insertRule treats unquoted url() as one token (#1208)");
    check(
        &mut p,
        r#"var ok = []; var bad = [];
         ['.a{background:url(/img/}.png)}', '.a{background:URL( /a{b}.png )}', '.a{background:url(a\\)}b.png)}',
          '.a{background:url(a"}b)}', '@import url(a}b.css);', '.a{background:url( "x}.png" )}'].forEach(function (r) {
            try { s.insertRule(r, 0); ok.push(1); } catch (e) { ok.push(e.name); } });
         ['.a{background:url(/img/}.png)', '.a{background:url(/img/}.png}', '.a{background:url(/img/}.png\\)}',
          '.a{b:xurl(})}'].forEach(function (r) {
            try { s.insertRule(r, 0); bad.push('ok'); } catch (e) { bad.push(e.name); } });
         ok.join(',') + '|' + bad.join(',')"#,
        "1,1,1,1,1,1|SyntaxError,SyntaxError,SyntaxError,SyntaxError",
    );
    check(
        &mut p,
        r#"var t3 = document.createElement('style'); document.head.appendChild(t3); var s3 = t3.sheet;
         s3.insertRule('.a{background:url(/img/}.png)}', 0); s3.cssRules[0].cssText"#,
        ".a{background:url(/img/}.png)}",
    );

    eprintln!("case: JS-5 emotion speedy path (sheet.insertRule at cssRules.length)");
    let mut p = setup(engine, None);
    check(
        &mut p,
        "var t = document.createElement('style'); document.head.appendChild(t); var s = t.sheet; \
         for (var i = 0; i < 200; i++) { s.insertRule('.c' + i + '{color:red}', s.cssRules.length); } \
         s.cssRules.length + '|' + s.cssRules[199].cssText + '|' + document.styleSheets.length",
        "200|.c199{color:red}|1",
    );

    eprintln!(
        "case: JS-6 insertRule over the length limit throws QuotaExceededError without growing"
    );
    check(
        &mut p,
        "var big = '.a{content:\"' + new Array(65537).join('x') + '\"}'; var nm = ''; \
         try { s.insertRule(big, 0); nm = 'ok'; } catch (e) { nm = e.name; } nm + '|' + s.cssRules.length",
        "QuotaExceededError|200",
    );

    eprintln!("case: JS-6 insertRule with no argument throws TypeError (not SyntaxError)");
    check(
        &mut p,
        "var nm2 = ''; try { s.insertRule(); } catch (e) { nm2 = e.name; } nm2 + '|' + s.cssRules.length",
        "TypeError|200",
    );

    eprintln!("case: JS-6 deleteRule refunds the untrimmed charge and detaches parentStyleSheet");
    check(
        &mut p,
        "var t4 = document.createElement('style'); document.head.appendChild(t4); var s4 = t4.sheet; \
         var pad = new Array(65001).join(' ') + '.a{}'; var kept = null; var okc = 0; \
         for (var m = 0; m < 65; m++) { s4.insertRule(pad, 0); if (m === 0) { kept = s4.cssRules[0]; } s4.deleteRule(0); okc++; } \
         var q4 = s4.insertRule(pad, 0); \
         [okc, q4, s4.cssRules.length, kept.parentStyleSheet === null, s4.cssRules[0].parentStyleSheet === s4].join('|')",
        "65|0|1|true|true",
    );

    eprintln!("case: JS-6 insertRule beyond 65536 rules throws QuotaExceededError");
    // 1 回の評価に 2 秒のタイムアウトがあるため、65536 回の挿入を 4 回の評価に分ける（状態は評価をまたいで残る）。
    check(
        &mut p,
        "var t2 = document.createElement('style'); document.head.appendChild(t2); var s2 = t2.sheet; \
         for (var j = 0; j < 16384; j++) { s2.insertRule('.a{}', s2.cssRules.length); } s2.cssRules.length",
        "16384",
    );
    for expected in ["32768", "49152", "65536"] {
        check(
            &mut p,
            "for (var j = 0; j < 16384; j++) { s2.insertRule('.a{}', s2.cssRules.length); } s2.cssRules.length",
            expected,
        );
    }
    check(
        &mut p,
        "var nm3 = ''; try { s2.insertRule('.a{}', 0); nm3 = 'ok'; } catch (e) { nm3 = e.name; } \
         s2.cssRules.length + '|' + nm3",
        "65536|QuotaExceededError",
    );

    eprintln!("case: JS-6 insertRule beyond the 4 MiB sheet total throws QuotaExceededError");
    check(
        &mut p,
        "var t3 = document.createElement('style'); document.head.appendChild(t3); var s3 = t3.sheet; \
         var chunk = '.a{content:\"' + new Array(60001).join('x') + '\"}'; var n3 = 0, nm4 = ''; \
         function fill(max) { try { for (var k = 0; k < max; k++) { s3.insertRule(chunk, s3.cssRules.length); n3++; } } \
         catch (e) { nm4 = e.name; } } fill(23); n3 + '|' + nm4",
        "23|",
    );
    // boa は 1 回の評価が 2 秒のタイムアウトに掛かるため、挿入ループを複数回の評価に分ける。
    check(&mut p, "fill(23); n3 + '|' + nm4", "46|");
    check(
        &mut p,
        "fill(24); n3 + '|' + nm4 + '|' + s3.cssRules.length",
        "69|QuotaExceededError|69",
    );

    eprintln!(
        "case: JS-5 document.styleSheets lists connected HTML style sheets in document order"
    );
    let mut p = setup_html(
        engine,
        "<html><head><style>.p{}</style></head><body><div></div><script>x</script></body></html>",
        None,
    );
    check(&mut p, "document.styleSheets.length", "1");
    check(
        &mut p,
        &format!(
            "var a = document.createElement('style'); var b = document.createElement('style'); \
             document.body.appendChild(b); document.head.appendChild(a); \
             document.body.appendChild(document.createElementNS('{SVG}', 'style')); \
             var ss = document.styleSheets; \
             [ss.length, ss[0].ownerNode === document.querySelector('style'), ss[1].ownerNode === a, ss[2].ownerNode === b, \
              ss.item(2) === ss[2], String(ss.item(3)), String(ss[3])].join('|')"
        ),
        "3|true|true|true|true|null|undefined",
    );
    check(
        &mut p,
        "document.body.removeChild(b); [document.styleSheets.length, String(b.sheet)].join('|')",
        "2|null",
    );

    eprintln!("case: JS-5 stylesheet shim exposes no globals and removes its handoff");
    check(
        &mut p,
        "typeof globalThis.__fandheDomStyleSheetHandoff + '|' + typeof CSSStyleSheet + '|' + typeof StyleSheetList + '|' + typeof CSSRule",
        "undefined|undefined|undefined|undefined",
    );

    eprintln!("case: JS-5 stylesheet shim survives reinstall");
    p.rt.install_dom_shim(&p.bridge).expect("reinstall");
    check(
        &mut p,
        "var q = document.createElement('style'); document.head.appendChild(q); \
         q.sheet.insertRule('.r{}', 0) + '|' + document.styleSheets.length",
        "0|3",
    );
}

/// JS-5・REPAIR-3・#1245: `Element.attributes`（静的スナップショット）と `Attr`。Playwright の
/// `previewNode` が `element.attributes.length` を辿って落ちていた。
fn attributes_cases(engine: &str) {
    eprintln!("case: JS-5 Element.attributes / Attr (#1245)");
    let mut p = setup_html(
        engine,
        "<!DOCTYPE html><html><head></head><body><p id=\"t\" data-role=\"greeting\">x</p><svg id=\"s\" viewBox=\"0 0 1 1\"></svg><script>s</script></body></html>",
        None,
    );
    check(
        &mut p,
        "var a = document.getElementById('t'); a.attributes.length + '|' + a.attributes[0].name + '=' + a.attributes[0].value + '|' + a.attributes.item(1).name + '|' + a.attributes.item(9)",
        "2|id=t|data-role|null",
    );
    check(
        &mut p,
        "a.attributes.getNamedItem('DATA-ROLE').value + '|' + a.attributes.getNamedItem('x')",
        "greeting|null",
    );
    check(
        &mut p,
        "var at = a.attributes[0]; at.nodeType + '|' + at.specified + '|' + (at.ownerElement === a) + '|' + (at instanceof Node) + '|' + at.nodeName + '|' + at.textContent",
        "2|true|true|false|id|t",
    );
    check(
        &mut p,
        "var m = a.attributes; a.setAttribute('z','1'); m.length + '|' + a.attributes.length + '|' + a.attributes[2].name",
        "2|3|z",
    );
    check(
        &mut p,
        "a.removeAttribute('id'); a.attributes.length + '|' + a.attributes[0].name",
        "2|data-role",
    );
    check(
        &mut p,
        "var sv = document.getElementById('s'); sv.attributes[1].name + '|' + sv.attributes.getNamedItem('viewbox') + '|' + sv.attributes.getNamedItem('viewBox').value",
        "viewBox|null|0 0 1 1",
    );
    check(
        &mut p,
        "String('attributes' in document.body) + '|' + ('attributes' in document)",
        "true|false",
    );
    // Playwright InjectedScript.previewNode 相当のループ。
    check(
        &mut p,
        "var q = document.createElement('p'); q.setAttribute('id','t'); q.setAttribute('data-role','greeting'); var out = ''; for (var i = 0; i < q.attributes.length; i++) { out += ' ' + q.attributes[i].name + '=\"' + q.attributes[i].value + '\"'; } out",
        " id=\"t\" data-role=\"greeting\"",
    );
    check(
        &mut p,
        "try { Object.getOwnPropertyDescriptor(Element.prototype,'attributes').get.call({}); 'ok' } catch (e) { e instanceof TypeError ? 'TypeError' : 'other' }",
        "TypeError",
    );
    check(
        &mut p,
        "try { new (a.attributes[0].constructor)(); 'ok' } catch (e) { 'threw' }",
        "threw",
    );
}

/// JS-5・REPAIR-3・#1241: `childNodes` / `firstChild` / `lastChild` / `nextSibling` / `previousSibling` /
/// `nodeValue` / `data`。React のハイドレーションが SSR 済み DOM を Text・Comment 込みで辿る。
fn child_traversal_cases(engine: &str) {
    eprintln!("case: JS-5 child traversal over Doctype / Comment / Text / Element (#1241)");
    let mut p = setup_html(
        engine,
        "<!DOCTYPE html><html><head></head><body><!--$--><div id=\"a\">x<b>y</b>z</div><!--/$--><script>s</script></body></html>",
        None,
    );
    check(
        &mut p,
        "var dt = document.firstChild; dt.nodeType + '|' + dt.nodeName + '|' + dt.name + '|' + document.childNodes.length",
        "10|html|html|2",
    );
    check(
        &mut p,
        "String(document.lastChild === document.documentElement) + '|' + (dt.nextSibling === document.documentElement) + '|' + (dt.previousSibling === null)",
        "true|true|true",
    );
    check(
        &mut p,
        "var c0 = document.body.firstChild; c0.nodeType + '|' + c0.nodeName + '|' + c0.data + '|' + c0.nodeValue + '|' + c0.length",
        "8|#comment|$|$|1",
    );
    check(
        &mut p,
        "var a = document.getElementById('a'); String(c0.nextSibling === a) + '|' + (c0.previousSibling === null) + '|' + (a.previousSibling === c0)",
        "true|true|true",
    );
    check(
        &mut p,
        "a.childNodes.length + '|' + a.firstChild.nodeType + '|' + a.firstChild.nodeValue + '|' + a.firstChild.nextSibling.tagName + '|' + a.lastChild.data",
        "3|3|x|B|z",
    );
    check(
        &mut p,
        "String(a.lastChild.previousSibling.previousSibling === a.firstChild) + '|' + (a.childNodes.item(1) === a.childNodes[1]) + '|' + a.childNodes.item(9) + '|' + (a.childNodes[2] === a.lastChild)",
        "true|true|null|true",
    );
    check(
        &mut p,
        "var tail = a.nextSibling; tail.nodeType + '|' + tail.data + '|' + (tail.nextSibling === document.body.lastChild) + '|' + document.body.lastChild.tagName + '|' + (document.body.lastChild.nextSibling === null)",
        "8|/$|true|SCRIPT|true",
    );
    check(
        &mut p,
        "String(a.firstChild instanceof Text) + '|' + (a.firstChild instanceof CharacterData) + '|' + (a.firstChild instanceof Node) + '|' + (c0 instanceof Comment) + '|' + (c0 instanceof CharacterData) + '|' + (c0 instanceof Text) + '|' + (dt instanceof DocumentType) + '|' + (dt instanceof Node)",
        "true|true|true|true|true|false|true|true",
    );
    check(
        &mut p,
        "String(a.nodeValue) + '|' + document.nodeValue + '|' + a.hasChildNodes() + '|' + a.firstChild.hasChildNodes() + '|' + document.createElement('i').hasChildNodes()",
        "null|null|true|false|false",
    );
    check(
        &mut p,
        "typeof Comment + '|' + typeof CharacterData + '|' + typeof DocumentType + '|' + (function () { try { new Comment(); return 'no'; } catch (e) { return e instanceof TypeError ? 'TypeError' : 'other'; } })()",
        "function|function|function|TypeError",
    );
    check(
        &mut p,
        "try { Object.getOwnPropertyDescriptor(Node.prototype, 'firstChild').get.call({}); 'no' } \
         catch (e) { e instanceof TypeError ? 'TypeError' : 'other' }",
        "TypeError",
    );

    eprintln!("case: JS-5 nodeValue / data setters write character data only (#1241)");
    check(
        &mut p,
        "a.firstChild.nodeValue = 'X'; a.lastChild.data = null; 'done'",
        "done",
    );
    check(
        &mut p,
        "a.firstChild.data + '|' + JSON.stringify(a.lastChild.data) + '|' + a.childNodes.length",
        "X|\"\"|3",
    );
    check(&mut p, "c0.data = '$!'; c0.nodeValue", "$!");
    assert!(p.html().contains("<!--$!-->"), "html: {}", p.html());
    assert!(
        p.html().contains("<div id=\"a\">X<b>y</b></div>"),
        "html: {}",
        p.html()
    );
    check(
        &mut p,
        "a.nodeValue = 'q'; a.childNodes.length + '|' + a.firstChild.data",
        "3|X",
    );
    check(
        &mut p,
        "a.firstChild.nodeValue = undefined; var nv = JSON.stringify(a.firstChild.data); \
         a.firstChild.data = undefined; nv + '|' + a.firstChild.data",
        "\"\"|undefined",
    );

    eprintln!("case: JS-5 characterData mutation on Comment is observed (#1241)");
    check(
        &mut p,
        "var seen = []; var mo = new MutationObserver(function (r) { \
           for (var i = 0; i < r.length; i++) seen.push(r[i].type); }); \
         mo.observe(c0, { characterData: true }); c0.data = 'm'; __fandheRunMicrotasks(); seen.join(',')",
        "characterData",
    );

    eprintln!("case: JS-5 traversal follows appendChild / removeChild (#1241)");
    check(
        &mut p,
        "var t = document.createTextNode('new'); a.appendChild(t); \
         String(a.lastChild === t) + '|' + (t.previousSibling === a.childNodes[2]) + '|' + a.childNodes.length",
        "true|true|4",
    );
    check(
        &mut p,
        "a.removeChild(t); String(t.nextSibling) + '|' + t.previousSibling + '|' + a.childNodes.length",
        "null|null|3",
    );
}

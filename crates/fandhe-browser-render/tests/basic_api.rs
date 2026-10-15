//! `HostRenderer` を core の `Renderer` トレイト越し（`&dyn Renderer`）に検証する結合テスト
//! （RENDER-3・RENDER-4。TASK-38.8a・#237。設計 `docs/design/servo-host-design.md` §7・§8.1）。
//!
//! Servo を含まない偽ホスト（`support/mod.rs`。プロトコルを話すだけのテスト用プロセス）を相手に、
//! 正常系 4 API（load・screenshot・visibility・bounding_box）と異常系（タイムアウト・プロトコル
//! 違反・巨大フレーム・不正 JSON・クラッシュ後の再起動）を既定 feature で検証する。実ホストでの
//! 試験は #998（TASK-38.8b）。HostProcess 層の試験は `host_process.rs`。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。

#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use fandhe_browser_core::render::{ImageFormat, PageRef, Renderer, ScreenshotOptions};
use fandhe_browser_render::host_renderer::HostRenderer;
use support::*;

fn t_renderer_roundtrip() {
    let root = Root::new("r-ok", "ok");
    let concrete = renderer(&root, base_timeouts(), false);
    let r: &dyn Renderer = &concrete;
    let loaded = r
        .load(&PageRef::new("https://example.com/a"))
        .expect("load");
    assert_eq!(loaded.final_url(), "https://example.com/a");
    let shot = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect("shot");
    assert_eq!(shot.format, ImageFormat::Png);
    assert_eq!((shot.width, shot.height), (1280, 800));
    assert_eq!(
        shot.data,
        vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
    );
    assert!(r.element_visibility(&sel("#a")).expect("vis").visible);
    let bb = r.bounding_box(&sel("#a")).expect("bb");
    assert_eq!((bb.x, bb.y, bb.width, bb.height), (8.5, -2.0, 120.25, 18.0));
    assert_eq!(count(&root.0, "loads"), 1);
    // 2 回目の load（別 URL）はナビゲーション世代が進み、ホストへ再 load される。
    r.load(&PageRef::new("https://example.com/b"))
        .expect("load2");
    r.capture_screenshot(&ScreenshotOptions::new())
        .expect("shot2");
    assert_eq!(count(&root.0, "loads"), 2);
    assert_eq!(count(&root.0, "spawns"), 1);
}

fn t_renderer_not_loaded_before_load() {
    let root = Root::new("r-notloaded", "ok");
    let r = renderer(&root, base_timeouts(), false);
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("shot");
    assert_eq!(variant(&e), "PageNotLoaded");
    let e = r.element_visibility(&sel("#a")).expect_err("vis");
    assert_eq!(variant(&e), "PageNotLoaded");
    let e = r.bounding_box(&sel("#a")).expect_err("bb");
    assert_eq!(variant(&e), "PageNotLoaded");
    assert_eq!(count(&root.0, "spawns"), 0, "no host spawn before load");
}

fn t_renderer_rejects_urls() {
    let root = Root::new("r-urls", "ok");
    let r = renderer(&root, base_timeouts(), false);
    for (url, expected) in [
        ("file:///etc/passwd", "Internal"),
        ("javascript:alert(1)", "Internal"),
        ("https://user:pw@example.com/", "Internal"),
        ("http://127.0.0.1/", "Internal"),
        ("http://localhost/", "Internal"),
        ("http://2130706433/", "Internal"),
        ("about:blank", "PageNotLoaded"),
    ] {
        let e = r.load(&PageRef::new(url)).expect_err(url);
        assert_eq!(variant(&e), expected, "{url}");
        if let Some(d) = e.internal_detail() {
            assert!(!d.contains("127.0.0.1") && !d.contains("passwd"), "{d}");
        }
    }
    assert_eq!(
        count(&root.0, "spawns"),
        0,
        "rejected URLs never spawn the host"
    );

    // allow=true なら loopback も load できる。
    let root2 = Root::new("r-urls-allow", "ok");
    let r2 = renderer(&root2, base_timeouts(), true);
    let loaded = r2
        .load(&PageRef::new("http://127.0.0.1/"))
        .expect("allowed");
    assert_eq!(loaded.final_url(), "http://127.0.0.1/");
}

fn t_renderer_host_failures() {
    let root = Root::new("r-crash", "crash-on-screenshot");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new("https://example.com/")).expect("load");
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("crash");
    assert_eq!(variant(&e), "HostUnavailable");

    let root = Root::new("r-hang", "hang-query");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new("https://example.com/")).expect("load");
    let e = r.element_visibility(&sel("#a")).expect_err("hang");
    assert_eq!(variant(&e), "Timeout");

    let root = Root::new("r-garbage", "garbage-frame");
    let r = renderer(&root, short_timeouts(), false);
    let e = r
        .load(&PageRef::new("https://example.com/"))
        .expect_err("garbage");
    assert_eq!(variant(&e), "ProtocolMismatch");

    let root = Root::new("r-range", "bad-protocol-range");
    let r = renderer(&root, short_timeouts(), false);
    let e = r
        .load(&PageRef::new("https://example.com/"))
        .expect_err("range");
    assert_eq!(variant(&e), "ProtocolMismatch");
}

fn t_renderer_untrusted_responses() {
    let root = Root::new("r-b64", "bad-base64");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new("https://example.com/")).expect("load");
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("b64");
    assert_eq!(variant(&e), "ProtocolMismatch");

    let root = Root::new("r-notpng", "not-png");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new("https://example.com/")).expect("load");
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("png");
    assert_eq!(variant(&e), "ProtocolMismatch");

    let root = Root::new("r-nf", "not-found");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new("https://example.com/")).expect("load");
    let e = r.element_visibility(&sel("#a")).expect_err("nf");
    assert_eq!(variant(&e), "ElementNotFound");
    let e = r.bounding_box(&sel("#a")).expect_err("nf");
    assert_eq!(variant(&e), "ElementNotFound");
    assert!(!format!("{e:?}{e}").contains("secret.example"));
}

fn t_renderer_final_url_rechecked() {
    let root = Root::new("r-redirect", "redirect-private");
    let r = renderer(&root, short_timeouts(), false);
    let e = r
        .load(&PageRef::new("https://example.com/"))
        .expect_err("redirect");
    assert_eq!(variant(&e), "Internal");
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("after");
    assert_eq!(
        variant(&e),
        "PageNotLoaded",
        "failed load must not leave a page"
    );
}

const URL_A: &str = "https://example.com/a";
const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn log(root: &Root) -> String {
    std::fs::read_to_string(root.0.join("requests")).unwrap_or_default()
}

fn loaded(
    root_name: &str,
    scenario: &str,
    t: fandhe_browser_render::host_process::HostTimeouts,
) -> (Root, HostRenderer) {
    let root = Root::new(root_name, scenario);
    let r = renderer(&root, t, false);
    (root, r)
}

/// 正常系 4 API を具体値で検証し、ホストが受けた要求内容も照合する（受け入れ条件 1）。
fn t_api_roundtrip_concrete() {
    let root = Root::new("b-concrete", "ok");
    let concrete = renderer(&root, base_timeouts(), false);
    let r: &dyn Renderer = &concrete;
    let l = r.load(&PageRef::new(URL_A)).expect("load");
    assert_eq!(l.final_url(), URL_A);
    let s = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect("shot");
    assert_eq!(s.format, ImageFormat::Png);
    assert_eq!((s.width, s.height), (1280, 800));
    assert_eq!(s.data, PNG_SIG.to_vec());
    let s = r
        .capture_screenshot(&ScreenshotOptions::new().with_full_page(true))
        .expect("full");
    assert_eq!((s.width, s.height), (1280, 2400));
    assert!(r.element_visibility(&sel("#a")).expect("vis").visible);
    assert!(!r.element_visibility(&sel("#hidden")).expect("vis").visible);
    let bb = r.bounding_box(&sel("#a")).expect("bb");
    assert_eq!((bb.x, bb.y, bb.width, bb.height), (8.5, -2.0, 120.25, 18.0));
    assert_eq!(
        log(&root),
        format!(
            "load {URL_A}\nscreenshot full_page=false\nscreenshot full_page=true\nvisibility #a\nvisibility #hidden\nbounding_box #a\n"
        )
    );
    assert_eq!(count(&root.0, "spawns"), 1);
    assert_eq!(count(&root.0, "loads"), 1);
}

/// `Arc<dyn Renderer>` を複数スレッドから使える（Send + Sync。同時 in-flight 1 に直列化）。
fn t_api_shared_across_threads() {
    let root = Root::new("b-threads", "ok");
    let r: std::sync::Arc<dyn Renderer> =
        std::sync::Arc::new(renderer(&root, base_timeouts(), false));
    r.load(&PageRef::new(URL_A)).expect("load");
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let r = std::sync::Arc::clone(&r);
            std::thread::spawn(move || {
                for _ in 0..3 {
                    let s = r
                        .capture_screenshot(&ScreenshotOptions::new())
                        .expect("shot");
                    assert_eq!((s.width, s.height), (1280, 800));
                    let bb = r.bounding_box(&sel("#a")).expect("bb");
                    assert_eq!((bb.x, bb.width), (8.5, 120.25));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread");
    }
    assert_eq!(count(&root.0, "spawns"), 1);
}

fn expect_variant(r: &dyn Renderer, op: &str, expected: &str) {
    let e = match op {
        "load" => r.load(&PageRef::new(URL_A)).err(),
        "shot" => r.capture_screenshot(&ScreenshotOptions::new()).err(),
        "vis" => r.element_visibility(&sel("#a")).err(),
        "bb" => r.bounding_box(&sel("#a")).err(),
        _ => unreachable!(),
    };
    let e = e.unwrap_or_else(|| panic!("{op} must fail"));
    assert_eq!(variant(&e), expected, "{op}");
    assert!(!format!("{e:?}{e}").contains("secret.example"));
}

/// タイムアウト（load・screenshot・bounding_box・ハンドシェイク・ホスト自身の timeout）。
fn t_api_timeouts() {
    let started = Instant::now();
    let (root, r) = loaded("b-hang-load", "hang-load", short_timeouts());
    expect_variant(&r, "load", "Timeout");
    assert_gone(child_pid(&root, 1));
    expect_variant(&r, "shot", "PageNotLoaded");

    let root = Root::new("b-hang-query", "hang-query");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new(URL_A)).expect("load");
    expect_variant(&r, "shot", "Timeout");
    let root = Root::new("b-hang-bb", "hang-query");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new(URL_A)).expect("load");
    expect_variant(&r, "bb", "Timeout");

    let (_root, r) = loaded(
        "b-hello-timeout",
        "hello-timeout",
        short_timeouts().with_handshake(Duration::from_millis(200)),
    );
    expect_variant(&r, "load", "Timeout");

    let (_root, r) = loaded("b-host-timeout", "host-timeout", short_timeouts());
    expect_variant(&r, "load", "Timeout");
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// プロトコル違反・巨大フレーム・不正 JSON。巨大フレームは確保前に弾かれ孤児を残さない。
fn t_api_protocol_violations() {
    let started = Instant::now();
    let (_root, r) = loaded("b-idmis", "id-mismatch", short_timeouts());
    expect_variant(&r, "load", "ProtocolMismatch");

    let root = Root::new("b-wrong", "wrong-result");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new(URL_A)).expect("load");
    expect_variant(&r, "shot", "ProtocolMismatch");

    let (root, r) = loaded("b-oversize", "oversize-frame", short_timeouts());
    expect_variant(&r, "load", "ProtocolMismatch");
    assert_gone(child_pid(&root, 1));

    let root = Root::new("b-oversize-vis", "oversize-visibility");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new(URL_A)).expect("load");
    expect_variant(&r, "vis", "ProtocolMismatch");
    assert_gone(child_pid(&root, 1));

    let root = Root::new("b-oversize-shot", "oversize-screenshot");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new(URL_A)).expect("load");
    expect_variant(&r, "shot", "ProtocolMismatch");
    assert_gone(child_pid(&root, 1));

    let (_root, r) = loaded("b-garbage", "garbage-frame", short_timeouts());
    expect_variant(&r, "load", "ProtocolMismatch");
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// クラッシュ・違反の後、呼び出し側が load し直さなくても新ホストで再ロードされ回復する。
fn t_api_recovery_after_failure() {
    for (name, scenario, op) in [
        ("b-rec-shot", "crash-on-screenshot\nok", "shot"),
        ("b-rec-bb", "crash-on-bbox\nok", "bb"),
    ] {
        let root = Root::new(name, scenario);
        let r = renderer(&root, short_timeouts(), false);
        r.load(&PageRef::new(URL_A)).expect("load");
        expect_variant(&r, op, "HostUnavailable");
        assert_gone(child_pid(&root, 1));
        if op == "shot" {
            let s = r
                .capture_screenshot(&ScreenshotOptions::new())
                .expect("again");
            assert_eq!((s.width, s.height), (1280, 800));
        } else {
            let bb = r.bounding_box(&sel("#a")).expect("again");
            assert_eq!((bb.x, bb.y, bb.width, bb.height), (8.5, -2.0, 120.25, 18.0));
        }
        assert_eq!(count(&root.0, "spawns"), 2, "{name}");
        assert_eq!(
            count(&root.0, "loads"),
            2,
            "{name}: page reloaded on new host"
        );
    }

    let root = Root::new("b-rec-garbage", "garbage-on-screenshot\nok");
    let r = renderer(&root, short_timeouts(), false);
    r.load(&PageRef::new(URL_A)).expect("load");
    expect_variant(&r, "shot", "ProtocolMismatch");
    assert_gone(child_pid(&root, 1));
    r.capture_screenshot(&ScreenshotOptions::new())
        .expect("again");
    assert_eq!(count(&root.0, "loads"), 2);

    let root = Root::new("b-rec-start", "crash-after-hello\nok");
    let r = renderer(&root, short_timeouts(), false);
    expect_variant(&r, "load", "HostUnavailable");
    assert_eq!(
        r.load(&PageRef::new(URL_A)).expect("again").final_url(),
        URL_A
    );
}

/// 起動直後の crash が 3 回続いたら以後ホストを起動しない（サーキットブレーカー）。
fn t_api_circuit_breaker() {
    let (root, r) = loaded("b-breaker", "crash-on-start", short_timeouts());
    for _ in 0..3 {
        expect_variant(&r, "load", "HostUnavailable");
    }
    assert_eq!(count(&root.0, "spawns"), 3);
    expect_variant(&r, "load", "HostUnavailable");
    assert_eq!(count(&root.0, "spawns"), 3, "breaker open: no more spawns");
}

fn main() {
    maybe_run_fake_host();
    let tests: Vec<(&str, fn())> = vec![
        ("renderer_roundtrip", t_renderer_roundtrip),
        ("renderer_not_loaded", t_renderer_not_loaded_before_load),
        ("renderer_rejects_urls", t_renderer_rejects_urls),
        ("renderer_host_failures", t_renderer_host_failures),
        ("renderer_untrusted", t_renderer_untrusted_responses),
        ("renderer_final_url", t_renderer_final_url_rechecked),
        ("api_roundtrip_concrete", t_api_roundtrip_concrete),
        ("api_shared_across_threads", t_api_shared_across_threads),
        ("api_timeouts", t_api_timeouts),
        ("api_protocol_violations", t_api_protocol_violations),
        ("api_recovery_after_failure", t_api_recovery_after_failure),
        ("api_circuit_breaker", t_api_circuit_breaker),
    ];
    run("basic_api", tests);
}

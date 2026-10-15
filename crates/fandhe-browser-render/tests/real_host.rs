//! 本物の Servo ホストバイナリと `HostRenderer`（core `Renderer` 実装）を繋ぐ結合テスト
//! （RENDER-3・RENDER-4・SEC-2。TASK-38.8b・#998。設計 `docs/design/servo-host-design.md` §7・§8.1）。
//!
//! 偽ホストを使う `basic_api.rs`（#237）と対になり、ループバックの固定 HTML を配信して
//! load・screenshot・visibility・bounding_box、load タイムアウト、不許可 URL の拒否を実 Servo で確認する。
//! ホストバイナリが要るため `Cargo.toml` で `test = false`（既定の `cargo test` から外す）とし、
//! `servo-host-ubuntu` が次のコマンドで明示実行する。
//! `FANDHE_BROWSER_RENDER_HOST=<絶対パス> cargo test -p fandhe-browser-render --test real_host -- --test-threads=1`
//!
//! 環境変数が未設定・不正なら skip せず panic する（fail-closed。通ったように見せない）。
//! `status`: core の `PageLoad` は `final_url` しか持たず、Servo 0.6.0 のホストも返さないため、
//! `HostProcess` 層で `status() == None` を確認する（作り出さない。REPAIR-3）。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fandhe_browser_core::render::{
    ElementRef, ImageFormat, PageRef, RenderError, Renderer, ScreenshotOptions,
};
use fandhe_browser_render::host_process::{
    HOST_BINARY_ENV, HostProcess, HostProcessConfig, HostTimeouts, resolve_host_binary,
};
use fandhe_browser_render::host_renderer::{HostRenderer, HostRendererConfig};
use fandhe_browser_render::protocol::LoadParams;

/// スクリーンショット用（上半分が赤・下半分が青）。
const SHOT_BODY: &str = "<html><head><title>fixture</title></head><body style=\"margin:0\"><div style=\"height:400px;background:#f00\"></div><div style=\"height:400px;background:#00f\"></div></body></html>";

/// 可視性判定用。
const VIS_BODY: &str = "<html><head><title>vis</title></head><body style=\"margin:0\">\
<div id=\"shown\" style=\"width:100px;height:100px\">a</div>\
<div id=\"none\" style=\"display:none\">b</div>\
<div id=\"vh\" style=\"visibility:hidden;width:50px;height:50px\">c</div>\
<div id=\"off\" style=\"position:absolute;top:-1000px;width:50px;height:50px\">d</div>\
<div data-k=\"x'y\" style=\"width:10px;height:10px\">e</div>\
</body></html>";

/// 境界ボックス用（全要素が絶対配置で期待矩形が既知）。
const BOX_BODY: &str = "<html><head><title>box</title></head><body style=\"margin:0\">\
<div id=\"abs\" style=\"position:absolute;left:10px;top:20px;width:30px;height:40px\"></div>\
<div id=\"frac\" style=\"position:absolute;left:5.5px;top:7.25px;width:12.5px;height:3.75px\"></div>\
<div id=\"off\" style=\"position:absolute;left:0;top:-1000px;width:50px;height:50px\"></div>\
<div id=\"none\" style=\"display:none\"></div>\
</body></html>";

fn host_config(timeouts: HostTimeouts) -> HostProcessConfig {
    let env = std::env::var_os(HOST_BINARY_ENV)
        .unwrap_or_else(|| panic!("{HOST_BINARY_ENV} must be set to the servo host binary"));
    // exe_dir は渡さない（隣接ファイルを誤って拾わない）。
    let bin = resolve_host_binary(None, Some(env.as_os_str()), None)
        .unwrap_or_else(|e| panic!("invalid host binary: {e}"));
    HostProcessConfig::new(bin).with_timeouts(timeouts)
}

fn renderer(timeouts: HostTimeouts, allow_private: bool, load_ms: Option<u32>) -> HostRenderer {
    HostRenderer::new(
        HostRendererConfig::new(host_config(timeouts))
            .with_allow_private_network_access(allow_private)
            .with_load_timeout_ms(load_ms),
    )
}

fn sel(s: &str) -> ElementRef {
    ElementRef::Selector(s.to_owned())
}

fn variant(e: &RenderError) -> &'static str {
    match e {
        RenderError::RenderingDisabled => "RenderingDisabled",
        RenderError::ElementNotFound => "ElementNotFound",
        RenderError::Timeout => "Timeout",
        RenderError::HostUnavailable => "HostUnavailable",
        RenderError::ProtocolMismatch => "ProtocolMismatch",
        RenderError::PageNotLoaded => "PageNotLoaded",
        RenderError::Internal(_) => "Internal",
        _ => "Other",
    }
}

/// ループバックの小さな HTTP サーバー。パスで本文を出し分け、`hang` なら応答しない。
struct Server {
    port: u16,
    stop: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn start(hang: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let (stop, stop_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let mut held = Vec::new();
            while stop_rx.try_recv().is_err() {
                match listener.accept() {
                    Ok((mut conn, _)) => {
                        let _ = conn.set_nonblocking(false);
                        if hang {
                            held.push(conn);
                            continue;
                        }
                        let mut buf = [0u8; 4096];
                        let n = conn.read(&mut buf).unwrap_or(0);
                        let head = String::from_utf8_lossy(buf.get(..n).unwrap_or(&[]));
                        let path = head.split_whitespace().nth(1).unwrap_or("");
                        let (status, body) = match path {
                            "/shot" => ("200 OK", SHOT_BODY),
                            "/vis" => ("200 OK", VIS_BODY),
                            "/box" => ("200 OK", BOX_BODY),
                            _ => ("404 Not Found", "<html><body>nf</body></html>"),
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = conn.write_all(resp.as_bytes());
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn be32(data: &[u8], at: usize) -> u32 {
    let b: [u8; 4] = data
        .get(at..at + 4)
        .expect("png header")
        .try_into()
        .expect("4 bytes");
    u32::from_be_bytes(b)
}

/// RENDER-3: load・screenshot・visibility・bounding_box が実ホストで具体値どおりに返る。
#[test]
fn render3_real_host_roundtrip() {
    let srv = Server::start(false);
    let concrete = renderer(HostTimeouts::default(), true, Some(20_000));
    let r: &dyn Renderer = &concrete;

    let shot_url = srv.url("/shot");
    let loaded = r.load(&PageRef::new(shot_url.as_str())).expect("load");
    assert_eq!(loaded.final_url(), shot_url);

    let shot = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect("screenshot");
    assert_eq!(shot.format, ImageFormat::Png);
    assert_eq!((shot.width, shot.height), (1280, 800));
    assert_eq!(
        shot.data.get(..8),
        Some(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..])
    );
    assert_eq!(shot.data.get(12..16), Some(&b"IHDR"[..]));
    assert_eq!((be32(&shot.data, 16), be32(&shot.data, 20)), (1280, 800));

    // 未対応の full_page は成功を装わない。
    let e = r
        .capture_screenshot(&ScreenshotOptions::new().with_full_page(true))
        .expect_err("full_page");
    assert_eq!(variant(&e), "Internal");

    r.load(&PageRef::new(srv.url("/vis"))).expect("load vis");
    for (s, expected) in [
        ("#shown", true),
        ("#none", false),
        ("#vh", false),
        ("#off", false),
        ("div[data-k=\"x'y\"]", true),
    ] {
        let v = r.element_visibility(&sel(s)).expect(s);
        assert_eq!(v.visible, expected, "{s}");
    }
    let e = r.element_visibility(&sel("#missing")).expect_err("missing");
    assert_eq!(variant(&e), "ElementNotFound");

    r.load(&PageRef::new(srv.url("/box"))).expect("load box");
    for (s, exp) in [
        ("#abs", (10.0, 20.0, 30.0, 40.0)),
        ("#frac", (5.5, 7.25, 12.5, 3.75)),
        ("#off", (0.0, -1000.0, 50.0, 50.0)),
        ("#none", (0.0, 0.0, 0.0, 0.0)),
    ] {
        let b = r.bounding_box(&sel(s)).expect(s);
        assert_eq!((b.x, b.y, b.width, b.height), exp, "{s}");
    }

    // 接続拒否の load は失敗し、その後は前のページを返さない。
    let dead = format!("http://127.0.0.1:{}/", closed_port());
    assert!(r.load(&PageRef::new(dead)).is_err());
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("after failed load");
    assert_eq!(variant(&e), "PageNotLoaded");
}

/// RENDER-4: 応答しないサーバーへの load は Timeout で失敗し、同じ renderer で回復できる。
#[test]
fn render4_real_host_load_timeout() {
    let hang = Server::start(true);
    let ok = Server::start(false);
    let timeouts = HostTimeouts::default()
        .with_load_default(Duration::from_secs(2))
        .with_load_max(Duration::from_secs(5));
    let r = renderer(timeouts, true, Some(1_000));

    let started = Instant::now();
    let e = r
        .load(&PageRef::new(hang.url("/")))
        .expect_err("hang must fail");
    assert_eq!(variant(&e), "Timeout");
    assert!(started.elapsed() < Duration::from_secs(15));
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("after timeout");
    assert_eq!(variant(&e), "PageNotLoaded");

    let url = ok.url("/shot");
    let loaded = r.load(&PageRef::new(url.as_str())).expect("recover");
    assert_eq!(loaded.final_url(), url);
}

/// RENDER-3・SEC-2: 許可外 URL はクライアントが拒否し（ホスト起動前）、通常利用は壊れない。
#[test]
fn render3_real_host_rejects_disallowed_urls_client_side() {
    let srv = Server::start(false);
    let r = renderer(HostTimeouts::default(), true, Some(20_000));
    let userinfo = format!("http://user:pw@127.0.0.1:{}/", srv.port);
    for (url, expected) in [
        ("file:///etc/passwd", "Internal"),
        ("data:text/html,x", "Internal"),
        ("javascript:alert(1)", "Internal"),
        (userinfo.as_str(), "Internal"),
        ("about:blank", "PageNotLoaded"),
    ] {
        let e = r.load(&PageRef::new(url)).expect_err(url);
        assert_eq!(variant(&e), expected, "{url}");
        if let Some(d) = e.internal_detail() {
            assert!(!d.contains("passwd") && !d.contains("127.0.0.1"), "{d}");
        }
    }
    let url = srv.url("/shot");
    let loaded = r.load(&PageRef::new(url.as_str())).expect("load");
    assert_eq!(loaded.final_url(), url);

    // ループバックは既定（allow=false）で拒否される。
    let deny = renderer(HostTimeouts::default(), false, Some(20_000));
    let e = deny.load(&PageRef::new(url)).expect_err("loopback denied");
    assert_eq!(variant(&e), "Internal");
}

/// RENDER-3: HostProcess 層。hello の capabilities と、`status` を作り出さないこと（REPAIR-3）。
#[test]
fn render3_real_host_process_layer() {
    let srv = Server::start(false);
    let mut p = HostProcess::new(host_config(HostTimeouts::default()));
    let url = srv.url("/shot");
    let res = p
        .load(&LoadParams::new(&url, Some(20_000)).expect("params"), 1)
        .expect("load");
    assert_eq!(res.final_url(), url);
    assert_eq!(res.status(), None);

    let hello = p.hello().expect("hello");
    let mut caps = hello.capabilities.clone();
    caps.sort();
    assert_eq!(
        caps,
        [
            "bounding_box",
            "load",
            "screenshot",
            "shutdown",
            "visibility"
        ]
    );

    let workdir = p.workdir_path().expect("workdir");
    assert!(workdir.exists());
    p.shutdown();
    assert!(!workdir.exists(), "workdir must be removed after shutdown");
}

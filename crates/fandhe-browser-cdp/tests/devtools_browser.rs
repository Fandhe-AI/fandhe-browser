//! `/devtools/browser/{id}` WebSocket 受け口の結合テスト
//! （TASK-41（41.4）・#173、ビヘイビア `CDP-1`・`SEC-2`・MS-3）。
//!
//! 実 TCP（`127.0.0.1:0`）で core の `Server` に `router` と `browser_websocket_config` を
//! 配線し、std の `TcpStream` で RFC 6455 のハンドシェイクとフレームを手組みして検証する
//! （ハッシュ・WS クライアント依存を追加しない）。`CdpState` の構築には `Profile::open` が
//! 必要で、非 unix では `ProfileError::Unsupported` を返す仕様のため `#[cfg(unix)]` とする
//! （任意の skip ではなく `tests/json_endpoints.rs` と同じ理由）。

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fandhe_backend_core::server::Server;
use fandhe_browser_cdp::{BrowserId, CdpState, browser_websocket_config, router};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::{Value, json};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// RFC 6455 4.2.2 の例示キーと、その `Sec-WebSocket-Accept`。
const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// 一時ディレクトリ（drop で再帰削除。外部依存は追加しない）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!("fandhe-cdp-ws-test-{}-{n}", std::process::id())))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 実サーバーを起動してアドレスを返す。タスクはテスト終了時にランタイムごと破棄される。
async fn start(dir: &TempDir) -> SocketAddr {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    let st = Arc::new(CdpState::with_browser_id(
        app,
        BrowserId::parse("fixed-1").unwrap(),
    ));
    let bound = Server::new()
        .handler(router(Arc::clone(&st)))
        .websocket(browser_websocket_config(&st).expect("ws config"))
        .bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    tokio::spawn(bound.run());
    addr
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

/// `\r\n\r\n` までを 1 バイトずつ読む（後続フレームを読み過ぎない）。
fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut b).expect("read head");
        assert_eq!(
            n,
            1,
            "unexpected EOF, got: {:?}",
            String::from_utf8_lossy(&buf)
        );
        buf.push(b[0]);
    }
    String::from_utf8(buf).unwrap()
}

fn status_of(head: &str) -> u16 {
    head.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// アップグレード要求を送り、応答ヘッドを返す。
fn upgrade(s: &mut TcpStream, path: &str, host: &str, extra: &str) -> String {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    read_head(s)
}

/// マスク付きテキストフレームを送る（ペイロードは 126 バイト未満）。
fn send_text(s: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    assert!(payload.len() < 126);
    let key = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&key);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    s.write_all(&frame).unwrap();
}

/// サーバー発の非マスクテキストフレームを 1 つ読んで JSON にする。
fn read_text_json(s: &mut TcpStream) -> Value {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).expect("frame header");
    assert_eq!(h[0], 0x81, "expected FIN + text frame");
    assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
    let len = usize::from(h[1] & 0x7f);
    assert!(len < 126);
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).expect("frame payload");
    serde_json::from_slice(&payload).expect("json payload")
}

#[tokio::test]
async fn cdp1_devtools_browser_ws_handshake_and_roundtrip() {
    let dir = TempDir::new();
    let addr = start(&dir).await;
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        let head = upgrade(
            &mut s,
            "/devtools/browser/fixed-1",
            &format!("127.0.0.1:{}", addr.port()),
            "",
        );
        assert_eq!(status_of(&head), 101, "head: {head}");
        assert!(head.contains(&format!("Sec-WebSocket-Accept: {WS_ACCEPT}")));

        send_text(&mut s, r#"{"id":1,"method":"Browser.getVersion"}"#);
        assert_eq!(
            read_text_json(&mut s),
            json!({"id": 1, "error": {"code": -32601, "message": "method not implemented"}})
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp1_json_version_ws_url_is_connectable() {
    let dir = TempDir::new();
    let addr = start(&dir).await;
    tokio::task::spawn_blocking(move || {
        let host = format!("127.0.0.1:{}", addr.port());
        let mut s = connect(addr);
        s.write_all(
            format!("GET /json/version HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).expect("read response");
        let text = String::from_utf8(raw).unwrap();
        let body = text.split("\r\n\r\n").nth(1).expect("body");
        let v: Value = serde_json::from_str(body).expect("json body");
        let url = v["webSocketDebuggerUrl"]
            .as_str()
            .expect("ws url")
            .to_owned();
        assert_eq!(url, format!("ws://{host}/devtools/browser/fixed-1"));

        // Playwright の discovery -> connect と同じく、返された URL のパスへ接続する。
        let path = url.strip_prefix(&format!("ws://{host}")).unwrap();
        let mut s = connect(addr);
        let head = upgrade(&mut s, path, &host, "");
        assert_eq!(status_of(&head), 101, "head: {head}");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp1_devtools_browser_rejects_unknown_id() {
    let dir = TempDir::new();
    let addr = start(&dir).await;
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        let head = upgrade(
            &mut s,
            "/devtools/browser/other",
            &format!("127.0.0.1:{}", addr.port()),
            "",
        );
        assert_eq!(status_of(&head), 404, "head: {head}");
        assert!(!head.contains("Sec-WebSocket-Accept"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp1_devtools_browser_rejects_foreign_host() {
    let dir = TempDir::new();
    let addr = start(&dir).await;
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        let head = upgrade(
            &mut s,
            "/devtools/browser/fixed-1",
            &format!("evil.example:{}", addr.port()),
            "",
        );
        assert_eq!(status_of(&head), 403, "head: {head}");
        assert!(!head.contains("Sec-WebSocket-Accept"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp1_devtools_browser_rejects_origin() {
    let dir = TempDir::new();
    let addr = start(&dir).await;
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        let head = upgrade(
            &mut s,
            "/devtools/browser/fixed-1",
            &format!("127.0.0.1:{}", addr.port()),
            "Origin: https://evil.example\r\n",
        );
        assert_eq!(status_of(&head), 403, "head: {head}");
        assert!(!head.contains("Sec-WebSocket-Accept"));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp1_devtools_browser_plain_get_is_404() {
    let dir = TempDir::new();
    let addr = start(&dir).await;
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        let host = format!("127.0.0.1:{}", addr.port());
        s.write_all(
            format!("GET /devtools/browser/fixed-1 HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes(),
        )
        .unwrap();
        let head = read_head(&mut s);
        assert_eq!(status_of(&head), 404, "head: {head}");
    })
    .await
    .unwrap();
}

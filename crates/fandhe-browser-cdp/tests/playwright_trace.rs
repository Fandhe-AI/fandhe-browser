//! Playwright `newPage()` 到達までの CDP トレース再生テスト
//! （TASK-43（43.1）・#246、ビヘイビア `CDP-2`・`CDP-6`・MS-4）。
//!
//! `harness/playwright-trace/results/newpage-trace.jsonl`（`make trace-playwright` が実 Playwright から
//! 収集・正規化したトレース。スキーマは同ディレクトリの README）を実サーバーへ再生し、
//! 「Playwright が送ったリクエストに対し、現状の実装が記録どおりの応答を返す」ことを具体値で固定する。
//! Node 不要・ネットワーク不要で常時実行できる回帰フックであり、原因調査（TASK-43.2・#247）と
//! 追加実装（TASK-43.3・#249）で応答が変わったら、トレースを再取得して本ファイルの期待値
//! （`EXPECTED_*`）を更新する。ハンドラ・no-op は本タスクでは追加しない（`SEC-2`・`REPAIR-3`）。
//!
//! `CdpState` の構築に `Profile::open` が必要で、非 unix では `ProfileError::Unsupported` を返す仕様のため
//! `#[cfg(unix)]` とする（任意の skip ではなく `tests/devtools_browser.rs` と同じ理由）。

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fandhe_backend_core::server::Server;
use fandhe_browser_cdp::{BrowserId, CdpState, endpoints};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::{Value, json};

/// 収集済みトレース（リポジトリ内ファイル。`docs/spec` は参照しない）。
const TRACE: &str = include_str!("../../../harness/playwright-trace/results/newpage-trace.jsonl");

/// トレース行数・1 行サイズの上限（フィクスチャ破損時の無制限処理を防ぐ）。
const MAX_TRACE_LINES: usize = 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;

/// 現状のトレースが示す到達点（2026-10 時点）。Playwright が最初に送る `Browser.getVersion` が
/// 未実装（`-32601`。`CDP-6`）で、`connectOverCDP` が失敗する。
const EXPECTED_SEND_METHODS: [&str; 1] = ["Browser.getVersion"];

static COUNTER: AtomicUsize = AtomicUsize::new(0);

const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// 一時ディレクトリ（drop で再帰削除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!("fandhe-cdp-trace-test-{}-{n}", std::process::id())))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn start(dir: &TempDir) -> (SocketAddr, Arc<CdpState>) {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    // トレース収集サーバー（examples/trace_server.rs）と同じブラウザ ID。
    let st = Arc::new(CdpState::with_browser_id(
        app,
        BrowserId::parse("trace-1").unwrap(),
    ));
    let (router, ws_config) = endpoints(&st).expect("endpoints").into_parts();
    let bound = Server::new()
        .handler(router)
        .websocket(ws_config)
        .bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    tokio::spawn(bound.run());
    (addr, st)
}

/// トレースの 1 レコード（`seq` 以外は種別ごとに参照する）。
fn records() -> Vec<Value> {
    TRACE
        .lines()
        .map(|l| serde_json::from_str(l).expect("trace line is JSON"))
        .collect()
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).expect("read head"), 1, "unexpected EOF");
        buf.push(b[0]);
    }
    String::from_utf8(buf).unwrap()
}

fn status_of(head: &str) -> u16 {
    head.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// 1 回限りの HTTP GET を送り、ステータスコードを返す。
fn http_get_status(addr: SocketAddr, path: &str) -> u16 {
    let mut s = connect(addr);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes()).unwrap();
    status_of(&read_head(&mut s))
}

fn upgrade(s: &mut TcpStream, path: &str, port: u16) -> u16 {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    status_of(&read_head(s))
}

/// マスク付きテキストフレームを送る（16 bit 拡張長まで。`Target.setAutoAttach` 等の 126 バイト超に対応）。
fn send_text(s: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    let key = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        let len = u16::try_from(payload.len()).expect("payload fits 16 bit length");
        frame.push(0x80 | 126);
        frame.extend_from_slice(&len.to_be_bytes());
    }
    frame.extend_from_slice(&key);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    s.write_all(&frame).unwrap();
}

fn read_text_json(s: &mut TcpStream) -> Value {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).expect("frame header");
    assert_eq!(h[0], 0x81, "expected FIN + text frame");
    assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
    let mut len = usize::from(h[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        s.read_exact(&mut ext).expect("extended length");
        len = usize::from(u16::from_be_bytes(ext));
    }
    assert!(len <= MAX_LINE_BYTES, "payload too large for test helper");
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).expect("frame payload");
    serde_json::from_slice(&payload).expect("json payload")
}

#[test]
fn cdp2_playwright_trace_is_well_formed() {
    assert!(TRACE.ends_with('\n') && !TRACE.contains('\r'), "LF only");
    assert!(TRACE.lines().count() <= MAX_TRACE_LINES);
    assert!(TRACE.lines().all(|l| l.len() <= MAX_LINE_BYTES));
    let recs = records();
    for (i, r) in recs.iter().enumerate() {
        assert_eq!(r["seq"], json!(i), "seq must be consecutive");
    }
    assert_eq!(recs[0]["kind"], json!("meta"));
    assert_eq!(recs[0]["schema"], json!(1));
    assert_eq!(recs[0]["playwright"], json!("1.63.0"));
    let sends: Vec<&str> = recs
        .iter()
        .filter(|r| r["kind"] == "cdp" && r["dir"] == "send")
        .map(|r| r["message"]["method"].as_str().expect("method is a string"))
        .collect();
    assert_eq!(sends, EXPECTED_SEND_METHODS);
    // 現状の到達点: connectOverCDP のみ失敗し、newContext / newPage には到達しない。
    let stages: Vec<(&str, bool)> = recs
        .iter()
        .filter(|r| r["kind"] == "stage")
        .map(|r| (r["name"].as_str().unwrap(), r["ok"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        stages,
        [
            ("connectOverCDP(http)", false),
            ("connectOverCDP(ws)", false)
        ]
    );
}

#[tokio::test]
async fn cdp2_playwright_trace_http_discovery_matches_recorded_status() {
    let dir = TempDir::new();
    let (addr, _st) = start(&dir).await;
    let expected: Vec<(String, u16)> = records()
        .iter()
        .filter(|r| r["kind"] == "http")
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_owned(),
                u16::try_from(r["status"].as_u64().unwrap()).unwrap(),
            )
        })
        .collect();
    // Playwright は /json/version/（末尾スラッシュ付き）を要求し、現状は 404 になる（#247 の調査対象）。
    assert_eq!(
        expected,
        [
            ("/json/version".to_owned(), 200),
            ("/json/version/".to_owned(), 404)
        ]
    );
    tokio::task::spawn_blocking(move || {
        for (path, status) in expected {
            assert_eq!(http_get_status(addr, &path), status, "path: {path}");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp2_playwright_trace_replay_matches_recorded_responses() {
    let dir = TempDir::new();
    let (addr, st) = start(&dir).await;
    let recs = records();
    let sends: Vec<Value> = recs
        .iter()
        .filter(|r| r["kind"] == "cdp" && r["dir"] == "send")
        .map(|r| r["message"].clone())
        .collect();
    let recvs: Vec<Value> = recs
        .iter()
        .filter(|r| r["kind"] == "cdp" && r["dir"] == "recv")
        .map(|r| r["message"].clone())
        .collect();
    let replayed = tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        assert_eq!(
            upgrade(&mut s, "/devtools/browser/trace-1", addr.port()),
            101
        );
        let mut got = Vec::new();
        for req in &sends {
            send_text(&mut s, &req.to_string());
            // イベントを挟みうるため、対応する id の応答が来るまで読む。
            loop {
                let m = read_text_json(&mut s);
                let done = m["id"] == req["id"];
                got.push(m);
                if done {
                    break;
                }
            }
        }
        got
    })
    .await
    .unwrap();
    assert_eq!(
        replayed,
        [json!({"id": 1, "error": {"code": -32601, "message": "method not implemented"}})]
    );
    assert_eq!(replayed, recvs, "replay must match the recorded responses");

    // 受信メソッドログ（CDP-6）がトレースの送信メソッドと一致する。
    let snap = st.received_methods();
    let logged: Vec<(&str, u64, u64)> = snap
        .methods
        .iter()
        .map(|(k, c)| (k.as_str(), c.handled, c.unimplemented))
        .collect();
    assert_eq!(logged, [("Browser.getVersion", 0, 1)], "snapshot: {snap:?}");
}

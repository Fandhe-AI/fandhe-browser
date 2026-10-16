//! ローカル fixture HTTP サーバー（TASK-44.1・#476、ビヘイビア `CDP-2`・MS-4）。
//!
//! Playwright 側スクリプト（`harness/playwright-connect/connect.mjs`）が将来 `goto` する先を
//! テストが自前で用意するための最小の HTTP/1.1 サーバー。外部ネットワークへ出さないため
//! `127.0.0.1:0` にのみ bind し、1 接続 1 応答（`Connection: close`）で処理する。
//! 到達したリクエストパスを記録するので、テストは「ページが実際に取得されたか」を検証できる
//! （検証本体は TASK-44.3・#478 の担当。ここは基盤のみ）。
//!
//! スクリプトへは URL を環境変数 [`FIXTURE_URL_ENV`] で渡す（シェル文字列は経由しない）。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// fixture ページの URL をスクリプトへ渡す環境変数名。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub const FIXTURE_URL_ENV: &str = "FANDHE_FIXTURE_URL";

/// リクエストヘッダの読み取り上限（超過分は打ち切る。無制限確保の防止）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

/// 1 ルート（`GET <path>` に対する固定応答）。
#[derive(Debug, Clone)]
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub struct FixtureRoute {
    pub path: String,
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

impl FixtureRoute {
    /// `200 text/html` の固定応答。
    #[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
    pub fn html(path: &str, body: &str) -> Self {
        Self {
            path: path.to_string(),
            status: 200,
            content_type: "text/html; charset=utf-8".to_string(),
            body: body.to_string(),
        }
    }
}

/// 起動済み fixture サーバー。drop で accept ループを止めて終了を待つ。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub struct FixtureServer {
    pub addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FixtureServer {
    /// `path` の fixture ページ URL（`http://127.0.0.1:<port><path>`）。
    #[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    /// これまでに到達したリクエストパス（到達順。未定義ルートへの到達も含む）。
    #[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // ブロック中の accept を起こす（接続自体は即座に閉じられる）。
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(2));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while head.len() < MAX_REQUEST_HEAD_BYTES && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(buf.get(..n)?),
        }
    }
    let text = String::from_utf8_lossy(&head);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    Some(parts.next()?.to_string())
}

#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
fn handle(mut stream: TcpStream, routes: &[FixtureRoute], hits: &Mutex<Vec<String>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(path) = read_request_path(&mut stream) else {
        return;
    };
    if let Ok(mut g) = hits.lock() {
        g.push(path.clone());
    }
    let (status, ctype, body) = match routes.iter().find(|r| r.path == path) {
        Some(r) => (r.status, r.content_type.as_str(), r.body.as_str()),
        None => (404, "text/plain; charset=utf-8", "not found"),
    };
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

/// fixture サーバーを専用スレッドで起動する（`127.0.0.1:0` のみ bind）。
#[allow(dead_code)] // 取り込むテストバイナリごとに使う項目が異なる（未使用は当該バイナリのみ）
pub fn start_fixture_server(routes: Vec<FixtureRoute>) -> FixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let addr = listener.local_addr().expect("local_addr");
    let hits = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (h, s) = (Arc::clone(&hits), Arc::clone(&stop));
    let thread = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if s.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(c) = conn {
                handle(c, &routes, &h);
            }
        }
    });
    FixtureServer {
        addr,
        hits,
        stop,
        thread: Some(thread),
    }
}

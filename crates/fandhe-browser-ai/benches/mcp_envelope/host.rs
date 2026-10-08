//! MCP エンベロープ測定ハーネスの loopback ホスト（TASK-97.1・Issue #385・`PLUG-5`・`MS-9`）。
//!
//! 本物の `fandhe_browser_ai::api::router`（`GET /ai/snapshot`・`POST /ai/plugins/register`）を
//! `127.0.0.1:0` だけで待ち受ける最小 HTTP/1.1 ループに載せる。`mcp_envelope.rs` の main が起動し、
//! mcp バイナリ（`FANDHE_BROWSER_HOST_ADDR` で接続先を受け取る）と、ハーネス自身の直接 GET
//! （方式 B 本体の応答・レイテンシ基準）の双方が同じ経路でここへ到達する。
//!
//! mcp 側の `host.rs`・`register.rs` はどちらも `Connection: close` を送るため 1 接続 1 要求で足りる。
//! 外部入力扱いの上限: 要求ヘッダ 16 KiB・本文 1 MiB・直接 GET の応答 8 MiB。超過は 400 で切断する。
//! 待ち受けは loopback のみ（外部インターフェースへ公開しない）。本物のルータの Host 検証も残す。

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_backend_routes::Router;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_DIRECT_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// loopback で ai ルータを待ち受けるホスト。Drop で停止する。
pub struct BenchHost {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl BenchHost {
    /// `127.0.0.1:0` で待ち受けを開始する。
    pub fn start(router: Router) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("mcp-envelope-host".to_string())
            .spawn(move || serve(listener, router, stop_for_thread))?;
        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
        })
    }

    /// 待ち受けアドレス。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for BenchHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // accept で待機中のスレッドを起こす（失敗しても Drop は続行する）。
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(listener: TcpListener, router: Router, stop: Arc<AtomicBool>) {
    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(_) => {
            eprintln!("mcp-envelope: failed to build host runtime");
            return;
        }
    };
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let _ = handle_connection(stream, &router, &rt);
    }
}

fn handle_connection(
    mut stream: TcpStream,
    router: &Router,
    rt: &tokio::runtime::Runtime,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head, consumed) = loop {
        if buf.len() > MAX_HEAD_BYTES {
            return reject(&mut stream);
        }
        match parse_request_head(&buf) {
            Ok(ParseOutcome::Complete { head, consumed }) => break (head, consumed),
            Ok(ParseOutcome::Incomplete) => {}
            Err(_) => return reject(&mut stream),
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let content_length = match head.header("content-length") {
        None => 0,
        Some(v) => match v.trim().parse::<usize>() {
            Ok(n) if n <= MAX_BODY_BYTES => n,
            _ => return reject(&mut stream),
        },
    };
    let mut body: Vec<u8> = buf.get(consumed..).unwrap_or_default().to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        body.extend_from_slice(&chunk[..n]);
        if body.len() > MAX_BODY_BYTES {
            return reject(&mut stream);
        }
    }
    body.truncate(content_length);
    let response = rt.block_on(router.dispatch(&head, &body));
    stream.write_all(&response.serialize(false))?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn reject(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(&Response::empty(400).serialize(false))?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

/// ホストへの直接 GET の結果（ステータスと本文）。
pub struct DirectResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 直接 GET（接続から読了まで）。応答は 8 MiB で打ち切って失敗にする。
pub fn http_get(addr: SocketAddr, path: &str) -> Result<DirectResponse, String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| "failed to connect to bench host".to_string())?;
    let io_err = |_| "bench host I/O error".to_string();
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(io_err)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(io_err)?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(io_err)?;
    let mut raw = Vec::new();
    (&mut stream)
        .take(MAX_DIRECT_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(io_err)?;
    if raw.len() > MAX_DIRECT_RESPONSE_BYTES {
        return Err("bench host response too large".to_string());
    }
    parse_response(&raw)
}

/// 応答バイト列をステータスと本文へ分解する（chunked は使わない前提）。
fn parse_response(raw: &[u8]) -> Result<DirectResponse, String> {
    let malformed = || "malformed bench host response".to_string();
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(malformed)?;
    let head = std::str::from_utf8(&raw[..sep]).map_err(|_| malformed())?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(malformed)?;
    Ok(DirectResponse {
        status,
        body: raw[sep + 4..].to_vec(),
    })
}

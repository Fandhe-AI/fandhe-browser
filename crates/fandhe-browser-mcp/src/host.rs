//! ホスト（fandhe-browser 本体）への最小 HTTP/1.1 クライアント（TASK-94.3・PLUG-2・PLUG-3・MS-9）。
//!
//! `navigate` ツール（`POST /ai/navigate`）と `snapshot` ツール（`GET /ai/snapshot`・TASK-94.4・
//! PLUG-4）がホストを呼ぶために使う。PLUG-1 により
//! workspace 内の他 crate へは依存できず、HTTP クライアント系の新規依存も承認されていないため、
//! `std::net` のみで実装する。接続先は loopback の IP リテラルに限り（DNS 解決なし・SSRF 防止）、
//! 接続・全体の期限と応答サイズ上限を持ち、リダイレクトは追従しない。
//! 呼び出し元は `server.rs` の各ツールハンドラ（`spawn_blocking` 越し）。

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// ホスト接続先を指定する環境変数名。
pub(crate) const HOST_ADDR_ENV: &str = "FANDHE_BROWSER_HOST_ADDR";
/// 既定の接続先（ホスト公開サーバーの既定 loopback アドレス）。
pub(crate) const DEFAULT_HOST_ADDR: &str = "127.0.0.1:9333";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// 要求全体の期限。ホスト側の取得待ちを含むため長めに取る。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// navigate 用の応答読み取り上限（バイト）。
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// ホストへの同時リクエスト数の上限（blocking スレッド無制限増加の防止）。
const MAX_CONCURRENT_REQUESTS: usize = 4;
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// 同時実行枠の保持者。drop で枠を返す。blocking クロージャへ move して使う。
#[derive(Debug)]
pub(crate) struct HostPermit(());

impl Drop for HostPermit {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 同時実行枠を確保する。上限に達していれば `None`（呼び出し側は isError で返す）。
pub(crate) fn try_acquire() -> Option<HostPermit> {
    // fetch_update は新しい toolchain で非推奨（try_update へ改名）のため、
    // toolchain 差に依存しない compare_exchange_weak ループで実装する。
    let mut current = IN_FLIGHT.load(Ordering::Acquire);
    loop {
        if current >= MAX_CONCURRENT_REQUESTS {
            return None;
        }
        match IN_FLIGHT.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Some(HostPermit(())),
            Err(actual) => current = actual,
        }
    }
}

/// ホスト呼び出しの失敗。MCP 応答へは固定の英語文言（`Display`）のみ流す。
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum HostError {
    InvalidAddr,
    NonLoopbackAddr,
    Connect,
    Io,
    Timeout,
    ResponseTooLarge,
    MalformedResponse,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidAddr => "invalid host address",
            Self::NonLoopbackAddr => "host address must be a loopback IP address",
            Self::Connect => "failed to connect to host",
            Self::Io => "host I/O error",
            Self::Timeout => "host request timed out",
            Self::ResponseTooLarge => "host response too large",
            Self::MalformedResponse => "malformed host response",
        })
    }
}

/// ホストの応答（ステータスコードと本文）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HostResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// 接続先を解決する。`None` は既定値。IP:port 形式の loopback のみ受理する。
pub(crate) fn resolve_host_addr(raw: Option<&str>) -> Result<SocketAddr, HostError> {
    let addr: SocketAddr = raw
        .unwrap_or(DEFAULT_HOST_ADDR)
        .parse()
        .map_err(|_| HostError::InvalidAddr)?;
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(HostError::NonLoopbackAddr)
    }
}

/// 環境変数から接続先を解決する（呼び出しごとに読む）。
pub(crate) fn resolve_from_env() -> Result<SocketAddr, HostError> {
    match std::env::var(HOST_ADDR_ENV) {
        Ok(v) => resolve_host_addr(Some(&v)),
        Err(std::env::VarError::NotPresent) => resolve_host_addr(None),
        Err(_) => Err(HostError::InvalidAddr),
    }
}

/// 要求メッセージを組み立てる。ヘッダは定数と検証済みアドレスのみで構成する。
/// `body` が `Some` なら JSON 本文付き（POST 用）、`None` なら本文なし（GET 用）。
fn build_request(addr: SocketAddr, method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let mut msg = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            b.len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    }
    .into_bytes();
    if let Some(b) = body {
        msg.extend_from_slice(b);
    }
    msg
}

/// 応答バイト列をステータスと本文へ分解する。
///
/// 制約: `Transfer-Encoding: chunked` はデコードしない。chunked 応答の本文は JSON として
/// 解析できず失敗扱いになる（誤って成功を装うことはない安全側の挙動）。ホストは
/// `Content-Length` 付きで返す前提（将来 chunked 対応する場合は PLUG-3 の拡張として追加）。
fn parse_response(raw: &[u8]) -> Result<HostResponse, HostError> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(HostError::MalformedResponse)?;
    let head = raw.get(..sep).ok_or(HostError::MalformedResponse)?;
    let body = raw.get(sep + 4..).ok_or(HostError::MalformedResponse)?;
    let head = std::str::from_utf8(head).map_err(|_| HostError::MalformedResponse)?;
    let line = head.lines().next().ok_or(HostError::MalformedResponse)?;
    let rest = line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .ok_or(HostError::MalformedResponse)?;
    let code = rest.split(' ').next().unwrap_or_default();
    let status: u16 = code.parse().map_err(|_| HostError::MalformedResponse)?;
    // 応答の完全性: Content-Length は重複不可・本文長と一致必須、Transfer-Encoding は未対応で拒否。
    let mut declared: Option<usize> = None;
    for h in head.lines().skip(1) {
        // コロンのないヘッダ行は不正応答として拒否する（従来の防御を維持）。
        let Some((name, value)) = h.split_once(':') else {
            return Err(HostError::MalformedResponse);
        };
        if name.trim().eq_ignore_ascii_case("transfer-encoding") {
            return Err(HostError::MalformedResponse);
        }
        if name.trim().eq_ignore_ascii_case("content-length") {
            let v = value.trim();
            if declared.is_some() || v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HostError::MalformedResponse);
            }
            declared = Some(v.parse().map_err(|_| HostError::MalformedResponse)?);
        }
    }
    if declared.is_some_and(|n| n != body.len()) {
        return Err(HostError::MalformedResponse);
    }
    Ok(HostResponse {
        status,
        body: body.to_vec(),
    })
}

fn map_io(e: &std::io::Error) -> HostError {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HostError::Timeout,
        _ => HostError::Io,
    }
}

/// JSON 本文を `path` へ POST し応答を返す（ブロッキング。`spawn_blocking` から呼ぶ）。
pub(crate) fn post_json(
    addr: SocketAddr,
    path: &'static str,
    body: &[u8],
) -> Result<HostResponse, HostError> {
    request(addr, "POST", path, Some(body), MAX_RESPONSE_BYTES)
}

/// `path` へ GET し応答を返す（ブロッキング）。応答上限は呼び出し側が指定する（snapshot 用）。
pub(crate) fn get(
    addr: SocketAddr,
    path: &'static str,
    max_response_bytes: usize,
) -> Result<HostResponse, HostError> {
    request(addr, "GET", path, None, max_response_bytes)
}

fn request(
    addr: SocketAddr,
    method: &'static str,
    path: &'static str,
    body: Option<&[u8]>,
    max_response_bytes: usize,
) -> Result<HostResponse, HostError> {
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    let mut stream =
        TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|_| HostError::Connect)?;
    stream
        .set_write_timeout(Some(TOTAL_TIMEOUT))
        .map_err(|_| HostError::Io)?;
    stream
        .write_all(&build_request(addr, method, path, body))
        .map_err(|e| map_io(&e))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HostError::Timeout);
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| HostError::Io)?;
        let n = stream.read(&mut buf).map_err(|e| map_io(&e))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(buf.get(..n).ok_or(HostError::Io)?);
        if raw.len() > max_response_bytes {
            return Err(HostError::ResponseTooLarge);
        }
    }
    parse_response(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PLUG-3 / TASK-94.3: 既定値と loopback（v4 / v6）を受理する。
    #[test]
    fn plug3_resolve_accepts_default_and_loopback() {
        assert_eq!(
            resolve_host_addr(None),
            Ok("127.0.0.1:9333".parse().unwrap())
        );
        assert!(resolve_host_addr(Some("[::1]:8080")).is_ok());
    }

    /// PLUG-3 / TASK-94.3: 非 loopback・ホスト名・URL・空は拒否する。
    #[test]
    fn plug3_resolve_rejects_non_loopback_and_malformed() {
        assert_eq!(
            resolve_host_addr(Some("192.168.0.1:80")),
            Err(HostError::NonLoopbackAddr)
        );
        for bad in ["localhost:9333", "http://127.0.0.1:9333", "", "127.0.0.1"] {
            assert_eq!(resolve_host_addr(Some(bad)), Err(HostError::InvalidAddr));
        }
    }

    /// PLUG-3 / TASK-94.3: 要求メッセージの形。
    #[test]
    fn plug3_request_shape() {
        let msg = build_request(
            "127.0.0.1:1".parse().unwrap(),
            "POST",
            "/ai/navigate",
            Some(b"{}"),
        );
        assert_eq!(
            String::from_utf8(msg).unwrap(),
            "POST /ai/navigate HTTP/1.1\r\nHost: 127.0.0.1:1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
        );
    }

    /// PLUG-3 / TASK-94.3: 応答の分解（正常・不正）。
    #[test]
    fn plug3_parse_response() {
        let r = parse_response(b"HTTP/1.1 502 Bad Gateway\r\nX: y\r\n\r\n{\"ok\":false}").unwrap();
        assert_eq!(r.status, 502);
        assert_eq!(r.body, b"{\"ok\":false}");
        for bad in [
            &b""[..],
            b"HTTP/1.1 200 OK",
            b"FOO 200 OK\r\n\r\n",
            b"HTTP/1.1 abc\r\n\r\n",
            // コロンのないヘッダ行（例: Content-Length 999）は拒否する。
            b"HTTP/1.1 200 OK\r\nContent-Length 999\r\n\r\n{\"ok\":true}",
        ] {
            assert_eq!(parse_response(bad), Err(HostError::MalformedResponse));
        }
    }

    /// PLUG-4 / TASK-94.4: GET 要求は本文・Content-Length・Content-Type を持たない。
    #[test]
    fn plug4_get_request_shape() {
        let msg = build_request("127.0.0.1:1".parse().unwrap(), "GET", "/ai/snapshot", None);
        assert_eq!(
            String::from_utf8(msg).unwrap(),
            "GET /ai/snapshot HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n"
        );
    }

    /// PLUG-4 / TASK-94.4: Content-Length 検証（一致は成功、不一致・重複・不正値は拒否）。
    #[test]
    fn plug4_parse_response_validates_content_length() {
        let ok = parse_response(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}").unwrap();
        assert_eq!(ok.body, b"{}");
        for bad in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nContent-Length: -2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
        ] {
            assert_eq!(parse_response(bad), Err(HostError::MalformedResponse));
        }
    }

    /// PLUG-4 / TASK-94.4: 同時実行枠は上限で打ち切られ、drop で返却される。
    #[test]
    fn plug4_permits_are_bounded_and_released() {
        let held: Vec<_> = std::iter::from_fn(try_acquire).collect();
        assert_eq!(held.len(), MAX_CONCURRENT_REQUESTS);
        assert!(try_acquire().is_none());
        drop(held);
        assert!(try_acquire().is_some());
    }
}

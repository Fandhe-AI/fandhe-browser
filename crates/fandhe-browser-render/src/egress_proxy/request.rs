//! CONNECT 要求ヘッダの受信と厳格な解析（RENDER-3・SEC-2・TASK-38・#1434）。
//!
//! [`super::server`] のハンドラから呼ばれる。要求行は全体一致で検査し、単独の LF・CR・
//! ヘッダ終端後の余剰バイト・userinfo 付き authority 等を拒否して、リクエストスマグリング
//! やインジェクションの余地を残さない（security.md「インジェクション」）。外部入力の経路の
//! ため `unwrap`・`expect`・添字アクセスは使わない。2 行目以降のヘッダは形式だけ検査して
//! 読み捨てる（上流へ転送しない。識別面の書き換えをしない。SEC-2）。

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 要求を拒否する理由。ハンドラが HTTP ステータスとカウンタへ写像する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reject {
    /// 形式不正（400）。
    BadRequest,
    /// `CONNECT` 以外のメソッド（405）。
    MethodNotAllowed,
    /// ヘッダが上限超過（431）。
    HeaderTooLarge,
    /// ヘッダ受信の期限超過（408）。
    Deadline,
    /// プロキシ停止中（応答なしで閉じる）。
    Stopped,
}

/// CONNECT の宛先ホスト。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Host {
    Ip(IpAddr),
    Name(String),
}

/// 解析済みの CONNECT 宛先。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConnectTarget {
    pub(super) host: Host,
    pub(super) port: u16,
}

const TERMINATOR: &[u8] = b"\r\n\r\n";
const READ_CHUNK: usize = 1024;
const MAX_HOST_BYTES: usize = 253;

fn find_terminator(buf: &[u8]) -> Option<usize> {
    buf.windows(TERMINATOR.len()).position(|w| w == TERMINATOR)
}

/// 単独の LF、または LF 以外が続く CR があるか（末尾の CR は後続待ちなので無視）。
fn has_bare_newline(buf: &[u8]) -> bool {
    buf.iter().enumerate().any(|(i, &b)| match b {
        b'\n' => i == 0 || buf.get(i - 1) != Some(&b'\r'),
        b'\r' => buf.get(i + 1).is_some_and(|&n| n != b'\n'),
        _ => false,
    })
}

/// ヘッダ終端までを期限・上限つきで読む。`set_timeout` は読み取り timeout の設定
/// （ハンドラが `Conn::set_read_timeout` を渡す）。停止フラグは `tick` ごとに確認する。
pub(super) fn read_request_head<R: Read>(
    reader: &mut R,
    set_timeout: &mut dyn FnMut(Duration) -> io::Result<()>,
    deadline: Instant,
    tick: Duration,
    max_bytes: usize,
    stop: &AtomicBool,
) -> Result<Vec<u8>, Reject> {
    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(READ_CHUNK));
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Reject::Stopped);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Reject::Deadline);
        }
        set_timeout(remaining.min(tick)).map_err(|_| Reject::BadRequest)?;
        match reader.read(&mut chunk) {
            Ok(0) => return Err(Reject::BadRequest),
            Ok(n) => {
                if let Some(read) = chunk.get(..n) {
                    buf.extend_from_slice(read);
                }
                if has_bare_newline(&buf) {
                    return Err(Reject::BadRequest);
                }
                if find_terminator(&buf).is_some() {
                    return Ok(buf);
                }
                if buf.len() > max_bytes {
                    return Err(Reject::HeaderTooLarge);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Reject::BadRequest),
        }
    }
}

/// RFC 9110 の tchar。
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_tchar)
}

/// 受信済みのヘッダ塊を解析して CONNECT 宛先を返す（純粋関数）。
pub(super) fn parse_connect(buf: &[u8], max_bytes: usize) -> Result<ConnectTarget, Reject> {
    let pos = find_terminator(buf).ok_or(Reject::BadRequest)?;
    let end = pos + TERMINATOR.len();
    if end > max_bytes {
        return Err(Reject::HeaderTooLarge);
    }
    // ヘッダ終端後の余剰バイトは拒否する（トンネル開始前に届いた先走りデータを黙って
    // 転送しない。スマグリング対策）。
    if buf.len() > end {
        return Err(Reject::BadRequest);
    }
    let head = buf.get(..pos).ok_or(Reject::BadRequest)?;
    if has_bare_newline(head) {
        return Err(Reject::BadRequest);
    }
    let head = std::str::from_utf8(head).map_err(|_| Reject::BadRequest)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(Reject::BadRequest)?;
    if !request_line.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(Reject::BadRequest);
    }
    let parts: Vec<&str> = request_line.split(' ').collect();
    let [method, target, version] = parts.as_slice() else {
        return Err(Reject::BadRequest);
    };
    if !is_token(method) || target.is_empty() || *version != "HTTP/1.1" {
        return Err(Reject::BadRequest);
    }
    if *method != "CONNECT" {
        return Err(Reject::MethodNotAllowed);
    }
    let parsed = parse_authority(target)?;
    for line in lines {
        validate_header_line(line)?;
    }
    Ok(parsed)
}

fn validate_header_line(line: &str) -> Result<(), Reject> {
    if line.starts_with([' ', '\t']) {
        return Err(Reject::BadRequest);
    }
    let (name, value) = line.split_once(':').ok_or(Reject::BadRequest)?;
    if !is_token(name) {
        return Err(Reject::BadRequest);
    }
    if !value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
    {
        return Err(Reject::BadRequest);
    }
    Ok(())
}

fn parse_port(s: &str) -> Result<u16, Reject> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Reject::BadRequest);
    }
    match s.parse::<u16>() {
        Ok(p) if p != 0 => Ok(p),
        _ => Err(Reject::BadRequest),
    }
}

fn parse_authority(s: &str) -> Result<ConnectTarget, Reject> {
    if let Some(rest) = s.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']').ok_or(Reject::BadRequest)?;
        let port = tail.strip_prefix(':').ok_or(Reject::BadRequest)?;
        let v6: Ipv6Addr = inner.parse().map_err(|_| Reject::BadRequest)?;
        return Ok(ConnectTarget {
            host: Host::Ip(IpAddr::V6(v6)),
            port: parse_port(port)?,
        });
    }
    let (host, port) = s.rsplit_once(':').ok_or(Reject::BadRequest)?;
    let port = parse_port(port)?;
    Ok(ConnectTarget {
        host: parse_host(host)?,
        port,
    })
}

fn parse_host(host: &str) -> Result<Host, Reject> {
    if host.is_empty()
        || host.len() > MAX_HOST_BYTES
        || host.starts_with('.')
        || host.contains("..")
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
    {
        return Err(Reject::BadRequest);
    }
    // 数字とドットだけの表記は厳密な IPv4 リテラルに限る（`127.1` のような省略形を
    // リゾルバに解釈させない）。
    if host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        let v4: Ipv4Addr = host.parse().map_err(|_| Reject::BadRequest)?;
        return Ok(Host::Ip(IpAddr::V4(v4)));
    }
    Ok(Host::Name(host.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 8192;

    fn parse(s: &str) -> Result<ConnectTarget, Reject> {
        parse_connect(s.as_bytes(), MAX)
    }

    fn read_all(data: Vec<u8>) -> Result<Vec<u8>, Reject> {
        let mut cur = std::io::Cursor::new(data);
        read_request_head(
            &mut cur,
            &mut |_| Ok(()),
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(50),
            MAX,
            &AtomicBool::new(false),
        )
    }

    /// RENDER-3: 正常な CONNECT（名前）。
    #[test]
    fn render_3_parses_name_target() {
        let t = parse("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n").unwrap();
        assert_eq!(t.host, Host::Name("example.com".to_owned()));
        assert_eq!(t.port, 443);
    }

    /// RENDER-3: ポートは制限しない。IPv4・IPv6 リテラル。
    #[test]
    fn render_3_parses_ip_literals_any_port() {
        let t = parse("CONNECT 93.184.216.34:8080 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(t.host, Host::Ip("93.184.216.34".parse().unwrap()));
        assert_eq!(t.port, 8080);
        let t = parse("CONNECT [2001:db8::1]:443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(t.host, Host::Ip("2001:db8::1".parse().unwrap()));
        assert_eq!(t.port, 443);
    }

    /// RENDER-3: CONNECT 以外は 405（小文字の `connect` も token として妥当なので 405）。
    #[test]
    fn render_3_non_connect_is_405() {
        assert_eq!(
            parse("GET http://x/ HTTP/1.1\r\n\r\n"),
            Err(Reject::MethodNotAllowed)
        );
        assert_eq!(
            parse("connect x:443 HTTP/1.1\r\n\r\n"),
            Err(Reject::MethodNotAllowed)
        );
    }

    /// SEC-2: 版・改行・空白の厳格検査。
    #[test]
    fn sec_2_rejects_malformed_request_line() {
        for bad in [
            "CONNECT a:443 HTTP/1.0\r\n\r\n",
            "CONNECT a:443\n HTTP/1.1\r\n\r\n",
            "CONNECT a:443 HTTP/1.1\n\r\n\r\n",
            "CONNECT a:443 HTTP/1.1\r\r\n\r\n",
            "CONNECT  a:443 HTTP/1.1\r\n\r\n",
            " CONNECT a:443 HTTP/1.1\r\n\r\n",
            "CONNECT a:443 HTTP/1.1 \r\n\r\n",
            "CONNECT a:443\tHTTP/1.1\r\n\r\n",
            "CONNECT a:443\r\n\r\n",
            "CONNECT a:443 HTTP/1.1\r\nbad header\r\n\r\n",
            "CONNECT a:443 HTTP/1.1\r\n folded: x\r\n\r\n",
        ] {
            assert_eq!(parse(bad), Err(Reject::BadRequest), "{bad:?}");
        }
    }

    /// SEC-2: authority の検査。
    #[test]
    fn sec_2_rejects_bad_authority() {
        for bad in [
            "a",
            "a:",
            "a:0",
            "a:65536",
            "a:+443",
            "user@a:443",
            "a:443/path",
            "[fe80::1%eth0]:443",
            ":443",
            "é.example:443",
            "127.1:443",
            "1.2.3.4.5:443",
            ".a:443",
            "a..b:443",
            "[::1]443",
            "[::1:443",
        ] {
            let req = format!("CONNECT {bad} HTTP/1.1\r\n\r\n");
            assert_eq!(parse(&req), Err(Reject::BadRequest), "{bad:?}");
        }
    }

    /// SEC-2: ヘッダ終端後の余剰バイトは拒否する。
    #[test]
    fn sec_2_rejects_trailing_bytes() {
        assert_eq!(
            parse("CONNECT a:443 HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\n\r\n"),
            Err(Reject::BadRequest)
        );
    }

    /// RENDER-3: 上限超過は `HeaderTooLarge`（解析時・読み取り時の両方）。
    #[test]
    fn render_3_header_too_large() {
        let big = format!("CONNECT a:443 HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX));
        assert_eq!(parse(&big), Err(Reject::HeaderTooLarge));
        assert_eq!(read_all(vec![b'a'; MAX * 2]), Err(Reject::HeaderTooLarge));
    }

    /// SEC-2: 読み取りは単独 LF を終端待ちせず即座に 400 にする。
    #[test]
    fn sec_2_reader_rejects_bare_lf_early() {
        assert_eq!(
            read_all(b"CONNECT a:443 HTTP/1.1\n".to_vec()),
            Err(Reject::BadRequest)
        );
    }
}

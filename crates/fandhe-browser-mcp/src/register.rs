//! ホストへの起動時自己申告（TASK-94.5・PLUG-2・PLUG-3・MS-9）。
//!
//! `main.rs` が MCP セッション開始より前に `register_with_host` を 1 回呼び、ホスト
//! （`fandhe-browser-cli` が合成する `/ai/*` ルータ）の `POST /ai/plugins/register` へ
//! マニフェストを送る。ホスト側の契約の正本は `docs/design/host-api.schema.json`。
//! PLUG-1 により他 crate には依存せず、依存追加も避けるため `std::net` による
//! 1 回限りの HTTP/1.1 POST を自前で実装する（応答は untrusted として扱う）。
//!
//! 接続先は環境変数 `FANDHE_BROWSER_HOST_ADDR`（`ip:port`）で指定し、未設定時は
//! `127.0.0.1:9333`。SSRF 防止のため loopback の IP リテラルのみ許可し、DNS 解決はしない。

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use rmcp::serde_json::{self, Value, json};

use crate::server::SERVER_PROTOCOL_VERSION;

/// ホストのアドレスを指定する環境変数名。
pub(crate) const HOST_ADDR_ENV: &str = "FANDHE_BROWSER_HOST_ADDR";
/// 環境変数が未設定のときの接続先（cli の既定 bind アドレスと同じ）。
pub(crate) const DEFAULT_HOST_ADDR: &str = "127.0.0.1:9333";
/// 登録エンドポイントのパス。
const REGISTER_PATH: &str = "/ai/plugins/register";
/// マニフェストで申告するツール名。現時点では提供ツールが無いため空で、空の間は
/// 自己申告を延期する（`RegisterOutcome::Deferred`）。ホスト側スキーマは `tools` に 1 件以上を
/// 要求する（`minItems: 1`）ため、未実装の navigate / snapshot を暫定申告すると存在しない
/// ツールが提供機能として公開される（REPAIR-3 違反）。将来仕様: TASK-94.3（navigate）・
/// TASK-94.4（snapshot）で実装したツール名をここへ追加した時点で自己申告が有効になる。
/// 実装との整合は TASK-94.6 で検証する。
pub(crate) const DECLARED_TOOLS: [&str; 0] = [];
/// 応答の読み取り上限（バイト）。
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
/// 接続タイムアウト。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// 登録処理全体の期限。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// 登録失敗の理由。Display は固定の英語文言で、ホスト応答の本文は出さない。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum RegisterError {
    /// アドレスが `ip:port` として解析できない（空・非 UTF-8 を含む）。
    InvalidAddr,
    /// loopback 以外のアドレス。接続しない。
    NonLoopbackAddr,
    /// 接続できなかった。
    Connect(io::Error),
    /// 送受信中の I/O エラー。
    Io(io::Error),
    /// 期限内に完了しなかった。
    Timeout,
    /// 応答が上限を超えた。
    ResponseTooLarge,
    /// 応答が HTTP として不正。
    MalformedResponse,
    /// ホストが登録を拒否した。`code` は安全な文字種のときのみ保持する。
    Rejected { status: u16, code: Option<String> },
    /// 200 だが本文が登録成功の形でない。
    UnexpectedBody,
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAddr => write!(
                f,
                "invalid host address in {HOST_ADDR_ENV} (expected ip:port)"
            ),
            Self::NonLoopbackAddr => {
                write!(f, "host address must be a loopback address")
            }
            Self::Connect(e) => write!(f, "cannot connect to host: {e}"),
            Self::Io(e) => write!(f, "I/O error while talking to host: {e}"),
            Self::Timeout => write!(f, "timed out waiting for host"),
            Self::ResponseTooLarge => write!(f, "host response too large"),
            Self::MalformedResponse => write!(f, "malformed host response"),
            Self::Rejected { status, code } => match code {
                Some(code) => write!(f, "host rejected registration: status {status} ({code})"),
                None => write!(f, "host rejected registration: status {status}"),
            },
            Self::UnexpectedBody => write!(f, "unexpected host response body"),
        }
    }
}

/// 登録の成功結果。真偽値にせず将来の拡張余地を残す（REPAIR-4）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegisterOutcome {
    /// 新規に登録された。
    Registered,
    /// ホストに同 id が既に登録済み（`duplicate_plugin_id`）。ホストのレジストリは
    /// 上書き・削除 API を持たないため、プラグインだけの再起動で必ず起きる。
    /// ホスト側のマニフェストが旧版の可能性がある（暫定扱い。REPAIR-3）。
    AlreadyRegistered,
    /// 提供ツールが未実装のため自己申告を行わなかった（接続もしない。REPAIR-3）。
    Deferred,
}

/// 環境変数の生値から接続先を決める（純関数）。loopback の `ip:port` のみ許可する。
pub(crate) fn resolve_host_addr(raw: Option<&str>) -> Result<SocketAddr, RegisterError> {
    let addr: SocketAddr = raw
        .unwrap_or(DEFAULT_HOST_ADDR)
        .parse()
        .map_err(|_| RegisterError::InvalidAddr)?;
    if !addr.ip().is_loopback() {
        return Err(RegisterError::NonLoopbackAddr);
    }
    Ok(addr)
}

/// 自己申告するマニフェスト（`host-api.schema.json` の PluginManifest）。
/// `runtime` の値は TASK-98.2 の判定規則が未実装のため暫定（PLUG-6）。
/// 指定したツール名一覧でマニフェストを組み立てる（ツールが 1 件以上のときのみ送る）。
fn manifest_for(tools: &[&str]) -> Value {
    json!({
        "id": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "stdio",
        "tools": tools,
        "permissions": ["network.fetch", "dom.read"],
        "protocolVersion": SERVER_PROTOCOL_VERSION.as_str(),
        "runtime": "native",
        "language": "rust",
    })
}

/// 登録要求の HTTP/1.1 メッセージを組み立てる。ヘッダは定数と検証済みアドレスのみ。
/// `Origin` は付けない（ホストは Origin 付きを 403 にする）。
fn build_request(addr: SocketAddr, body: &[u8]) -> Vec<u8> {
    let mut req = format!(
        "POST {REGISTER_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    req
}

/// 応答からステータスコードと本文を取り出す。
fn parse_response(raw: &[u8]) -> Result<(u16, &[u8]), RegisterError> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(RegisterError::MalformedResponse)?;
    let head = raw.get(..sep).ok_or(RegisterError::MalformedResponse)?;
    let body = raw.get(sep + 4..).ok_or(RegisterError::MalformedResponse)?;
    let head = std::str::from_utf8(head).map_err(|_| RegisterError::MalformedResponse)?;
    let line = head
        .lines()
        .next()
        .ok_or(RegisterError::MalformedResponse)?;
    let rest = line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .ok_or(RegisterError::MalformedResponse)?;
    let status = rest
        .split(' ')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(RegisterError::MalformedResponse)?;
    Ok((status, body))
}

/// 本文 JSON の `code` を、安全な文字種（`[a-z_]{1,64}`）のときのみ返す（ログ注入防止）。
fn safe_code(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let code = v.get("code")?.as_str()?;
    let ok = !code.is_empty()
        && code.len() <= 64
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    ok.then(|| code.to_owned())
}

/// ステータスと本文から登録結果を判定する。200 でも `ok == true` かつ id 一致を要求する。
fn interpret(status: u16, body: &[u8]) -> Result<RegisterOutcome, RegisterError> {
    match status {
        200 => {
            let v: Value =
                serde_json::from_slice(body).map_err(|_| RegisterError::UnexpectedBody)?;
            let ok = v.get("ok").and_then(Value::as_bool) == Some(true)
                && v.get("id").and_then(Value::as_str) == Some(env!("CARGO_PKG_NAME"));
            if ok {
                Ok(RegisterOutcome::Registered)
            } else {
                Err(RegisterError::UnexpectedBody)
            }
        }
        409 if safe_code(body).as_deref() == Some("duplicate_plugin_id") => {
            Ok(RegisterOutcome::AlreadyRegistered)
        }
        _ => Err(RegisterError::Rejected {
            status,
            code: safe_code(body),
        }),
    }
}

/// 応答全体（EOF まで）を上限付きで読む。`deadline` を累積の締切とし、各 read の前に
/// 残り時間で `rearm`（read timeout の再設定）を呼ぶ。締切超過は `Timeout`。
/// 1 バイトずつ遅延送信されても全体期限で打ち切るため（read timeout は 1 回あたりの上限）。
fn read_limited(
    r: &mut impl Read,
    max: usize,
    deadline: Instant,
    mut rearm: impl FnMut(Duration) -> io::Result<()>,
) -> Result<Vec<u8>, RegisterError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(RegisterError::Timeout)?;
        rearm(left).map_err(RegisterError::Io)?;
        let n = match r.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(RegisterError::Timeout);
            }
            Err(e) => return Err(RegisterError::Io(e)),
        };
        if n == 0 {
            return Ok(buf);
        }
        let part = chunk.get(..n).ok_or(RegisterError::MalformedResponse)?;
        buf.extend_from_slice(part);
        if buf.len() > max {
            return Err(RegisterError::ResponseTooLarge);
        }
    }
}

/// ホストへ自己申告する。`main` が MCP セッション開始前に 1 回だけ呼ぶ。
/// 提供ツールが無い間は何もせず `Deferred` を返す（接続も環境変数の解釈もしない）。
/// リダイレクト追従・再試行はしない。
pub(crate) fn register_with_host() -> Result<RegisterOutcome, RegisterError> {
    if DECLARED_TOOLS.is_empty() {
        return Ok(RegisterOutcome::Deferred);
    }
    register_tools(&DECLARED_TOOLS)
}

/// `tools` を申告するマニフェストを、環境変数で指定されたホストへ送る。
fn register_tools(tools: &[&str]) -> Result<RegisterOutcome, RegisterError> {
    let raw = match std::env::var(HOST_ADDR_ENV) {
        Ok(v) => Some(v),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => return Err(RegisterError::InvalidAddr),
    };
    let addr = resolve_host_addr(raw.as_deref())?;
    register_to(addr, tools)
}

/// `addr` のホストへ `tools` を申告するマニフェストを 1 回 POST する。
fn register_to(addr: SocketAddr, tools: &[&str]) -> Result<RegisterOutcome, RegisterError> {
    let started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| {
        if e.kind() == io::ErrorKind::TimedOut {
            RegisterError::Timeout
        } else {
            RegisterError::Connect(e)
        }
    })?;
    let remaining = TOTAL_TIMEOUT
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or(RegisterError::Timeout)?;
    // 書き込みと最初の read は残り時間で頭打ちにし、以降の read は read_limited が再設定する。
    stream
        .set_read_timeout(Some(remaining))
        .map_err(RegisterError::Io)?;
    stream
        .set_write_timeout(Some(remaining))
        .map_err(RegisterError::Io)?;
    let body =
        serde_json::to_vec(&manifest_for(tools)).map_err(|_| RegisterError::UnexpectedBody)?;
    stream
        .write_all(&build_request(addr, &body))
        .map_err(RegisterError::Io)?;
    let deadline = started + TOTAL_TIMEOUT;
    let reader = stream.try_clone().map_err(RegisterError::Io)?;
    let mut rd = &stream;
    let resp = read_limited(&mut rd, MAX_RESPONSE_BYTES, deadline, |d| {
        reader.set_read_timeout(Some(d))
    })?;
    let (status, body) = parse_response(&resp)?;
    interpret(status, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = include_str!("../../../docs/design/host-api.schema.json");

    /// PLUG-2 / TASK-94.5: 未設定は既定アドレス、loopback の IPv4/IPv6 は受理する。
    #[test]
    fn plug2_resolve_addr_accepts_loopback() {
        assert_eq!(
            resolve_host_addr(None).expect("default").to_string(),
            "127.0.0.1:9333"
        );
        assert_eq!(
            resolve_host_addr(Some("[::1]:9333"))
                .expect("v6")
                .to_string(),
            "[::1]:9333"
        );
    }

    /// PLUG-2 / TASK-94.5: 非 loopback は NonLoopbackAddr、ホスト名・URL・空は InvalidAddr。
    #[test]
    fn plug2_resolve_addr_rejects_bad_values() {
        for raw in ["192.0.2.1:80", "0.0.0.0:9333"] {
            assert!(matches!(
                resolve_host_addr(Some(raw)),
                Err(RegisterError::NonLoopbackAddr)
            ));
        }
        for raw in ["localhost:9333", "http://127.0.0.1:9333", ""] {
            assert!(matches!(
                resolve_host_addr(Some(raw)),
                Err(RegisterError::InvalidAddr)
            ));
        }
    }

    /// PLUG-3 / TASK-94.5: マニフェストの各キーが具体値で、スキーマ契約に適合する。
    #[test]
    fn plug3_manifest_values_match_schema() {
        let m = manifest_for(&["navigate", "snapshot"]);
        assert_eq!(m["id"], "fandhe-browser-mcp");
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(m["transport"], "stdio");
        assert_eq!(m["tools"], json!(["navigate", "snapshot"]));
        assert_eq!(m["permissions"], json!(["network.fetch", "dom.read"]));
        assert_eq!(m["protocolVersion"], "2025-11-25");
        assert_eq!(m["runtime"], "native");
        assert_eq!(m["language"], "rust");

        let schema: Value = serde_json::from_str(SCHEMA).expect("schema json");
        let def = &schema["$defs"]["PluginManifest"];
        let props = def["properties"].as_object().expect("properties");
        let obj = m.as_object().expect("object");
        for k in obj.keys() {
            assert!(props.contains_key(k), "unknown manifest key {k}");
        }
        for r in def["required"].as_array().expect("required") {
            let k = r.as_str().expect("str");
            assert!(obj.contains_key(k), "missing required key {k}");
        }
        let transports = props["transport"]["enum"].as_array().expect("enum");
        assert!(transports.contains(&m["transport"]));
        let perms = props["permissions"]["items"]["enum"]
            .as_array()
            .expect("enum");
        for p in m["permissions"].as_array().expect("perms") {
            assert!(perms.contains(p), "permission {p} not in schema");
        }
    }

    /// PLUG-2 / TASK-94.5: 要求バイト列の先頭行・ヘッダ・Content-Length、Origin 不在。
    #[test]
    fn plug2_build_request_format() {
        let addr: SocketAddr = "127.0.0.1:9333".parse().expect("addr");
        let req = String::from_utf8(build_request(addr, b"{\"a\":1}")).expect("utf8");
        assert!(req.starts_with("POST /ai/plugins/register HTTP/1.1\r\n"));
        assert!(req.contains("\r\nHost: 127.0.0.1:9333\r\n"));
        assert!(req.contains("\r\nContent-Type: application/json\r\n"));
        assert!(req.contains("\r\nContent-Length: 7\r\n"));
        assert!(!req.to_ascii_lowercase().contains("origin"));
        assert!(req.ends_with("\r\n\r\n{\"a\":1}"));
    }

    fn resp(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn run(status: u16, body: &str) -> Result<RegisterOutcome, RegisterError> {
        let raw = resp(status, body);
        let (s, b) = parse_response(&raw)?;
        interpret(s, b)
    }

    /// PLUG-2 / TASK-94.5: 200 + 正しい本文は Registered、409 duplicate は AlreadyRegistered。
    #[test]
    fn plug2_interpret_success_cases() {
        assert_eq!(
            run(200, r#"{"ok":true,"id":"fandhe-browser-mcp"}"#).expect("ok"),
            RegisterOutcome::Registered
        );
        assert_eq!(
            run(409, r#"{"code":"duplicate_plugin_id","message":"x"}"#).expect("dup"),
            RegisterOutcome::AlreadyRegistered
        );
    }

    /// PLUG-2 / TASK-94.5: それ以外のステータスは Rejected（status と code を保持）。
    #[test]
    fn plug2_interpret_rejections() {
        for (st, body, code) in [
            (409, r#"{"code":"other"}"#, Some("other")),
            (400, r#"{"code":"invalid_id"}"#, Some("invalid_id")),
            (403, "{}", None),
            (415, "", None),
            (
                429,
                r#"{"code":"plugin_registry_full"}"#,
                Some("plugin_registry_full"),
            ),
            (500, "oops", None),
            (302, "", None),
        ] {
            match run(st, body) {
                Err(RegisterError::Rejected { status, code: c }) => {
                    assert_eq!(status, st);
                    assert_eq!(c.as_deref(), code);
                }
                other => panic!("status {st}: {other:?}"),
            }
        }
    }

    /// PLUG-2 / TASK-94.5: 200 でも ok 欠落・id 不一致・非 JSON は UnexpectedBody。
    #[test]
    fn plug2_interpret_unexpected_200_bodies() {
        for body in [
            "{}",
            r#"{"ok":true,"id":"other"}"#,
            r#"{"ok":false,"id":"fandhe-browser-mcp"}"#,
            "not json",
        ] {
            assert!(
                matches!(run(200, body), Err(RegisterError::UnexpectedBody)),
                "{body}"
            );
        }
    }

    /// PLUG-2 / TASK-94.5: 不正な code（大文字・制御文字・長すぎ）は None、不正応答は MalformedResponse。
    #[test]
    fn plug2_unsafe_code_dropped_and_malformed_detected() {
        for body in [
            r#"{"code":"Bad"}"#.to_owned(),
            "{\"code\":\"a\\u001bb\"}".to_owned(),
            format!(r#"{{"code":"{}"}}"#, "a".repeat(65)),
        ] {
            match run(400, &body) {
                Err(RegisterError::Rejected { code, .. }) => assert_eq!(code, None),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            parse_response(b"garbage\r\n\r\n"),
            Err(RegisterError::MalformedResponse)
        ));
        assert!(matches!(
            parse_response(b"HTTP/1.1 200 OK\r\n"),
            Err(RegisterError::MalformedResponse)
        ));
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// PLUG-2 / TASK-94.5: 応答が上限を超えたら ResponseTooLarge。
    #[test]
    fn plug2_read_limited_rejects_oversized() {
        let data = vec![b'a'; 100];
        assert!(matches!(
            read_limited(&mut data.as_slice(), 50, far(), |_| Ok(())),
            Err(RegisterError::ResponseTooLarge)
        ));
        assert_eq!(
            read_limited(&mut data.as_slice(), 100, far(), |_| Ok(()))
                .expect("ok")
                .len(),
            100
        );
    }

    /// 1 バイトずつ遅延して返す reader（遅延送信ホストの模擬）。
    struct Drip;
    impl Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(20));
            if let Some(b) = buf.first_mut() {
                *b = b'a';
            }
            Ok(1)
        }
    }

    /// PLUG-2 / TASK-94.5: 1 read ごとは短くても累積締切を超えたら Timeout（上限到達前）。
    #[test]
    fn plug2_read_limited_enforces_total_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(100);
        let r = read_limited(&mut Drip, MAX_RESPONSE_BYTES, deadline, |_| Ok(()));
        assert!(matches!(r, Err(RegisterError::Timeout)), "{r:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// loopback の空きポートで 1 接続だけ受け、受信した要求全体を返しつつ `status` / `body` を応答する。
    fn fake_host(status: u16, body: &'static str) -> (SocketAddr, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if rest.len() >= len {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            String::from_utf8_lossy(&buf).into_owned()
        });
        (addr, handle)
    }

    /// PLUG-2 / TASK-94.5: 提供ツールが無い間は接続せず Deferred を返す（REPAIR-3）。
    #[test]
    fn plug2_register_with_host_defers_while_no_tools() {
        assert!(DECLARED_TOOLS.is_empty());
        assert_eq!(
            register_with_host().expect("deferred"),
            RegisterOutcome::Deferred
        );
    }

    /// PLUG-2 / TASK-94.5: 実接続で POST を送り、200 は Registered、409 duplicate は AlreadyRegistered。
    #[test]
    fn plug2_register_to_sends_manifest_and_interprets_response() {
        let (addr, h) = fake_host(200, r#"{"ok":true,"id":"fandhe-browser-mcp"}"#);
        assert_eq!(
            register_to(addr, &["navigate"]).expect("ok"),
            RegisterOutcome::Registered
        );
        let req = h.join().expect("join");
        assert!(
            req.starts_with("POST /ai/plugins/register HTTP/1.1\r\n"),
            "{req}"
        );
        let body: Value =
            serde_json::from_str(req.split_once("\r\n\r\n").expect("body").1).expect("json");
        assert_eq!(body, manifest_for(&["navigate"]));

        let (addr, h) = fake_host(409, r#"{"code":"duplicate_plugin_id"}"#);
        assert_eq!(
            register_to(addr, &["navigate"]).expect("dup"),
            RegisterOutcome::AlreadyRegistered
        );
        h.join().expect("join");
    }

    /// PLUG-2 / TASK-94.5: ホストが 400 で拒否したら status と code を保持した Rejected になる。
    #[test]
    fn plug2_register_to_rejected_keeps_status_and_code() {
        let (addr, h) = fake_host(400, r#"{"code":"invalid_id"}"#);
        match register_to(addr, &["navigate"]) {
            Err(RegisterError::Rejected { status, code }) => {
                assert_eq!(status, 400);
                assert_eq!(code.as_deref(), Some("invalid_id"));
            }
            other => panic!("{other:?}"),
        }
        h.join().expect("join");
    }

    /// PLUG-2 / TASK-94.5: 各 read の前に残り時間で rearm が呼ばれ、締切以下の値になる。
    #[test]
    fn plug2_read_limited_rearms_with_remaining_time() {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut seen = Vec::new();
        let data = b"abc".to_vec();
        read_limited(&mut data.as_slice(), 100, deadline, |d| {
            seen.push(d);
            Ok(())
        })
        .expect("ok");
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|d| *d <= Duration::from_millis(500)));
    }
}

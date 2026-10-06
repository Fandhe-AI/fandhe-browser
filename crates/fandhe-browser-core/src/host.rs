//! `Host` ヘッダ検証（DNS rebinding 対策）の共有実装（`CDP-1`・`SEC-4`・MS-3/MS-4）。
//!
//! cdp の `/json/*` ルータ（TASK-41.3）と ai の `/ai/*` ルータ（TASK-19.1）が同一の判定を
//! 使うため、両 crate の下位である core に置く（cdp と ai は互いに依存しない）。
//! ホストは `localhost` か IP リテラルに限り、それ以外（= DNS rebinding で到達した任意の
//! ホスト名）は拒否する。生のヘッダ文字列は保持せず、parse した値から組み立て直す。
//! 呼び出し元ハンドラは [`HostError`] を 400（`Malformed`）/ 403（`NotAllowed`）へ写像する。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// `Host` ヘッダ値の最大長（バイト）。ホスト名の上限 253 + `:` + ポート 5 桁 + 余裕。
const MAX_HOST_HEADER_LEN: usize = 262;

/// 検証・正規化済みの `host[:port]`。生のヘッダ文字列を保持しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority(String);

impl Authority {
    /// 正規化済みの authority 文字列（`127.0.0.1:9222`・`[::1]:9222`・`localhost` 等）。
    ///
    /// `/json/version` の `webSocketDebuggerUrl` 組み立てに使う（TASK-41.4）。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Host` ヘッダ値を検証して authority を作る。
    pub fn from_host_header(value: Option<&str>) -> Result<Self, HostError> {
        let value = value.ok_or(HostError::Malformed)?;
        if value.is_empty() || value.len() > MAX_HOST_HEADER_LEN {
            return Err(HostError::Malformed);
        }
        let (host, port) = split_host_port(value)?;
        let port = match port {
            None => None,
            Some(p) => Some(parse_port(p)?),
        };
        let host = canonical_host(host)?;
        Ok(Self(match port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        }))
    }
}

/// `Host` ヘッダ検証エラー。入力値を含めない（ログ・応答への漏えい防止）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostError {
    /// ヘッダが無い・空・長すぎる・形式不正（HTTP 400）。
    Malformed,
    /// 形式は正しいが localhost でも IP リテラルでもない（HTTP 403）。
    NotAllowed,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("invalid host header"),
            Self::NotAllowed => f.write_str("host not allowed"),
        }
    }
}

impl std::error::Error for HostError {}

/// `host[:port]` を分割する。IPv6 は `[` ... `]` を括弧付きのまま host として返す。
fn split_host_port(value: &str) -> Result<(&str, Option<&str>), HostError> {
    if value.starts_with('[') {
        let close = value.find(']').ok_or(HostError::Malformed)?;
        let host = value.get(..=close).ok_or(HostError::Malformed)?;
        let after = value.get(close + 1..).ok_or(HostError::Malformed)?;
        let port = match after {
            "" => None,
            s => Some(s.strip_prefix(':').ok_or(HostError::Malformed)?),
        };
        return Ok((host, port));
    }
    match value.rsplit_once(':') {
        // 括弧無しで `:` が複数ある（素の IPv6）は形式不正。
        Some((h, _)) if h.contains(':') => Err(HostError::Malformed),
        Some((h, p)) => Ok((h, Some(p))),
        None => Ok((value, None)),
    }
}

/// ポートは 1 文字以上の ASCII 数字のみで `u16` に収まること（`+` 等は拒否）。
fn parse_port(p: &str) -> Result<u16, HostError> {
    if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HostError::Malformed);
    }
    p.parse::<u16>().map_err(|_| HostError::Malformed)
}

/// ホストを `localhost` / IP リテラルに限定し、parse した値から組み立て直す。
fn canonical_host(host: &str) -> Result<String, HostError> {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        let ip: Ipv6Addr = inner.parse().map_err(|_| HostError::Malformed)?;
        return Ok(format!("[{ip}]"));
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Ok("localhost".to_owned());
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip.to_string());
    }
    // 制御文字・空白・不正文字を含むものは形式不正、それ以外のホスト名は許可外。
    let valid_name = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if valid_name {
        Err(HostError::NotAllowed)
    } else {
        Err(HostError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(h: &str) -> Result<String, HostError> {
        Authority::from_host_header(Some(h)).map(|a| a.as_str().to_owned())
    }

    #[test]
    fn cdp1_host_accepts_loopback_forms() {
        assert_eq!(auth("127.0.0.1:9222").unwrap(), "127.0.0.1:9222");
        assert_eq!(auth("localhost:9222").unwrap(), "localhost:9222");
        assert_eq!(auth("LOCALHOST").unwrap(), "localhost");
        assert_eq!(auth("[::1]:9222").unwrap(), "[::1]:9222");
        assert_eq!(auth("[::1]").unwrap(), "[::1]");
        assert_eq!(auth("127.0.0.1").unwrap(), "127.0.0.1");
    }

    #[test]
    fn cdp1_host_rejects_missing_and_malformed() {
        assert_eq!(Authority::from_host_header(None), Err(HostError::Malformed));
        let long = "a".repeat(300);
        for bad in [
            "",
            "127.0.0.1:",
            "127.0.0.1:+1",
            "127.0.0.1:65536",
            "[::1",
            "::1:9222",
            "[::1]x",
            "a b:9222",
            "localhost\r\nX: y",
            long.as_str(),
        ] {
            assert_eq!(auth(bad), Err(HostError::Malformed), "input: {bad:?}");
        }
    }

    #[test]
    fn cdp1_host_rejects_foreign_hostname() {
        assert_eq!(auth("evil.example:9222"), Err(HostError::NotAllowed));
        assert_eq!(auth("evil.example"), Err(HostError::NotAllowed));
        assert_eq!(HostError::NotAllowed.to_string(), "host not allowed");
    }
}

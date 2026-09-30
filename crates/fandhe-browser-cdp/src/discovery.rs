//! `/json/*` ディスカバリ応答の本体生成と Host ヘッダ検証（TASK-41（41.3）・#172、`CDP-1`・MS-3）。
//!
//! [`crate::server::router`] のハンドラから呼ばれる純関数群で、`CdpState` にも HTTP の型にも
//! 依存しない（3 OS で単体テストできる）。JSON は `serde_json` で組み立て、文字列連結はしない。
//!
//! # Host 検証（DNS rebinding 対策）
//!
//! 将来 `webSocketDebuggerUrl` の authority はリクエストの `Host` ヘッダから作る（Chromium と同方式。
//! cli が port 0 で bind してもルータ構築後に決まるアドレスへ追従できる）。ホストは `localhost`
//! か IP リテラルに限り、それ以外（= DNS rebinding で到達した任意ホスト名）は拒否する。
//! 生のヘッダ文字列は応答へ出さず、parse した値から authority を組み立て直す。
//! プロセス全体のアクセス制御（loopback 限定バインド）は TASK-41.5・`SEC-4` の責務。
//!
//! # スタブについて
//!
//! `/json/version` も `webSocketDebuggerUrl` を持たない。`/devtools/browser/{id}` の受け口
//! （TASK-41.4）が未実装のため、接続先として返さない（REPAIR-3）。受け口と同時に追加する。
//! `/json/list` の各要素はページごとの `webSocketDebuggerUrl`・`devtoolsFrontendUrl` を持たない。
//! `/devtools/page/{id}` 受け口が未実装のため存在しない URL を出さない（REPAIR-3）。
//! 受け口を実装するタスク（TASK-42 以降・`CDP-1`）で追加する。`title` も `TargetInfo` に
//! タイトルが無いため URL で代用する（Chromium もタイトル無しのページは URL を表示する）。
//! `/json/version` は実際に動くエンジンの値だけを返し、`V8-Version`・`WebKit-Version` は
//! 偽値を返さないため出さない（`security.md` の偽装禁止）。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde_json::{Value, json};

use crate::target::TargetInfo;

/// `Host` ヘッダ値の最大長（バイト）。ホスト名の上限 253 + `:` + ポート 5 桁 + 余裕。
const MAX_HOST_HEADER_LEN: usize = 262;

/// `Protocol-Version` に返す CDP のバージョン。
const PROTOCOL_VERSION: &str = "1.3";

/// 検証・正規化済みの `host[:port]`。生のヘッダ文字列を保持しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Authority(String);

impl Authority {
    /// 正規化済みの authority 文字列（`127.0.0.1:9222`・`[::1]:9222`・`localhost` 等）。
    ///
    /// 現状はテストのみが使う。TASK-41.4 で `webSocketDebuggerUrl` を追加する際に本番でも使う。
    #[cfg(test)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// `Host` ヘッダ値を検証して authority を作る。
    pub(crate) fn from_host_header(value: Option<&str>) -> Result<Self, HostError> {
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
pub(crate) enum HostError {
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

/// `/json/version` の本体（JSON）を作る。
///
/// `webSocketDebuggerUrl` は含めない。`/devtools/browser/{id}` の WS 受け口が未実装
/// （TASK-41.4）のため、接続できない URL を出さない（REPAIR-3）。受け口の実装と同時に
/// authority（[`Authority`]）と `BrowserId` から組み立てて追加する（`CDP-1`）。
pub(crate) fn version_body() -> Result<Vec<u8>, serde_json::Error> {
    let product = concat!("fandhe-browser/", env!("CARGO_PKG_VERSION"));
    let body = json!({
        "Browser": product,
        "Protocol-Version": PROTOCOL_VERSION,
        "User-Agent": product,
    });
    serde_json::to_vec(&body)
}

/// `/json/list`・`/json` の本体（ターゲット配列の JSON）を作る。
pub(crate) fn list_body(targets: &[TargetInfo]) -> Result<Vec<u8>, serde_json::Error> {
    let items: Vec<Value> = targets
        .iter()
        .map(|t| {
            json!({
                "id": t.target_id().as_str(),
                "type": t.kind().as_str(),
                "url": t.url(),
                "title": t.url(),
                "description": "",
            })
        })
        .collect();
    serde_json::to_vec(&items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{TargetKind, TargetRegistry};

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

    #[test]
    fn cdp1_version_body_has_real_values_only() {
        let v: Value = serde_json::from_slice(&version_body().unwrap()).unwrap();
        let product = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(v["Browser"], product.as_str());
        assert_eq!(v["User-Agent"], product.as_str());
        assert_eq!(v["Protocol-Version"], "1.3");
        assert!(v.get("webSocketDebuggerUrl").is_none());
        assert!(v.get("V8-Version").is_none());
        assert!(v.get("WebKit-Version").is_none());
    }

    #[test]
    fn cdp1_list_body_empty_and_ordered() {
        let empty: Value = serde_json::from_slice(&list_body(&[]).unwrap()).unwrap();
        assert_eq!(empty, json!([]));

        let r = TargetRegistry::new();
        let a = r
            .create_target(TargetKind::Page, "https://a.test/")
            .unwrap();
        let b = r
            .create_target(TargetKind::Page, "https://b.test/")
            .unwrap();
        let v: Value = serde_json::from_slice(&list_body(&r.targets()).unwrap()).unwrap();
        assert_eq!(
            v,
            json!([
                {"id": a.as_str(), "type": "page", "url": "https://a.test/",
                 "title": "https://a.test/", "description": ""},
                {"id": b.as_str(), "type": "page", "url": "https://b.test/",
                 "title": "https://b.test/", "description": ""},
            ])
        );
        assert!(v[0].get("webSocketDebuggerUrl").is_none());
    }
}

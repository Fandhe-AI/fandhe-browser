//! navigate ツールの入力検証と応答解釈（TASK-94.3・PLUG-2・PLUG-3・MS-9）。
//!
//! `server.rs` の navigate ハンドラから呼ばれる純関数群。ホストへの送信は `host.rs` が担う。
//!
//! 将来仕様（REPAIR-3）: ホストの `POST /ai/navigate` は PoC-15 の契約（要求 `{"url"}`・
//! 応答 `{"ok","url"}`）に基づく想定で、本リポのホスト（ai crate）には未実装。ホスト側の
//! エンドポイントとスキーマが確定した時点で応答解釈を合わせる。URL の最終的な SSRF・内部
//! アドレス判定はホストの `Fetcher` の責務で、ここは scheme 許可リストによる早期拒否に留める。

use rmcp::serde_json::{self, Value, json};

/// 受理する URL の最大バイト数。
pub(crate) const MAX_URL_BYTES: usize = 8192;
/// ホストの `code` として保持する最大長。
const MAX_CODE_LEN: usize = 64;

/// 入力 URL の拒否理由。
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum NavigateError {
    Empty,
    TooLong,
    InvalidCharacter,
    UnsupportedScheme,
}

impl std::fmt::Display for NavigateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Empty => "url must not be empty",
            Self::TooLong => "url is too long",
            Self::InvalidCharacter => "url contains whitespace or control characters",
            Self::UnsupportedScheme => "url scheme must be http, https or about:blank",
        })
    }
}

/// ホスト応答の解釈結果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NavigateOutcome {
    Success { url: Option<String> },
    Failure { status: u16, code: Option<String> },
}

/// URL を検証する。scheme は http / https（大文字小文字非区別）と `about:blank` のみ。
pub(crate) fn validate_url(raw: &str) -> Result<&str, NavigateError> {
    if raw.is_empty() {
        return Err(NavigateError::Empty);
    }
    if raw.len() > MAX_URL_BYTES {
        return Err(NavigateError::TooLong);
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(NavigateError::InvalidCharacter);
    }
    let lower = raw.to_ascii_lowercase();
    if lower == "about:blank" || lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(raw)
    } else {
        Err(NavigateError::UnsupportedScheme)
    }
}

/// ホストへ送る JSON 本文を生成する（文字列連結ではなく直列化で組み立てる）。
pub(crate) fn request_body(url: &str) -> Vec<u8> {
    json!({ "url": url }).to_string().into_bytes()
}

/// ホスト応答を解釈する。2xx かつ `ok == true` のときのみ成功。
/// 失敗時はホスト本文の自由文を流さず、安全な `code` のみ保持する。
pub(crate) fn interpret(status: u16, body: &[u8]) -> NavigateOutcome {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let ok = (200..300).contains(&status)
        && parsed
            .as_ref()
            .and_then(|v| v.get("ok"))
            .and_then(Value::as_bool)
            == Some(true);
    if ok {
        let url = parsed
            .as_ref()
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
            .filter(|u| u.len() <= MAX_URL_BYTES)
            .map(str::to_owned);
        return NavigateOutcome::Success { url };
    }
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_str)
        .filter(|c| {
            (1..=MAX_CODE_LEN).contains(&c.len())
                && c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        })
        .map(str::to_owned);
    NavigateOutcome::Failure { status, code }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PLUG-3 / TASK-94.3: 受理する URL。
    #[test]
    fn plug3_validate_accepts() {
        for u in ["https://example.com/", "HTTP://example.com", "about:blank"] {
            assert_eq!(validate_url(u), Ok(u));
        }
    }

    /// PLUG-3 / TASK-94.3: 拒否する URL。
    #[test]
    fn plug3_validate_rejects() {
        assert_eq!(validate_url(""), Err(NavigateError::Empty));
        assert_eq!(
            validate_url(&format!("http://a/{}", "x".repeat(MAX_URL_BYTES))),
            Err(NavigateError::TooLong)
        );
        assert_eq!(
            validate_url("http://a/\r\nX: y"),
            Err(NavigateError::InvalidCharacter)
        );
        assert_eq!(
            validate_url("http://a b"),
            Err(NavigateError::InvalidCharacter)
        );
        for u in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://x",
            "example.com",
        ] {
            assert_eq!(validate_url(u), Err(NavigateError::UnsupportedScheme));
        }
    }

    /// PLUG-3 / TASK-94.3: 要求本文は JSON 直列化される（引用符を含む URL でも壊れない）。
    #[test]
    fn plug3_request_body_is_json() {
        assert_eq!(
            request_body("https://e.com/?q=\"a\""),
            br#"{"url":"https://e.com/?q=\"a\""}"#
        );
    }

    /// PLUG-3 / TASK-94.3: 応答解釈。
    #[test]
    fn plug3_interpret() {
        assert_eq!(
            interpret(200, br#"{"ok":true,"url":"https://e.com/"}"#),
            NavigateOutcome::Success {
                url: Some("https://e.com/".into())
            }
        );
        let fail = |status, code: Option<&str>| NavigateOutcome::Failure {
            status,
            code: code.map(str::to_owned),
        };
        assert_eq!(
            interpret(
                502,
                br#"{"ok":false,"code":"fetch_failed","error":"secret detail"}"#
            ),
            fail(502, Some("fetch_failed"))
        );
        assert_eq!(interpret(200, br#"{"ok":false}"#), fail(200, None));
        assert_eq!(interpret(404, b"not json"), fail(404, None));
        assert_eq!(
            interpret(500, br#"{"code":"Bad Code!\n"}"#),
            fail(500, None)
        );
        assert_eq!(interpret(500, br#"{"ok":true}"#), fail(500, None));
    }
}

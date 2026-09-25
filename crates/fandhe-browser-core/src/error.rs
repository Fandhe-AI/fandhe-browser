//! `fandhe-browser-core` 全体で共有するエラー型。
//!
//! `fetch`（TASK-24.2・#36）・`parse`（TASK-24.4・#38）・`dom`（TASK-24.5・#39）・
//! `query`（TASK-24.7・#41）の各モジュールは、本モジュールが定義する [`Error`] /
//! [`Result`] を戻り値の共通土台として使う想定（TASK-24（24.1）・ビヘイビア
//! `CORE-1`）。`unsafe` は使わず、`thiserror`/`anyhow` 等の外部依存も追加しない
//! （dependency-policy.md の依存最小方針）。`fetch` 本実装（TASK-24.2・#36）に
//! 伴い `Cargo.toml` の `[dependencies]` へ `reqwest`・`rustls`（Issue #35 で
//! 採用承認済み）を追加したが、本モジュールはそれらの型を公開 API に漏らさず、
//! 汎用的な [`Error`] バリアントへ写像する（coding-rust.md「JS エンジンは
//! トレイト抽象越しに」と同様、外部クレートの具象型を上位 crate へ漏らさない
//! 方針を fetch にも適用する）。
//!
//! 呼び出し元は `fandhe-browser-ai`・`fandhe-browser-cdp` 等の上位 crate（本
//! crate から一方向に依存される）や、本 crate 内の各モジュール（`js_stub` を
//! 含む。TASK-24（24.9）・#43）を想定する。
//!
//! バリアントは現時点では汎用的なものに留め、fetch/parse/dom/query の各実装
//! （#36/#38/#39/#41）が固有のケース（HTTP ステータス・パースエラー位置等）を
//! 追加できるよう `#[non_exhaustive]` にしてある（REPAIR-4: 戻り値は将来拡張
//! できる構造にする）。

use std::fmt;
use std::time::Duration;

/// `fandhe-browser-core` の各モジュール（fetch/parse/dom/query）が返すエラー。
///
/// `#[non_exhaustive]` により、後続タスクでのバリアント追加が呼び出し側の
/// 非網羅 `match` を破壊的変更にしない（クレート外は `matches!` 等の部分一致で
/// 扱う想定）。
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// 入出力エラー（ファイル読み込み・将来のネットワーク I/O 等）。
    ///
    /// 申し送り（#36 fetch 実装向け）: `Display` 実装は内部の I/O エラー
    /// メッセージをそのまま表示するため、URL の userinfo（`user:pass@host`）
    /// やレスポンス本文など秘密情報・大量データを埋め込んだ `io::Error` を
    /// ここに詰めない（security.md 秘密情報混入防止・SSRF 観点）。
    Io(std::io::Error),
    /// 呼び出し元から渡された入力がそもそも不正な場合（例: 空の URL・
    /// 不正な HTML 断片）。
    InvalidInput {
        /// 人間・AI 双方が読める英語メッセージ（プログラム出力文字列は英語。
        /// japanese-style.md）。
        message: String,
    },
    /// 構文的には正しいが、本 crate が現時点で対応していない入力・機能。
    Unsupported {
        /// 未対応の内容を示す英語メッセージ。
        message: String,
    },
    /// JS 実行を要求されたが実行できない場合（`js_stub::execute_js_stub`。
    /// CORE-1・TASK-24（24.9）・#43）。
    ///
    /// `Unsupported`（構文的に正しいが未対応の入力・機能）とは意味を分ける:
    /// こちらは「JS エンジンがまだ統合されていない」「エンジン非同梱ビルド
    /// である」という実行環境側の事情を表す。`fandhe-browser-js`（TASK-28）
    /// の統合後、TASK-30（Issue #143・ビヘイビア `JS-2`）で V8 実呼び出しに
    /// 置換されるまでの間、および同梱ビルドでない場合の双方をこの variant
    /// で表現する（`js-engine.md` 決定 4）。
    JsExecutionUnavailable {
        /// 実行できない理由を示す英語メッセージ。呼び出し元から渡された
        /// スクリプト文字列は埋め込まない（外部入力の反響・ログ肥大化を
        /// 避けるため。security.md）。
        message: String,
    },
    /// `fetch::Fetcher::get`（TASK-24.2・#36・CORE-1）が、`FetchOptions` の
    /// 全体タイムアウト・接続タイムアウトを超過した場合に返す。
    Timeout {
        /// 超過した上限値（`FetchOptions::timeout` または `connect_timeout`）。
        limit: Duration,
    },
    /// `fetch::Fetcher::get` が、`FetchOptions::max_redirects` を超える
    /// リダイレクトを検出した場合に返す（security.md「不安全な設計」:
    /// 無限リダイレクトの上限検証）。
    TooManyRedirects {
        /// 許可した最大リダイレクト回数。
        limit: usize,
    },
    /// `fetch::Fetcher::get` が、`FetchOptions::max_body_bytes` を超える
    /// レスポンス本文を検出した場合に返す。`Content-Length` ヘッダによる
    /// 事前検査・ストリーミング読み込み中の逐次検査の両方で使う
    /// （security.md「不安全な設計」: 巨大レスポンスへの上限検証）。
    ResponseTooLarge {
        /// 許可した最大バイト数。
        limit: u64,
    },
    /// `fetch::Fetcher::get` が、`http`/`https` 以外の scheme（`file:` 等）
    /// への要求・リダイレクトを検出した場合に返す（security.md「SSRF」）。
    DisallowedScheme {
        /// 拒否した scheme 名のみを保持する。URL 全体（userinfo・パス等の
        /// 秘匿情報を含み得る）はここへ埋め込まない。
        scheme: String,
    },
    /// `fetch::Fetcher` の内部 HTTP クライアント（`reqwest`）が返したエラーを
    /// 写像したもの。`reqwest::Error` は公開 API に出さず（外部クレートの
    /// 具象型を上位 crate へ漏らさない方針）、`without_url()` を通した後の
    /// メッセージのみを保持する（URL の userinfo・クエリを含めないため。
    /// security.md 秘密情報混入防止）。
    Network {
        /// `reqwest::Error` の `without_url()` 後の Display 文字列。
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(source) => write!(f, "I/O error: {source}"),
            Error::InvalidInput { message } => write!(f, "invalid input: {message}"),
            Error::Unsupported { message } => write!(f, "unsupported: {message}"),
            Error::JsExecutionUnavailable { message } => {
                write!(f, "JS execution unavailable: {message}")
            }
            Error::Timeout { limit } => write!(f, "request timed out after {limit:?}"),
            Error::TooManyRedirects { limit } => {
                write!(f, "too many redirects (limit: {limit})")
            }
            Error::ResponseTooLarge { limit } => {
                write!(f, "response body exceeds limit of {limit} bytes")
            }
            Error::DisallowedScheme { scheme } => {
                write!(f, "disallowed URL scheme: {scheme}")
            }
            Error::Network { message } => write!(f, "network error: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(source) => Some(source),
            Error::InvalidInput { .. }
            | Error::Unsupported { .. }
            | Error::JsExecutionUnavailable { .. }
            | Error::Timeout { .. }
            | Error::TooManyRedirects { .. }
            | Error::ResponseTooLarge { .. }
            | Error::DisallowedScheme { .. }
            | Error::Network { .. } => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io(source)
    }
}

/// `fandhe-browser-core` の各モジュールが返す `Result` のエイリアス。
///
/// ライブラリコードは `panic!`/`unwrap`/`expect` を使わずこの型で伝播させる
/// （coding-rust.md「エラーハンドリング」）。
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    /// CORE-1: `Error::Io` の `Display` が内部 I/O エラーのメッセージを
    /// そのまま含むことを確認する。
    #[test]
    fn core_1_display_io_variant_includes_source_message() {
        let io_err = std::io::Error::other("boom");
        let err = Error::Io(io_err);
        assert_eq!(err.to_string(), "I/O error: boom");
    }

    /// CORE-1: `Error::InvalidInput` の `Display` がメッセージを含む。
    #[test]
    fn core_1_display_invalid_input_variant() {
        let err = Error::InvalidInput {
            message: "empty url".to_string(),
        };
        assert_eq!(err.to_string(), "invalid input: empty url");
    }

    /// CORE-1: `Error::Unsupported` の `Display` がメッセージを含む。
    #[test]
    fn core_1_display_unsupported_variant() {
        let err = Error::Unsupported {
            message: "gzip encoding".to_string(),
        };
        assert_eq!(err.to_string(), "unsupported: gzip encoding");
    }

    /// CORE-1: `Error::Io` の `source()` が内部エラーへ連鎖することを確認する。
    #[test]
    fn core_1_source_chains_for_io_variant() {
        let io_err = std::io::Error::other("boom");
        let err = Error::Io(io_err);
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            Some("boom".to_string())
        );
    }

    /// CORE-1: `Error::InvalidInput`/`Error::Unsupported` は連鎖する内部
    /// エラーを持たないため `source()` が `None` を返すことを確認する。
    #[test]
    fn core_1_source_is_none_for_leaf_variants() {
        let invalid = Error::InvalidInput {
            message: "x".to_string(),
        };
        let unsupported = Error::Unsupported {
            message: "y".to_string(),
        };
        assert_eq!(
            std::error::Error::source(&invalid).map(ToString::to_string),
            None::<String>
        );
        assert_eq!(
            std::error::Error::source(&unsupported).map(ToString::to_string),
            None::<String>
        );
    }

    /// CORE-1: `?` 演算子で使うための `From<io::Error>` 変換を確認する。
    #[test]
    fn core_1_from_io_error_converts_to_io_variant() {
        fn read() -> Result<()> {
            Err(std::io::Error::other("boom"))?
        }

        let err = read().expect_err("read は常に失敗する");
        assert!(matches!(err, Error::Io(_)));
        assert_eq!(err.to_string(), "I/O error: boom");
    }

    /// CORE-1: `Error` が `Send + Sync + 'static` を満たすことを静的に確認する
    /// （マルチスレッド環境・非同期ランタイムへ持ち出せることの保証）。
    #[test]
    fn core_1_error_is_send_sync_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<Error>();
    }

    /// CORE-1（TASK-24（24.9）・#43）: `Error::JsExecutionUnavailable` の
    /// `Display` がメッセージを含む。
    #[test]
    fn core_1_display_js_execution_unavailable_variant() {
        let err = Error::JsExecutionUnavailable {
            message: "js_stub boundary".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "JS execution unavailable: js_stub boundary"
        );
    }

    /// CORE-1（TASK-24（24.9）・#43）: `Error::JsExecutionUnavailable` は
    /// 連鎖する内部エラーを持たないため `source()` が `None` を返す。
    #[test]
    fn core_1_source_is_none_for_js_execution_unavailable_variant() {
        let err = Error::JsExecutionUnavailable {
            message: "js_stub boundary".to_string(),
        };
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            None::<String>
        );
    }

    /// CORE-1（TASK-24.2・#36）: `Error::Timeout` の `Display` が上限値を含む。
    #[test]
    fn core_1_display_timeout_variant() {
        let err = Error::Timeout {
            limit: Duration::from_millis(200),
        };
        assert_eq!(err.to_string(), "request timed out after 200ms");
    }

    /// CORE-1（TASK-24.2・#36）: `Error::TooManyRedirects` の `Display` が
    /// 上限回数を含む。
    #[test]
    fn core_1_display_too_many_redirects_variant() {
        let err = Error::TooManyRedirects { limit: 3 };
        assert_eq!(err.to_string(), "too many redirects (limit: 3)");
    }

    /// CORE-1（TASK-24.2・#36）: `Error::ResponseTooLarge` の `Display` が
    /// 上限バイト数を含む。
    #[test]
    fn core_1_display_response_too_large_variant() {
        let err = Error::ResponseTooLarge { limit: 1024 };
        assert_eq!(err.to_string(), "response body exceeds limit of 1024 bytes");
    }

    /// CORE-1（TASK-24.2・#36）: `Error::DisallowedScheme` の `Display` が
    /// scheme 名のみを含み、URL 全体は含まない。
    #[test]
    fn core_1_display_disallowed_scheme_variant() {
        let err = Error::DisallowedScheme {
            scheme: "file".to_string(),
        };
        assert_eq!(err.to_string(), "disallowed URL scheme: file");
    }

    /// CORE-1（TASK-24.2・#36）: `Error::Network` の `Display` がメッセージを
    /// 含む。
    #[test]
    fn core_1_display_network_variant() {
        let err = Error::Network {
            message: "connection refused".to_string(),
        };
        assert_eq!(err.to_string(), "network error: connection refused");
    }

    /// CORE-1（TASK-24.2・#36）: 新規バリアントは連鎖する内部エラーを持たない
    /// ため `source()` が `None` を返す。
    #[test]
    fn core_1_source_is_none_for_fetch_variants() {
        let variants = [
            Error::Timeout {
                limit: Duration::from_secs(30),
            },
            Error::TooManyRedirects { limit: 10 },
            Error::ResponseTooLarge {
                limit: 16 * 1024 * 1024,
            },
            Error::DisallowedScheme {
                scheme: "data".to_string(),
            },
            Error::Network {
                message: "boom".to_string(),
            },
        ];
        for variant in &variants {
            assert_eq!(
                std::error::Error::source(variant).map(ToString::to_string),
                None::<String>
            );
        }
    }
}

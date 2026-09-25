//! `fandhe-browser-core` 全体で共有するエラー型。
//!
//! `fetch`（TASK-24.2・#36）・`parse`（TASK-24.4・#38）・`dom`（TASK-24.5・#39）・
//! `query`（TASK-24.7・#41）の各モジュールは、本モジュールが定義する [`Error`] /
//! [`Result`] を戻り値の共通土台として使う想定（TASK-24（24.1）・ビヘイビア
//! `CORE-1`）。`unsafe` は使わず、`thiserror`/`anyhow` 等の外部依存も追加しない
//! （dependency-policy.md の依存最小方針。`Cargo.toml` の `[dependencies]` は
//! 引き続き空のまま）。
//!
//! 呼び出し元は `fandhe-browser-ai`・`fandhe-browser-cdp` 等の上位 crate（本
//! crate から一方向に依存される）や、本 crate 内の各モジュールを想定する。
//!
//! バリアントは現時点では汎用的なものに留め、fetch/parse/dom/query の各実装
//! （#36/#38/#39/#41）が固有のケース（HTTP ステータス・パースエラー位置等）を
//! 追加できるよう `#[non_exhaustive]` にしてある（REPAIR-4: 戻り値は将来拡張
//! できる構造にする）。

use std::fmt;

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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(source) => write!(f, "I/O error: {source}"),
            Error::InvalidInput { message } => write!(f, "invalid input: {message}"),
            Error::Unsupported { message } => write!(f, "unsupported: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(source) => Some(source),
            Error::InvalidInput { .. } | Error::Unsupported { .. } => None,
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
}

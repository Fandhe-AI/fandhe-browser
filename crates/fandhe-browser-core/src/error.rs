//! `fandhe-browser-core` 全体で共有するエラー型。
//!
//! `fetch`（TASK-24.2・#36）・`parse`（TASK-24.4・#38）・`dom`（TASK-24.5・#39）・
//! `selector`（TASK-24.7・#41）・`query`（TASK-24.10・#418）の各モジュールは、
//! 本モジュールが定義する [`Error`] / [`Result`] を戻り値の共通土台として
//! 使う想定（TASK-24（24.1）・ビヘイビア `CORE-1`）。`unsafe` は使わず、
//! `thiserror`/`anyhow` 等の外部依存も追加しない（dependency-policy.md の
//! 依存最小方針）。`Cargo.toml` の `[dependencies]` には `parse` が使う
//! `html5ever = "=0.40.1"`（TASK-24.4・#38）と、`fetch` 本実装（TASK-24.2・
//! #36）に伴う `reqwest`・`rustls`（いずれも Issue #35 で採用承認済み）が
//! あり、`selector`・`query`（TASK-24.7/24.10・#41/#418）は追加依存なしで
//! 実装する。本モジュール（[`Error`] / [`ParseError`]）はそれらの型を
//! バリアントの内部表現に漏らさず、汎用的な variant へ写像する（coding-rust.md
//! 「JS エンジンはトレイト抽象越しに」と同様、外部クレートの具象型を上位 crate へ
//! 漏らさない方針を fetch/parse のエラー表現にも適用する）。`dom`（TASK-24.5・
//! #39）は arena の型定義として `html5ever::QualName` 等を公開しており、この方針
//! の対象外（本モジュールのエラー型には限らない）。
//!
//! 呼び出し元は `fandhe-browser-ai`・`fandhe-browser-cdp` 等の上位 crate（本
//! crate から一方向に依存される）や、本 crate 内の各モジュール（`js_stub` を
//! 含む。TASK-24（24.9）・#43）を想定する。
//!
//! バリアントは現時点では汎用的なものに留め、fetch/parse/dom/selector/query
//! の各実装（#36/#38/#39/#41/#418）が固有のケース（HTTP ステータス・パース
//! エラー位置等）を追加できるよう `#[non_exhaustive]` にしてある
//! （REPAIR-4: 戻り値は将来拡張できる構造にする）。
//!
//! `config`（TASK-91（91.1）・Issue #214）は [`Error::Config`] を追加し、
//! [`crate::config::ConfigError`] を payload として保持する（`Parse`/
//! `ParseError` と同じ「専用エラー型を variant に包む」方式）。
//!
//! `cssom_profile`（TASK-100.3・Issue #267・`PLUG-8`）は [`Error::BrowserProfileLoad`]
//! （埋め込みデータ不正）と [`Error::BrowserProfileName`]（未知のプロファイル名）を
//! 追加する。CLI が variant で分岐して固定文言・終了コードへ写像できるようにするため、
//! `InvalidInput` へ潰さず専用エラー型を保持する。

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
    /// こちらは実行環境側の事情を表す。エンジンなしビルド、およびエンジン生成の
    /// 失敗（未同梱・未実装。TASK-30（30.3）・ビヘイビア `JS-2`・`js-engine.md`
    /// 決定 4）が該当する。エンジンが評価を試みて失敗した場合は
    /// [`Error::JsEvaluation`] を使う。
    JsExecutionUnavailable {
        /// 実行できない理由を示す英語メッセージ。呼び出し元から渡された
        /// スクリプト文字列は埋め込まない（外部入力の反響・ログ肥大化を
        /// 避けるため。security.md）。
        message: String,
    },
    /// JS エンジンがスクリプト評価に失敗した（`js_stub::execute_js_stub`。
    /// TASK-30（30.3）・`JS-2`・Issue #161）。
    ///
    /// Timeout・ResourceLimitExceeded・EngineUnavailable・EvaluationFailed 等の
    /// 区別を失わないよう [`fandhe_browser_js::JsEngineError`] をそのまま保持する
    /// （実際の打ち切り理由を隠さない。security.md）。エンジン抽象の型であり、
    /// V8 / boa の具象型は漏れない。
    JsEvaluation(fandhe_browser_js::JsEngineError),
    /// `parse` モジュール（TASK-24.4・#38・ビヘイビア `CORE-1`）が返す
    /// HTML パース固有のエラー。詳細は [`ParseError`] を参照。
    Parse(ParseError),
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
    /// `fetch::Fetcher::get` が、`FetchOptions::allow_private_network_access`
    /// が `false`（既定）のときに、取得先または各リダイレクト先（DNS 名の
    /// 解決結果・IP リテラル host のいずれか）がループバック・プライベート
    /// アドレス等の内部アドレスであると判定した場合に返す（security.md
    /// 「SSRF」）。DNS 名は `fetch::SafeResolver`、IP リテラル host は
    /// `fetch::reject_disallowed_address` が、初回リクエスト・リダイレクト先
    /// の双方で検証するため、この 1 variant で両方の経路をカバーする。
    DisallowedAddress {
        /// 拒否した解決先の IP アドレス文字列のみを保持する。ホスト名・
        /// URL 全体（userinfo・パス等）はここへ埋め込まない。
        address: String,
    },
    /// `fetch::Fetcher::get` が、同時実行できる DNS 解決スレッド数の上限
    /// （`fetch::resolve_blocking` が管理するプロセス全体のカウンタ）に
    /// 達しているときに返す。`fetch::SafeResolver::resolve` は名前解決の
    /// たびに専用の OS スレッドを生成するため、外部から多数の異なる URL を
    /// 取得させられる経路では、上限を設けないと DNS 応答遅延に比例して
    /// スレッド・メモリが無制限に増える（security.md「不安全な設計」。
    /// PR #430 コードレビュー指摘 P0）。
    TooManyConcurrentDnsResolutions {
        /// 許可した同時実行数の上限。
        limit: usize,
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
    /// `query` モジュール（TASK-24（24.10）・#418・ビヘイビア `CORE-1`）の
    /// `element_matches`/`query_selector_all`/`query_selector` が、内部の
    /// 照合メモ化キャッシュ（`MatchCache`）の合計エントリ数が
    /// `query::MAX_MATCH_CACHE_ENTRIES` を超えると判定した場合に返す。
    /// `ParseOptions::max_nodes`・`selector::MAX_SELECTORS_PER_LIST`・
    /// `selector::MAX_COMPOUNDS_PER_COMPLEX` の組み合わせ次第では理論上の
    /// メモリ上限がこの値を大きく上回るため、実効的なメモリ上限として
    /// 独立に検査する（security.md「不安全な設計」対策・PR #439 レビュー
    /// 指摘 P0）。一律 `false`/空の結果へフォールバックせず、明示的な API
    /// エラーとして呼び出し側に伝える。
    MatchCacheLimitExceeded {
        /// 適用された上限値（`query::MAX_MATCH_CACHE_ENTRIES`）。
        limit: usize,
    },
    /// `config` モジュール（TASK-91（91.1）・Issue #214・基盤タスクのため
    /// 対象ビヘイビアなし）が `fandhe-browser.toml` 相当の TOML 読み込み・
    /// 解釈に失敗した場合に返す。詳細は [`crate::config::ConfigError`] を参照。
    Config(crate::config::ConfigError),
    /// `cssom_profile`（TASK-100.3・Issue #267・`PLUG-8`）の埋め込みプロファイルデータの
    /// 読み込みに失敗した場合に返す（fail-closed。素通しへ落とさない）。
    BrowserProfileLoad(crate::cssom_profile::ProfileLoadError),
    /// `cssom_profile`（TASK-100.3・Issue #267・`PLUG-8`）へ未知のプロファイル名が
    /// 渡された場合に返す。payload 内の入力値は 64 バイトへ切り詰め済み。
    BrowserProfileName(crate::cssom_profile::BrowserProfileParseError),
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
            Error::JsEvaluation(source) => write!(f, "JS evaluation failed: {source}"),
            Error::Parse(source) => write!(f, "parse error: {source}"),
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
            Error::DisallowedAddress { address } => {
                write!(f, "disallowed target address: {address}")
            }
            Error::TooManyConcurrentDnsResolutions { limit } => {
                write!(f, "too many concurrent DNS resolutions (limit: {limit})")
            }
            Error::Network { message } => write!(f, "network error: {message}"),
            Error::MatchCacheLimitExceeded { limit } => {
                write!(f, "selector match cache exceeded limit of {limit} entries")
            }
            Error::Config(source) => write!(f, "configuration error: {source}"),
            Error::BrowserProfileLoad(source) => {
                write!(f, "browser profile data error: {source}")
            }
            Error::BrowserProfileName(source) => write!(f, "invalid browser profile: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(source) => Some(source),
            Error::Parse(source) => Some(source),
            Error::JsEvaluation(source) => Some(source),
            Error::InvalidInput { .. }
            | Error::Unsupported { .. }
            | Error::JsExecutionUnavailable { .. }
            | Error::Timeout { .. }
            | Error::TooManyRedirects { .. }
            | Error::ResponseTooLarge { .. }
            | Error::DisallowedScheme { .. }
            | Error::DisallowedAddress { .. }
            | Error::TooManyConcurrentDnsResolutions { .. }
            | Error::Network { .. }
            | Error::MatchCacheLimitExceeded { .. } => None,
            Error::Config(source) => Some(source),
            Error::BrowserProfileLoad(source) => Some(source),
            Error::BrowserProfileName(source) => Some(source),
        }
    }
}

impl From<ParseError> for Error {
    fn from(source: ParseError) -> Self {
        Error::Parse(source)
    }
}

impl From<crate::cssom_profile::ProfileLoadError> for Error {
    fn from(source: crate::cssom_profile::ProfileLoadError) -> Self {
        Error::BrowserProfileLoad(source)
    }
}

impl From<crate::cssom_profile::BrowserProfileParseError> for Error {
    fn from(source: crate::cssom_profile::BrowserProfileParseError) -> Self {
        Error::BrowserProfileName(source)
    }
}

impl From<crate::config::ConfigError> for Error {
    fn from(source: crate::config::ConfigError) -> Self {
        Error::Config(source)
    }
}

/// HTML パース固有のエラー情報（[`Error::Parse`] の payload）。
///
/// `parse` モジュール（TASK-24.4・#38）が返す。`#[non_exhaustive]` により、
/// 後続タスクでのバリアント追加を非破壊にする（REPAIR-4）。
#[derive(Debug)]
#[non_exhaustive]
pub enum ParseError {
    /// 入力サイズが `ParseOptions::max_input_bytes` を超えている
    /// （アロケーション前に検査する。security.md「無制限リソース確保」対策）。
    InputTooLarge {
        /// 実際の入力サイズ（バイト数）。
        len: usize,
        /// 許容上限（バイト数）。
        limit: usize,
    },
    /// `parse::parse_document_bytes` で入力が不正な UTF-8 だった。
    InvalidUtf8 {
        /// 妥当な UTF-8 として解釈できた先頭バイト数。
        valid_up_to: usize,
    },
    /// 構築したノード数が `ParseOptions::max_nodes` を超えた
    /// （無制限確保による DoS を防ぐための上限。security.md）。
    NodeLimitExceeded {
        /// 適用された上限値。
        limit: usize,
    },
    /// `ParseOptions::with_max_nodes` に `0` が指定された。
    ///
    /// arena は `Document` ルート用に最低 1 ノードを要するため `0` は
    /// 構築不能であり、黙って `1` へ引き上げる（公開 API が受け取った上限と
    /// 実際に適用される上限が乖離する）代わりに明示的に拒否する
    /// （REPAIR-6 P1 レビュー指摘）。
    InvalidMaxNodes {
        /// 呼び出し元が指定した値（常に `0`）。
        requested: usize,
    },
    /// `ParseErrorPolicy::Strict` 指定時に、回復可能なパースエラーが
    /// 1 件以上検出された（既定の `Recover` ポリシーではこの代わりに
    /// `Ok` を返し、診断情報として報告する）。
    Malformed {
        /// 検出されたパースエラー件数（saturating で加算）。
        error_count: u64,
        /// 先頭のパースエラーメッセージ（英語。html5ever が返す静的文字列）。
        first_message: Option<String>,
    },
    /// `TreeSink` の内部不変条件違反（契約外のハンドルでの呼び出し・
    /// `RefCell` の借用競合等）を検出した。html5ever 側の実装変更や
    /// 本 crate 側のバグを示す想定外経路であり、通常の入力では発生しない。
    Internal {
        /// 診断用の英語メッセージ（入力本文は含めない。security.md）。
        message: String,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::InputTooLarge { len, limit } => {
                write!(
                    f,
                    "input too large: {len} bytes exceeds limit of {limit} bytes"
                )
            }
            ParseError::InvalidUtf8 { valid_up_to } => {
                write!(f, "invalid UTF-8 input (valid up to byte {valid_up_to})")
            }
            ParseError::NodeLimitExceeded { limit } => {
                write!(f, "node limit of {limit} exceeded during parsing")
            }
            ParseError::InvalidMaxNodes { requested } => {
                write!(
                    f,
                    "invalid max_nodes: {requested} (must be at least 1; the arena always \
                     needs one node for the Document root)"
                )
            }
            ParseError::Malformed {
                error_count,
                first_message,
            } => match first_message {
                Some(msg) => write!(
                    f,
                    "malformed HTML: {error_count} parse error(s), first: {msg}"
                ),
                None => write!(f, "malformed HTML: {error_count} parse error(s)"),
            },
            ParseError::Internal { message } => write!(f, "internal parser error: {message}"),
        }
    }
}

impl std::error::Error for ParseError {}

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

    /// JS-2（TASK-30.3・#161）: `Error::JsEvaluation` の `Display` と `source()`。
    #[test]
    fn js_2_js_evaluation_display_and_source() {
        use fandhe_browser_js::JsEngineError;
        let cases = [
            (
                JsEngineError::EvaluationFailed("x".to_string()),
                "JS evaluation failed: script evaluation failed: x",
            ),
            (
                JsEngineError::BindingFailed("x".to_string()),
                "JS evaluation failed: binding registration failed: x",
            ),
            (
                JsEngineError::ResourceLimitExceeded("x".to_string()),
                "JS evaluation failed: script evaluation exceeded a resource limit: x",
            ),
            (
                JsEngineError::Timeout("x".to_string()),
                "JS evaluation failed: script evaluation timed out: x",
            ),
            (
                JsEngineError::EngineUnavailable("x".to_string()),
                "JS evaluation failed: js engine is temporarily unavailable: x",
            ),
        ];
        for (inner, expected) in cases {
            let err = Error::JsEvaluation(inner);
            assert_eq!(err.to_string(), expected);
            assert!(std::error::Error::source(&err).is_some());
        }
    }

    /// CORE-1（TASK-24.4・#38）: `ParseError` の各バリアントの `Display` が
    /// 具体値を含むことを確認する。
    #[test]
    fn core_1_display_parse_error_variants() {
        assert_eq!(
            ParseError::InputTooLarge {
                len: 200,
                limit: 100
            }
            .to_string(),
            "input too large: 200 bytes exceeds limit of 100 bytes"
        );
        assert_eq!(
            ParseError::InvalidUtf8 { valid_up_to: 3 }.to_string(),
            "invalid UTF-8 input (valid up to byte 3)"
        );
        assert_eq!(
            ParseError::NodeLimitExceeded { limit: 5 }.to_string(),
            "node limit of 5 exceeded during parsing"
        );
        assert_eq!(
            ParseError::InvalidMaxNodes { requested: 0 }.to_string(),
            "invalid max_nodes: 0 (must be at least 1; the arena always needs one node for the \
             Document root)"
        );
        assert_eq!(
            ParseError::Malformed {
                error_count: 2,
                first_message: Some("bad tag".to_string()),
            }
            .to_string(),
            "malformed HTML: 2 parse error(s), first: bad tag"
        );
        assert_eq!(
            ParseError::Malformed {
                error_count: 1,
                first_message: None,
            }
            .to_string(),
            "malformed HTML: 1 parse error(s)"
        );
        assert_eq!(
            ParseError::Internal {
                message: "borrow conflict".to_string()
            }
            .to_string(),
            "internal parser error: borrow conflict"
        );
    }

    /// CORE-1（TASK-24.4・#38）: `Error::Parse` の `Display` が内部の
    /// `ParseError` メッセージを包んで表示する。
    #[test]
    fn core_1_display_parse_variant_wraps_parse_error() {
        let err = Error::Parse(ParseError::NodeLimitExceeded { limit: 5 });
        assert_eq!(
            err.to_string(),
            "parse error: node limit of 5 exceeded during parsing"
        );
    }

    /// CORE-1（TASK-24.4・#38）: `Error::Parse` の `source()` が内部の
    /// `ParseError` へ連鎖することを確認する。
    #[test]
    fn core_1_source_chains_for_parse_variant() {
        let err = Error::Parse(ParseError::NodeLimitExceeded { limit: 5 });
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            Some("node limit of 5 exceeded during parsing".to_string())
        );
    }

    /// CORE-1（TASK-24.4・#38）: `?` 演算子で使うための
    /// `From<ParseError> for Error` 変換を確認する。
    #[test]
    fn core_1_from_parse_error_converts_to_parse_variant() {
        fn parse() -> Result<()> {
            Err(ParseError::NodeLimitExceeded { limit: 5 })?
        }

        let err = parse().expect_err("parse は常に失敗する");
        assert!(matches!(
            err,
            Error::Parse(ParseError::NodeLimitExceeded { limit: 5 })
        ));
    }

    /// CORE-1（TASK-24.4・#38）: `ParseError` が `Send + Sync + 'static` を
    /// 満たすことを静的に確認する（`Error` 全体の bound を壊さないことの保証）。
    #[test]
    fn core_1_parse_error_is_send_sync_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<ParseError>();
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

    /// CORE-1（TASK-24.2・#36）: `Error::DisallowedAddress` の `Display` が
    /// アドレスのみを含む。
    #[test]
    fn core_1_display_disallowed_address_variant() {
        let err = Error::DisallowedAddress {
            address: "127.0.0.1".to_string(),
        };
        assert_eq!(err.to_string(), "disallowed target address: 127.0.0.1");
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
            Error::DisallowedAddress {
                address: "10.0.0.1".to_string(),
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

    /// CORE-1（TASK-24（24.10）・#418）: `Error::MatchCacheLimitExceeded` の
    /// `Display` が上限値を含み、`source()` が `None` を返す。
    #[test]
    fn core_1_display_and_source_for_match_cache_limit_exceeded_variant() {
        let err = Error::MatchCacheLimitExceeded { limit: 1_000_000 };
        assert_eq!(
            err.to_string(),
            "selector match cache exceeded limit of 1000000 entries"
        );
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            None::<String>
        );
    }

    /// TASK-91（91.1）・Issue #214: `Error::Config` の `Display` が内部の
    /// `ConfigError` メッセージを包んで表示し、`source()` が連鎖することを
    /// 確認する。
    #[test]
    fn task_91_1_display_and_source_chain_for_config_variant() {
        let source = crate::config::ConfigError::InvalidValue {
            key: "profile.root",
            message: "profile.root must not be empty".to_string(),
        };
        let err = Error::Config(source);
        assert_eq!(
            err.to_string(),
            "configuration error: invalid value for \"profile.root\": profile.root must not be \
             empty"
        );
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            Some("invalid value for \"profile.root\": profile.root must not be empty".to_string())
        );
    }

    /// TASK-91（91.1）・Issue #214: `?` 演算子で使うための
    /// `From<ConfigError> for Error` 変換を確認する。
    #[test]
    fn task_91_1_from_config_error_converts_to_config_variant() {
        fn load() -> Result<()> {
            Err(crate::config::ConfigError::InvalidUtf8)?
        }

        let err = load().expect_err("load は常に失敗する");
        assert!(matches!(
            err,
            Error::Config(crate::config::ConfigError::InvalidUtf8)
        ));
    }

    /// TASK-100.3・Issue #267・`PLUG-8`: `BrowserProfileLoad` の Display・source・`From`。
    #[test]
    fn plug8_browser_profile_load_display_source_and_from() {
        use crate::cssom_profile::ProfileLoadError;
        fn load() -> Result<()> {
            Err(ProfileLoadError::BrowserMismatch)?
        }
        let err = load().expect_err("load は常に失敗する");
        assert!(matches!(
            err,
            Error::BrowserProfileLoad(ProfileLoadError::BrowserMismatch)
        ));
        assert_eq!(
            err.to_string(),
            "browser profile data error: profile data browser does not match requested profile"
        );
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            Some("profile data browser does not match requested profile".to_string())
        );
    }

    /// TASK-100.3・Issue #267・`PLUG-8`: `BrowserProfileName` の Display・source・`From`。
    #[test]
    fn plug8_browser_profile_name_display_source_and_from() {
        use crate::cssom_profile::BrowserProfileParseError;
        fn parse() -> Result<()> {
            Err(BrowserProfileParseError::Unknown {
                value: "firefox".to_string(),
            })?
        }
        let err = parse().expect_err("parse は常に失敗する");
        assert_eq!(
            err.to_string(),
            "invalid browser profile: unknown browser profile 'firefox' (expected chrome or safari)"
        );
        assert_eq!(
            std::error::Error::source(&err).map(ToString::to_string),
            Some("unknown browser profile 'firefox' (expected chrome or safari)".to_string())
        );
    }
}

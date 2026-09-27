//! `fandhe-browser-core` の各モジュール（`fetch`/`parse`/`dom`/`query`/
//! `js_stub`）が将来の計測で使う、可観測性データの共通レコード型。
//!
//! `REPAIR-9`（Must・検討中）は各モジュールの操作結果を、操作種別・成功/失敗・
//! レイテンシ・タイムスタンプを含む構造化ログまたはメトリクス形式で出力する
//! ことを求めている（`TASK-10（10.1）`・`MS-4`）。本モジュールはその最初の
//! 一歩として、全モジュール共通のレコード型（[`OperationRecord`]）とその
//! 構成要素（[`OperationKind`]・[`OperationOutcome`]・[`FailureKind`]）だけを
//! 定義する。各モジュール（fetch/parse/dom/query/js_stub）への計測の組み込み・
//! 出力先（ファイル・外部基盤）・保持期間は本モジュールの範囲外であり、
//! `TASK-10` の後続サブ Issue と Issue #218（`TASK-10（10.h1）`。出力形式・
//! 保持期間・収集基盤の決定。担当は人間、本 Issue 時点で未決）が担当する。
//!
//! [`OperationRecord::to_json_line`] は、workspace が `serde` 等のシリアライズ
//! 用クレートを持たない現時点での **暫定** エンコーダである（dependency-policy.md
//! の依存最小・ユーザー承認制のため、本 Issue では依存を追加しない）。
//! 出力形式が #218 で確定した際は、本関数の差し替え、または `serde` 導入
//! （導入する場合は改めてユーザー承認を要る）が見込まれる。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// 計測対象の操作種別（`fetch`/`parse`/`dom`/`query`/`js_stub` の各モジュール
/// に対応）。
///
/// `#[non_exhaustive]` により、後続タスクでの種別追加を非破壊にする
/// （REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationKind {
    /// `fetch` モジュールの操作。
    Fetch,
    /// `parse` モジュールの操作。
    Parse,
    /// `dom` モジュールの操作。
    Dom,
    /// `query` モジュールの操作。
    Query,
    /// `js_stub` モジュールの操作。
    JsStub,
}

impl OperationKind {
    /// ログ・メトリクス出力で使う、固定の snake_case ASCII 文字列を返す。
    ///
    /// 自由形式の文字列を持たせず固定値のみを返すことで、出力側での
    /// エスケープ処理を不要にし、ログ偽造・CRLF 混入の経路を構造的に
    /// 排除する（security.md「インジェクション」対策）。
    pub const fn as_str(self) -> &'static str {
        match self {
            OperationKind::Fetch => "fetch",
            OperationKind::Parse => "parse",
            OperationKind::Dom => "dom",
            OperationKind::Query => "query",
            OperationKind::JsStub => "js_stub",
        }
    }
}

/// 操作が失敗した場合の分類。[`crate::error::Error`] の各バリアントと
/// 1 対 1 に対応する。
///
/// `#[non_exhaustive]` により、後続タスクでの `Error` バリアント追加時に
/// 種別を追加できる（REPAIR-4）。`Error` が保持する message・scheme・
/// address 等の中身はここへ一切持ち込まない。`error.rs` が注意している
/// 秘密情報・URL の漏えい経路をログ側で作り直さないためである
/// （security.md「機密データの露出」対策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailureKind {
    /// [`Error::Io`] に対応。
    Io,
    /// [`Error::InvalidInput`] に対応。
    InvalidInput,
    /// [`Error::Unsupported`] に対応。
    Unsupported,
    /// [`Error::JsExecutionUnavailable`] に対応。
    JsExecutionUnavailable,
    /// [`Error::Parse`] に対応。
    Parse,
    /// [`Error::Timeout`] に対応。
    Timeout,
    /// [`Error::TooManyRedirects`] に対応。
    TooManyRedirects,
    /// [`Error::ResponseTooLarge`] に対応。
    ResponseTooLarge,
    /// [`Error::DisallowedScheme`] に対応。
    DisallowedScheme,
    /// [`Error::DisallowedAddress`] に対応。
    DisallowedAddress,
    /// [`Error::TooManyConcurrentDnsResolutions`] に対応。
    TooManyConcurrentDnsResolutions,
    /// [`Error::Network`] に対応。
    Network,
    /// [`Error::MatchCacheLimitExceeded`] に対応。
    MatchCacheLimitExceeded,
}

impl FailureKind {
    /// ログ・メトリクス出力で使う、固定の snake_case ASCII 文字列を返す。
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureKind::Io => "io",
            FailureKind::InvalidInput => "invalid_input",
            FailureKind::Unsupported => "unsupported",
            FailureKind::JsExecutionUnavailable => "js_execution_unavailable",
            FailureKind::Parse => "parse",
            FailureKind::Timeout => "timeout",
            FailureKind::TooManyRedirects => "too_many_redirects",
            FailureKind::ResponseTooLarge => "response_too_large",
            FailureKind::DisallowedScheme => "disallowed_scheme",
            FailureKind::DisallowedAddress => "disallowed_address",
            FailureKind::TooManyConcurrentDnsResolutions => "too_many_concurrent_dns_resolutions",
            FailureKind::Network => "network",
            FailureKind::MatchCacheLimitExceeded => "match_cache_limit_exceeded",
        }
    }
}

impl From<&Error> for FailureKind {
    /// `Error` の種別だけを写し取る。message・scheme・address 等の中身は
    /// 破棄し、レコードへ持ち込まない（security.md「機密データの露出」対策）。
    ///
    /// `Error` は `#[non_exhaustive]` だが、crate 内からの参照ではこの
    /// 属性は効かず網羅的な `match` を書ける。ワイルドカード（`_`）を
    /// 使わないことで、`Error` に将来バリアントが増えた際にコンパイル
    /// エラーで気付ける。
    fn from(error: &Error) -> Self {
        match error {
            Error::Io(_) => FailureKind::Io,
            Error::InvalidInput { .. } => FailureKind::InvalidInput,
            Error::Unsupported { .. } => FailureKind::Unsupported,
            Error::JsExecutionUnavailable { .. } => FailureKind::JsExecutionUnavailable,
            Error::Parse(_) => FailureKind::Parse,
            Error::Timeout { .. } => FailureKind::Timeout,
            Error::TooManyRedirects { .. } => FailureKind::TooManyRedirects,
            Error::ResponseTooLarge { .. } => FailureKind::ResponseTooLarge,
            Error::DisallowedScheme { .. } => FailureKind::DisallowedScheme,
            Error::DisallowedAddress { .. } => FailureKind::DisallowedAddress,
            Error::TooManyConcurrentDnsResolutions { .. } => {
                FailureKind::TooManyConcurrentDnsResolutions
            }
            Error::Network { .. } => FailureKind::Network,
            Error::MatchCacheLimitExceeded { .. } => FailureKind::MatchCacheLimitExceeded,
        }
    }
}

/// 操作の成否。失敗時は [`FailureKind`] を保持する。
///
/// `#[non_exhaustive]` により、`Failure` 以外の結果種別が将来必要になっても
/// 非破壊に追加できる（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationOutcome {
    /// 操作が成功した。
    Success,
    /// 操作が失敗した。
    Failure {
        /// 失敗の分類。
        kind: FailureKind,
    },
}

impl OperationOutcome {
    /// ログ・メトリクス出力で使う、固定の snake_case ASCII 文字列を返す。
    pub const fn as_str(self) -> &'static str {
        match self {
            OperationOutcome::Success => "success",
            OperationOutcome::Failure { .. } => "failure",
        }
    }

    /// `Result<T>` から [`OperationOutcome`] を作る利便関数。
    ///
    /// 呼び出し元（各モジュールの計測コード）が `Result` をそのまま渡せる
    /// ようにする薄いラッパー。
    pub fn from_result<T>(result: &Result<T>) -> Self {
        match result {
            Ok(_) => OperationOutcome::Success,
            Err(error) => OperationOutcome::Failure {
                kind: FailureKind::from(error),
            },
        }
    }
}

/// 1 回の操作計測を表す共通レコード。
///
/// フィールドは private とし、getter（[`OperationRecord::operation`] 等）を
/// 通してのみ参照させる。将来フィールドを追加してもコンストラクタ経由の
/// 呼び出し元を破壊しないためである（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OperationRecord {
    operation: OperationKind,
    outcome: OperationOutcome,
    latency: Duration,
    timestamp_unix_ms: u64,
}

impl OperationRecord {
    /// 操作種別・結果・レイテンシ・計測時刻からレコードを作る。
    ///
    /// `timestamp` を明示的に受け取ることで、呼び出し元（テストを含む）が
    /// 決定的な値を渡せるようにする。`timestamp` が UNIX エポックより前
    /// （`duration_since` が失敗する場合）は `Error::InvalidInput` を返す。
    /// `timestamp` のエポックからの経過時間（`as_millis()` が返す `u128`）が
    /// `u64` に収まらない場合も同様に `Error::InvalidInput` を返す
    /// （`latency` はここでは変換しない。`u64` への変換が必要になるのは
    /// [`OperationRecord::to_json_line`] が `latency_us` を組み立てる時点で、
    /// そちらは収まらない場合に `u64::MAX` へ飽和させる。詳細は同メソッドの
    /// ドキュメントを参照）。`unwrap`/`expect` は使わない
    /// （coding-rust.md「外部入力の経路」相当の防御的実装）。
    pub fn new(
        operation: OperationKind,
        outcome: OperationOutcome,
        latency: Duration,
        timestamp: SystemTime,
    ) -> Result<Self> {
        let since_epoch =
            timestamp
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Error::InvalidInput {
                    message: "timestamp is before unix epoch".to_string(),
                })?;
        let timestamp_unix_ms =
            u64::try_from(since_epoch.as_millis()).map_err(|_| Error::InvalidInput {
                message: "timestamp is too far in the future to fit in u64 milliseconds"
                    .to_string(),
            })?;
        Ok(Self {
            operation,
            outcome,
            latency,
            timestamp_unix_ms,
        })
    }

    /// `SystemTime::now()` を計測時刻として使う、[`OperationRecord::new`] の
    /// 薄い包み。
    pub fn now(
        operation: OperationKind,
        outcome: OperationOutcome,
        latency: Duration,
    ) -> Result<Self> {
        Self::new(operation, outcome, latency, SystemTime::now())
    }

    /// 計測対象の操作種別。
    pub const fn operation(&self) -> OperationKind {
        self.operation
    }

    /// 操作の成否。
    pub const fn outcome(&self) -> OperationOutcome {
        self.outcome
    }

    /// 計測されたレイテンシ。
    pub const fn latency(&self) -> Duration {
        self.latency
    }

    /// UNIX エポックからのミリ秒での計測時刻。
    pub const fn timestamp_unix_ms(&self) -> u64 {
        self.timestamp_unix_ms
    }

    /// レコードを JSON 1 行（JSON Lines 互換）へエンコードする。
    ///
    /// 出力形式・保持期間・収集基盤は Issue #218（`TASK-10（10.h1）`）で
    /// 未確定であり、本関数は **暫定** エンコーダである。`serde` 等の外部
    /// クレートを使わず、フィールド値が enum の固定 ASCII 文字列・整数の
    /// みであることを前提に手書きで組み立てる。自由形式の文字列を含まない
    /// ため、改行・引用符のエスケープ処理は不要（security.md
    /// 「インジェクション」対策）。
    ///
    /// フィールド順は固定: `operation` → `outcome` →（失敗時のみ）
    /// `failure_kind` → `latency_us` → `timestamp_unix_ms`。
    ///
    /// `latency_us` は `Duration::as_micros()`（`u128`）を `u64` へ変換した
    /// 値で、`u64` に収まらないほど大きい場合は `u64::MAX` へ飽和させる。
    /// レイテンシがこの桁に達することは実運用上あり得ず、エラーにするより
    /// 上限値を記録しておく方が可観測性データとして有用なため（呼び出し元
    /// の計測処理を失敗させたくない）。
    pub fn to_json_line(&self) -> String {
        let latency_us = u64::try_from(self.latency.as_micros()).unwrap_or(u64::MAX);
        // 想定最大長（`js_stub` + `too_many_concurrent_dns_resolutions` +
        // 2 つの u64::MAX 相当の桁数）に少し余裕を持たせた初期容量。
        // 超過してもアロケーションが 1 回増えるだけで、正しさには影響しない。
        let mut line = String::with_capacity(192);
        line.push('{');
        line.push_str("\"operation\":\"");
        line.push_str(self.operation.as_str());
        line.push_str("\",\"outcome\":\"");
        line.push_str(self.outcome.as_str());
        line.push('"');
        if let OperationOutcome::Failure { kind } = self.outcome {
            line.push_str(",\"failure_kind\":\"");
            line.push_str(kind.as_str());
            line.push('"');
        }
        line.push_str(",\"latency_us\":");
        line.push_str(&latency_us.to_string());
        line.push_str(",\"timestamp_unix_ms\":");
        line.push_str(&self.timestamp_unix_ms.to_string());
        line.push('}');
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `REPAIR-9`: `OperationKind::as_str` が全種別で安定した文字列を返す。
    #[test]
    fn repair_9_operation_kind_as_str_is_stable() {
        assert_eq!(OperationKind::Fetch.as_str(), "fetch");
        assert_eq!(OperationKind::Parse.as_str(), "parse");
        assert_eq!(OperationKind::Dom.as_str(), "dom");
        assert_eq!(OperationKind::Query.as_str(), "query");
        assert_eq!(OperationKind::JsStub.as_str(), "js_stub");
    }

    /// `REPAIR-9`: `FailureKind::as_str` が全 13 種別で安定した文字列を返す。
    /// ログ出力契約の本体であり、将来のタイポ（例:
    /// `too_many_concurrent_dns_resolutions` の誤記）を CI で検出するため
    /// 全 variant を具体値で検証する（coding-rust.md「期待値は具体値で書く」）。
    #[test]
    fn repair_9_failure_kind_as_str_is_stable() {
        assert_eq!(FailureKind::Io.as_str(), "io");
        assert_eq!(FailureKind::InvalidInput.as_str(), "invalid_input");
        assert_eq!(FailureKind::Unsupported.as_str(), "unsupported");
        assert_eq!(
            FailureKind::JsExecutionUnavailable.as_str(),
            "js_execution_unavailable"
        );
        assert_eq!(FailureKind::Parse.as_str(), "parse");
        assert_eq!(FailureKind::Timeout.as_str(), "timeout");
        assert_eq!(FailureKind::TooManyRedirects.as_str(), "too_many_redirects");
        assert_eq!(FailureKind::ResponseTooLarge.as_str(), "response_too_large");
        assert_eq!(FailureKind::DisallowedScheme.as_str(), "disallowed_scheme");
        assert_eq!(
            FailureKind::DisallowedAddress.as_str(),
            "disallowed_address"
        );
        assert_eq!(
            FailureKind::TooManyConcurrentDnsResolutions.as_str(),
            "too_many_concurrent_dns_resolutions"
        );
        assert_eq!(FailureKind::Network.as_str(), "network");
        assert_eq!(
            FailureKind::MatchCacheLimitExceeded.as_str(),
            "match_cache_limit_exceeded"
        );
    }

    /// `REPAIR-9`: 成功レコードのシリアライズ結果が期待する JSON 文字列と
    /// 完全一致する。
    #[test]
    fn repair_9_success_record_serializes_to_expected_json() {
        let timestamp = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let record = OperationRecord::new(
            OperationKind::Fetch,
            OperationOutcome::Success,
            Duration::from_micros(1500),
            timestamp,
        )
        .expect("timestamp is within range");
        assert_eq!(
            record.to_json_line(),
            "{\"operation\":\"fetch\",\"outcome\":\"success\",\"latency_us\":1500,\"timestamp_unix_ms\":1700000000000}"
        );
    }

    /// `REPAIR-9`: 失敗レコードには `outcome` の直後に `failure_kind` が
    /// 含まれる。
    #[test]
    fn repair_9_failure_record_includes_failure_kind() {
        let timestamp = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let record = OperationRecord::new(
            OperationKind::Fetch,
            OperationOutcome::Failure {
                kind: FailureKind::Timeout,
            },
            Duration::from_micros(1500),
            timestamp,
        )
        .expect("timestamp is within range");
        assert_eq!(
            record.to_json_line(),
            "{\"operation\":\"fetch\",\"outcome\":\"failure\",\"failure_kind\":\"timeout\",\"latency_us\":1500,\"timestamp_unix_ms\":1700000000000}"
        );
    }

    /// `REPAIR-9`: `Error` から `FailureKind` への変換が message 等の中身を
    /// 出力へ持ち込まない。
    #[test]
    fn repair_9_failure_kind_from_error_drops_message() {
        let invalid_input = Error::InvalidInput {
            message: "secret-token".to_string(),
        };
        let kind = FailureKind::from(&invalid_input);
        assert_eq!(kind, FailureKind::InvalidInput);
        let record = OperationRecord::now(
            OperationKind::Parse,
            OperationOutcome::Failure { kind },
            Duration::ZERO,
        )
        .expect("SystemTime::now() is after unix epoch");
        assert!(!record.to_json_line().contains("secret-token"));

        let disallowed_address = Error::DisallowedAddress {
            address: "127.0.0.1".to_string(),
        };
        let kind = FailureKind::from(&disallowed_address);
        assert_eq!(kind, FailureKind::DisallowedAddress);
        let record = OperationRecord::now(
            OperationKind::Fetch,
            OperationOutcome::Failure { kind },
            Duration::ZERO,
        )
        .expect("SystemTime::now() is after unix epoch");
        assert!(!record.to_json_line().contains("127.0.0.1"));
    }

    /// `REPAIR-9`: `OperationOutcome::from_result` が `Ok`/`Err` を正しく
    /// 写像する。
    #[test]
    fn repair_9_outcome_from_result() {
        let ok: Result<()> = Ok(());
        assert_eq!(
            OperationOutcome::from_result(&ok),
            OperationOutcome::Success
        );

        let err: Result<()> = Err(Error::Timeout {
            limit: Duration::from_secs(30),
        });
        assert_eq!(
            OperationOutcome::from_result(&err),
            OperationOutcome::Failure {
                kind: FailureKind::Timeout
            }
        );
    }

    /// `REPAIR-9`: UNIX エポックより前のタイムスタンプは panic せず
    /// `Error::InvalidInput` を返す。
    #[test]
    fn repair_9_timestamp_before_epoch_is_error() {
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        let result = OperationRecord::new(
            OperationKind::Dom,
            OperationOutcome::Success,
            Duration::ZERO,
            before_epoch,
        );
        match result {
            Err(Error::InvalidInput { message }) => {
                assert_eq!(message, "timestamp is before unix epoch");
            }
            other => panic!("expected Error::InvalidInput, got {other:?}"),
        }
    }

    /// `REPAIR-9`: `Duration::MAX` のレイテンシは `u64::MAX` へ飽和する。
    #[test]
    fn repair_9_latency_overflow_saturates() {
        let timestamp = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let record = OperationRecord::new(
            OperationKind::Query,
            OperationOutcome::Success,
            Duration::MAX,
            timestamp,
        )
        .expect("timestamp is within range");
        assert_eq!(
            record.to_json_line(),
            "{\"operation\":\"query\",\"outcome\":\"success\",\"latency_us\":18446744073709551615,\"timestamp_unix_ms\":1700000000000}"
        );
    }

    /// `REPAIR-9`: 出力に改行・復帰が含まれず、`{`/`}` で始終する。
    #[test]
    fn repair_9_json_line_has_no_newline() {
        let timestamp = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let record = OperationRecord::new(
            OperationKind::JsStub,
            OperationOutcome::Success,
            Duration::from_micros(1),
            timestamp,
        )
        .expect("timestamp is within range");
        let line = record.to_json_line();
        assert!(!line.contains('\n'));
        assert!(!line.contains('\r'));
        assert!(line.starts_with('{'));
        assert!(line.ends_with('}'));
    }
}

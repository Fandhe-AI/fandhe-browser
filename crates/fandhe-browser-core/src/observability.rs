//! `fandhe-browser-core` の各モジュール（`fetch`/`parse`/`dom`/`query`/
//! `js_stub`）が将来の計測で使う、可観測性データの共通レコード型。
//!
//! `REPAIR-9`（Must・検討中）は各モジュールの操作結果を、操作種別・成功/失敗・
//! レイテンシ・タイムスタンプを含む構造化ログまたはメトリクス形式で出力する
//! ことを求めている（`TASK-10（10.1）`・`MS-4`）。本モジュールはその最初の
//! 一歩として、全モジュール共通のレコード型（[`OperationRecord`]）とその
//! 構成要素（[`OperationKind`]・[`OperationOutcome`]・[`FailureKind`]）だけを
//! 定義する。
//!
//! `TASK-10.2.1`（Issue #549）で、レコードの受け口（[`OperationRecorder`]・
//! [`RecorderHandle`]）とテスト・簡易集計用の有界メモリ内集計器
//! （[`InMemoryRecorder`]）を追加し、`fetch`・`parse`・`dom` へ計装を組み込んだ。
//! `query`・`js_stub` へは Issue #550（`TASK-10.2.2`）で組み込み済み。
//!
//! 出力形式・保持期間・収集基盤は Issue #218（`TASK-10（10.h1）`）で決定済みで、
//! 詳細は `docs/design/observability.md` に記録している（JSON Lines のみ・既定無効・
//! 有効時の出力先は stderr・外部収集基盤は MVP で採用しない）。Issue #221
//! （`TASK-10.3`）で、その決定に沿った出力先 [`JsonLinesRecorder`]（stderr 向けの
//! 別名 [`StderrRecorder`]）を追加した。config の `[observability]` セクション・CLI
//! フラグ・ファイル出力（ローテーション）・起動時イベントは未配線で、後続の担当
//! （`REPAIR-9` の残り）である。recorder は呼び出し側が options 経由で明示注入する
//! 方式で、未設定（既定）なら一切記録しない（process-global は並列テストで記録が
//! 混ざるため採らない）。
//!
//! [`OperationRecord::to_json_line`] は `serde` を使わない自前のエンコーダで、#218 の
//! 決定により JSON Lines の 1 行表現としてそのまま採用する（依存追加なし）。

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
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
    /// [`Error::JsEvaluation`] に対応（TASK-30（30.3）・`JS-2`）。
    JsEvaluation,
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
    /// [`Error::Config`] に対応（TASK-91（91.1）・Issue #214）。
    Config,
    /// [`Error::BrowserProfileLoad`] に対応（TASK-100.3・Issue #267・`PLUG-8`）。
    BrowserProfileLoad,
    /// [`Error::BrowserProfileName`] に対応（TASK-100.3・Issue #267・`PLUG-8`）。
    BrowserProfileName,
    /// [`Error::Dom`] に対応（TASK-107・Issue #772・`JS-5`/`JS-6`）。
    Dom,
}

impl FailureKind {
    /// ログ・メトリクス出力で使う、固定の snake_case ASCII 文字列を返す。
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureKind::Io => "io",
            FailureKind::InvalidInput => "invalid_input",
            FailureKind::Unsupported => "unsupported",
            FailureKind::JsExecutionUnavailable => "js_execution_unavailable",
            FailureKind::JsEvaluation => "js_evaluation",
            FailureKind::Parse => "parse",
            FailureKind::Timeout => "timeout",
            FailureKind::TooManyRedirects => "too_many_redirects",
            FailureKind::ResponseTooLarge => "response_too_large",
            FailureKind::DisallowedScheme => "disallowed_scheme",
            FailureKind::DisallowedAddress => "disallowed_address",
            FailureKind::TooManyConcurrentDnsResolutions => "too_many_concurrent_dns_resolutions",
            FailureKind::Network => "network",
            FailureKind::MatchCacheLimitExceeded => "match_cache_limit_exceeded",
            FailureKind::Config => "config",
            FailureKind::BrowserProfileLoad => "browser_profile_load",
            FailureKind::BrowserProfileName => "browser_profile_name",
            FailureKind::Dom => "dom",
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
            Error::JsEvaluation(_) => FailureKind::JsEvaluation,
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
            Error::Config(_) => FailureKind::Config,
            Error::BrowserProfileLoad(_) => FailureKind::BrowserProfileLoad,
            Error::BrowserProfileName(_) => FailureKind::BrowserProfileName,
            Error::Dom(_) => FailureKind::Dom,
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

/// 操作レコードの受け口（`REPAIR-9`・`TASK-10.2.1`）。
///
/// `fetch`・`parse`・`dom`・`query`・`js_stub` の計装コードが 1 操作ごとに [`OperationRecord`] を
/// 渡す。in-process の Rust トレイトであり、プラグイン境界（PLUG 系）でも
/// 動的ライブラリのロードでもない。
///
/// 実装は次の契約を守ること。async の `fetch` ホットパスから同期的に
/// 呼ばれるため、(1) panic しない、(2) 長時間ブロックしない、(3) 記録の
/// 失敗を操作の失敗へ波及させない（戻り値を持たない理由）。
pub trait OperationRecorder: Send + Sync {
    /// レコードを 1 件受け取る。
    fn record(&self, record: &OperationRecord);
}

/// [`OperationRecorder`] の共有ハンドル。既定は無効（何も記録しない）。
///
/// `FetchOptions`・`ParseOptions`・`JsStubOptions`・`dom::Document` が保持し
/// （`query` は照合対象 `Document` のものを使う）、計装箇所は
/// [`RecorderHandle::is_enabled`] で無効時の `Instant::now()` を省ける。
#[derive(Clone, Default)]
pub struct RecorderHandle {
    inner: Option<Arc<dyn OperationRecorder>>,
}

impl fmt::Debug for RecorderHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecorderHandle")
            .field("enabled", &self.inner.is_some())
            .finish()
    }
}

impl RecorderHandle {
    /// `recorder` へ記録する有効なハンドルを作る。
    pub fn new(recorder: Arc<dyn OperationRecorder>) -> Self {
        Self {
            inner: Some(recorder),
        }
    }

    /// 何も記録しない無効なハンドルを作る（[`Default`] と同義）。
    pub fn disabled() -> Self {
        Self::default()
    }

    /// recorder が設定されているか。
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// `Result` の成否から [`OperationRecord`] を作って記録する。無効なら何もしない。
    pub(crate) fn record_result<T>(
        &self,
        kind: OperationKind,
        result: &Result<T>,
        latency: Duration,
    ) {
        self.record_outcome(kind, OperationOutcome::from_result(result), latency);
    }

    /// 成否を指定して記録する。`Result` を返さない操作（dom 等）向け。
    pub(crate) fn record_outcome(
        &self,
        kind: OperationKind,
        outcome: OperationOutcome,
        latency: Duration,
    ) {
        let Some(recorder) = &self.inner else {
            return;
        };
        // システム時計が UNIX エポックより前の場合はレコードを黙って捨てる。
        // 記録の失敗で本来の操作（fetch/parse/dom）を失敗させないため。
        if let Ok(record) = OperationRecord::now(kind, outcome, latency) {
            recorder.record(&record);
        }
    }
}

/// [`InMemoryRecorder::counts`] の集計結果。
///
/// `#[non_exhaustive]`（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct OperationCounts {
    /// 成功した操作の件数。
    pub success: u64,
    /// 失敗した操作の件数。
    pub failure: u64,
}

#[derive(Debug, Default)]
struct InMemoryInner {
    records: Vec<OperationRecord>,
    capacity: usize,
    dropped: u64,
}

/// `Vec` の事前確保の上限。巨大な `capacity` 指定による確保増幅を防ぐ。
const IN_MEMORY_PREALLOC_LIMIT: usize = 64;

/// 件数上限付きのメモリ内 [`OperationRecorder`]。
///
/// **テスト・簡易集計用のユーティリティであり、本番の出力先ではない**
/// （本番の出力先は [`StderrRecorder`]。`REPAIR-9`・`TASK-10.3`・#221）。
/// 上限超過分は捨てて [`InMemoryRecorder::dropped`] に数える。
#[derive(Debug, Default)]
pub struct InMemoryRecorder {
    inner: Mutex<InMemoryInner>,
}

impl InMemoryRecorder {
    /// 最大 `capacity` 件を保持する recorder を作る。
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(InMemoryInner {
                records: Vec::with_capacity(capacity.min(IN_MEMORY_PREALLOC_LIMIT)),
                capacity,
                dropped: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, InMemoryInner> {
        // poison は回復して続行する（記録側の panic を呼び出し元へ波及させない）。
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 保持中のレコードのコピーを記録順に返す。
    pub fn records(&self) -> Vec<OperationRecord> {
        self.lock().records.clone()
    }

    /// 上限超過で捨てたレコード数。
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// 保持中のレコードから `kind` の成功・失敗件数を数える。
    /// 上限超過で捨てたレコードは数えない。
    pub fn counts(&self, kind: OperationKind) -> OperationCounts {
        let inner = self.lock();
        let mut counts = OperationCounts::default();
        for record in inner.records.iter().filter(|r| r.operation() == kind) {
            match record.outcome() {
                OperationOutcome::Success => counts.success = counts.success.saturating_add(1),
                _ => counts.failure = counts.failure.saturating_add(1),
            }
        }
        counts
    }
}

impl OperationRecorder for InMemoryRecorder {
    fn record(&self, record: &OperationRecord) {
        let mut inner = self.lock();
        if inner.records.len() < inner.capacity {
            inner.records.push(*record);
        } else {
            inner.dropped = inner.dropped.saturating_add(1);
        }
    }
}

/// 有界キューの既定容量（件数）。1 件は高々数百バイトの JSON 1 行なので、
/// 満杯時でも保持するメモリは小さい。
pub const DEFAULT_JSON_LINES_QUEUE_CAPACITY: usize = 1024;

/// JSON Lines（1 レコード 1 行）で `W` へ書き出す [`OperationRecorder`]
/// （`REPAIR-9`・`TASK-10.3`・Issue #221。形式は #218 の決定）。
///
/// cli が生成し、`FetchOptions::with_recorder`・`ParseOptions::with_recorder`・
/// `JsStubOptions::with_recorder`・`Document::set_recorder` へ
/// `Arc<dyn OperationRecorder>` として注入する想定（現時点で core の外に注入箇所は
/// なく、config・CLI からの有効化は未配線）。各行は [`OperationRecord::to_json_line`]
/// の結果に `\n` を 1 つ付けたもので、操作種別・成否・レイテンシ・タイムスタンプを含む。
///
/// 契約（`REPAIR-9`・`TASK-10.3`）: [`OperationRecorder::record`] は呼び出し元
/// （async の fetch 経路を含む）を**ブロックしない**。`record` は行を有界キューへ
/// `try_send` するだけで、`write_all` と `flush` は専用の writer スレッドが行う。
/// キューが満杯（受け手が詰まり writer が進まない場合など）のときは、そのレコードを
/// 捨てて [`JsonLinesRecorder::dropped`] に数え、操作は継続する。panic しない。
/// 書き込み・flush の失敗は握りつぶして [`JsonLinesRecorder::write_failures`] に数える
/// （操作の失敗へ波及させない）。1 行は 1 回の `write_all` で書くため行は混ざらず、
/// 記録順は（捨てられない限り）キューへ入れた順に保たれる。
pub struct JsonLinesRecorder<W: Write + Send + 'static> {
    tx: SyncSender<String>,
    /// writer スレッド。起動に失敗した場合は `None`（全レコードを `dropped` に数える）。
    worker: Option<JoinHandle<W>>,
    write_failures: Arc<AtomicU64>,
    dropped: AtomicU64,
}

/// stderr へ JSON Lines を書き出す recorder（`REPAIR-9`・`TASK-10.3`）。
pub type StderrRecorder = JsonLinesRecorder<io::Stderr>;

impl<W: Write + Send + 'static> JsonLinesRecorder<W> {
    /// 既定容量（[`DEFAULT_JSON_LINES_QUEUE_CAPACITY`]）のキューで `writer` へ書き出す
    /// recorder を作り、writer スレッドを起動する。
    pub fn new(writer: W) -> Self {
        Self::with_capacity(writer, DEFAULT_JSON_LINES_QUEUE_CAPACITY)
    }

    /// キュー容量 `capacity` 件（0 は 1 件に切り上げ）で recorder を作る。
    pub fn with_capacity(writer: W, capacity: usize) -> Self {
        let (tx, rx) = sync_channel::<String>(capacity.max(1));
        let write_failures = Arc::new(AtomicU64::new(0));
        let failures = Arc::clone(&write_failures);
        let worker = std::thread::Builder::new()
            .name("fandhe-jsonl-recorder".to_owned())
            .spawn(move || {
                let mut writer = writer;
                // 送信側が全て閉じ、キューを掃き終えたら終了する。
                while let Ok(line) = rx.recv() {
                    let result = writer
                        .write_all(line.as_bytes())
                        .and_then(|()| writer.flush());
                    if result.is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
                writer
            })
            .ok();
        Self {
            tx,
            worker,
            write_failures,
            dropped: AtomicU64::new(0),
        }
    }

    /// 書き込み（または flush）に失敗した件数。
    pub fn write_failures(&self) -> u64 {
        self.write_failures.load(Ordering::Relaxed)
    }

    /// キュー満杯（または writer スレッド不在）で捨てたレコード数。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// キューを掃き終えるまで待って writer を取り出す（テスト・終了時用）。
    ///
    /// writer がブロックしている間は戻らない。writer スレッドが起動できなかった、
    /// または writer が panic した場合は `None`。
    pub fn into_inner(self) -> Option<W> {
        let Self { tx, worker, .. } = self;
        drop(tx);
        worker?.join().ok()
    }
}

impl JsonLinesRecorder<io::Stderr> {
    /// stderr 向けの recorder を作る。
    pub fn stderr() -> Self {
        Self::new(io::stderr())
    }
}

impl<W: Write + Send + 'static> fmt::Debug for JsonLinesRecorder<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonLinesRecorder")
            .field("write_failures", &self.write_failures())
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

impl<W: Write + Send + 'static> OperationRecorder for JsonLinesRecorder<W> {
    fn record(&self, record: &OperationRecord) {
        let mut line = record.to_json_line();
        line.push('\n');
        // ブロックしない。満杯・切断のときは捨てて数える。
        if self.tx.try_send(line).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: OperationKind, outcome: OperationOutcome) -> OperationRecord {
        OperationRecord::now(kind, outcome, Duration::ZERO).expect("now is after epoch")
    }

    /// `REPAIR-9`: 上限超過分は捨てて `dropped` に数える。
    #[test]
    fn repair_9_in_memory_recorder_respects_capacity() {
        let r = InMemoryRecorder::with_capacity(2);
        for _ in 0..3 {
            r.record(&rec(OperationKind::Fetch, OperationOutcome::Success));
        }
        assert_eq!(r.records().len(), 2);
        assert_eq!(r.dropped(), 1);
    }

    /// `REPAIR-9`: 種別ごとの成功・失敗件数を数える。
    #[test]
    fn repair_9_in_memory_recorder_counts_by_kind() {
        let r = InMemoryRecorder::with_capacity(10);
        let fail = OperationOutcome::Failure {
            kind: FailureKind::Timeout,
        };
        r.record(&rec(OperationKind::Fetch, OperationOutcome::Success));
        r.record(&rec(OperationKind::Fetch, OperationOutcome::Success));
        r.record(&rec(OperationKind::Fetch, fail));
        r.record(&rec(OperationKind::Parse, OperationOutcome::Success));
        let fetch = r.counts(OperationKind::Fetch);
        assert_eq!((fetch.success, fetch.failure), (2, 1));
        let parse = r.counts(OperationKind::Parse);
        assert_eq!((parse.success, parse.failure), (1, 0));
        let dom = r.counts(OperationKind::Dom);
        assert_eq!((dom.success, dom.failure), (0, 0));
    }

    /// `REPAIR-9`: 無効ハンドルは何もせず、有効ハンドルは 1 件記録する。
    #[test]
    fn repair_9_handle_enabled_and_disabled() {
        let disabled = RecorderHandle::disabled();
        assert!(!disabled.is_enabled());
        disabled.record_outcome(
            OperationKind::Dom,
            OperationOutcome::Success,
            Duration::ZERO,
        );
        let mem = Arc::new(InMemoryRecorder::with_capacity(4));
        let handle = RecorderHandle::new(mem.clone());
        assert!(handle.is_enabled());
        let ok: Result<()> = Ok(());
        handle.record_result(OperationKind::Dom, &ok, Duration::ZERO);
        assert_eq!(mem.records().len(), 1);
    }

    /// `REPAIR-9`: `RecorderHandle` の `Debug` は具体値で安定している。
    #[test]
    fn repair_9_handle_debug_is_stable() {
        assert_eq!(
            format!("{:?}", RecorderHandle::disabled()),
            "RecorderHandle { enabled: false }"
        );
        let handle = RecorderHandle::new(Arc::new(InMemoryRecorder::with_capacity(1)));
        assert_eq!(format!("{handle:?}"), "RecorderHandle { enabled: true }");
    }

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
        assert_eq!(FailureKind::JsEvaluation.as_str(), "js_evaluation");
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

    /// 共有バッファへ書く `Write`（poison・並行テスト用）。
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailWrite;
    impl Write for FailWrite {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailFlush;
    impl Write for FailFlush {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("flush failed"))
        }
    }

    fn fixed_records() -> (OperationRecord, OperationRecord) {
        let ts = UNIX_EPOCH + Duration::from_millis(1_700_000_000_000);
        let ok = OperationRecord::new(
            OperationKind::Fetch,
            OperationOutcome::Success,
            Duration::from_micros(1500),
            ts,
        )
        .expect("timestamp is within range");
        let ng = OperationRecord::new(
            OperationKind::Fetch,
            OperationOutcome::Failure {
                kind: FailureKind::Timeout,
            },
            Duration::from_micros(1500),
            ts,
        )
        .expect("timestamp is within range");
        (ok, ng)
    }

    /// `REPAIR-9`: 1 レコード 1 行で、各行が `to_json_line()` + 改行と完全一致する。
    #[test]
    fn repair_9_json_lines_recorder_writes_one_line_per_record() {
        let (ok, ng) = fixed_records();
        let r = JsonLinesRecorder::new(Vec::<u8>::new());
        r.record(&ok);
        r.record(&ng);
        assert_eq!(r.write_failures(), 0);
        let out = String::from_utf8(r.into_inner().expect("writer")).expect("utf8");
        assert_eq!(
            out,
            format!("{}\n{}\n", ok.to_json_line(), ng.to_json_line())
        );
        assert_eq!(out.lines().count(), 2);
    }

    /// `REPAIR-9`: 出力に操作種別・成否・レイテンシ・タイムスタンプが含まれる。
    #[test]
    fn repair_9_json_lines_output_contains_required_fields() {
        let (_, ng) = fixed_records();
        let r = JsonLinesRecorder::new(Vec::<u8>::new());
        r.record(&ng);
        let out = String::from_utf8(r.into_inner().expect("writer")).expect("utf8");
        for key in [
            "\"operation\":\"fetch\"",
            "\"outcome\":\"failure\"",
            "\"latency_us\":1500",
            "\"timestamp_unix_ms\":1700000000000",
        ] {
            assert!(out.contains(key), "missing {key} in {out}");
        }
    }

    /// 非同期の writer スレッドが条件を満たすまで待つ（最大 10 秒）。
    fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn buf_text(buf: &SharedBuf) -> String {
        let out = buf.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8(out.clone()).expect("utf8")
    }

    /// `REPAIR-9`: 書き込み失敗は握りつぶして数える。
    #[test]
    fn repair_9_json_lines_recorder_swallows_write_errors() {
        let (ok, _) = fixed_records();
        let r = JsonLinesRecorder::new(FailWrite);
        r.record(&ok);
        r.record(&ok);
        wait_until(|| r.write_failures() == 2);
        assert_eq!(r.write_failures(), 2);
        assert_eq!(r.dropped(), 0);
    }

    /// `REPAIR-9`: flush のみ失敗しても失敗として数える。
    #[test]
    fn repair_9_json_lines_recorder_flush_error_counts_as_failure() {
        let (ok, _) = fixed_records();
        let r = JsonLinesRecorder::new(FailFlush);
        r.record(&ok);
        wait_until(|| r.write_failures() == 1);
        assert_eq!(r.write_failures(), 1);
    }

    /// `REPAIR-9`: `RecorderHandle` 経由でも 1 行出力される。
    #[test]
    fn repair_9_json_lines_recorder_via_handle() {
        let buf = SharedBuf::default();
        let handle = RecorderHandle::new(Arc::new(JsonLinesRecorder::new(buf.clone())));
        handle.record_outcome(
            OperationKind::Parse,
            OperationOutcome::Success,
            Duration::from_micros(10),
        );
        wait_until(|| buf_text(&buf).lines().count() == 1);
        let text = buf_text(&buf);
        assert!(text.contains("\"operation\":\"parse\""), "{text}");
    }

    /// `REPAIR-9`: 並行記録でも行が混ざらない（4 スレッド × 25 件 = 100 行）。
    #[test]
    fn repair_9_json_lines_recorder_concurrent_lines_not_interleaved() {
        let (ok, _) = fixed_records();
        let buf = SharedBuf::default();
        let r = Arc::new(JsonLinesRecorder::new(buf.clone()));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let r = Arc::clone(&r);
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        r.record(&ok);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(r.dropped(), 0);
        wait_until(|| buf_text(&buf).lines().count() == 100);
        let text = buf_text(&buf);
        for line in text.lines() {
            assert_eq!(line, ok.to_json_line());
        }
    }

    /// 書き込みが始まったら通知し、解放されるまで止まる `Write`（受け手が詰まった
    /// stderr パイプの模擬）。
    struct StalledSink {
        entered: std::sync::mpsc::Sender<()>,
        gate: std::sync::mpsc::Receiver<()>,
        out: SharedBuf,
    }

    impl Write for StalledSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let _ = self.entered.send(());
            // gate の送信側が drop されると recv が Err で戻り、解放される。
            let _ = self.gate.recv();
            self.out.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// `REPAIR-9`・`TASK-10.3`: sink が詰まっても `record()` は即座に戻り、
    /// キュー満杯分は厳密な件数で `dropped()` に数える。解放後は受理済みの行だけが
    /// 順に出力される。
    #[test]
    fn repair_9_json_lines_recorder_does_not_block_on_stalled_sink() {
        let (ok, _) = fixed_records();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let out = SharedBuf::default();
        let sink = StalledSink {
            entered: entered_tx,
            gate: gate_rx,
            out: out.clone(),
        };
        let r = JsonLinesRecorder::with_capacity(sink, 3);

        // 1 件目を writer が取り出して write 内で詰まるまで待つ（キューは空になる）。
        r.record(&ok);
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("writer entered");

        // 詰まったまま 3 件はキューに入り、残り 7 件は捨てられる。全体で即座に戻る。
        let started = std::time::Instant::now();
        for _ in 0..10 {
            r.record(&ok);
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "record() blocked on a stalled sink"
        );
        assert_eq!(r.dropped(), 7);
        assert_eq!(r.write_failures(), 0);

        // 解放すると受理済みの 1 + 3 = 4 行だけが出力される。
        drop(gate_tx);
        let sink = r.into_inner().expect("writer");
        drop(sink);
        let text = buf_text(&out);
        assert_eq!(text.lines().count(), 4);
        for line in text.lines() {
            assert_eq!(line, ok.to_json_line());
        }
    }

    /// `REPAIR-9`: 容量 0 は 1 件に切り上げる（以降の `record()` が即座に捨てるだけに
    /// ならない）。
    #[test]
    fn repair_9_json_lines_recorder_zero_capacity_rounds_up_to_one() {
        let (ok, _) = fixed_records();
        let r = JsonLinesRecorder::with_capacity(Vec::<u8>::new(), 0);
        r.record(&ok);
        let out = String::from_utf8(r.into_inner().expect("writer")).expect("utf8");
        assert_eq!(out, format!("{}\n", ok.to_json_line()));
    }

    /// `REPAIR-9`: stderr recorder を構築できる（stderr の内容は検証しない）。
    #[test]
    fn repair_9_stderr_recorder_constructs() {
        let r: StderrRecorder = JsonLinesRecorder::stderr();
        assert_eq!(r.write_failures(), 0);
        assert_eq!(r.dropped(), 0);
    }
}

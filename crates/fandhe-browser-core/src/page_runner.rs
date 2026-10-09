//! ページスクリプトランナー: `<script>` の順次実行・`src` の取得・総量上限・
//! DOMContentLoaded / load の発火と失敗の集約
//! （TASK-109・MS-6・Issue #780・#781・ビヘイビア `JS-4`・`JS-6`・`JS-8`）。
//!
//! 「HTML を受け取り、`<script>` を文書順に実行し、変更後の HTML と実行結果の診断を返す」
//! 入口。部品は次のとおり組み合わせる。
//!
//! - [`crate::page_script::collect_page_scripts`]（#774）: 実行対象の収集
//! - [`crate::fetch::Fetcher`]: `src` の取得（`JS-8`。別の HTTP クライアントは作らない）
//! - [`crate::dom_bridge::DomBridge`]（#777）と `JsRuntime::install_dom_shim_with_options`
//!   （#778・#779・#781）: ページ内 JS から DOM を操作する経路とイベント shim
//! - [`fandhe_browser_js::EvaluateOptions`]（#776）: スクリプトごとの実時間上限の強制
//!
//! 呼び出し元は、TASK-112（#784）の専用スレッド上のアクター（`Page.navigate` への配線）を
//! 想定する。
//!
//! # 処理の 3 段構成
//!
//! 1. **prepare（同期）**: パースと収集、base URL の検証。
//! 2. **load（async）**: 文書順に `src` を [`Fetcher`] で取得しつつ、スクリプト総バイトを
//!    評価する。すべての `.await` は [`JsRuntime`] / [`DomBridge`] の生成より前に置き、
//!    future を `Send` に保つ。
//! 3. **execute（同期）**: 取得済みのソースを文書順に実行し、打ち切られていなければ
//!    DOMContentLoaded → load を発火する。
//!
//! # 契約
//!
//! - ページごとに新しい [`JsRuntime`] と [`DomBridge`] を作り、ランナーが所有して返す前に
//!   解放する。グローバル状態はページ間で漏れない。
//! - 解放の順序は「診断の取得、`DomBridge::detach`、`JsRuntime` の drop、シリアライズ」。
//!   打ち切り後に放棄されたネイティブ呼び出しが遅れて DOM を変更しても、detach 済みのため
//!   結果には影響せず、子プロセス版エンジンは drop で kill / wait される（`JS-6`）。
//! - 例外は集約して後続を続ける。実時間超過・リソース上限・エンジン不能・件数 / バイト上限は
//!   fail-closed で以降の実行を打ち切り、残りを [`ScriptOutcome::NotExecuted`] にする。
//!   打ち切り後は DOMContentLoaded / load を発火せず、その時点の DOM を確定して返す。
//! - 実行可能なソース（inline、または取得に成功した `src`）が 1 件も無いページでは
//!   ランタイムを作らない（子プロセスを起動しない。`PERF-7`）。
//! - `src` の取得は呼び出し側が所有する [`Fetcher`] 経由のみ。scheme・内部アドレス・
//!   リダイレクト先の検査は Fetcher に一元化されており（SSRF 対策。`SEC-2`）、拒否された
//!   スクリプトは [`ScriptOutcome::SrcFetchFailed`] として後続の実行を続ける。診断には
//!   URL 本文を残さず、失敗の種別だけを残す（クエリにトークンを含み得るため）。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - `<base href>` による `src` の解決（ページの base URL に対する相対解決のみ）。
//! - `src` 応答の Content-Type / nosniff 検査と charset 判定（`CORE-5` (7)。UTF-8 の
//!   非可逆変換のみ）。
//! - `src` 取得全体の時間予算（現状は Fetcher のタイムアウト × 件数。`page_wall_time` は
//!   JS 実行だけを対象とする）。
//! - 要素レベルの EventTarget・`onload` / `onreadystatechange` プロパティ・
//!   キャプチャ / 完全なバブリング、`async` / `defer` の順序近似（全て文書順で実行する）。
//! - 上限値の設定ファイルからの読み込みと専用スレッド化は TASK-112（#784）の担当。
//! - 可観測性（対応する `OperationKind` が無い）は未対応。
//! - パースは既定の `scripting_enabled = false` のため `<noscript>` の扱いが実ブラウザと異なる。
//! - スクリプトの completion 値が結果サイズ上限を超えると例外として扱われる
//!   （ソースの書き換えによる回避はしない）。

use std::cell::Cell;
use std::time::{Duration, Instant};

use fandhe_browser_js::{EvaluateOptions, JsEngineError};
use reqwest::Url;

use crate::config::JsConfig;
use crate::dom::Document;
use crate::dom_bridge::{
    BridgeDiagnostics, DocumentReadyState, DomBridge, DomBridgeError, DomBridgeLimits,
    parse_location,
};
use crate::error::Error;
use crate::fetch::{FetchOptions, Fetcher};
use crate::js_stub::JsRuntime;
use crate::page_script::{
    CollectedScripts, ScriptCollectionOptions, ScriptDiagnosticKind, ScriptSource,
    collect_page_scripts,
};
use crate::parse::{ParseOptions, parse_document};
use crate::{NodeId, dom_bridge::truncate_utf8};

pub use crate::dom_bridge::LifecycleEvent;

/// スクリプト 1 件あたりの実時間上限の既定（`EvaluateOptions` の既定と同じ 2 秒）。
pub const DEFAULT_SCRIPT_WALL_TIME: Duration = EvaluateOptions::DEFAULT_TIMEOUT;
/// ページ全体（shim の注入を含む全スクリプト）の実時間上限の既定（5 秒）。
pub const DEFAULT_PAGE_WALL_TIME: Duration = Duration::from_secs(5);
/// 例外メッセージを保持する最大バイト数（ページ由来の文字列のため上限を設ける）。
pub const MAX_SCRIPT_ERROR_MESSAGE_BYTES: usize = 4096;
/// 実行するスクリプト本文（inline と取得した `src`）の総バイト数の既定上限（4 MiB。`JS-6`）。
pub const DEFAULT_MAX_TOTAL_SCRIPT_BYTES: usize = 4 * 1024 * 1024;
/// `src` スクリプト 1 件の本文バイト数の既定上限（1 MiB。`JS-6`）。
pub const DEFAULT_MAX_SRC_SCRIPT_BYTES: usize = 1024 * 1024;

/// `src` 取得用の [`FetchOptions`]（本文上限を [`DEFAULT_MAX_SRC_SCRIPT_BYTES`] にしたもの）。
///
/// ランナーに渡す [`Fetcher`] はこの設定（または同等以下の `max_body_bytes`）で構築することを
/// 推奨する。上限が大きい Fetcher でもランナー側で 1 件ごとの上限を事後検査するが、
/// 巨大な応答の受信自体は Fetcher の上限まで進むため。内部アドレスへのアクセスは既定で拒否
/// される（`allow_private_network_access = false`）。
pub fn page_script_fetch_options() -> FetchOptions {
    FetchOptions::new().with_max_body_bytes(DEFAULT_MAX_SRC_SCRIPT_BYTES as u64)
}

/// ランナーへの入力（HTML とベース URL）。
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PageRunInput<'a> {
    html: &'a str,
    base_url: &'a str,
}

impl<'a> PageRunInput<'a> {
    /// `html` と、`window.location` に設定し `src` の解決にも使うベース URL
    /// （http / https のみ）から作る。
    pub fn new(html: &'a str, base_url: &'a str) -> Self {
        Self { html, base_url }
    }

    /// 入力 HTML。
    pub fn html(&self) -> &'a str {
        self.html
    }

    /// ベース URL。
    pub fn base_url(&self) -> &'a str {
        self.base_url
    }
}

/// ランナーの設定。上限値の設定ファイルからの読み込みは TASK-112 の担当。
///
/// スクリプト件数の上限は [`ScriptCollectionOptions::max_scripts`]（収集時の上限）が
/// 単一の情報源で、超過すると [`AbortKind::ScriptCount`] で打ち切る（`JS-6`）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PageRunOptions {
    parse: ParseOptions,
    collection: ScriptCollectionOptions,
    bridge_limits: DomBridgeLimits,
    script_wall_time: Duration,
    page_wall_time: Duration,
    max_total_script_bytes: usize,
    max_src_script_bytes: usize,
}

impl Default for PageRunOptions {
    fn default() -> Self {
        Self {
            parse: ParseOptions::default(),
            collection: ScriptCollectionOptions::default(),
            bridge_limits: DomBridgeLimits::default(),
            script_wall_time: DEFAULT_SCRIPT_WALL_TIME,
            page_wall_time: DEFAULT_PAGE_WALL_TIME,
            max_total_script_bytes: DEFAULT_MAX_TOTAL_SCRIPT_BYTES,
            max_src_script_bytes: DEFAULT_MAX_SRC_SCRIPT_BYTES,
        }
    }
}

impl PageRunOptions {
    /// パース設定を差し替える。
    #[must_use]
    pub fn with_parse(mut self, parse: ParseOptions) -> Self {
        self.parse = parse;
        self
    }

    /// スクリプト収集の上限を差し替える。
    #[must_use]
    pub fn with_collection(mut self, collection: ScriptCollectionOptions) -> Self {
        self.collection = collection;
        self
    }

    /// DOM ブリッジの上限を差し替える。
    #[must_use]
    pub fn with_bridge_limits(mut self, limits: DomBridgeLimits) -> Self {
        self.bridge_limits = limits;
        self
    }

    /// スクリプト 1 件あたりの実時間上限を設定する。
    #[must_use]
    pub fn with_script_wall_time(mut self, limit: Duration) -> Self {
        self.script_wall_time = limit;
        self
    }

    /// ページ全体の実時間上限を設定する。
    #[must_use]
    pub fn with_page_wall_time(mut self, limit: Duration) -> Self {
        self.page_wall_time = limit;
        self
    }

    /// スクリプト本文の総バイト数の上限を設定する。
    #[must_use]
    pub fn with_max_total_script_bytes(mut self, limit: usize) -> Self {
        self.max_total_script_bytes = limit;
        self
    }

    /// `src` スクリプト 1 件の本文バイト数の上限を設定する。
    #[must_use]
    pub fn with_max_src_script_bytes(mut self, limit: usize) -> Self {
        self.max_src_script_bytes = limit;
        self
    }

    /// パース設定。
    pub fn parse(&self) -> &ParseOptions {
        &self.parse
    }

    /// スクリプト収集の上限。
    pub fn collection(&self) -> &ScriptCollectionOptions {
        &self.collection
    }

    /// DOM ブリッジの上限。
    pub fn bridge_limits(&self) -> &DomBridgeLimits {
        &self.bridge_limits
    }

    /// スクリプト 1 件あたりの実時間上限。
    pub fn script_wall_time(&self) -> Duration {
        self.script_wall_time
    }

    /// ページ全体の実時間上限。
    pub fn page_wall_time(&self) -> Duration {
        self.page_wall_time
    }

    /// スクリプト本文の総バイト数の上限。
    pub fn max_total_script_bytes(&self) -> usize {
        self.max_total_script_bytes
    }

    /// `src` スクリプト 1 件の本文バイト数の上限。
    pub fn max_src_script_bytes(&self) -> usize {
        self.max_src_script_bytes
    }
}

/// 実時間上限の適用範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WallTimeScope {
    /// スクリプト 1 件あたりの上限。
    Script,
    /// ページ全体の上限。
    Page,
}

impl WallTimeScope {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Page => "page",
        }
    }
}

/// スクリプトのバイト上限の適用範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptBytesScope {
    /// 実行するスクリプト本文の総バイト数。
    Total,
    /// `src` スクリプト 1 件の本文バイト数。
    PerScript,
}

impl ScriptBytesScope {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::PerScript => "per_script",
        }
    }
}

/// ページの実行を打ち切った理由（超過した上限の種別と値を残す。`JS-6`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AbortKind {
    /// 実時間上限を超えた。
    WallTime {
        /// どの上限か。
        scope: WallTimeScope,
        /// 超過した上限の値。
        limit: Duration,
    },
    /// エンジンのリソース上限（ヒープ等）に達した。
    ResourceLimit,
    /// エンジンが使えなくなった（子プロセスの終了等）。shim が失われるため継続しない。
    EngineUnavailable,
    /// 上記以外のエンジン側の失敗（未知の種別を含む。fail-closed）。
    EngineError,
    /// スクリプト件数の上限を超えた（上限は収集時の `max_scripts`）。
    ScriptCount {
        /// 超過した上限の値。
        limit: usize,
    },
    /// スクリプト本文のバイト数の上限を超えた。
    ScriptBytes {
        /// どの上限か。
        scope: ScriptBytesScope,
        /// 超過した上限の値。
        limit: usize,
    },
}

impl AbortKind {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WallTime { .. } => "wall_time",
            Self::ResourceLimit => "resource_limit",
            Self::EngineUnavailable => "engine_unavailable",
            Self::EngineError => "engine_error",
            Self::ScriptCount { .. } => "script_count",
            Self::ScriptBytes { .. } => "script_bytes",
        }
    }
}

/// `src` スクリプトの取得に失敗した理由の種別。URL・エラー本文は残さない
/// （クエリにトークンを含み得るため。security.md「秘密情報の混入防止」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SrcFetchErrorKind {
    /// `src` を base URL に対して URL として解決できない、または Fetcher が URL を不正と判定した。
    InvalidUrl,
    /// http / https 以外の scheme（リダイレクト先を含む）。
    DisallowedScheme,
    /// 内部アドレスなど許可されないアドレス（DNS 解決結果・リダイレクト先を含む）。
    DisallowedAddress,
    /// リダイレクト回数の上限超過。
    TooManyRedirects,
    /// 取得のタイムアウト。
    Timeout,
    /// 接続失敗などのネットワークエラー。
    Network,
    /// 2xx 以外の HTTP ステータス（実行しない）。
    HttpStatus {
        /// 受信したステータスコード。
        status: u16,
    },
    /// 上記以外（未知のエラー。fail-closed で実行しない）。
    Other,
}

impl SrcFetchErrorKind {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::DisallowedScheme => "disallowed_scheme",
            Self::DisallowedAddress => "disallowed_address",
            Self::TooManyRedirects => "too_many_redirects",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::HttpStatus { .. } => "http_status",
            Self::Other => "other",
        }
    }
}

/// スクリプト 1 件の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptOutcome {
    /// 正常終了（completion 値は保持しない）。
    Succeeded,
    /// 例外で終了した。後続のスクリプトは続行する。
    Exception {
        /// 例外メッセージ（[`MAX_SCRIPT_ERROR_MESSAGE_BYTES`] で切り詰め済み）。
        message: String,
        /// 切り詰めたか。
        truncated: bool,
    },
    /// 収集時に実行対象から外れた（`type=module` 等。#774 の診断）。
    Skipped {
        /// 外れた理由。
        kind: ScriptDiagnosticKind,
        /// 件数を伴う診断（上限超過）の件数。
        count: Option<usize>,
    },
    /// `src` の取得に失敗したため実行しなかった（後続の実行は続ける。`JS-8`）。
    SrcFetchFailed {
        /// 失敗の種別。
        kind: SrcFetchErrorKind,
    },
    /// このスクリプトが原因でページの実行を打ち切った。
    Aborted {
        /// 打ち切りの理由。
        kind: AbortKind,
        /// エンジンのメッセージ（切り詰め済み）。
        message: Option<String>,
    },
    /// 先行スクリプトの打ち切りにより実行しなかった。
    NotExecuted {
        /// 打ち切りの理由。
        kind: AbortKind,
    },
}

impl ScriptOutcome {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Exception { .. } => "exception",
            Self::Skipped { .. } => "skipped",
            Self::SrcFetchFailed { .. } => "src_fetch_failed",
            Self::Aborted { .. } => "aborted",
            Self::NotExecuted { .. } => "not_executed",
        }
    }
}

/// スクリプト 1 件分の記録。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptRunRecord {
    index: Option<usize>,
    node: Option<NodeId>,
    outcome: ScriptOutcome,
}

impl ScriptRunRecord {
    /// 文書順の通し番号（件数上限の診断など特定できないものは `None`）。
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// 対応する `<script>` ノード。
    pub fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// 結果。
    pub fn outcome(&self) -> &ScriptOutcome {
        &self.outcome
    }
}

/// ライフサイクルイベントの発火結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LifecycleOutcome {
    /// 発火した（リスナーの例外は [`PageRunOutput::bridge_diagnostics`] の
    /// `listener_errors` に集約され、発火自体は成功扱い）。
    Dispatched,
    /// このイベントの発火中にページの実行を打ち切った。
    Aborted {
        /// 打ち切りの理由。
        kind: AbortKind,
    },
    /// 打ち切り済みのため発火しなかった。
    NotFired {
        /// 打ち切りの理由。
        kind: AbortKind,
    },
}

impl LifecycleOutcome {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Aborted { .. } => "aborted",
            Self::NotFired { .. } => "not_fired",
        }
    }
}

/// ライフサイクルイベント 1 件分の記録。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LifecycleEventRecord {
    event: LifecycleEvent,
    outcome: LifecycleOutcome,
}

impl LifecycleEventRecord {
    /// イベントの種別。
    pub fn event(&self) -> LifecycleEvent {
        self.event
    }

    /// 発火結果。
    pub fn outcome(&self) -> &LifecycleOutcome {
        &self.outcome
    }
}

/// ページの実行を打ち切った記録。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageAbort {
    kind: AbortKind,
    index: Option<usize>,
}

impl PageAbort {
    /// 打ち切りの理由。
    pub fn kind(&self) -> &AbortKind {
        &self.kind
    }

    /// 原因になったスクリプトの通し番号。
    pub fn index(&self) -> Option<usize> {
        self.index
    }
}

/// ランナーの結果。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PageRunOutput {
    html: String,
    scripts: Vec<ScriptRunRecord>,
    dropped_collection_diagnostics: usize,
    abort: Option<PageAbort>,
    lifecycle: Vec<LifecycleEventRecord>,
    bridge_diagnostics: BridgeDiagnostics,
    elapsed: Duration,
}

impl PageRunOutput {
    /// 実行後の DOM をシリアライズした HTML。
    pub fn html(&self) -> &str {
        &self.html
    }

    /// 実行後の HTML を取り出す。
    pub fn into_html(self) -> String {
        self.html
    }

    /// スクリプトごとの記録（通し番号の昇順）。
    pub fn scripts(&self) -> &[ScriptRunRecord] {
        &self.scripts
    }

    /// 収集時の診断のうち、上限で記録しなかった件数。
    pub fn dropped_collection_diagnostics(&self) -> usize {
        self.dropped_collection_diagnostics
    }

    /// ページを打ち切った理由。打ち切っていなければ `None`。
    pub fn abort(&self) -> Option<&PageAbort> {
        self.abort.as_ref()
    }

    /// DOMContentLoaded → load の発火結果（この順。`JS-4`）。
    pub fn lifecycle(&self) -> &[LifecycleEventRecord] {
        &self.lifecycle
    }

    /// ブリッジが集めた診断（`console`・無視した `location` 操作・リスナー例外）。
    pub fn bridge_diagnostics(&self) -> &BridgeDiagnostics {
        &self.bridge_diagnostics
    }

    /// ページ時計で測った経過時間（JS 実行の開始から。`src` の取得時間は含まない）。
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// 次のスクリプトに課す実時間上限を決める。ページの残りが 0 なら `None`（評価せず打ち切る）。
///
/// スクリプト単位の上限がページの残りに収まれば `Script`、収まらなければ残りで丸めて `Page`。
fn script_budget(
    per_script: Duration,
    page_limit: Duration,
    elapsed: Duration,
) -> Option<(Duration, WallTimeScope)> {
    let remaining = page_limit.saturating_sub(elapsed);
    if remaining.is_zero() {
        return None;
    }
    if per_script <= remaining {
        Some((per_script, WallTimeScope::Script))
    } else {
        Some((remaining, WallTimeScope::Page))
    }
}

/// ブリッジのエラーを `crate::Error` へ写す（URL 等の入力は反響しない）。
fn bridge_error(e: DomBridgeError) -> Error {
    match e {
        DomBridgeError::InvalidLocation { .. } => Error::InvalidInput {
            message: format!("invalid base URL: {e}"),
        },
        other => Error::Unsupported {
            message: format!("DOM bridge failure: {other}"),
        },
    }
}

fn truncated_message(message: &str) -> (String, bool) {
    let (kept, truncated) = truncate_utf8(message, MAX_SCRIPT_ERROR_MESSAGE_BYTES);
    (kept.to_string(), truncated)
}

/// パース・収集済みのページ（load 段へ渡す）。
#[derive(Debug)]
struct PreparedPage {
    document: Document,
    collected: CollectedScripts,
    /// 検証・正規化済みの base URL（`src` の解決に使う）。
    base: Url,
}

/// エントリ 1 件のソースの取得状況（`collected.entries()` と同じ並びの添字で対応する）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptLoad {
    /// inline（本文は収集結果が持つ）。
    Inline,
    /// 取得に成功した `src` の本文。
    Ready(String),
    /// 取得に失敗した `src`。
    Failed(SrcFetchErrorKind),
    /// バイト上限超過により取得しなかった。
    Unfetched,
}

/// load 段の結果。
#[derive(Debug)]
struct LoadedScripts {
    loads: Vec<ScriptLoad>,
    /// バイト上限を超えたエントリの通し番号と理由（最初の 1 件。以降は取得しない）。
    byte_abort: Option<(usize, AbortKind)>,
}

/// `src` 取得の失敗の分類。
#[derive(Debug, Clone, PartialEq, Eq)]
enum SrcFailure {
    /// 実行せず後続を続ける失敗。
    Fetch(SrcFetchErrorKind),
    /// 1 件あたりのバイト上限超過（ページ実行を打ち切る）。
    TooLarge,
}

/// Fetcher のエラーを分類する。URL・メッセージは写さない。
fn classify_fetch_error(e: &Error) -> SrcFailure {
    SrcFailure::Fetch(match e {
        Error::ResponseTooLarge { .. } => return SrcFailure::TooLarge,
        Error::InvalidInput { .. } => SrcFetchErrorKind::InvalidUrl,
        Error::DisallowedScheme { .. } => SrcFetchErrorKind::DisallowedScheme,
        Error::DisallowedAddress { .. } => SrcFetchErrorKind::DisallowedAddress,
        Error::TooManyRedirects { .. } => SrcFetchErrorKind::TooManyRedirects,
        Error::Timeout { .. } => SrcFetchErrorKind::Timeout,
        Error::Network { .. } | Error::TooManyConcurrentDnsResolutions { .. } => {
            SrcFetchErrorKind::Network
        }
        _ => SrcFetchErrorKind::Other,
    })
}

/// 取得した応答のステータスと本文長を検査する（2xx 以外は実行しない）。
fn check_src_response(
    status: u16,
    body_len: usize,
    max_src_bytes: usize,
) -> Result<(), SrcFailure> {
    if !(200..300).contains(&status) {
        return Err(SrcFailure::Fetch(SrcFetchErrorKind::HttpStatus { status }));
    }
    if body_len > max_src_bytes {
        return Err(SrcFailure::TooLarge);
    }
    Ok(())
}

/// `src` 1 件を解決・取得する。取得は [`Fetcher`] 経由のみ（`JS-8`）。
async fn fetch_src(
    fetcher: &Fetcher,
    base: &Url,
    src: &str,
    max_src_bytes: usize,
) -> Result<String, SrcFailure> {
    let resolved = base
        .join(src)
        .map_err(|_| SrcFailure::Fetch(SrcFetchErrorKind::InvalidUrl))?;
    // ネットワークへ出ずに scheme・IP リテラルを弾く（get 内の検査と同一。SSRF 対策）。
    fetcher
        .validate_url(resolved.as_str())
        .map_err(|e| classify_fetch_error(&e))?;
    // 上限は受信中に強制する（本文全体を確保してから検査しない）。
    let limit = u64::try_from(max_src_bytes).unwrap_or(u64::MAX);
    let response = fetcher
        .get_with_max_body(resolved.as_str(), limit)
        .await
        .map_err(|e| classify_fetch_error(&e))?;
    check_src_response(response.status(), response.body().len(), max_src_bytes)?;
    // 非可逆変換は不正バイトを 3 バイトの置換文字にするため、確保前に変換後の長さを検査する。
    if lossy_len_exceeds(response.body(), max_src_bytes) {
        return Err(SrcFailure::TooLarge);
    }
    Ok(response.body_text_lossy())
}

/// UTF-8 非可逆変換後の長さが `limit` を超えるかを、文字列を確保せずに判定する。
///
/// 不正バイト列 1 区間は置換文字（3 バイト）1 個になる。加算は `checked_add` で行う。
fn lossy_len_exceeds(body: &[u8], limit: usize) -> bool {
    let mut total: usize = 0;
    for chunk in body.utf8_chunks() {
        let add = chunk
            .valid()
            .len()
            .saturating_add(if chunk.invalid().is_empty() { 0 } else { 3 });
        total = match total.checked_add(add) {
            Some(t) if t <= limit => t,
            _ => return true,
        };
    }
    false
}

/// 文書順にエントリを走査し、`src` を取得しつつスクリプト本文の総バイトを評価する。
///
/// バイト上限（総量・`src` 1 件）を超えた時点で以降の `src` を取得せず、超過位置を
/// [`LoadedScripts::byte_abort`] に残す（`JS-6`）。累積は `checked_add` で計算する。
async fn load_sources(
    collected: &CollectedScripts,
    base: &Url,
    fetcher: &Fetcher,
    options: &PageRunOptions,
) -> LoadedScripts {
    let mut loads = Vec::with_capacity(collected.entries().len());
    let mut byte_abort: Option<(usize, AbortKind)> = None;
    let mut total: usize = 0;
    let total_exceeded = |index: usize| {
        (
            index,
            AbortKind::ScriptBytes {
                scope: ScriptBytesScope::Total,
                limit: options.max_total_script_bytes,
            },
        )
    };
    for entry in collected.entries() {
        if byte_abort.is_some() {
            loads.push(ScriptLoad::Unfetched);
            continue;
        }
        match entry.source() {
            ScriptSource::Inline(text) => {
                match total.checked_add(text.len()) {
                    Some(t) if t <= options.max_total_script_bytes => total = t,
                    _ => byte_abort = Some(total_exceeded(entry.index())),
                }
                loads.push(ScriptLoad::Inline);
            }
            ScriptSource::External(src) => {
                // 総量の残りも受信上限に含める（超過分を確保しない）。
                let remaining = options.max_total_script_bytes.saturating_sub(total);
                let limit = options.max_src_script_bytes.min(remaining);
                match fetch_src(fetcher, base, src, limit).await {
                    Ok(text) => match total.checked_add(text.len()) {
                        Some(t) if t <= options.max_total_script_bytes => {
                            total = t;
                            loads.push(ScriptLoad::Ready(text));
                        }
                        _ => {
                            byte_abort = Some(total_exceeded(entry.index()));
                            loads.push(ScriptLoad::Unfetched);
                        }
                    },
                    Err(SrcFailure::Fetch(kind)) => loads.push(ScriptLoad::Failed(kind)),
                    Err(SrcFailure::TooLarge) if remaining < options.max_src_script_bytes => {
                        byte_abort = Some(total_exceeded(entry.index()));
                        loads.push(ScriptLoad::Unfetched);
                    }
                    Err(SrcFailure::TooLarge) => {
                        byte_abort = Some((
                            entry.index(),
                            AbortKind::ScriptBytes {
                                scope: ScriptBytesScope::PerScript,
                                limit: options.max_src_script_bytes,
                            },
                        ));
                        loads.push(ScriptLoad::Unfetched);
                    }
                }
            }
        }
    }
    LoadedScripts { loads, byte_abort }
}

/// パースと収集、base URL の検証（同期）。
fn prepare(input: &PageRunInput<'_>, options: &PageRunOptions) -> crate::Result<PreparedPage> {
    // 不正な base URL はここで Err にする（黙って about:blank で続行しない）。
    let base = parse_location(input.base_url).map_err(|m| Error::InvalidInput {
        message: format!("invalid base URL: {m}"),
    })?;
    let parsed = parse_document(input.html, &options.parse)?;
    let document = parsed.document;
    let collected = collect_page_scripts(&document, &options.collection);
    Ok(PreparedPage {
        document,
        collected,
        base,
    })
}

/// ページの `<script>` を文書順に実行し、変更後の HTML と診断を返す
/// （`JS-4`・`JS-6`・`JS-8`）。
///
/// `src` は `fetcher` 経由でのみ取得する（推奨設定は [`page_script_fetch_options`]）。
/// tokio ランタイム上で await すること（[`Fetcher::get`] の契約）。
///
/// 入力エラー（不正な base URL・パース失敗）、JS 無効のビルド、エンジン生成・shim 注入の失敗、
/// シリアライズ上限超過は `Err`（成功を装わない）。スクリプト側の失敗・`src` の取得失敗・
/// 上限超過は [`PageRunOutput`] に集約する。
pub async fn run_page_scripts(
    input: &PageRunInput<'_>,
    js: &JsConfig,
    options: &PageRunOptions,
    fetcher: &Fetcher,
) -> crate::Result<PageRunOutput> {
    run_with_fetcher_and_factory(input, options, fetcher, || JsRuntime::from_config(js)).await
}

async fn run_with_fetcher_and_factory(
    input: &PageRunInput<'_>,
    options: &PageRunOptions,
    fetcher: &Fetcher,
    make_runtime: impl FnOnce() -> crate::Result<JsRuntime>,
) -> crate::Result<PageRunOutput> {
    let prepared = prepare(input, options)?;
    // すべての await はここまで（JsRuntime / DomBridge の生成前）に済ませる。
    let loaded = load_sources(&prepared.collected, &prepared.base, fetcher, options).await;
    execute(input, options, prepared, loaded, make_runtime)
}

/// 取得済みのソースを実行する（同期）。ネットワークを使わない単体テストの継ぎ目でもある。
fn execute(
    input: &PageRunInput<'_>,
    options: &PageRunOptions,
    prepared: PreparedPage,
    loaded: LoadedScripts,
    make_runtime: impl FnOnce() -> crate::Result<JsRuntime>,
) -> crate::Result<PageRunOutput> {
    let PreparedPage {
        document: doc,
        collected,
        ..
    } = prepared;
    let LoadedScripts { loads, byte_abort } = loaded;

    let bridge = DomBridge::new(options.bridge_limits.clone());
    bridge.attach(doc).map_err(bridge_error)?;
    bridge.set_location(input.base_url).map_err(bridge_error)?;

    // 実行され得るソース（バイト上限の超過位置より前の inline / 取得成功した src）の有無。
    let abort_index = byte_abort.as_ref().map(|(i, _)| *i);
    let has_runnable = collected
        .entries()
        .iter()
        .zip(loads.iter())
        .take_while(|(e, _)| abort_index.is_none_or(|a| e.index() < a))
        .any(|(_, l)| matches!(l, ScriptLoad::Inline | ScriptLoad::Ready(_)));
    let page_start = Instant::now();

    let mut records: Vec<ScriptRunRecord> = Vec::new();
    let mut abort: Option<PageAbort> = None;
    let mut runtime: Option<JsRuntime> = None;

    if has_runnable {
        // shim 注入の各評価の直前に、ページ時計から残り予算を再計算する（先行 shim や
        // 初期化の消費時間を差し引く。期限切れならそこで打ち切る。`JS-6`）。
        let last_scope = Cell::new(WallTimeScope::Page);
        let mut next_options = || match script_budget(
            options.script_wall_time,
            options.page_wall_time,
            page_start.elapsed(),
        ) {
            Some((budget, scope)) => {
                last_scope.set(scope);
                Ok(EvaluateOptions::default().with_timeout(budget))
            }
            None => {
                last_scope.set(WallTimeScope::Page);
                Err(JsEngineError::Timeout(
                    "page wall time exhausted before shim installation".to_string(),
                ))
            }
        };
        // 評価前に予算が尽きていれば runtime を作らず打ち切る。
        if script_budget(
            options.script_wall_time,
            options.page_wall_time,
            page_start.elapsed(),
        )
        .is_none()
        {
            abort = Some(page_wall_abort(options, None));
        } else {
            let mut rt = make_runtime()?;
            match rt.install_dom_shim_with(&bridge, &mut next_options) {
                Ok(_) => runtime = Some(rt),
                // shim 評価がタイムアウトした場合はページの打ち切りとして集約する
                // （shim が無い runtime では継続できないため runtime は捨てる）。
                Err(Error::JsEvaluation(JsEngineError::Timeout(_))) => {
                    let scope = last_scope.get();
                    let limit = match scope {
                        WallTimeScope::Script => options.script_wall_time,
                        WallTimeScope::Page => options.page_wall_time,
                    };
                    abort = Some(PageAbort {
                        kind: AbortKind::WallTime { scope, limit },
                        index: None,
                    });
                }
                Err(e) => return Err(e),
            }
        }
    }

    let ctx = RunContext {
        bridge: &bridge,
        options,
        page_start,
    };
    for (entry, load) in collected.entries().iter().zip(loads.iter()) {
        let outcome = if let Some(a) = &abort {
            ScriptOutcome::NotExecuted {
                kind: a.kind.clone(),
            }
        } else if let Some((_, kind)) = byte_abort.as_ref().filter(|(i, _)| *i == entry.index()) {
            abort = Some(PageAbort {
                kind: kind.clone(),
                index: Some(entry.index()),
            });
            ScriptOutcome::Aborted {
                kind: kind.clone(),
                message: None,
            }
        } else {
            match (entry.source(), load) {
                (_, ScriptLoad::Failed(kind)) => ScriptOutcome::SrcFetchFailed { kind: *kind },
                (ScriptSource::Inline(src), _)
                | (ScriptSource::External(_), ScriptLoad::Ready(src)) => {
                    match runtime.as_mut() {
                        Some(rt) => {
                            let o =
                                run_one(rt, &ctx, entry.node(), entry.index(), src, &mut abort)?;
                            // リスナー保持件数の上限超過は、超過させたスクリプトの直後に確定し
                            // 後続のスクリプトを実行しない（`JS-6`）。
                            if abort.is_none()
                                && bridge.listener_limit_exceeded().map_err(bridge_error)?
                            {
                                abort = Some(PageAbort {
                                    kind: AbortKind::ResourceLimit,
                                    index: Some(entry.index()),
                                });
                            }
                            o
                        }
                        // runtime が無いのは abort 済みのときだけ（上の分岐で処理済み）。
                        None => ScriptOutcome::NotExecuted {
                            kind: AbortKind::EngineError,
                        },
                    }
                }
                _ => ScriptOutcome::NotExecuted {
                    kind: AbortKind::EngineError,
                },
            }
        };
        records.push(ScriptRunRecord {
            index: Some(entry.index()),
            node: Some(entry.node()),
            outcome,
        });
    }

    // 件数上限の超過（収集時の診断）は、全エントリの実行後に打ち切りとして確定する（`JS-6`）。
    if abort.is_none()
        && let Some(d) = collected
            .diagnostics()
            .iter()
            .find(|d| d.kind() == ScriptDiagnosticKind::ScriptLimitExceeded)
    {
        abort = Some(PageAbort {
            kind: AbortKind::ScriptCount {
                limit: options.collection.max_scripts(),
            },
            index: d.index(),
        });
    }

    // shim のリスナー保持件数の上限超過は、リソース上限による打ち切りとして確定する（`JS-6`）。
    // 以降のライフサイクルは発火しない（fail-closed）。
    if abort.is_none() && bridge.listener_limit_exceeded().map_err(bridge_error)? {
        abort = Some(PageAbort {
            kind: AbortKind::ResourceLimit,
            index: None,
        });
    }

    let lifecycle = fire_lifecycle(&ctx, runtime.as_mut(), &mut abort)?;

    let elapsed = page_start.elapsed();
    let bridge_diagnostics = bridge.take_diagnostics().map_err(bridge_error)?;
    // detach で DOM を確定してから runtime を解放する（遅れた変更を結果へ混ぜない）。
    let document = bridge
        .detach()
        .map_err(bridge_error)?
        .ok_or_else(|| Error::Unsupported {
            message: "DOM bridge has no attached document".to_string(),
        })?;
    drop(runtime);
    let html = document.serialize_html()?.into_html();

    Ok(finish(
        html,
        records,
        &collected,
        abort,
        lifecycle,
        bridge_diagnostics,
        elapsed,
    ))
}

/// ページの実時間上限超過による打ち切り記録を作る。
fn page_wall_abort(options: &PageRunOptions, index: Option<usize>) -> PageAbort {
    PageAbort {
        kind: AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit: options.page_wall_time,
        },
        index,
    }
}

/// [`run_one`] が参照するページ共通の実行コンテキスト。
struct RunContext<'a> {
    bridge: &'a DomBridge,
    options: &'a PageRunOptions,
    /// ページ実行の開始時刻（ページ時計。`JS-6`）。
    page_start: Instant,
}

/// [`evaluate_with_budget`] の結果。
enum Evaluated {
    Completed,
    Exception {
        message: String,
        truncated: bool,
    },
    Aborted {
        kind: AbortKind,
        message: Option<String>,
    },
    /// ページの残り時間が無く、評価しなかった。
    BudgetExhausted(AbortKind),
}

/// ページ時計の残り予算を課して `src` を評価し、エンジンの結果を [`Evaluated`] に写す。
fn evaluate_with_budget(rt: &mut JsRuntime, ctx: &RunContext<'_>, src: &str) -> Evaluated {
    let RunContext {
        options,
        page_start,
        ..
    } = ctx;
    let Some((timeout, scope)) = script_budget(
        options.script_wall_time,
        options.page_wall_time,
        page_start.elapsed(),
    ) else {
        return Evaluated::BudgetExhausted(page_wall_abort(options, None).kind);
    };

    let result = rt.execute_with_options(src, &EvaluateOptions::default().with_timeout(timeout));

    // 評価後にもページ時計を確認する。子プロセス経路の切り上げ・猶予で、残り時間を
    // 超えて正常応答が届くことがある（`JS-6`）。
    let page_expired = page_start.elapsed() > options.page_wall_time;

    let (kind, message) = match result {
        Ok(_) if page_expired => (page_wall_abort(options, None).kind, None),
        Ok(_) => return Evaluated::Completed,
        Err(Error::JsEvaluation(JsEngineError::EvaluationFailed(m))) => {
            let (message, truncated) = truncated_message(&m);
            if page_expired {
                (page_wall_abort(options, None).kind, Some(message))
            } else {
                return Evaluated::Exception { message, truncated };
            }
        }
        Err(Error::JsEvaluation(JsEngineError::Timeout(m))) => {
            let limit = match scope {
                WallTimeScope::Script => options.script_wall_time,
                WallTimeScope::Page => options.page_wall_time,
            };
            (
                AbortKind::WallTime { scope, limit },
                Some(truncated_message(&m).0),
            )
        }
        Err(Error::JsEvaluation(JsEngineError::ResourceLimitExceeded(m))) => {
            (AbortKind::ResourceLimit, Some(truncated_message(&m).0))
        }
        Err(Error::JsEvaluation(JsEngineError::EngineUnavailable(m))) => {
            (AbortKind::EngineUnavailable, Some(truncated_message(&m).0))
        }
        Err(Error::JsEvaluation(_)) | Err(_) => (AbortKind::EngineError, None),
    };
    Evaluated::Aborted { kind, message }
}

/// スクリプト 1 件を評価し、結果を記録用の [`ScriptOutcome`] に写す。
/// 打ち切りになる場合は `abort` に理由を入れる。
fn run_one(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
    node: NodeId,
    index: usize,
    src: &str,
    abort: &mut Option<PageAbort>,
) -> crate::Result<ScriptOutcome> {
    ctx.bridge
        .set_current_script(Some(node))
        .map_err(bridge_error)?;
    let evaluated = evaluate_with_budget(rt, ctx, src);
    ctx.bridge.set_current_script(None).map_err(bridge_error)?;

    Ok(match evaluated {
        Evaluated::Completed => ScriptOutcome::Succeeded,
        Evaluated::Exception { message, truncated } => {
            ScriptOutcome::Exception { message, truncated }
        }
        Evaluated::Aborted { kind, message } => {
            *abort = Some(PageAbort {
                kind: kind.clone(),
                index: Some(index),
            });
            ScriptOutcome::Aborted { kind, message }
        }
        // このスクリプトは実行していない（原因ではない）ので index は付けない。
        Evaluated::BudgetExhausted(kind) => {
            *abort = Some(page_wall_abort(ctx.options, None));
            ScriptOutcome::NotExecuted { kind }
        }
    })
}

/// 全スクリプトの実行後に DOMContentLoaded → load を 1 回ずつ発火する（`JS-4`）。
///
/// 打ち切り済みなら（理由を問わず）発火せず、`readyState` も `loading` のまま残す
/// （その時点の DOM を確定する fail-closed。`JS-6`）。リスナーの例外は shim が捕捉して
/// ブリッジ診断（`listener_errors`）へ集約するため、ここでは発火の成否だけを扱う。
/// リスナーはページのコードなので wall time 予算の対象になる。
/// 実行可能なソースが無く runtime が無い場合、リスナーは存在し得ないので評価せず
/// `readyState` の遷移だけ行う。
fn fire_lifecycle(
    ctx: &RunContext<'_>,
    mut runtime: Option<&mut JsRuntime>,
    abort: &mut Option<PageAbort>,
) -> crate::Result<Vec<LifecycleEventRecord>> {
    let steps = [
        (
            LifecycleEvent::DomContentLoaded,
            DocumentReadyState::Interactive,
        ),
        (LifecycleEvent::Load, DocumentReadyState::Complete),
    ];
    let mut records = Vec::with_capacity(steps.len());
    for (event, ready_state) in steps {
        if let Some(a) = abort.as_ref() {
            records.push(LifecycleEventRecord {
                event,
                outcome: LifecycleOutcome::NotFired {
                    kind: a.kind.clone(),
                },
            });
            continue;
        }
        ctx.bridge
            .set_ready_state(ready_state)
            .map_err(bridge_error)?;
        let outcome = match runtime.as_deref_mut() {
            // 固定文字列のみ（ページ由来の値を埋め込まない）。
            Some(rt) => match evaluate_with_budget(
                rt,
                ctx,
                &format!("__fandheLifecycle(\"{}\")", event.as_str()),
            ) {
                Evaluated::Completed | Evaluated::Exception { .. } => LifecycleOutcome::Dispatched,
                Evaluated::Aborted { kind, .. } => {
                    *abort = Some(PageAbort {
                        kind: kind.clone(),
                        index: None,
                    });
                    LifecycleOutcome::Aborted { kind }
                }
                Evaluated::BudgetExhausted(kind) => {
                    *abort = Some(page_wall_abort(ctx.options, None));
                    LifecycleOutcome::Aborted { kind }
                }
            },
            None => LifecycleOutcome::Dispatched,
        };
        // リスナーが上限を超えて登録された場合は、以降のイベントを発火しない（`JS-6`）。
        if abort.is_none() && ctx.bridge.listener_limit_exceeded().map_err(bridge_error)? {
            *abort = Some(PageAbort {
                kind: AbortKind::ResourceLimit,
                index: None,
            });
        }
        records.push(LifecycleEventRecord { event, outcome });
    }
    Ok(records)
}

/// 実行結果と収集時の診断を通し番号で統合して出力にまとめる。
fn finish(
    html: String,
    mut scripts: Vec<ScriptRunRecord>,
    collected: &CollectedScripts,
    abort: Option<PageAbort>,
    lifecycle: Vec<LifecycleEventRecord>,
    bridge_diagnostics: BridgeDiagnostics,
    elapsed: Duration,
) -> PageRunOutput {
    for d in collected.diagnostics() {
        scripts.push(ScriptRunRecord {
            index: d.index(),
            node: d.node(),
            outcome: ScriptOutcome::Skipped {
                kind: d.kind(),
                count: d.count(),
            },
        });
    }
    // 安定ソート。通し番号の無い診断（件数上限）は末尾に置く。
    scripts.sort_by_key(|r| r.index.unwrap_or(usize::MAX));
    PageRunOutput {
        html,
        scripts,
        dropped_collection_diagnostics: collected.dropped_diagnostics(),
        abort,
        lifecycle,
        bridge_diagnostics,
        elapsed,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::js_shim::JS_SHIM_SOURCES;
    use fandhe_browser_js::{EngineKind, JsEngine, JsValue, NativeFn};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// `JS-6`: 不正バイトは 3 バイトに膨らむ前提で、確保前に上限判定できる。
    #[test]
    fn lossy_len_exceeds_counts_replacement_chars() {
        assert!(!lossy_len_exceeds(b"abc", 3));
        assert!(lossy_len_exceeds(b"abcd", 3));
        // 0xff 1 バイトは U+FFFD（3 バイト）になる。
        assert!(!lossy_len_exceeds(&[0xff], 3));
        assert!(lossy_len_exceeds(&[0xff, 0xff], 5));
        assert!(!lossy_len_exceeds(&[0xff, 0xff], 6));
    }

    type Calls = Arc<Mutex<Vec<(String, Duration)>>>;
    type Log = Arc<Mutex<Vec<String>>>;

    /// テスト用エンジン。shim は成功させ、それ以外は script 文字列で振る舞いを分岐する。
    struct FakeEngine {
        calls: Calls,
        dropped: Arc<AtomicBool>,
        /// `__dom.op` の束縛（`readyState` の観測用）。
        op: Option<NativeFn>,
        /// ライフサイクル発火・`ready` スクリプトの観測ログ（`<イベント>@<readyState>`）。
        log: Log,
        /// ライフサイクルの発火評価をタイムアウトさせる。
        lifecycle_timeout: bool,
    }

    impl FakeEngine {
        fn ready_state(&mut self) -> String {
            let Some(op) = self.op.as_mut() else {
                return "no-op".to_string();
            };
            match op(&[JsValue::String("readyState".to_string())]) {
                Ok(JsValue::String(s)) => s,
                other => format!("{other:?}"),
            }
        }
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl JsEngine for FakeEngine {
        fn evaluate_script(
            &mut self,
            script: &str,
            options: &EvaluateOptions,
        ) -> Result<JsValue, JsEngineError> {
            if JS_SHIM_SOURCES.iter().any(|s| s.source == script) {
                return Ok(JsValue::Undefined);
            }
            if let Some(ev) = script
                .strip_prefix("__fandheLifecycle(\"")
                .and_then(|s| s.strip_suffix("\")"))
            {
                let state = self.ready_state();
                if let Ok(mut log) = self.log.lock() {
                    log.push(format!("{ev}@{state}"));
                }
                if self.lifecycle_timeout {
                    return Err(JsEngineError::Timeout("t".to_string()));
                }
                return Ok(JsValue::Undefined);
            }
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((script.to_string(), options.timeout()));
            }
            if script == "ready" {
                let state = self.ready_state();
                if let Ok(mut log) = self.log.lock() {
                    log.push(format!("script@{state}"));
                }
                return Ok(JsValue::Undefined);
            }
            if script.starts_with("throw") {
                // 4 KiB 境界の検証用: "throw:<n>" で n バイトのメッセージ。
                let n = script
                    .split_once(':')
                    .and_then(|(_, n)| n.parse::<usize>().ok())
                    .unwrap_or(1);
                return Err(JsEngineError::EvaluationFailed("x".repeat(n)));
            }
            if let Some(ms) = script.strip_prefix("sleep:") {
                // 評価が予算を超えて正常応答する経路の再現用。
                let ms = ms.parse::<u64>().unwrap_or(0);
                std::thread::sleep(Duration::from_millis(ms));
                return Ok(JsValue::Undefined);
            }
            match script {
                "timeout" => Err(JsEngineError::Timeout("t".to_string())),
                "oom" => Err(JsEngineError::ResourceLimitExceeded("r".to_string())),
                "unavailable" => Err(JsEngineError::EngineUnavailable("u".to_string())),
                _ => Ok(JsValue::Undefined),
            }
        }

        fn inject_global_function(
            &mut self,
            _name: &str,
            _func: NativeFn,
        ) -> Result<(), JsEngineError> {
            Ok(())
        }

        fn bind_dom_like_object(
            &mut self,
            _name: &str,
            methods: Vec<(String, NativeFn)>,
        ) -> Result<(), JsEngineError> {
            self.op = methods.into_iter().find(|(n, _)| n == "op").map(|(_, f)| f);
            Ok(())
        }
    }

    struct Harness {
        calls: Calls,
        dropped: Arc<AtomicBool>,
        factory_called: Arc<AtomicBool>,
        log: Log,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                dropped: Arc::new(AtomicBool::new(false)),
                factory_called: Arc::new(AtomicBool::new(false)),
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// FakeEngine を返す runtime factory（呼ばれたら `factory_called` を立てる）。
        fn factory(&self, lifecycle_timeout: bool) -> impl FnOnce() -> crate::Result<JsRuntime> {
            let (calls, dropped, called, log) = (
                self.calls.clone(),
                self.dropped.clone(),
                self.factory_called.clone(),
                self.log.clone(),
            );
            move || {
                called.store(true, Ordering::SeqCst);
                Ok(JsRuntime::from_engine_for_test(
                    EngineKind::V8,
                    Box::new(FakeEngine {
                        calls,
                        dropped,
                        op: None,
                        log,
                        lifecycle_timeout,
                    }),
                ))
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().map(|l| l.clone()).unwrap_or_default()
        }
    }

    /// 既定の load 結果: inline は `Inline`、`src` は `Unfetched`。
    fn default_loads(collected: &CollectedScripts) -> LoadedScripts {
        LoadedScripts {
            loads: collected
                .entries()
                .iter()
                .map(|e| match e.source() {
                    ScriptSource::Inline(_) => ScriptLoad::Inline,
                    _ => ScriptLoad::Unfetched,
                })
                .collect(),
            byte_abort: None,
        }
    }

    /// load 段を飛ばし、`build` が作った load 結果で execute 段を実行する
    /// （ネットワークを使わない継ぎ目）。
    fn run_loaded(
        html: &str,
        options: &PageRunOptions,
        lifecycle_timeout: bool,
        build: impl FnOnce(&CollectedScripts) -> LoadedScripts,
    ) -> (crate::Result<PageRunOutput>, Harness) {
        let h = Harness::new();
        let input = PageRunInput::new(html, "https://example.test/");
        let out = prepare(&input, options).and_then(|prepared| {
            let loaded = build(&prepared.collected);
            execute(
                &input,
                options,
                prepared,
                loaded,
                h.factory(lifecycle_timeout),
            )
        });
        (out, h)
    }

    fn run(html: &str, options: &PageRunOptions) -> (crate::Result<PageRunOutput>, Harness) {
        run_loaded(html, options, false, default_loads)
    }

    fn scripts_seen(h: &Harness) -> Vec<String> {
        h.calls
            .lock()
            .map(|c| c.iter().map(|(s, _)| s.clone()).collect())
            .unwrap_or_default()
    }

    fn outcomes(out: &PageRunOutput) -> Vec<&'static str> {
        out.scripts().iter().map(|r| r.outcome().as_str()).collect()
    }

    /// JS-4: inline は文書順に評価される。
    #[test]
    fn js_4_inline_scripts_run_in_document_order() {
        let (out, h) = run(
            "<script>1</script><script>2</script><script>3</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "2", "3"]);
        assert_eq!(outcomes(&out), vec!["succeeded"; 3]);
        assert!(out.abort().is_none());
    }

    /// JS-4: module・未対応 type は評価されず、通し番号順に Skipped として並ぶ。
    #[test]
    fn js_4_module_and_unsupported_are_skipped_not_executed() {
        let (out, h) = run(
            "<script>1</script><script type=module>2</script><script type=application/json>3</script><script>4</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "4"]);
        let kinds: Vec<_> = out
            .scripts()
            .iter()
            .map(|r| (r.index(), r.outcome().as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (Some(0), "succeeded"),
                (Some(1), "skipped"),
                (Some(2), "skipped"),
                (Some(3), "succeeded"),
            ]
        );
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::Skipped {
                kind: ScriptDiagnosticKind::SkippedModule,
                count: None
            }
        );
    }

    /// PERF-7 / JS-4: スクリプトの無いページでは factory が呼ばれず html が返る。
    #[test]
    fn js_4_no_inline_scripts_does_not_create_runtime() {
        let (out, h) = run("<p>hi</p>", &PageRunOptions::default());
        let out = out.expect("run");
        assert!(!h.factory_called.load(Ordering::SeqCst));
        assert!(out.html().contains("<p>hi</p>"));
        assert!(out.scripts().is_empty());
    }

    /// JS-4: 例外は集約し後続を続ける。
    #[test]
    fn js_4_exception_does_not_stop_following_scripts() {
        let (out, h) = run(
            "<script>1</script><script>throw:5</script><script>3</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "throw:5", "3"]);
        assert_eq!(outcomes(&out), vec!["succeeded", "exception", "succeeded"]);
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::Exception {
                message: "xxxxx".to_string(),
                truncated: false
            }
        );
        assert!(out.abort().is_none());
    }

    /// JS-4: 例外メッセージは 4096 バイトちょうどまで保持し、超えたら切り詰める。
    #[test]
    fn js_4_exception_message_truncated_at_4kib() {
        let (out, _) = run(
            "<script>throw:4096</script><script>throw:4097</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        match out.scripts()[0].outcome() {
            ScriptOutcome::Exception { message, truncated } => {
                assert_eq!(message.len(), 4096);
                assert!(!truncated);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match out.scripts()[1].outcome() {
            ScriptOutcome::Exception { message, truncated } => {
                assert_eq!(message.len(), 4096);
                assert!(*truncated);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // 多バイト文字の境界で切れる（文字境界を割らない）。
        let (kept, truncated) = truncated_message(&format!("{}あ", "x".repeat(4095)));
        assert_eq!(kept.len(), 4095);
        assert!(truncated);
    }

    /// JS-6: タイムアウトでその後を打ち切り、原因の上限（scope と値）を残す。
    #[test]
    fn js_6_timeout_aborts_remaining_with_wall_time() {
        let (out, h) = run(
            "<script>1</script><script>timeout</script><script>3</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        let expected = AbortKind::WallTime {
            scope: WallTimeScope::Script,
            limit: Duration::from_secs(2),
        };
        assert_eq!(scripts_seen(&h), vec!["1", "timeout"]);
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::Aborted {
                kind: expected.clone(),
                message: Some("t".to_string())
            }
        );
        assert_eq!(
            out.scripts()[2].outcome(),
            &ScriptOutcome::NotExecuted {
                kind: expected.clone()
            }
        );
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind(), &expected);
        assert_eq!(abort.index(), Some(1));
    }

    /// JS-6: 予算計算の境界値。
    #[test]
    fn js_6_script_budget_boundaries() {
        let ms = Duration::from_millis;
        let s2 = Duration::from_secs(2);
        let page = Duration::from_secs(5);
        assert_eq!(script_budget(s2, page, page), None);
        assert_eq!(script_budget(s2, page, page + ms(1)), None);
        assert_eq!(
            script_budget(s2, page, page - ms(1)),
            Some((ms(1), WallTimeScope::Page))
        );
        assert_eq!(
            script_budget(s2, page, page - s2),
            Some((s2, WallTimeScope::Script))
        );
        assert_eq!(
            script_budget(Duration::ZERO, page, Duration::ZERO),
            Some((Duration::ZERO, WallTimeScope::Script))
        );
    }

    /// JS-6: 既定設定ではエンジンへスクリプトごとに 2 秒が渡る。
    #[test]
    fn js_6_default_budget_passes_2s_to_engine() {
        let (out, h) = run(
            "<script>1</script><script>2</script>",
            &PageRunOptions::default(),
        );
        out.expect("run");
        let calls = h.calls.lock().expect("lock");
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|(_, t)| *t == Duration::from_secs(2)));
    }

    /// JS-6: ページ上限 0 では何も実行せず、ランタイムも作らない。
    #[test]
    fn js_6_page_wall_time_zero_runs_nothing() {
        let opts = PageRunOptions::default().with_page_wall_time(Duration::ZERO);
        let (out, h) = run("<script>1</script><script>2</script>", &opts);
        let out = out.expect("run");
        let kind = AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit: Duration::ZERO,
        };
        assert!(!h.factory_called.load(Ordering::SeqCst));
        assert!(scripts_seen(&h).is_empty());
        assert_eq!(
            out.scripts()
                .iter()
                .map(|r| r.outcome().clone())
                .collect::<Vec<_>>(),
            vec![
                ScriptOutcome::NotExecuted { kind: kind.clone() },
                ScriptOutcome::NotExecuted { kind: kind.clone() }
            ]
        );
        assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind));
    }

    /// JS-6: 最終スクリプトが予算を超えて正常応答してもページ期限超過として記録する。
    #[test]
    fn js_6_final_script_over_page_deadline_is_aborted() {
        let limit = Duration::from_millis(20);
        let opts = PageRunOptions::default().with_page_wall_time(limit);
        let (out, _h) = run("<script>sleep:60</script>", &opts);
        let out = out.expect("run");
        let kind = AbortKind::WallTime {
            scope: WallTimeScope::Page,
            limit,
        };
        assert_eq!(
            out.scripts()[0].outcome(),
            &ScriptOutcome::Aborted {
                kind: kind.clone(),
                message: None
            }
        );
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind(), &kind);
        assert_eq!(abort.index(), Some(0));
    }

    /// JS-6: ページの残りが小さいとスクリプトのタイムアウトが丸められ、scope は Page。
    #[test]
    fn js_6_page_budget_clamps_script_timeout() {
        let opts = PageRunOptions::default().with_page_wall_time(Duration::from_millis(500));
        let (out, h) = run("<script>timeout</script>", &opts);
        let out = out.expect("run");
        let t = h.calls.lock().expect("lock")[0].1;
        assert!(
            t > Duration::ZERO && t <= Duration::from_millis(500),
            "{t:?}"
        );
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::WallTime {
                scope: WallTimeScope::Page,
                limit: Duration::from_millis(500)
            })
        );
    }

    /// JS-6: リソース上限・エンジン不能は打ち切り（残りは NotExecuted）。
    #[test]
    fn js_6_resource_limit_and_engine_unavailable_abort() {
        for (src, kind) in [
            ("oom", AbortKind::ResourceLimit),
            ("unavailable", AbortKind::EngineUnavailable),
        ] {
            let html = format!("<script>{src}</script><script>2</script>");
            let (out, h) = run(&html, &PageRunOptions::default());
            let out = out.expect("run");
            assert_eq!(scripts_seen(&h), vec![src.to_string()]);
            assert_eq!(out.scripts()[0].outcome().as_str(), "aborted");
            assert_eq!(
                out.scripts()[1].outcome(),
                &ScriptOutcome::NotExecuted { kind: kind.clone() }
            );
            assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind));
        }
    }

    /// JS-6: 打ち切り後もエンジンは解放される。
    #[test]
    fn js_6_runtime_is_dropped_after_abort() {
        let (out, h) = run("<script>timeout</script>", &PageRunOptions::default());
        out.expect("run");
        assert!(h.dropped.load(Ordering::SeqCst));
    }

    /// JS-2 / JS-4: JS 無効のランタイムで inline があると Err（成功を装わない）。
    #[test]
    fn js_4_disabled_runtime_returns_err() {
        let input = PageRunInput::new("<script>1</script>", "https://example.test/");
        let options = PageRunOptions::default();
        let err = prepare(&input, &options)
            .and_then(|p| {
                let l = default_loads(&p.collected);
                execute(&input, &options, p, l, || Ok(JsRuntime::disabled()))
            })
            .expect_err("disabled");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    }

    /// SEC-2: http / https 以外の base URL は拒否され、メッセージに URL を含まない。
    #[test]
    fn js_4_invalid_base_url_is_rejected() {
        for url in ["file:///etc/passwd", "javascript:alert(1)"] {
            let input = PageRunInput::new("<script>1</script>", url);
            let err = prepare(&input, &PageRunOptions::default()).expect_err("invalid");
            assert!(matches!(err, Error::InvalidInput { .. }), "{err}");
            assert!(!err.to_string().contains("passwd"));
            assert!(!err.to_string().contains("alert"));
        }
    }

    // ---- #781: src 取得・総量上限・ライフサイクル ----

    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn fetcher_loopback() -> Fetcher {
        Fetcher::new(FetchOptions::new().with_allow_private_network_access(true)).expect("fetcher")
    }

    /// `127.0.0.1` の空きポートで、パスから生の HTTP 応答を返す最小サーバーを起動する。
    fn spawn_http(respond: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let respond = Arc::new(respond);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let respond = respond.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = [0u8; 1024];
                    let mut data = Vec::new();
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                data.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                                if data.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let head = String::from_utf8_lossy(&data).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let _ = stream.write_all(&respond(&path));
                    let _ = stream.flush();
                });
            }
        });
        port
    }

    fn http_response(status: &str, extra_headers: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// 実 Fetcher で load 段から execute 段まで通す。
    async fn run_fetching(
        html: &str,
        base: &str,
        options: &PageRunOptions,
        fetcher: &Fetcher,
    ) -> (crate::Result<PageRunOutput>, Harness) {
        let h = Harness::new();
        let input = PageRunInput::new(html, base);
        let out = run_with_fetcher_and_factory(&input, options, fetcher, h.factory(false)).await;
        (out, h)
    }

    fn kinds(out: &PageRunOutput) -> Vec<ScriptOutcome> {
        out.scripts().iter().map(|r| r.outcome().clone()).collect()
    }

    /// JS-4: inline と取得済み src が文書順に実行される。
    #[test]
    fn js_4_src_runs_in_document_order_with_inline() {
        let (out, h) = run_loaded(
            "<script>1</script><script src=\"a.js\"></script><script>3</script>",
            &PageRunOptions::default(),
            false,
            |_| LoadedScripts {
                loads: vec![
                    ScriptLoad::Inline,
                    ScriptLoad::Ready("2".to_string()),
                    ScriptLoad::Inline,
                ],
                byte_abort: None,
            },
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "2", "3"]);
        assert_eq!(outcomes(&out), vec!["succeeded"; 3]);
    }

    /// JS-4 / JS-8: 取得失敗は種別を記録して後続を続ける。
    #[test]
    fn js_4_src_fetch_failure_records_kind_and_continues() {
        let (out, h) = run_loaded(
            "<script>1</script><script src=\"a.js\"></script><script>3</script>",
            &PageRunOptions::default(),
            false,
            |_| LoadedScripts {
                loads: vec![
                    ScriptLoad::Inline,
                    ScriptLoad::Failed(SrcFetchErrorKind::DisallowedAddress),
                    ScriptLoad::Inline,
                ],
                byte_abort: None,
            },
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["1", "3"]);
        assert_eq!(
            kinds(&out),
            vec![
                ScriptOutcome::Succeeded,
                ScriptOutcome::SrcFetchFailed {
                    kind: SrcFetchErrorKind::DisallowedAddress
                },
                ScriptOutcome::Succeeded
            ]
        );
        assert_eq!(out.scripts()[1].outcome().as_str(), "src_fetch_failed");
        assert!(out.abort().is_none());
    }

    /// PERF-7: src だけで取得が全部失敗したページでは runtime を作らない。
    #[test]
    fn js_4_src_only_failed_page_does_not_create_runtime() {
        let (out, h) = run_loaded(
            "<script src=\"a.js\"></script>",
            &PageRunOptions::default(),
            false,
            |_| LoadedScripts {
                loads: vec![ScriptLoad::Failed(SrcFetchErrorKind::Network)],
                byte_abort: None,
            },
        );
        let out = out.expect("run");
        assert!(!h.factory_called.load(Ordering::SeqCst));
        assert_eq!(outcomes(&out), vec!["src_fetch_failed"]);
        assert_eq!(
            out.lifecycle()
                .iter()
                .map(|l| (l.event(), l.outcome().as_str()))
                .collect::<Vec<_>>(),
            vec![
                (LifecycleEvent::DomContentLoaded, "dispatched"),
                (LifecycleEvent::Load, "dispatched")
            ]
        );
    }

    /// JS-6: 既定値は 64 件・4 MiB・1 MiB。
    #[test]
    fn js_6_default_limits_are_64_4mib_1mib() {
        let o = PageRunOptions::default();
        assert_eq!(o.collection().max_scripts(), 64);
        assert_eq!(o.max_total_script_bytes(), 4 * 1024 * 1024);
        assert_eq!(o.max_src_script_bytes(), 1024 * 1024);
        assert_eq!(DEFAULT_MAX_TOTAL_SCRIPT_BYTES, 4_194_304);
        assert_eq!(DEFAULT_MAX_SRC_SCRIPT_BYTES, 1_048_576);
    }

    /// JS-6: 件数の境界。上限ちょうどは打ち切らず、+1 で script_count。
    #[test]
    fn js_6_script_count_boundary_ok_at_limit_aborts_above() {
        let opts = PageRunOptions::default()
            .with_collection(ScriptCollectionOptions::default().with_max_scripts(2));
        let (out, _) = run("<script>1</script><script>2</script>", &opts);
        let out = out.expect("run");
        assert!(out.abort().is_none());
        assert_eq!(out.lifecycle().len(), 2);

        let (out, h) = run(
            "<script>1</script><script>2</script><script>3</script>",
            &opts,
        );
        let out = out.expect("run");
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind(), &AbortKind::ScriptCount { limit: 2 });
        assert_eq!(abort.kind().as_str(), "script_count");
        assert_eq!(abort.index(), Some(2));
        // 上限内の 2 件は実行され、イベントは発火しない。
        assert_eq!(scripts_seen(&h), vec!["1", "2"]);
        assert!(h.log().is_empty());
        for l in out.lifecycle() {
            assert_eq!(
                l.outcome(),
                &LifecycleOutcome::NotFired {
                    kind: AbortKind::ScriptCount { limit: 2 }
                }
            );
        }
    }

    /// JS-6: バイト上限の超過位置は Aborted、以降は NotExecuted、イベントは発火しない。
    #[test]
    fn js_6_byte_abort_marks_aborted_then_not_executed() {
        let kind = AbortKind::ScriptBytes {
            scope: ScriptBytesScope::Total,
            limit: 7,
        };
        let k = kind.clone();
        let (out, h) = run_loaded(
            "<script>12345</script><script>67</script><script>8</script>",
            &PageRunOptions::default(),
            false,
            move |c| {
                let mut l = default_loads(c);
                l.byte_abort = Some((1, k));
                l
            },
        );
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["12345"]);
        assert_eq!(
            kinds(&out),
            vec![
                ScriptOutcome::Succeeded,
                ScriptOutcome::Aborted {
                    kind: kind.clone(),
                    message: None
                },
                ScriptOutcome::NotExecuted { kind: kind.clone() }
            ]
        );
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind().as_str(), "script_bytes");
        assert_eq!(abort.index(), Some(1));
        assert!(h.log().is_empty());
    }

    /// JS-6: 総バイトの境界（load 段）。ちょうど上限は通り、+1 で total の打ち切り。
    #[tokio::test]
    async fn js_6_total_bytes_boundary() {
        let fetcher = fetcher_loopback();
        let base = Url::parse("https://example.test/").expect("url");
        let html = "<script>12345</script><script>67</script><script>8</script>";
        // 合計 8 バイト。上限 8 は通り、7 は 3 件目、6 は 2 件目で超過する。
        for (limit, expected) in [(8usize, None), (7, Some(2usize)), (6, Some(1))] {
            let options = PageRunOptions::default().with_max_total_script_bytes(limit);
            let prepared = prepare(&PageRunInput::new(html, base.as_str()), &options).expect("p");
            let loaded = load_sources(&prepared.collected, &base, &fetcher, &options).await;
            assert_eq!(
                loaded.byte_abort,
                expected.map(|i| (
                    i,
                    AbortKind::ScriptBytes {
                        scope: ScriptBytesScope::Total,
                        limit
                    }
                )),
                "limit={limit}"
            );
            assert_eq!(loaded.loads.len(), 3);
        }
    }

    /// JS-6: src 1 件の境界。ちょうど上限は通り、+1 で per_script。
    #[tokio::test]
    async fn js_6_src_per_script_bytes_boundary() {
        let port = spawn_http(|path| match path {
            "/ten.js" => http_response("200 OK", "", "0123456789"),
            _ => http_response("200 OK", "", "0123456789A"),
        });
        let fetcher = fetcher_loopback();
        let options = PageRunOptions::default().with_max_src_script_bytes(10);
        let base = format!("http://127.0.0.1:{port}/");

        let (out, h) = run_fetching(
            "<script src=\"/ten.js\"></script><script>after</script>",
            &base,
            &options,
            &fetcher,
        )
        .await;
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["0123456789", "after"]);
        assert!(out.abort().is_none());

        let (out, h) = run_fetching(
            "<script>first</script><script src=\"/eleven.js\"></script><script>after</script>",
            &base,
            &options,
            &fetcher,
        )
        .await;
        let out = out.expect("run");
        let kind = AbortKind::ScriptBytes {
            scope: ScriptBytesScope::PerScript,
            limit: 10,
        };
        assert_eq!(scripts_seen(&h), vec!["first"]);
        assert_eq!(
            kinds(&out),
            vec![
                ScriptOutcome::Succeeded,
                ScriptOutcome::Aborted {
                    kind: kind.clone(),
                    message: None
                },
                ScriptOutcome::NotExecuted { kind: kind.clone() }
            ]
        );
        assert_eq!(out.abort().map(|a| a.index()), Some(Some(1)));
    }

    /// JS-6: Fetcher 側の本文上限（ResponseTooLarge）も per_script の打ち切りになる。
    #[tokio::test]
    async fn js_6_fetcher_response_too_large_maps_to_per_script() {
        let port = spawn_http(|_| http_response("200 OK", "", "0123456789A"));
        let fetcher = Fetcher::new(
            FetchOptions::new()
                .with_allow_private_network_access(true)
                .with_max_body_bytes(10),
        )
        .expect("fetcher");
        let (out, _) = run_fetching(
            "<script src=\"/x.js\"></script>",
            &format!("http://127.0.0.1:{port}/"),
            &PageRunOptions::default(),
            &fetcher,
        )
        .await;
        assert_eq!(
            out.expect("run").abort().map(|a| a.kind().clone()),
            Some(AbortKind::ScriptBytes {
                scope: ScriptBytesScope::PerScript,
                limit: DEFAULT_MAX_SRC_SCRIPT_BYTES
            })
        );
    }

    /// JS-4: readyState は loading（実行中）→ interactive（DCL）→ complete（load）。各 1 回、この順。
    #[test]
    fn js_4_lifecycle_order_and_ready_state() {
        let (out, h) = run(
            "<script>ready</script><script>ready</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(
            h.log(),
            vec![
                "script@loading",
                "script@loading",
                "DOMContentLoaded@interactive",
                "load@complete"
            ]
        );
        assert_eq!(
            out.lifecycle()
                .iter()
                .map(|l| (l.event(), l.outcome().clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    LifecycleEvent::DomContentLoaded,
                    LifecycleOutcome::Dispatched
                ),
                (LifecycleEvent::Load, LifecycleOutcome::Dispatched)
            ]
        );
    }

    /// JS-6: 打ち切り後はイベントを発火しない（readyState も loading のまま）。
    #[test]
    fn js_6_lifecycle_not_fired_after_abort() {
        let (out, h) = run("<script>timeout</script>", &PageRunOptions::default());
        let out = out.expect("run");
        assert!(h.log().is_empty());
        let expected = AbortKind::WallTime {
            scope: WallTimeScope::Script,
            limit: Duration::from_secs(2),
        };
        assert_eq!(out.lifecycle().len(), 2);
        for l in out.lifecycle() {
            assert_eq!(
                l.outcome(),
                &LifecycleOutcome::NotFired {
                    kind: expected.clone()
                }
            );
        }
    }

    /// JS-6: リスナー評価が wall time を超えたら打ち切り（index なし）。load は発火しない。
    #[test]
    fn js_6_lifecycle_dispatch_timeout_aborts() {
        let (out, h) = run_loaded(
            "<script>1</script>",
            &PageRunOptions::default(),
            true,
            default_loads,
        );
        let out = out.expect("run");
        let kind = AbortKind::WallTime {
            scope: WallTimeScope::Script,
            limit: Duration::from_secs(2),
        };
        assert_eq!(h.log(), vec!["DOMContentLoaded@interactive"]);
        assert_eq!(
            out.lifecycle()
                .iter()
                .map(|l| l.outcome().clone())
                .collect::<Vec<_>>(),
            vec![
                LifecycleOutcome::Aborted { kind: kind.clone() },
                LifecycleOutcome::NotFired { kind: kind.clone() }
            ]
        );
        let abort = out.abort().expect("abort");
        assert_eq!(abort.kind(), &kind);
        assert_eq!(abort.index(), None);
    }

    /// JS-8: Fetcher のエラーを種別へ分類する（URL・メッセージは写さない）。
    #[test]
    fn js_8_classify_fetch_error_table() {
        let f = SrcFailure::Fetch;
        let cases: Vec<(Error, SrcFailure)> = vec![
            (
                Error::DisallowedAddress {
                    address: "10.0.0.1".to_string(),
                },
                f(SrcFetchErrorKind::DisallowedAddress),
            ),
            (
                Error::DisallowedScheme {
                    scheme: "file".to_string(),
                },
                f(SrcFetchErrorKind::DisallowedScheme),
            ),
            (
                Error::TooManyRedirects { limit: 5 },
                f(SrcFetchErrorKind::TooManyRedirects),
            ),
            (
                Error::Timeout {
                    limit: Duration::from_secs(1),
                },
                f(SrcFetchErrorKind::Timeout),
            ),
            (
                Error::Network {
                    message: "x".to_string(),
                },
                f(SrcFetchErrorKind::Network),
            ),
            (
                Error::InvalidInput {
                    message: "x".to_string(),
                },
                f(SrcFetchErrorKind::InvalidUrl),
            ),
            (Error::ResponseTooLarge { limit: 1 }, SrcFailure::TooLarge),
        ];
        for (err, expected) in cases {
            assert_eq!(classify_fetch_error(&err), expected, "{err}");
        }
    }

    /// JS-6 / JS-8: 応答の検査（2xx のみ実行・本文長は上限ちょうどまで）。
    #[test]
    fn js_6_check_src_response_boundaries() {
        for status in [200u16, 204, 299] {
            assert_eq!(check_src_response(status, 10, 10), Ok(()), "{status}");
        }
        for status in [199u16, 301, 404, 500] {
            assert_eq!(
                check_src_response(status, 0, 10),
                Err(SrcFailure::Fetch(SrcFetchErrorKind::HttpStatus { status }))
            );
        }
        assert_eq!(check_src_response(200, 11, 10), Err(SrcFailure::TooLarge));
    }

    /// SEC-2 / JS-8: 既定の Fetcher では scheme・内部アドレスの src を取得せず、後続の inline は実行される。
    #[tokio::test]
    async fn js_8_disallowed_src_urls_are_not_fetched_and_page_continues() {
        let fetcher = Fetcher::new(FetchOptions::new()).expect("fetcher");
        let cases = [
            ("file:///etc/passwd", SrcFetchErrorKind::DisallowedScheme),
            (
                "data:text/javascript,1",
                SrcFetchErrorKind::DisallowedScheme,
            ),
            (
                "ftp://example.test/a.js",
                SrcFetchErrorKind::DisallowedScheme,
            ),
            (
                "http://127.0.0.1:1/a.js",
                SrcFetchErrorKind::DisallowedAddress,
            ),
            ("http://10.0.0.1/a.js", SrcFetchErrorKind::DisallowedAddress),
            ("http://[::1]/a.js", SrcFetchErrorKind::DisallowedAddress),
            (
                "http://169.254.169.254/latest/meta-data",
                SrcFetchErrorKind::DisallowedAddress,
            ),
        ];
        for (url, kind) in cases {
            let html = format!("<script src=\"{url}\"></script><script>after</script>");
            let (out, h) = run_fetching(
                &html,
                "https://example.test/",
                &PageRunOptions::default(),
                &fetcher,
            )
            .await;
            let out = out.expect("run");
            assert_eq!(
                kinds(&out),
                vec![
                    ScriptOutcome::SrcFetchFailed { kind },
                    ScriptOutcome::Succeeded
                ],
                "{url}"
            );
            assert_eq!(scripts_seen(&h), vec!["after"], "{url}");
        }
    }

    /// JS-4 / JS-8: ローカルサーバーの src を取得して実行し、404・危険なリダイレクトは種別だけ記録する。
    #[tokio::test]
    async fn js_8_local_server_src_status_and_redirect() {
        let port = spawn_http(|path| {
            if path.starts_with("/dir/ok.js") {
                http_response("200 OK", "", "okbody")
            } else if path.starts_with("/dir/redir.js") {
                http_response("302 Found", "Location: file:///etc/passwd\r\n", "")
            } else {
                http_response("404 Not Found", "", "nope")
            }
        });
        let fetcher = fetcher_loopback();
        let html = concat!(
            "<script src=\"ok.js?token=secret\"></script>",
            "<script src=\"missing.js?token=secret\"></script>",
            "<script src=\"redir.js?token=secret\"></script>",
            "<script>after</script>"
        );
        let (out, h) = run_fetching(
            html,
            &format!("http://127.0.0.1:{port}/dir/"),
            &PageRunOptions::default(),
            &fetcher,
        )
        .await;
        let out = out.expect("run");
        assert_eq!(scripts_seen(&h), vec!["okbody", "after"]);
        assert_eq!(
            kinds(&out),
            vec![
                ScriptOutcome::Succeeded,
                ScriptOutcome::SrcFetchFailed {
                    kind: SrcFetchErrorKind::HttpStatus { status: 404 }
                },
                ScriptOutcome::SrcFetchFailed {
                    kind: SrcFetchErrorKind::DisallowedScheme
                },
                ScriptOutcome::Succeeded
            ]
        );
        // 診断に URL・クエリ・パスを残さない。
        let dump = format!("{:?}", out.scripts());
        for banned in ["127.0.0.1", "secret", "passwd", "missing"] {
            assert!(!dump.contains(banned), "{banned} in {dump}");
        }
    }

    /// 公開 API の future が Send であること（await は runtime / bridge 生成前に済ませる）。
    #[test]
    fn js_4_run_page_scripts_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let fetcher = Fetcher::new(FetchOptions::new()).expect("fetcher");
        let js = JsConfig::default();
        let options = PageRunOptions::default();
        let input = PageRunInput::new("<p>x</p>", "https://example.test/");
        assert_send(&run_page_scripts(&input, &js, &options, &fetcher));
    }
}

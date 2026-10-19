//! ページスクリプトランナー: `<script>` の順次実行・`src` の取得・総量上限・
//! DOMContentLoaded / load の発火・仮想時間のタイマーループ・ページ内 `fetch` / XHR の取得と失敗の集約
//! （TASK-109・TASK-110・TASK-111・MS-6・Issue #780・#781・#782・#783・ビヘイビア `JS-4`・`JS-6`・`JS-7`・`JS-8`）。
//!
//! 「HTML を受け取り、`<script>` を文書順に実行し、変更後の HTML と実行結果の診断を返す」
//! 入口。部品は次のとおり組み合わせる。
//!
//! - [`crate::page_script::collect_page_scripts`]（#774）: 実行対象の収集
//! - [`crate::fetch::Fetcher`]: `src` とページ内 `fetch` / XHR の取得（`JS-8`。別の HTTP クライアントは作らない）
//! - [`crate::page_net`]（#783）: ページ内 `fetch` / XHR の要求キューと件数・累計バイトの上限
//! - [`crate::dom_bridge::DomBridge`]（#777）と `JsRuntime::install_dom_shim_with_options`
//!   （#778・#779・#781）: ページ内 JS から DOM を操作する経路とイベント shim
//! - [`fandhe_browser_js::EvaluateOptions`]（#776）: スクリプトごとの実時間上限の強制
//!
//! 呼び出し元は、TASK-112（#784）の専用スレッド上のアクター（[`crate::page_actor`]。実装済み。
//! `Page.navigate` への配線は #785 で実装済み）を想定する。
//!
//! # 処理の 4 段構成
//!
//! 1. **prepare（同期）**: パースと収集、base URL の検証。
//! 2. **load（async）**: 文書順に `src` を [`Fetcher`] で取得しつつ、スクリプト総バイトを
//!    評価する。この段の `.await` は [`JsRuntime`] / [`DomBridge`] の生成より前にある。
//! 3. **execute（async）**: 取得済みのソースを文書順に実行し（`type=module` のエントリは
//!    `module_graph` がここでグラフを Fetcher 経由で取得して compile・link・evaluate する。
//!    Issue #1116）、打ち切られていなければ
//!    DOMContentLoaded → load を発火する。各 `<script>` とライフサイクルイベントの後に
//!    microtask checkpoint（`__fandheRunMicrotasks()`）を評価する。
//! 4. **event loop（async。`JS-7`・`JS-8`）**: load の後に、保留中のネットワーク要求（ページ内
//!    `fetch` / XHR）を 1 件ずつ Fetcher で取得して `__fandheNetDeliver(id)` で配送し、要求が無ければ
//!    保留中のタイマーを仮想時間の期限順に `__fandheRunTimer(id)` で発火する（実時間は待たない）。
//!    終了条件は「保留中の要求とタイマーが 0」。発火回数・仮想時間・保留中タイマー数の
//!    上限を超えたら [`AbortKind`] で打ち切り、その時点の DOM を確定する。実時間の上限
//!    （スクリプト単位・ページ全体）はタイマーの評価にも課す。
//!
//! # 契約
//!
//! - ページごとに新しい [`JsRuntime`] と [`DomBridge`] を作り、ランナーが所有して返す前に
//!   解放する。グローバル状態はページ間で漏れない。
//! - 解放の順序は「診断の取得、`DomBridge::detach`、`JsRuntime` の drop、シリアライズ」。
//!   打ち切り後に放棄されたネイティブ呼び出しが遅れて DOM を変更しても、detach 済みのため
//!   結果には影響せず、子プロセス版エンジンは drop で kill / wait される（`JS-6`）。
//! - **future は `Send` ではない**（TASK-111）。ネットワーク取得を runtime が生きている間の
//!   イベントループ段で `.await` するため。`Send + Sync` が要る呼び出し元（TASK-112 のアクター）は、
//!   専用スレッドが runtime とランナーを所有する形で使い、この future をそのスレッド上で駆動する。
//! - ページ内 `fetch` / XHR の取得は呼び出し側が所有する [`Fetcher`] 経由のみ（`JS-8`）。scheme・
//!   内部アドレス・リダイレクト先の検査は Fetcher に一元化し、拒否は reject / `onerror` と
//!   [`BridgeDiagnostics::net_rejections`] で表す（URL は残さない）。件数・累計バイトの上限は
//!   [`PageNetLimits`]（`JS-6`）。上限超過でもページは打ち切らず、以降の要求を reject する。
//! - 打ち切り後はイベントループを回さない。タイマーのコールバック例外は
//!   [`BridgeDiagnostics::timer_errors`] に集約し、後続のタイマーを続ける。
//! - 例外は集約して後続を続ける。実時間超過・リソース上限・エンジン不能・件数 / バイト上限は
//!   fail-closed で以降の実行を打ち切り、残りを [`ScriptOutcome::NotExecuted`] にする。
//!   打ち切り後は DOMContentLoaded / load を発火せず、その時点の DOM を確定して返す。
//! - 実行可能なソース（inline、または取得に成功した `src`）が 1 件も無いページでは
//!   ランタイムを作らない（子プロセスを起動しない。`PERF-7`）。module のエントリは、取得が
//!   execute 段まで分からないため実行可能とみなす（module gate 有効時に限った `PERF-7` からの
//!   逸脱。REPAIR-3）。
//! - ES module（gate 有効時のみ。[`ScriptCollectionOptions::with_execute_modules`]）: 静的 `import` の
//!   グラフを [`Fetcher`] 経由で取得し、件数（[`PageRunOptions::max_modules`]）・深さ
//!   （[`MAX_MODULE_GRAPH_DEPTH`]）・バイト数・実時間・MIME・オリジン（初版は同一オリジンのみ）を
//!   fail-closed で検査する。詳細と差異は `module_graph` の doc を参照。
//!   動的 `import()` はイベントループ段で（ネット要求・タイマーより先に）1 件ずつ取り出し、同じ経路で
//!   取得して完了させる（Issue #1117。要求数の上限は [`MAX_PAGE_DYNAMIC_IMPORTS`]、処理結果は
//!   [`PageRunOutput::dynamic_imports`]）。boa は動的 `import()` 未対応のため要求が生じない。
//! - `src` の取得は呼び出し側が所有する [`Fetcher`] 経由のみ。scheme・内部アドレス・
//!   リダイレクト先の検査は Fetcher に一元化されており（SSRF 対策。`SEC-2`）、拒否された
//!   スクリプトは [`ScriptOutcome::SrcFetchFailed`] として後続の実行を続ける。診断には
//!   URL 本文を残さず、失敗の種別だけを残す（クエリにトークンを含み得るため）。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - `<base href>` による `src` の解決（ページの base URL に対する相対解決のみ）。
//! - classic の `src` 応答の Content-Type / nosniff 検査と charset 判定（`CORE-5` (7)。UTF-8 の
//!   非可逆変換のみ）。module の応答は JavaScript MIME type を必須とする点が非対称（#1116）。
//! - `src` 取得全体の時間予算（現状は Fetcher のタイムアウト × 件数。`page_wall_time` は
//!   JS 実行だけを対象とする）。
//! - 要素レベルの EventTarget・`onload` / `onreadystatechange` プロパティ・
//!   キャプチャ / 完全なバブリング、`async` / `defer` の順序近似（全て文書順で実行する）。
//! - 上限値の設定ファイルからの読み込みは未対応（専用スレッド化は [`crate::page_actor`] で実装済み）。
//! - イベントループは全スクリプトと load の後に回す近似で、実ブラウザのようにスクリプトの間へ
//!   0ms タイマーが割り込む順序にはならない。ネイティブ `Promise` のジョブは仮想ループへ
//!   統合しない（エンジン依存。TASK-30）。HTML のネスト時 4ms クランプも行わない。
//!   文字列ハンドラ（`setTimeout("code")`）は評価せず診断のみ残す（意図的な差異）。
//!   `Date` は実時間のままで仮想時間と連動しない。
//! - 仮想時間の上限（既定 30 秒）より後に期限を持つタイマーが残るページは、load の後に
//!   [`AbortKind::VirtualTime`] の診断が付く（DOM の確定には影響しない。TASK-113 で再評価）。
//! - ページ内 `fetch` / XHR（#783）: GET のみ。ヘッダ・Cookie・認証は送らない。CORS 判定・
//!   Content-Type / charset 判定・ネットワーク待ちの独立した時間予算は未実装（待ちはページ時計に含み、
//!   1 件の待ちは Fetcher のタイムアウトで頭打ち）。応答は仮想時間 0 の遅延として、同時刻のタイマーより
//!   先に配送する近似。boa ではネイティブ `Promise` のジョブが drain されず、`fetch` の続きが走らない
//!   （XHR は動く。js crate の `run_jobs` 未対応。TASK-30）。
//! - 可観測性（対応する `OperationKind` が無い）は未対応。
//! - パースは既定の `scripting_enabled = false` のため `<noscript>` の扱いが実ブラウザと異なる。
//! - スクリプトの completion 値が結果サイズ上限を超えると例外として扱われる
//!   （ソースの書き換えによる回避はしない）。

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use fandhe_browser_js::{DynamicImportRequest, EvaluateOptions, JsEngineError};
use reqwest::Url;

use crate::config::JsConfig;
use crate::dom::Document;
use crate::dom_bridge::{
    BridgeDiagnostics, DocumentReadyState, DomBridge, DomBridgeError, DomBridgeLimits,
    ResourceLimitKind, parse_location,
};
use crate::error::Error;
use crate::event_loop::{EventLoopLimits, NextStep, TimerLimitKind};
use crate::fetch::{FetchOptions, Fetcher};
use crate::js_stub::JsRuntime;
use crate::page_net::{NetOutcome, NetRejectKind, NetReport, PageNetLimits};
use crate::page_script::{
    CollectedScripts, ScriptCollectionOptions, ScriptDiagnosticKind, ScriptKind, ScriptSource,
    collect_page_scripts,
};
use crate::parse::{ParseOptions, parse_document};
use crate::{NodeId, dom_bridge::truncate_utf8};

pub use crate::dom_bridge::LifecycleEvent;

mod exception_summary;
mod module_graph;

pub use exception_summary::{ExceptionKind, ExceptionMessageClass, ExceptionSummary};

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
/// ページあたりの ES module の件数の既定上限（64。inline の module と取得した依存の合計。
/// `JS-6`・Issue #1116）。
pub const DEFAULT_MAX_PAGE_MODULES: usize = 64;
/// module グラフの深さの上限（32。entry を 0 とし、33 段目のモジュールは取得しない。
/// `JS-6`・Issue #1116）。
pub const MAX_MODULE_GRAPH_DEPTH: usize = 32;
/// ページあたりの動的 `import()` 要求数の上限（256。js crate の保留キューの上限と同じ。
/// キャッシュに当たる要求は件数枠を消費しないため、繰り返しによる無限の周回をここで抑える。
/// `JS-6`・Issue #1117）。
pub const MAX_PAGE_DYNAMIC_IMPORTS: usize = 256;

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

/// ランナーの設定。上限値の設定ファイルからの読み込みは未対応（REPAIR-3）。
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
    max_modules: usize,
    event_loop: EventLoopLimits,
    net_limits: PageNetLimits,
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
            max_modules: DEFAULT_MAX_PAGE_MODULES,
            event_loop: EventLoopLimits::default(),
            net_limits: PageNetLimits::default(),
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

    /// ページあたりの ES module の件数の上限を設定する（`JS-6`。既定は
    /// [`DEFAULT_MAX_PAGE_MODULES`]。`with_execute_modules(true)` のときだけ意味を持つ）。
    #[must_use]
    pub fn with_max_modules(mut self, limit: usize) -> Self {
        self.max_modules = limit;
        self
    }

    /// イベントループ（タイマー）の上限を差し替える（`JS-6`・`JS-7`）。
    #[must_use]
    pub fn with_event_loop_limits(mut self, limits: EventLoopLimits) -> Self {
        self.event_loop = limits;
        self
    }

    /// ページ内 `fetch` / `XMLHttpRequest` の上限（要求件数・累計バイト）を差し替える（`JS-6`・`JS-8`）。
    #[must_use]
    pub fn with_net_limits(mut self, limits: PageNetLimits) -> Self {
        self.net_limits = limits;
        self
    }

    /// ページあたりの ES module の件数の上限。
    pub fn max_modules(&self) -> usize {
        self.max_modules
    }

    /// ページ内 `fetch` / `XMLHttpRequest` の上限。
    pub fn net_limits(&self) -> &PageNetLimits {
        &self.net_limits
    }

    /// イベントループ（タイマー）の上限。
    pub fn event_loop_limits(&self) -> &EventLoopLimits {
        &self.event_loop
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
    /// スクリプト件数の上限を超えた（classic は収集時の `max_scripts`、ES module は
    /// [`PageRunOptions::max_modules`]）。
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
    /// タイマーの発火回数の上限を超えた（`JS-6`・`JS-7`）。
    TimerFires {
        /// 超過した上限の値。
        limit: usize,
    },
    /// 仮想時間の上限を超える期限のタイマーが残った（`JS-6`・`JS-7`）。
    VirtualTime {
        /// 超過した上限の値。
        limit: Duration,
    },
    /// 保留中のタイマー数の上限を超えた（`JS-6`・`JS-7`）。
    RegisteredTimers {
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
            Self::TimerFires { .. } => "timer_fires",
            Self::VirtualTime { .. } => "virtual_time",
            Self::RegisteredTimers { .. } => "registered_timers",
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
    /// module の specifier が bare（`/`・`./`・`../` で始まらず、絶対 URL でもない）。
    BareSpecifier,
    /// ページと別オリジンの module（リダイレクト先を含む。初版は同一オリジンのみ許可。
    /// 設計 `es-module-design.md` §6）。
    CrossOrigin,
    /// module の応答の MIME が JavaScript MIME type でない（Content-Type が無い場合を含む）。
    UnsupportedMimeType,
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
            Self::BareSpecifier => "bare_specifier",
            Self::CrossOrigin => "cross_origin",
            Self::UnsupportedMimeType => "unsupported_mime_type",
            Self::Other => "other",
        }
    }
}

/// スクリプト 1 件の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptOutcome {
    /// 正常終了。完了値は変換も保持もしない（オブジェクト等の変換できない完了値でも成功。
    /// `JS-4`・Issue #1134）。
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
    /// ES module の評価が完了せず保留のままになった（top-level await・未完了の動的 `import()`）。
    /// 成功を装わない（`REPAIR-3`）。後続の実行は続ける。
    ModulePending,
    /// エンジンが ES module の評価に未対応だった（`NotYetImplemented`）。後続の実行は続ける。
    ModuleUnsupported,
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
            Self::ModulePending => "module_pending",
            Self::ModuleUnsupported => "module_unsupported",
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
    exception: Option<ExceptionSummary>,
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

    /// 例外の要約（分類名と不足識別子名。生メッセージ・URL は含まない。`JS-5`・Issue #1133）。
    ///
    /// 現状は `type=module` の評価失敗（`outcome` が `Exception`）のときだけ埋まる。
    /// classic の例外は将来対応で、常に `None`（`REPAIR-3`）。`outcome` の固定文は変えない。
    pub fn exception_summary(&self) -> Option<&ExceptionSummary> {
        self.exception.as_ref()
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

/// イベントループ（仮想時間のタイマー）の実行結果（`JS-7`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct EventLoopReport {
    fired: usize,
    virtual_elapsed: Duration,
    pending_at_end: usize,
}

impl EventLoopReport {
    /// 発火したタイマーの回数（interval は発火ごとに数える）。
    pub fn fired(&self) -> usize {
        self.fired
    }

    /// 仮想時計の進み（実時間ではない）。
    pub fn virtual_elapsed(&self) -> Duration {
        self.virtual_elapsed
    }

    /// 終了時点で保留中だったタイマー数（上限で打ち切ったときに残る）。
    pub fn pending_at_end(&self) -> usize {
        self.pending_at_end
    }
}

/// 動的 `import()` が失敗した理由の種別（`JS-4`・`JS-8`・Issue #1117）。
///
/// URL・specifier・エンジンの生メッセージは残さない（クエリにトークンを含み得るため）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DynamicImportFailureKind {
    /// 解決・取得の失敗（bare specifier・別オリジン・MIME・ステータス等）。
    Fetch(SrcFetchErrorKind),
    /// 構文エラー等で compile できなかった。
    CompileFailed,
    /// 取得・link は成功したが、対象の評価が例外で終わり `import()` が reject された。
    EvaluationFailed,
    /// エンジンが module に未対応。
    Unsupported,
    /// 呼び出し元 module を特定できなかった（子の再起動後等。fail-closed）。
    UnknownReferrer,
    /// 上限・実時間・エンジン不能でページを打ち切った（打ち切りの種別）。
    Limit(AbortKind),
}

impl DynamicImportFailureKind {
    /// ログ・診断・`import()` の reject メッセージで使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fetch(kind) => kind.as_str(),
            Self::CompileFailed => "module_compile_failed",
            Self::EvaluationFailed => "module_evaluation_failed",
            Self::Unsupported => "module_unsupported",
            Self::UnknownReferrer => "unknown_referrer",
            Self::Limit(kind) => kind.as_str(),
        }
    }
}

/// 動的 `import()` の処理結果（`JS-4`・Issue #1117）。gate 無効のページでは全項目 0。
///
/// 失敗の記録は種別だけで URL を含まない。件数は [`MAX_PAGE_DYNAMIC_IMPORTS`] で頭打ち。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DynamicImportReport {
    pub(crate) requested: usize,
    pub(crate) fulfilled: usize,
    pub(crate) rejected: usize,
    pub(crate) pending: usize,
    pub(crate) failures: Vec<DynamicImportFailureKind>,
    pub(crate) evaluation_errors: Vec<ExceptionSummary>,
}

impl DynamicImportReport {
    /// エンジンから取り出した要求の件数。
    pub fn requested(&self) -> usize {
        self.requested
    }

    /// 取得・評価に成功し、`import()` の Promise が実際に resolve した件数。
    pub fn fulfilled(&self) -> usize {
        self.fulfilled
    }

    /// reject した件数（打ち切りで reject したものを含む）。
    pub fn rejected(&self) -> usize {
        self.rejected
    }

    /// 完了を通知したが Promise が未確定のままの件数（top-level await が待ち続けている等）。
    /// resolve にも reject にも数えない。
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// reject の理由（種別のみ。処理順）。
    pub fn failures(&self) -> &[DynamicImportFailureKind] {
        &self.failures
    }

    /// `failures` 中の `EvaluationFailed` それぞれの例外の要約（同数・同順。`JS-5`・Issue #1133）。
    /// 生メッセージ・URL は含まない。特定できないものは `Other` の要約になる。
    pub fn evaluation_errors(&self) -> &[ExceptionSummary] {
        &self.evaluation_errors
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
    event_loop: EventLoopReport,
    net: NetReport,
    dynamic_imports: DynamicImportReport,
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

    /// イベントループ（仮想時間のタイマー）の結果（`JS-7`）。
    pub fn event_loop(&self) -> &EventLoopReport {
        &self.event_loop
    }

    /// ページ内 `fetch` / `XMLHttpRequest` の実行結果（`JS-8`）。
    pub fn net(&self) -> &NetReport {
        &self.net
    }

    /// 動的 `import()` の処理結果（`JS-4`。gate 無効なら全項目 0。Issue #1117）。
    pub fn dynamic_imports(&self) -> &DynamicImportReport {
        &self.dynamic_imports
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
    /// ES module（inline / `src`）。取得と compile は execute 段で `module_graph` が行う
    /// （依存は compile の後でないと分からず、ページ時計の下で取得するため。Issue #1116）。
    Module,
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
        Error::CrossOriginRedirect => SrcFetchErrorKind::CrossOrigin,
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

/// `final_url` が `request` と同一オリジン（scheme・host・port）かを判定する。解析不能は偽。
fn same_origin_url(request: &Url, final_url: &str) -> bool {
    Url::parse(final_url).is_ok_and(|u| u.origin() == request.origin())
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
        // module は取得も計上もせず execute 段へ回す（Issue #1116）。
        if entry.kind() == ScriptKind::Module {
            loads.push(ScriptLoad::Module);
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
    let loaded = load_sources(&prepared.collected, &prepared.base, fetcher, options).await;
    execute(input, options, prepared, loaded, fetcher, make_runtime).await
}

/// 取得済みのソースを実行する。ネットワークを使うのはイベントループ段のページ内
/// `fetch` / `XMLHttpRequest` の取得（`JS-8`）だけで、`src` の取得は済んでいる。
/// ネットワークを使わない単体テストの継ぎ目でもある。
///
/// runtime を保持したまま `.await` するため、この future は `Send` ではない（TASK-111）。
async fn execute(
    input: &PageRunInput<'_>,
    options: &PageRunOptions,
    prepared: PreparedPage,
    loaded: LoadedScripts,
    fetcher: &Fetcher,
    make_runtime: impl FnOnce() -> crate::Result<JsRuntime>,
) -> crate::Result<PageRunOutput> {
    let PreparedPage {
        document: doc,
        collected,
        base,
    } = prepared;
    let LoadedScripts { loads, byte_abort } = loaded;

    let bridge = DomBridge::new(options.bridge_limits.clone());
    bridge.attach(doc).map_err(bridge_error)?;
    bridge.set_location(input.base_url).map_err(bridge_error)?;
    bridge
        .set_event_loop_limits(options.event_loop.clone())
        .map_err(bridge_error)?;
    bridge
        .set_net_limits(options.net_limits)
        .map_err(bridge_error)?;

    // 実行され得るソース（バイト上限の超過位置より前の inline / 取得成功した src）の有無。
    let abort_index = byte_abort.as_ref().map(|(i, _)| *i);
    let has_runnable = collected
        .entries()
        .iter()
        .zip(loads.iter())
        .take_while(|(e, _)| abort_index.is_none_or(|a| e.index() < a))
        .any(|(_, l)| {
            matches!(
                l,
                ScriptLoad::Inline | ScriptLoad::Ready(_) | ScriptLoad::Module
            )
        });
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
        fetcher,
        base: &base,
    };
    // ページで 1 つの module グラフ（モジュールマップ・件数・総バイトの計上。Issue #1116）。
    let mut graph = module_graph::ModuleGraph::new();
    for (entry, load) in collected.entries().iter().zip(loads.iter()) {
        // module 評価失敗の例外の要約（`outcome` が `Exception` のままのときだけ記録に載せる。Issue #1133）。
        let mut module_exception: Option<ExceptionSummary> = None;
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
            // classic の本文は実行時点で総量へ加算する（文書順の会計。後続の classic の分で
            // 先行の module を打ち切らない。`JS-6`・Issue #1116）。
            let classic_len = match (load, entry.source()) {
                (ScriptLoad::Ready(src), _) | (ScriptLoad::Inline, ScriptSource::Inline(src)) => {
                    Some(src.len())
                }
                _ => None,
            };
            if let Some(len) = classic_len
                && !graph.account_classic(len, options.max_total_script_bytes)
            {
                let kind = AbortKind::ScriptBytes {
                    scope: ScriptBytesScope::Total,
                    limit: options.max_total_script_bytes,
                };
                abort = Some(PageAbort {
                    kind: kind.clone(),
                    index: Some(entry.index()),
                });
                records.push(ScriptRunRecord {
                    index: Some(entry.index()),
                    node: Some(entry.node()),
                    outcome: ScriptOutcome::Aborted {
                        kind,
                        message: None,
                    },
                    exception: None,
                });
                continue;
            }
            match (entry.source(), load) {
                (_, ScriptLoad::Failed(kind)) => ScriptOutcome::SrcFetchFailed { kind: *kind },
                (
                    source,
                    load @ (ScriptLoad::Inline | ScriptLoad::Ready(_) | ScriptLoad::Module),
                ) => {
                    match runtime.as_mut() {
                        Some(rt) => {
                            let mut o = match (load, source) {
                                (ScriptLoad::Module, _) => {
                                    let result = module_graph::run_module_entry(
                                        rt,
                                        &ctx,
                                        &mut graph,
                                        source,
                                        entry.index(),
                                        &mut abort,
                                    )
                                    .await;
                                    module_exception = result.exception;
                                    result.outcome
                                }
                                (ScriptLoad::Ready(src), _) | (_, ScriptSource::Inline(src)) => {
                                    run_one(rt, &ctx, entry.node(), entry.index(), src, &mut abort)?
                                }
                                _ => ScriptOutcome::NotExecuted {
                                    kind: AbortKind::EngineError,
                                },
                            };
                            // microtask checkpoint（スクリプト末尾の `queueMicrotask` を実行する。`JS-7`）。
                            // 上限超過済みなら checkpoint を回さない（超過後に callback が DOM を
                            // 変更しないよう、下の上限判定で打ち切る。`JS-6`）。
                            if abort.is_none()
                                && tripped_abort_kind(&ctx)?.is_none()
                                && let Some((kind, message)) = microtask_checkpoint(rt, &ctx)
                            {
                                abort = Some(PageAbort {
                                    kind: kind.clone(),
                                    index: Some(entry.index()),
                                });
                                o = ScriptOutcome::Aborted { kind, message };
                            }
                            // リスナー・タイマーの保持件数の上限超過は、超過させたスクリプトの直後に
                            // 確定し後続のスクリプトを実行しない（`JS-6`）。
                            if abort.is_none()
                                && let Some(kind) = tripped_abort_kind(&ctx)?
                            {
                                abort = Some(PageAbort {
                                    kind: kind.clone(),
                                    index: Some(entry.index()),
                                });
                                // 原因スクリプト自身の結果も打ち切りとして記録する（`JS-6`）。
                                ScriptOutcome::Aborted {
                                    kind,
                                    message: None,
                                }
                            } else {
                                o
                            }
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
        let exception = if matches!(outcome, ScriptOutcome::Exception { .. }) {
            module_exception
        } else {
            None
        };
        records.push(ScriptRunRecord {
            index: Some(entry.index()),
            node: Some(entry.node()),
            outcome,
            exception,
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
    if abort.is_none()
        && let Some(kind) = tripped_abort_kind(&ctx)?
    {
        abort = Some(PageAbort { kind, index: None });
    }

    let lifecycle = fire_lifecycle(&ctx, runtime.as_mut(), &mut abort)?;
    let mut dynamic_imports = DynamicImportReport::default();
    let event_loop = run_event_loop(
        &ctx,
        fetcher,
        runtime.as_mut(),
        &mut graph,
        &mut dynamic_imports,
        &mut abort,
    )
    .await?;

    let elapsed = page_start.elapsed();
    let net = bridge.net_report().map_err(bridge_error)?;
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
        RunTail {
            lifecycle,
            bridge_diagnostics,
            elapsed,
            event_loop,
            net,
            dynamic_imports,
        },
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
    /// module グラフの取得に使う Fetcher（`JS-8`。呼び出し側が所有する唯一の取得経路）。
    fetcher: &'a Fetcher,
    /// 検証済みの文書の base URL（inline module の referrer・同一オリジン判定の基準）。
    base: &'a Url,
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

/// 評価の失敗（例外以外）を打ち切りの種別とメッセージに写す。
///
/// 呼び出し元: classic の [`evaluate_with_budget`] と、module の compile・link・evaluate
/// （`module_graph`。Issue #1116）。`scope` は `Timeout` のときに超過した上限の適用範囲。
/// 未知の種別は fail-closed で `EngineError`（メッセージなし）。
fn engine_error_abort(
    err: &Error,
    scope: WallTimeScope,
    options: &PageRunOptions,
) -> (AbortKind, Option<String>) {
    match err {
        Error::JsEvaluation(JsEngineError::Timeout(m)) => {
            let limit = match scope {
                WallTimeScope::Script => options.script_wall_time,
                WallTimeScope::Page => options.page_wall_time,
            };
            (
                AbortKind::WallTime { scope, limit },
                Some(truncated_message(m).0),
            )
        }
        Error::JsEvaluation(JsEngineError::ResourceLimitExceeded(m)) => {
            (AbortKind::ResourceLimit, Some(truncated_message(m).0))
        }
        Error::JsEvaluation(JsEngineError::EngineUnavailable(m)) => {
            (AbortKind::EngineUnavailable, Some(truncated_message(m).0))
        }
        _ => (AbortKind::EngineError, None),
    }
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

    // 完了値は誰も使わないため、変換せず捨てる（オブジェクト等の完了値を例外と誤報告しない。
    // ソースは書き換えない。`JS-4`・Issue #1134）。
    let result = rt.execute_with_options(
        src,
        &EvaluateOptions::default()
            .with_timeout(timeout)
            .with_discard_completion_value(true),
    );

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
        Err(e) => engine_error_abort(&e, scope, options),
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
        // microtask checkpoint（リスナー内の `queueMicrotask` を実行する。`JS-7`）。
        let outcome = match (outcome, runtime.as_deref_mut()) {
            (LifecycleOutcome::Dispatched, Some(rt))
                if abort.is_none() && tripped_abort_kind(ctx)?.is_none() =>
            {
                match microtask_checkpoint(rt, ctx) {
                    Some((kind, _)) => {
                        *abort = Some(PageAbort {
                            kind: kind.clone(),
                            index: None,
                        });
                        LifecycleOutcome::Aborted { kind }
                    }
                    None => LifecycleOutcome::Dispatched,
                }
            }
            (other, _) => other,
        };
        // リスナー・タイマーが上限を超えて登録された場合は、以降のイベントを発火しない（`JS-6`）。
        if abort.is_none()
            && let Some(kind) = tripped_abort_kind(ctx)?
        {
            *abort = Some(PageAbort { kind, index: None });
        }
        records.push(LifecycleEventRecord { event, outcome });
    }
    Ok(records)
}

/// ブリッジのリソース上限（リスナー・保留中タイマー数）の超過を [`AbortKind`] に写す（`JS-6`）。
fn tripped_abort_kind(ctx: &RunContext<'_>) -> crate::Result<Option<AbortKind>> {
    // shim は op のエラー（操作予算切れ）を握りつぶし評価が完了扱いになるため、microtask
    // checkpoint などの後でもここで検出して打ち切る（`JS-6`・fail-closed）。
    if ctx.bridge.op_limit_tripped().map_err(bridge_error)? {
        return Ok(Some(AbortKind::ResourceLimit));
    }
    Ok(
        match ctx.bridge.resource_limit_tripped().map_err(bridge_error)? {
            None => None,
            Some(ResourceLimitKind::RegisteredTimers) => Some(AbortKind::RegisteredTimers {
                limit: ctx.options.event_loop.max_registered_timers(),
            }),
            // 未知の種別も含め、リソース上限として打ち切る（fail-closed）。
            Some(_) => Some(AbortKind::ResourceLimit),
        },
    )
}

/// microtask checkpoint を評価する。打ち切りになる場合だけ理由とエンジンのメッセージを返す
/// （例外は shim が `timer_errors` に集約するため、ここでは継続扱い。`JS-7`）。
fn microtask_checkpoint(
    rt: &mut JsRuntime,
    ctx: &RunContext<'_>,
) -> Option<(AbortKind, Option<String>)> {
    match evaluate_with_budget(rt, ctx, "__fandheRunMicrotasks()") {
        Evaluated::Completed | Evaluated::Exception { .. } => None,
        Evaluated::Aborted { kind, message } => Some((kind, message)),
        Evaluated::BudgetExhausted(kind) => Some((kind, None)),
    }
}

/// load の後に、保留中のタイマーを仮想時間の期限順に発火する（`JS-7`・`JS-6`）。
///
/// 打ち切り済み・runtime 無しなら回さない。1 周ごとに「保留中タイマー数の超過 → 次のタイマー
/// （空なら正常終了、仮想時間・発火回数の超過は打ち切り）→ 評価（実時間の上限つき）」の順で判定する。
/// 評価する文字列は固定の関数名と整数の ID だけで、ページ由来の値を埋め込まない。
async fn run_event_loop(
    ctx: &RunContext<'_>,
    fetcher: &Fetcher,
    runtime: Option<&mut JsRuntime>,
    graph: &mut module_graph::ModuleGraph,
    dynamic_imports: &mut DynamicImportReport,
    abort: &mut Option<PageAbort>,
) -> crate::Result<EventLoopReport> {
    if abort.is_none()
        && let Some(rt) = runtime
    {
        // 動的 `import()` は gate 有効のページだけ取り出す（無効なら `take` を呼ばない。`JS-4`）。
        let mut take_dynamic = ctx.options.collection.execute_modules();
        // 取り出し済み・未処理の要求（1 周に 1 件処理する）。
        let mut dynamic_queue: VecDeque<DynamicImportRequest> = VecDeque::new();
        loop {
            if let Some(kind) = tripped_abort_kind(ctx)? {
                // 取り出し済みで未処理の動的 import は reject として計上してから打ち切る。
                for _ in dynamic_queue.drain(..) {
                    dynamic_imports.rejected = dynamic_imports.rejected.saturating_add(1);
                    dynamic_imports
                        .failures
                        .push(DynamicImportFailureKind::Limit(kind.clone()));
                }
                *abort = Some(PageAbort { kind, index: None });
                break;
            }
            // 動的 `import()` の保留を取り出し、1 件ずつ取得して完了させる（Issue #1117）。
            // ネット要求・タイマーより先に処理する（応答は仮想時間 0 の近似）。
            // 取り出し自体は JS を走らせないため、ページの残り時間が尽きていても行う。残っていれば
            // 下で abort し、要求を数える（取りこぼして idle の成功終了にしない）。
            if take_dynamic && dynamic_queue.is_empty() {
                match rt.take_dynamic_import_requests() {
                    Ok(requests) => {
                        // 取り出した時点で全件を計上する（打ち切りで残っても requested が過小にならない）。
                        dynamic_imports.requested =
                            dynamic_imports.requested.saturating_add(requests.len());
                        dynamic_queue.extend(requests);
                    }
                    // boa 等の未対応エンジンは「要求なし」。以降このページでは呼ばない。
                    Err(Error::JsEvaluation(JsEngineError::NotYetImplemented(_))) => {
                        take_dynamic = false;
                    }
                    Err(e) => {
                        let (kind, _) = engine_error_abort(&e, WallTimeScope::Page, ctx.options);
                        *abort = Some(PageAbort { kind, index: None });
                        break;
                    }
                }
            }
            if let Some(request) = dynamic_queue.pop_front() {
                if script_budget(
                    ctx.options.script_wall_time,
                    ctx.options.page_wall_time,
                    ctx.page_start.elapsed(),
                )
                .is_none()
                {
                    // JS を走らせる予算が無い。エンジンに残る要求（取り出し済みの未処理分を含む）を
                    // 数えて reject 扱いにし、ページを打ち切る（成功終了にしない）。
                    let abort_kind = page_wall_abort(ctx.options, None);
                    let dropped = dynamic_queue.len().saturating_add(1);
                    dynamic_imports.rejected = dynamic_imports.rejected.saturating_add(dropped);
                    for _ in 0..dropped {
                        dynamic_imports
                            .failures
                            .push(DynamicImportFailureKind::Limit(abort_kind.kind.clone()));
                    }
                    dynamic_queue.clear();
                    *abort = Some(abort_kind);
                    break;
                }
                if !module_graph::pump_one_dynamic_import(
                    rt,
                    ctx,
                    graph,
                    &request,
                    dynamic_imports,
                    abort,
                )
                .await?
                {
                    // どの打ち切り経路でも、取り出し済みで未処理の要求を reject として計上する。
                    if let Some(tripped) = abort.as_ref() {
                        let kind = tripped.kind.clone();
                        for _ in dynamic_queue.drain(..) {
                            dynamic_imports.rejected = dynamic_imports.rejected.saturating_add(1);
                            dynamic_imports
                                .failures
                                .push(DynamicImportFailureKind::Limit(kind.clone()));
                        }
                    }
                    break;
                }
                continue;
            }
            // 保留中のネットワーク要求（ページ内 fetch / XHR。`JS-8`）は、タイマーより先に
            // 1 件ずつ取得して配送する（応答は仮想時間 0 の遅延の近似）。
            if ctx.bridge.net_pending().map_err(bridge_error)? > 0 {
                if script_budget(
                    ctx.options.script_wall_time,
                    ctx.options.page_wall_time,
                    ctx.page_start.elapsed(),
                )
                .is_none()
                {
                    *abort = Some(page_wall_abort(ctx.options, None));
                    break;
                }
                if !pump_one_net_request(ctx, fetcher, rt, abort).await? {
                    break;
                }
                continue;
            }
            // 待機すべきタイマーが無ければ正常終了する（残り時間 0 でも abort 扱いにしない）。
            let (_, _, pending) = ctx.bridge.event_loop_state().map_err(bridge_error)?;
            if pending == 0 {
                break;
            }
            // ページの残り時間が無いなら、タイマーを取り出して発火回数に数える前に打ち切る
            // （取り出すと評価されないまま one-shot が失われ、報告が実態とずれる）。
            if script_budget(
                ctx.options.script_wall_time,
                ctx.options.page_wall_time,
                ctx.page_start.elapsed(),
            )
            .is_none()
            {
                *abort = Some(page_wall_abort(ctx.options, None));
                break;
            }
            let id = match ctx.bridge.next_timer().map_err(bridge_error)? {
                NextStep::Due(id) => id,
                NextStep::Idle => break,
                NextStep::Limit(kind) => {
                    let limits = &ctx.options.event_loop;
                    let kind = match kind {
                        TimerLimitKind::VirtualTime => AbortKind::VirtualTime {
                            limit: limits.max_virtual_time(),
                        },
                        // 未知の種別も発火回数の超過として打ち切る（fail-closed）。
                        _ => AbortKind::TimerFires {
                            limit: limits.max_timer_fires(),
                        },
                    };
                    *abort = Some(PageAbort { kind, index: None });
                    break;
                }
            };
            match evaluate_with_budget(rt, ctx, &format!("__fandheRunTimer({})", id.get())) {
                Evaluated::Completed | Evaluated::Exception { .. } => {}
                Evaluated::Aborted { kind, .. } => {
                    *abort = Some(PageAbort { kind, index: None });
                    break;
                }
                Evaluated::BudgetExhausted(_) => {
                    *abort = Some(page_wall_abort(ctx.options, None));
                    break;
                }
            }
            // shim は op のエラー（操作予算切れ）を握りつぶし、コールバックが走らないまま
            // 完了する。予算切れはここで検出して打ち切る（`JS-6`）。
            if ctx.bridge.op_limit_tripped().map_err(bridge_error)? {
                *abort = Some(PageAbort {
                    kind: AbortKind::ResourceLimit,
                    index: None,
                });
                break;
            }
            ctx.bridge.rearm_timer(id).map_err(bridge_error)?;
        }
    }
    let (virtual_elapsed, fired, pending_at_end) =
        ctx.bridge.event_loop_state().map_err(bridge_error)?;
    Ok(EventLoopReport {
        fired,
        virtual_elapsed,
        pending_at_end,
    })
}

/// 保留中のネットワーク要求を 1 件、Fetcher で取得して JS へ配送する（`JS-8`・`JS-6`）。
///
/// 取得は [`Fetcher::get_with_max_body`] だけを使い、scheme・内部アドレス・リダイレクト先の
/// 検査は Fetcher に任せる（別の HTTP クライアントは作らない。SSRF 対策。`SEC-2`）。本文の上限は
/// 累計の残りバイト数で受信中に強制する。評価する文字列は固定の関数名と整数 ID だけで、
/// ページ由来の値は埋め込まない。打ち切った場合は `abort` に理由を入れて `false` を返す。
async fn pump_one_net_request(
    ctx: &RunContext<'_>,
    fetcher: &Fetcher,
    rt: &mut JsRuntime,
    abort: &mut Option<PageAbort>,
) -> crate::Result<bool> {
    let Some(request) = ctx.bridge.take_next_net_request().map_err(bridge_error)? else {
        return Ok(true);
    };
    let remaining = ctx.bridge.net_remaining_bytes().map_err(bridge_error)?;
    let outcome = if remaining == 0 {
        NetOutcome::Rejected(NetRejectKind::ResponseBytesLimit)
    } else {
        // 取得待ちもページの残り実時間で打ち切る（応答しない取得先で Fetcher の既定まで待たない）。
        let page_left = ctx
            .options
            .page_wall_time
            .saturating_sub(ctx.page_start.elapsed());
        match fetcher
            .get_for_page_net(request.url.as_str(), remaining, page_left)
            .await
        {
            Ok(response) => {
                let cap = usize::try_from(remaining).unwrap_or(usize::MAX);
                // 非可逆変換で膨らむ分も、確保前に累計の残りへ収まるか判定する。
                if lossy_len_exceeds(response.body(), cap) {
                    NetOutcome::Rejected(NetRejectKind::ResponseBytesLimit)
                } else if !same_origin_url(&request.url, response.final_url()) {
                    // CORS 未実装の間は別オリジンへのリダイレクト先の応答を公開しない。
                    NetOutcome::Rejected(NetRejectKind::CrossOrigin)
                } else {
                    NetOutcome::Response {
                        status: response.status(),
                        body: response.body_text_lossy(),
                        final_url: response.final_url().to_owned(),
                    }
                }
            }
            // 実効上限が累計の残り由来なら累計上限、それより小さければ 1 件の上限。
            Err(Error::ResponseTooLarge { limit }) if limit >= remaining => {
                NetOutcome::Rejected(NetRejectKind::ResponseBytesLimit)
            }
            Err(Error::Timeout { .. })
                if ctx.page_start.elapsed() >= ctx.options.page_wall_time =>
            {
                // ページの実時間上限による取得のキャンセル。配送せずページを打ち切る。
                *abort = Some(page_wall_abort(ctx.options, None));
                return Ok(false);
            }
            Err(e) => NetOutcome::Rejected(NetRejectKind::from_fetch_error(&e)),
        }
    };
    ctx.bridge
        .begin_net_delivery(&request, outcome)
        .map_err(bridge_error)?;
    let delivered = evaluate_with_budget(rt, ctx, &format!("__fandheNetDeliver({})", request.id));
    // 配送後は JS の振る舞いに依存せず、Rust 側の本文を必ず解放する。
    ctx.bridge.finish_net_delivery().map_err(bridge_error)?;
    match delivered {
        Evaluated::Completed | Evaluated::Exception { .. } => {}
        Evaluated::Aborted { kind, .. } => {
            *abort = Some(PageAbort { kind, index: None });
            return Ok(false);
        }
        Evaluated::BudgetExhausted(_) => {
            *abort = Some(page_wall_abort(ctx.options, None));
            return Ok(false);
        }
    }
    // shim は op のエラー（操作予算切れ）を握りつぶすため、ここで検出して打ち切る（`JS-6`）。
    if ctx.bridge.op_limit_tripped().map_err(bridge_error)? {
        *abort = Some(PageAbort {
            kind: AbortKind::ResourceLimit,
            index: None,
        });
        return Ok(false);
    }
    // Promise の反応・XHR コールバック内の `queueMicrotask` を実行する。
    if let Some((kind, _)) = microtask_checkpoint(rt, ctx) {
        *abort = Some(PageAbort { kind, index: None });
        return Ok(false);
    }
    Ok(true)
}

/// [`finish`] へ渡す、スクリプト実行以外の実行結果。
struct RunTail {
    lifecycle: Vec<LifecycleEventRecord>,
    bridge_diagnostics: BridgeDiagnostics,
    elapsed: Duration,
    event_loop: EventLoopReport,
    net: NetReport,
    dynamic_imports: DynamicImportReport,
}

/// 実行結果と収集時の診断を通し番号で統合して出力にまとめる。
fn finish(
    html: String,
    mut scripts: Vec<ScriptRunRecord>,
    collected: &CollectedScripts,
    abort: Option<PageAbort>,
    tail: RunTail,
) -> PageRunOutput {
    for d in collected.diagnostics() {
        scripts.push(ScriptRunRecord {
            index: d.index(),
            node: d.node(),
            outcome: ScriptOutcome::Skipped {
                kind: d.kind(),
                count: d.count(),
            },
            exception: None,
        });
    }
    // 安定ソート。通し番号の無い診断（件数上限）は末尾に置く。
    scripts.sort_by_key(|r| r.index.unwrap_or(usize::MAX));
    PageRunOutput {
        html,
        scripts,
        dropped_collection_diagnostics: collected.dropped_diagnostics(),
        abort,
        lifecycle: tail.lifecycle,
        bridge_diagnostics: tail.bridge_diagnostics,
        elapsed: tail.elapsed,
        event_loop: tail.event_loop,
        net: tail.net,
        dynamic_imports: tail.dynamic_imports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js_shim::JS_SHIM_SOURCES;
    use fandhe_browser_js::{EngineKind, JsEngine, JsValue, NativeFn};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
        /// タイマー評価（`__fandheRunTimer`）で眠る時間（ミリ秒。予算超過の再現用）。
        timer_sleep_ms: Arc<AtomicU64>,
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
            // microtask checkpoint とタイマー発火の固定ディスパッチャー評価は、スクリプトの
            // 観測（`calls`）に混ぜない。タイマー発火は `timer:<id>` として `log` に残す。
            if script == "__fandheRunMicrotasks()" {
                return Ok(JsValue::Undefined);
            }
            if let Some(id) = script
                .strip_prefix("__fandheRunTimer(")
                .and_then(|s| s.strip_suffix(')'))
            {
                if let Ok(mut log) = self.log.lock() {
                    log.push(format!("timer:{id}"));
                }
                let sleep = self.timer_sleep_ms.load(Ordering::SeqCst);
                if sleep > 0 {
                    std::thread::sleep(Duration::from_millis(sleep));
                }
                return Ok(JsValue::Undefined);
            }
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((script.to_string(), options.timeout()));
            }
            if let Some(spec) = script.strip_prefix("settimeout:") {
                // "settimeout:<ms>[:<repeat 0|1>[:<count>]]" で `timerSet` op を count 回呼ぶ。
                let mut it = spec.split(':');
                let ms = it.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                let repeat = it.next() == Some("1");
                let count = it.next().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                for _ in 0..count {
                    if let Some(op) = self.op.as_mut() {
                        let _ = op(&[
                            JsValue::String("timerSet".to_string()),
                            JsValue::Number(ms),
                            JsValue::Bool(repeat),
                        ]);
                    }
                }
                return Ok(JsValue::Undefined);
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
        timer_sleep_ms: Arc<AtomicU64>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                dropped: Arc::new(AtomicBool::new(false)),
                factory_called: Arc::new(AtomicBool::new(false)),
                log: Arc::new(Mutex::new(Vec::new())),
                timer_sleep_ms: Arc::new(AtomicU64::new(0)),
            }
        }

        /// FakeEngine を返す runtime factory（呼ばれたら `factory_called` を立てる）。
        fn factory(&self, lifecycle_timeout: bool) -> impl FnOnce() -> crate::Result<JsRuntime> {
            let (calls, dropped, called, log, timer_sleep_ms) = (
                self.calls.clone(),
                self.dropped.clone(),
                self.factory_called.clone(),
                self.log.clone(),
                self.timer_sleep_ms.clone(),
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
                        timer_sleep_ms,
                    }),
                ))
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().map(|l| l.clone()).unwrap_or_default()
        }
    }

    /// 現スレッドの tokio ランタイムで future を完了させる（同期テストの継ぎ目）。
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(f)
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
        let fetcher = Fetcher::new(FetchOptions::new()).expect("fetcher");
        let out = prepare(&input, options).and_then(|prepared| {
            let loaded = build(&prepared.collected);
            block_on(execute(
                &input,
                options,
                prepared,
                loaded,
                &fetcher,
                h.factory(lifecycle_timeout),
            ))
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
                block_on(execute(&input, &options, p, l, &fetcher_loopback(), || {
                    Ok(JsRuntime::disabled())
                }))
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
        // module の既定（Issue #1116）。
        assert_eq!(o.max_modules(), 64);
        assert_eq!(DEFAULT_MAX_PAGE_MODULES, 64);
        assert_eq!(MAX_MODULE_GRAPH_DEPTH, 32);
        assert_eq!(o.with_max_modules(3).max_modules(), 3);
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
                Error::CrossOriginRedirect,
                f(SrcFetchErrorKind::CrossOrigin),
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

    // 注: 以前はここで公開 API の future が `Send` であることを検査していた（await を runtime /
    // bridge の生成前に済ませる契約）。TASK-111（#783）でページ内 fetch / XHR の取得を
    // runtime が生きている間のイベントループ段で `.await` するため、future は `!Send` になった。
    // `Send + Sync` が必要な呼び出し元（TASK-112 のアクター）は、専用スレッドが runtime と
    // ランナーを所有する形で使う。

    fn timer_log(h: &Harness) -> Vec<String> {
        h.log()
            .into_iter()
            .filter(|l| l.starts_with("timer:"))
            .collect()
    }

    /// JS-7: タイマーは仮想時間の期限順に発火し、仮想時計だけが進む。
    #[test]
    fn js_7_timers_fire_in_deadline_order_on_virtual_clock() {
        let (out, h) = run(
            "<script>settimeout:100</script><script>settimeout:50</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert!(out.abort().is_none());
        assert_eq!(timer_log(&h), vec!["timer:2", "timer:1"]);
        let r = out.event_loop();
        assert_eq!(r.fired(), 2);
        assert_eq!(r.virtual_elapsed(), Duration::from_millis(100));
        assert_eq!(r.pending_at_end(), 0);
        // load の後に回る。
        let log = h.log();
        let load = log
            .iter()
            .position(|l| l.starts_with("load@"))
            .expect("load");
        let first = log.iter().position(|l| l == "timer:2").expect("timer");
        assert!(load < first);
    }

    /// JS-6: 発火回数は 1,000 回目まで発火し、1,001 回目で打ち切る。
    #[test]
    fn js_6_timer_fire_limit_aborts_runaway_interval() {
        let (out, h) = run(
            "<script>settimeout:0:1</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(timer_log(&h).len(), 1000);
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::TimerFires { limit: 1000 })
        );
        assert_eq!(out.event_loop().fired(), 1000);
        assert_eq!(out.event_loop().pending_at_end(), 1);
    }

    /// JS-6: 上限値は設定できる。
    #[test]
    fn js_6_timer_fire_limit_is_configurable() {
        let options = PageRunOptions::default()
            .with_event_loop_limits(EventLoopLimits::default().with_max_timer_fires(3));
        let (out, h) = run("<script>settimeout:5:1</script>", &options);
        let out = out.expect("run");
        assert_eq!(timer_log(&h).len(), 3);
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::TimerFires { limit: 3 })
        );
        assert_eq!(
            out.event_loop().virtual_elapsed(),
            Duration::from_millis(15)
        );
    }

    /// JS-6: 保留中のタイマー数は 1,000 件まで登録でき、1,001 件目でスクリプトを打ち切る。
    #[test]
    fn js_6_registered_timers_boundary() {
        let (out, h) = run(
            "<script>settimeout:0:0:1000</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert!(out.abort().is_none());
        assert_eq!(out.event_loop().fired(), 1000);
        assert_eq!(timer_log(&h).len(), 1000);

        let (out, h) = run(
            "<script>settimeout:0:0:1001</script><script>after</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        let kind = AbortKind::RegisteredTimers { limit: 1000 };
        assert_eq!(out.abort().map(|a| a.kind().clone()), Some(kind.clone()));
        assert_eq!(
            out.scripts()[0].outcome(),
            &ScriptOutcome::Aborted {
                kind: kind.clone(),
                message: None
            }
        );
        assert_eq!(
            out.scripts()[1].outcome(),
            &ScriptOutcome::NotExecuted { kind: kind.clone() }
        );
        assert!(timer_log(&h).is_empty());
        assert_eq!(out.event_loop().pending_at_end(), 1000);
        assert_eq!(out.lifecycle()[0].outcome().as_str(), "not_fired");
    }

    /// JS-6: 仮想時間は 30,000ms の期限までは発火し、30,001ms で打ち切る。
    #[test]
    fn js_6_virtual_time_limit_boundary() {
        let (out, h) = run(
            "<script>settimeout:30000</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert!(out.abort().is_none());
        assert_eq!(timer_log(&h), vec!["timer:1"]);
        assert_eq!(out.event_loop().virtual_elapsed(), Duration::from_secs(30));

        let (out, h) = run(
            "<script>settimeout:30001</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::VirtualTime {
                limit: Duration::from_secs(30)
            })
        );
        assert!(timer_log(&h).is_empty());
        assert_eq!(out.event_loop().pending_at_end(), 1);
        // load は発火済みで、DOM の確定には影響しない。
        assert_eq!(out.lifecycle()[1].outcome().as_str(), "dispatched");
    }

    /// JS-6: 打ち切り済みならイベントループを回さない。
    #[test]
    fn js_6_aborted_page_does_not_run_event_loop() {
        let (out, h) = run(
            "<script>settimeout:10</script><script>timeout</script>",
            &PageRunOptions::default(),
        );
        let out = out.expect("run");
        assert!(out.abort().is_some());
        assert!(timer_log(&h).is_empty());
        assert_eq!(out.event_loop().fired(), 0);
    }

    /// JS-6: タイマーの評価にも実時間の上限を課す。
    #[test]
    fn js_6_timer_evaluation_is_bounded_by_page_wall_time() {
        let options = PageRunOptions::default().with_page_wall_time(Duration::from_millis(100));
        let h = Harness::new();
        h.timer_sleep_ms.store(300, Ordering::SeqCst);
        let html = "<script>settimeout:1:1</script>";
        let input = PageRunInput::new(html, "https://example.test/");
        let out = prepare(&input, &options)
            .and_then(|prepared| {
                let loaded = default_loads(&prepared.collected);
                block_on(execute(
                    &input,
                    &options,
                    prepared,
                    loaded,
                    &fetcher_loopback(),
                    h.factory(false),
                ))
            })
            .expect("run");
        assert_eq!(
            out.abort().map(|a| a.kind().clone()),
            Some(AbortKind::WallTime {
                scope: WallTimeScope::Page,
                limit: Duration::from_millis(100)
            })
        );
        assert_eq!(timer_log(&h).len(), 1);
    }
}

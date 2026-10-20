//! ページ内 JS から core の [`Document`] を操作する DOM ブリッジ（TASK-108・Issue #777・
//! ビヘイビア `JS-5`・`JS-6`。SSOT: `js-engine.md`「ページ内 JS 実行の設計制約」決定 3）。
//!
//! JS 側 shim（`document` / `Node`。#778・#779 の担当）はノード ID（数値）だけを持ち、
//! 少数のネイティブ関数 `__dom.op(opName, ...args)` で本モジュールへ転送する
//! （`ObjectHandle` の往復は使わない）。呼び出し元は JS エンジン（`dyn JsEngine` 経由）と、
//! `Document` を取り付け・取り出すページ実行ランナー（TASK-109・TASK-112）。
//! JS からの引数はすべて untrusted として検証し、不正値で DOM を壊さず panic もしない。
//!
//! # 呼び出し規約
//!
//! - `__dom.op(opName, arg1, arg2, ...)` の可変長スカラー引数。`JsValue` は配列を持てず、
//!   core に JSON 依存も無いため。戻り値も単一スカラー（`querySelectorAll` は ID の
//!   10 進カンマ区切り文字列。空なら `""`）。
//! - 登録は `bind_dom_like_object("__dom", [("op", f)])`（[`DomBridge::register`]）。
//!   `inject_global_function("__dom.op", f)` はドット入りの名前のグローバルを作るだけで
//!   JS の `__dom.op(...)` 構文では呼べないため採用しない。
//! - 許可リスト外の操作名・引数の過不足・型不一致・文字列長超過は `Err`（JS 側では例外）。
//!
//! # ノード ID
//!
//! JS へ渡す数値は `generation * 2^32 + index`（`generation` は 1 以上 2^21 未満、
//! `index` は arena 添字）で、`2^53` 未満のため `f64` に正確に載る。世代は
//! [`DomBridge::attach`] ごとに増え、別ページ・破棄済み文書の ID や、世代 0 の生の添字は
//! 世代不一致で拒否する。世代が枯渇（2^21 回の `attach`）した後の `attach` は `Err`
//! （fail-closed。新しい [`DomBridge`] を作る）。
//!
//! # 上限（`JS-6`）
//!
//! [`DomBridgeLimits`]: 文字列引数のバイト数・1 操作の応答バイト数・ページ単位の操作回数。
//! 操作回数は検証より前に数え、超過後は同じページで拒否し続ける。ノード数・属性値長等は
//! `Document` 側の [`crate::DomLimits`] が効く。
//!
//! # ページの状態（`readyState` / `currentScript`）
//!
//! `document.readyState` と `document.currentScript` は shim（JS 側）ではなく本ブリッジが
//! Rust 側で保持する。ページ JS から書き換えられないようにするためで、ページ実行ランナー
//! （TASK-109）が [`DomBridge::set_ready_state`]・[`DomBridge::set_current_script`] で更新し、
//! shim は引数 0 個の `readyState` / `currentScript` 操作で読む。
//!
//! # ページの状態（`location`・診断。#779）
//!
//! `window.location` の実体も Rust 側が持つ。ランナー（TASK-109・TASK-112）が
//! [`DomBridge::set_location`] へ取得結果の最終 URL を渡し（http / https のみ・userinfo 除去。
//! 未設定は `about:blank`）、shim は `getLocation` を毎回 op で読む。ページ JS が変えられるのは
//! `hash`（`setLocationHash`）と、`history.pushState` / `replaceState` による同一オリジン内の
//! URL 書き換え（`historySetUrl`。同一オリジンの検査は Rust 側。#1171）だけで、それ以外の遷移系の
//! 代入・呼び出し（`history.back` / `forward` / `go` を含む）は実行せず
//! `ignoreLocationChange` として診断に記録する（`JS-5`）。`console` のメッセージも
//! `consoleMessage` で診断に溜める。診断は [`DomBridge::take_diagnostics`] で回収し、
//! `attach` ごとに空へ戻る。診断に URL 本文は保存しない（種別と長さのみ）。
//! `navigatorUserAgent` は [`crate::fetch::USER_AGENT`] を返し、実ブラウザを装わない（`SEC-2`）。
//!
//! `isFocusable`（引数 1 個: ノード ID）は要素が `focus()` でフォーカスできるかを返す読み取り専用 op
//! （`document.activeElement` の追跡用。判定は `focus.rs`。#1185）。
//!
//! # 未実装（REPAIR-3）
//!
//! - `hashchange` イベントの発火と `location` 代入による実際の遷移は未実装（後続 issue）。
//!   `history.back` / `forward` / `go` によるページ内履歴の移動と `popstate` の配送は shim 内で
//!   実装済み（#1177・`JS-5`。取得・遷移はしない）。`go(0)`（再読み込み）と、移動を実行できなかった
//!   場合だけが `ignoreLocationChange` で診断に残る。
//! - #771 のスパイクで判明する追加操作（`parentNode` 等）は後続 issue で足す。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use reqwest::Url;

use fandhe_browser_js::{JsEngine, JsEngineError, JsValue, NativeFn};

use crate::dom::NodeData;
use crate::dom::{Document, NodeId, SerializeScope};
use crate::error::DomError;
use crate::error::Error;
use crate::query::{query_selector_all_str_bounded, query_selector_str};

mod focus;
mod page_env;
mod url_parse;
pub use page_env::{
    BridgeDiagnostics, ConsoleLevel, ConsoleMessage, IgnoredLocationChange, LifecycleEvent,
    ListenerError, LocationChangeKind, LocationMember, MAX_BRIDGE_DIAGNOSTICS,
    MAX_CONSOLE_MESSAGE_BYTES, MAX_LOCATION_URL_BYTES, NetCallbackError, TimerCallbackSource,
    TimerError,
};
use page_env::{apply_hash, location_member, rewrite_same_origin};

use crate::event_loop::{
    EventLoopLimits, NextStep, PendingEntry, TimerId, TimerQueue, normalize_delay,
};
use crate::page_net::{
    NET_BODY_CHUNK_BYTES, NetOutcome, NetQueue, NetReport, NetRequestKind, PageNetLimits,
    PendingRequest,
};
// ページランナーが例外メッセージの切り詰めに再利用する（#780）。
pub(crate) use page_env::{parse_location, truncate_utf8};

/// JS に公開するグローバルオブジェクト名（`__dom.op(...)` の `__dom`）。
pub const DOM_BRIDGE_OBJECT_NAME: &str = "__dom";
/// 上記オブジェクトのメソッド名（`__dom.op` の `op`）。
pub const DOM_BRIDGE_METHOD_NAME: &str = "op";

/// 文字列引数（操作名を含む）の既定上限（UTF-8 バイト）。
pub const DEFAULT_MAX_STRING_ARG_BYTES: usize = 64 * 1024;
/// 1 操作の文字列応答の既定上限（UTF-8 バイト）。
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// 1 ページあたりの操作回数の既定上限。
pub const DEFAULT_MAX_OPS_PER_PAGE: u64 = 100_000;

/// ランナーが打ち切りに使うリソース上限の種別（`JS-6`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceLimitKind {
    /// ライフサイクルのリスナー保持件数。
    Listeners,
    /// 保留中のタイマー数。
    RegisteredTimers,
    /// 保留中の microtask 数（`queueMicrotask`。shim が上限超過を通知する。`JS-6`）。
    Microtasks,
    /// 保留中の MutationObserver のレコード数・登録数（shim が上限超過を通知する。`JS-6`）。
    MutationObservers,
}

/// 世代の上限（排他）。`generation << 32` が `2^53` 未満に収まる値。
const GENERATION_LIMIT: u32 = 1 << 21;
/// `f64` が整数を正確に表せる上限（排他）。
const MAX_SAFE_INTEGER_EXCLUSIVE: f64 = 9_007_199_254_740_992.0;

/// ブリッジの上限設定（`JS-6`）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DomBridgeLimits {
    max_string_arg_bytes: usize,
    max_response_bytes: usize,
    max_ops_per_page: u64,
}

impl Default for DomBridgeLimits {
    fn default() -> Self {
        Self {
            max_string_arg_bytes: DEFAULT_MAX_STRING_ARG_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_ops_per_page: DEFAULT_MAX_OPS_PER_PAGE,
        }
    }
}

impl DomBridgeLimits {
    /// 文字列引数の上限（UTF-8 バイト）を設定する。
    pub fn with_max_string_arg_bytes(mut self, bytes: usize) -> Self {
        self.max_string_arg_bytes = bytes;
        self
    }

    /// 1 操作の応答の上限（UTF-8 バイト）を設定する。
    pub fn with_max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    /// 1 ページあたりの操作回数の上限を設定する。
    pub fn with_max_ops_per_page(mut self, ops: u64) -> Self {
        self.max_ops_per_page = ops;
        self
    }

    /// 文字列引数の上限（UTF-8 バイト）。
    pub fn max_string_arg_bytes(&self) -> usize {
        self.max_string_arg_bytes
    }

    /// 1 操作の応答の上限（UTF-8 バイト）。
    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }

    /// 1 ページあたりの操作回数の上限。
    pub fn max_ops_per_page(&self) -> u64 {
        self.max_ops_per_page
    }
}

/// ブリッジが返すエラー（`JS-5`・`JS-6`）。
///
/// `Display` は英語の固定文言と数値のみで、JS 由来の文字列（操作名・セレクタ・属性値）は
/// 反響しない（security.md）。JS 側へは [`JsEngineError::EvaluationFailed`] として渡る。
#[derive(Debug)]
#[non_exhaustive]
pub enum DomBridgeError {
    /// 許可リスト外の操作名。
    UnknownOperation {
        /// 操作名のバイト長。
        len: usize,
    },
    /// 第 1 引数（操作名）が無い。
    MissingOperationName,
    /// 引数の個数が操作の定義と一致しない。
    ArityMismatch {
        /// 操作名。
        operation: &'static str,
        /// 期待した個数（操作名を除く）。
        expected: usize,
        /// 実際の個数（操作名を除く）。
        actual: usize,
    },
    /// 引数の型が合わない。
    TypeMismatch {
        /// 操作名。
        operation: &'static str,
        /// 引数位置（操作名を 0 とする）。
        index: usize,
        /// 期待した型。
        expected: &'static str,
    },
    /// ノード ID が不正（NaN・負・非整数・範囲外・存在しない）。
    InvalidNodeId {
        /// 引数位置（操作名を 0 とする）。
        index: usize,
        /// 静的な理由。
        reason: &'static str,
    },
    /// ノード ID の世代が現在のページと一致しない（別ページ・破棄済み文書）。
    StaleGeneration {
        /// 引数位置（操作名を 0 とする）。
        index: usize,
    },
    /// 文字列引数が上限を超えた。
    StringTooLong {
        /// 引数位置（操作名を 0 とする）。
        index: usize,
        /// 実際のバイト長。
        len: usize,
        /// 上限。
        limit: usize,
    },
    /// 応答が上限を超えた。
    ResponseTooLarge {
        /// 実際のバイト長。
        len: usize,
        /// 上限。
        limit: usize,
    },
    /// ページ単位の操作回数上限を超えた（以後も拒否し続ける）。
    OpLimitExceeded {
        /// 上限。
        limit: u64,
    },
    /// `Document` が取り付けられていない（未 attach または detach 済み）。
    Detached,
    /// 世代が枯渇して新しい `Document` を取り付けられない。
    GenerationExhausted,
    /// `set_location` に渡された URL が不正（http / https 以外・解析不能）。URL は反響しない。
    InvalidLocation {
        /// 静的な理由。
        reason: &'static str,
    },
    /// ノードの arena 添字が ID に載る範囲（`u32`）を超えた。
    NodeIndexOutOfRange,
    /// 未実装の操作（REPAIR-3）。
    Unsupported {
        /// 操作名。
        operation: &'static str,
    },
    /// 内部状態の Mutex が汚染された。
    StatePoisoned,
    /// core の DOM / query / selector 層のエラー。
    Core(Error),
}

impl std::fmt::Display for DomBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOperation { len } => write!(f, "unknown DOM operation ({len} bytes)"),
            Self::MissingOperationName => write!(f, "DOM operation name is missing"),
            Self::ArityMismatch {
                operation,
                expected,
                actual,
            } => write!(
                f,
                "{operation} expects {expected} argument(s) but got {actual}"
            ),
            Self::TypeMismatch {
                operation,
                index,
                expected,
            } => write!(f, "{operation}: argument {index} must be {expected}"),
            Self::InvalidNodeId { index, reason } => {
                write!(f, "invalid node id at argument {index}: {reason}")
            }
            Self::StaleGeneration { index } => {
                write!(f, "node id at argument {index} belongs to another page")
            }
            Self::StringTooLong { index, len, limit } => write!(
                f,
                "string argument {index} of {len} bytes exceeds limit of {limit} bytes"
            ),
            Self::ResponseTooLarge { len, limit } => {
                write!(f, "response of {len} bytes exceeds limit of {limit} bytes")
            }
            Self::OpLimitExceeded { limit } => {
                write!(f, "DOM operation limit of {limit} per page exceeded")
            }
            Self::Detached => write!(f, "no document is attached to the DOM bridge"),
            Self::GenerationExhausted => write!(f, "DOM bridge generation counter is exhausted"),
            Self::InvalidLocation { reason } => write!(f, "invalid location: {reason}"),
            Self::NodeIndexOutOfRange => write!(f, "node index does not fit in a node id"),
            Self::Unsupported { operation } => write!(f, "{operation} is not supported yet"),
            Self::StatePoisoned => write!(f, "DOM bridge state is poisoned"),
            Self::Core(source) => write!(f, "DOM operation failed: {source}"),
        }
    }
}

impl std::error::Error for DomBridgeError {}

impl From<Error> for DomBridgeError {
    fn from(source: Error) -> Self {
        Self::Core(source)
    }
}

impl From<DomBridgeError> for JsEngineError {
    fn from(source: DomBridgeError) -> Self {
        JsEngineError::EvaluationFailed(source.to_string())
    }
}

/// 許可リストの操作（`JS-5`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomOp {
    DocumentRoot,
    Body,
    Head,
    CreateElement,
    CreateTextNode,
    GetElementById,
    QuerySelector,
    QuerySelectorAll,
    AppendChild,
    InsertBefore,
    RemoveChild,
    SetAttribute,
    RemoveAttribute,
    GetAttribute,
    GetTextContent,
    SetTextContent,
    GetInnerHtml,
    SetInnerHtml,
    ReadyState,
    CurrentScript,
    GetLocation,
    SetLocationHash,
    HistorySetUrl,
    IgnoreLocationChange,
    NavigatorUserAgent,
    ConsoleMessage,
    LifecycleListenerError,
    LifecycleInstallOpen,
    LifecycleListenerLimit,
    TimerSet,
    TimerClear,
    TimerConsume,
    TimerPending,
    TimerOriginCapture,
    TimerOrigin,
    MicrotaskLimit,
    TimerCallbackError,
    TimerStringHandlerIgnored,
    NetRequest,
    NetConsume,
    NetReadBody,
    NetFinalUrl,
    NetCallbackError,
    NetOptionsIgnored,
    NetCancel,
    IsInclusiveAncestor,
    IsFocusable,
    MutationObserverLimit,
    MutationOptionsIgnored,
    UrlParse,
    /// HTML 名前空間の要素の local name を返す（#1186。dom.js の IDL 属性の種別判定用）。
    HtmlLocalName,
}

impl DomOp {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "documentRoot" => Self::DocumentRoot,
            "body" => Self::Body,
            "head" => Self::Head,
            "createElement" => Self::CreateElement,
            "createTextNode" => Self::CreateTextNode,
            "getElementById" => Self::GetElementById,
            "querySelector" => Self::QuerySelector,
            "querySelectorAll" => Self::QuerySelectorAll,
            "appendChild" => Self::AppendChild,
            "insertBefore" => Self::InsertBefore,
            "removeChild" => Self::RemoveChild,
            "setAttribute" => Self::SetAttribute,
            "removeAttribute" => Self::RemoveAttribute,
            "getAttribute" => Self::GetAttribute,
            "getTextContent" => Self::GetTextContent,
            "setTextContent" => Self::SetTextContent,
            "getInnerHTML" => Self::GetInnerHtml,
            "setInnerHTML" => Self::SetInnerHtml,
            "readyState" => Self::ReadyState,
            "currentScript" => Self::CurrentScript,
            "getLocation" => Self::GetLocation,
            "setLocationHash" => Self::SetLocationHash,
            "historySetUrl" => Self::HistorySetUrl,
            "ignoreLocationChange" => Self::IgnoreLocationChange,
            "navigatorUserAgent" => Self::NavigatorUserAgent,
            "consoleMessage" => Self::ConsoleMessage,
            "lifecycleListenerError" => Self::LifecycleListenerError,
            "lifecycleInstallOpen" => Self::LifecycleInstallOpen,
            "lifecycleListenerLimit" => Self::LifecycleListenerLimit,
            "timerSet" => Self::TimerSet,
            "timerClear" => Self::TimerClear,
            "timerConsume" => Self::TimerConsume,
            "timerPending" => Self::TimerPending,
            "timerOriginCapture" => Self::TimerOriginCapture,
            "timerOrigin" => Self::TimerOrigin,
            "microtaskLimit" => Self::MicrotaskLimit,
            "timerCallbackError" => Self::TimerCallbackError,
            "timerStringHandlerIgnored" => Self::TimerStringHandlerIgnored,
            "netRequest" => Self::NetRequest,
            "netConsume" => Self::NetConsume,
            "netReadBody" => Self::NetReadBody,
            "netFinalUrl" => Self::NetFinalUrl,
            "netCallbackError" => Self::NetCallbackError,
            "netOptionsIgnored" => Self::NetOptionsIgnored,
            "netCancel" => Self::NetCancel,
            "isInclusiveAncestor" => Self::IsInclusiveAncestor,
            "isFocusable" => Self::IsFocusable,
            "htmlLocalName" => Self::HtmlLocalName,
            "mutationObserverLimit" => Self::MutationObserverLimit,
            "mutationOptionsIgnored" => Self::MutationOptionsIgnored,
            "urlParse" => Self::UrlParse,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::DocumentRoot => "documentRoot",
            Self::Body => "body",
            Self::Head => "head",
            Self::CreateElement => "createElement",
            Self::CreateTextNode => "createTextNode",
            Self::GetElementById => "getElementById",
            Self::QuerySelector => "querySelector",
            Self::QuerySelectorAll => "querySelectorAll",
            Self::AppendChild => "appendChild",
            Self::InsertBefore => "insertBefore",
            Self::RemoveChild => "removeChild",
            Self::SetAttribute => "setAttribute",
            Self::RemoveAttribute => "removeAttribute",
            Self::GetAttribute => "getAttribute",
            Self::GetTextContent => "getTextContent",
            Self::SetTextContent => "setTextContent",
            Self::GetInnerHtml => "getInnerHTML",
            Self::SetInnerHtml => "setInnerHTML",
            Self::ReadyState => "readyState",
            Self::CurrentScript => "currentScript",
            Self::GetLocation => "getLocation",
            Self::SetLocationHash => "setLocationHash",
            Self::HistorySetUrl => "historySetUrl",
            Self::IgnoreLocationChange => "ignoreLocationChange",
            Self::NavigatorUserAgent => "navigatorUserAgent",
            Self::ConsoleMessage => "consoleMessage",
            Self::LifecycleListenerError => "lifecycleListenerError",
            Self::LifecycleInstallOpen => "lifecycleInstallOpen",
            Self::LifecycleListenerLimit => "lifecycleListenerLimit",
            Self::TimerSet => "timerSet",
            Self::TimerClear => "timerClear",
            Self::TimerConsume => "timerConsume",
            Self::TimerPending => "timerPending",
            Self::TimerOriginCapture => "timerOriginCapture",
            Self::TimerOrigin => "timerOrigin",
            Self::MicrotaskLimit => "microtaskLimit",
            Self::TimerCallbackError => "timerCallbackError",
            Self::TimerStringHandlerIgnored => "timerStringHandlerIgnored",
            Self::NetRequest => "netRequest",
            Self::NetConsume => "netConsume",
            Self::NetReadBody => "netReadBody",
            Self::NetFinalUrl => "netFinalUrl",
            Self::NetCallbackError => "netCallbackError",
            Self::NetOptionsIgnored => "netOptionsIgnored",
            Self::NetCancel => "netCancel",
            Self::IsInclusiveAncestor => "isInclusiveAncestor",
            Self::IsFocusable => "isFocusable",
            Self::HtmlLocalName => "htmlLocalName",
            Self::MutationObserverLimit => "mutationObserverLimit",
            Self::MutationOptionsIgnored => "mutationOptionsIgnored",
            Self::UrlParse => "urlParse",
        }
    }

    /// 操作名を除いた引数の個数。
    fn arity(self) -> usize {
        match self {
            Self::DocumentRoot
            | Self::Body
            | Self::Head
            | Self::ReadyState
            | Self::CurrentScript
            | Self::NavigatorUserAgent
            | Self::LifecycleInstallOpen
            | Self::LifecycleListenerLimit
            | Self::MicrotaskLimit
            | Self::TimerOriginCapture
            | Self::MutationObserverLimit
            | Self::TimerStringHandlerIgnored => 0,
            Self::CreateElement
            | Self::CreateTextNode
            | Self::GetElementById
            | Self::GetTextContent
            | Self::GetInnerHtml
            | Self::GetLocation
            | Self::SetLocationHash
            | Self::HistorySetUrl
            | Self::TimerClear
            | Self::TimerConsume
            | Self::TimerPending
            | Self::NetConsume
            | Self::NetReadBody
            | Self::NetFinalUrl
            | Self::NetOptionsIgnored
            | Self::MutationOptionsIgnored
            | Self::IsFocusable
            | Self::HtmlLocalName
            | Self::NetCancel => 1,
            Self::QuerySelector
            | Self::QuerySelectorAll
            | Self::AppendChild
            | Self::RemoveChild
            | Self::RemoveAttribute
            | Self::GetAttribute
            | Self::SetTextContent
            | Self::SetInnerHtml
            | Self::IgnoreLocationChange
            | Self::TimerSet
            | Self::TimerOrigin
            | Self::IsInclusiveAncestor
            | Self::UrlParse => 2,
            Self::InsertBefore | Self::SetAttribute | Self::ConsoleMessage => 3,
            Self::TimerCallbackError
            | Self::LifecycleListenerError
            | Self::NetCallbackError
            | Self::NetRequest => 4,
        }
    }
}

/// `document.readyState` の値（`JS-5`。ページ実行ランナーが [`DomBridge::set_ready_state`] で進める）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DocumentReadyState {
    /// `"loading"`（`attach` 直後の既定）。
    Loading,
    /// `"interactive"`。
    Interactive,
    /// `"complete"`。
    Complete,
}

impl DocumentReadyState {
    /// JS に見せる文字列。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Interactive => "interactive",
            Self::Complete => "complete",
        }
    }
}

/// 取り付け中のページの状態。
struct PageState {
    document: Document,
    ready_state: DocumentReadyState,
    current_script: Option<NodeId>,
    /// `None` は `about:blank`。
    location: Option<Url>,
    diagnostics: BridgeDiagnostics,
    generation: u32,
    op_count: u64,
    /// shim 注入中か（[`DomBridge::set_shim_installing`] だけが変更する）。
    shim_installing: bool,
    op_limit_tripped: bool,
    /// 仮想時間のタイマーキュー（`JS-7`）。
    timers: TimerQueue,
    /// タイマーの登録元 stack を shim に捕捉させるか（opt-in。既定 false。Issue #1176）。
    capture_timer_origin: bool,
    /// タイマーの上限（ランナーが [`DomBridge::set_event_loop_limits`] で設定する）。
    event_loop_limits: EventLoopLimits,
    /// ページ内 fetch / XHR の要求キューと上限（`JS-8`・`JS-6`）。
    net: NetQueue,
}

struct BridgeState {
    page: Option<PageState>,
    /// 次に払い出す世代（1 始まり。0 は生の添字を無効にするため使わない）。
    next_generation: u32,
    limits: DomBridgeLimits,
}

/// `__dom.op` のディスパッチと ID 検証を担うブリッジ（`JS-5`・`JS-6`）。
///
/// `Clone` は同じ内部状態を共有する（エンジンへ登録した [`NativeFn`] と、ランナー側の
/// `attach` / `detach` が同じページを見るため）。
#[derive(Clone)]
pub struct DomBridge {
    state: Arc<Mutex<BridgeState>>,
}

impl std::fmt::Debug for DomBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomBridge").finish_non_exhaustive()
    }
}

/// `index` 番目の引数（操作名を 0 とする）から数値 ID を検証して取り出す。
fn decode_node_id(
    value: Option<&JsValue>,
    index: usize,
    operation: &'static str,
    generation: u32,
    node_count: usize,
) -> Result<NodeId, DomBridgeError> {
    let Some(JsValue::Number(n)) = value else {
        return Err(DomBridgeError::TypeMismatch {
            operation,
            index,
            expected: "a node id number",
        });
    };
    let n = *n;
    let invalid = |reason| DomBridgeError::InvalidNodeId { index, reason };
    if !n.is_finite() {
        return Err(invalid("not finite"));
    }
    if n.fract() != 0.0 {
        return Err(invalid("not an integer"));
    }
    if n < 0.0 {
        return Err(invalid("negative"));
    }
    if n >= MAX_SAFE_INTEGER_EXCLUSIVE {
        return Err(invalid("out of range"));
    }
    // 上の検証で 0 以上 2^53 未満の整数と分かっているため、切り詰めなく変換できる。
    let raw = n as u64;
    if raw >> 32 != u64::from(generation) {
        return Err(DomBridgeError::StaleGeneration { index });
    }
    let idx = usize::try_from(raw & 0xFFFF_FFFF).map_err(|_| invalid("out of range"))?;
    if idx >= node_count {
        return Err(invalid("no such node"));
    }
    Ok(NodeId::new(idx))
}

/// 世代と arena 添字から JS へ渡す数値 ID を作る。
fn encode_node_id(generation: u32, id: NodeId) -> Result<f64, DomBridgeError> {
    let index = u32::try_from(id.index()).map_err(|_| DomBridgeError::NodeIndexOutOfRange)?;
    if generation == 0 || generation >= GENERATION_LIMIT {
        return Err(DomBridgeError::GenerationExhausted);
    }
    let raw = (u64::from(generation) << 32) | u64::from(index);
    // generation < 2^21 のため raw < 2^53 で f64 に正確に載る。
    Ok(raw as f64)
}

impl PageState {
    fn node_arg(
        &self,
        op: DomOp,
        args: &[JsValue],
        index: usize,
    ) -> Result<NodeId, DomBridgeError> {
        decode_node_id(
            args.get(index.saturating_sub(1)),
            index,
            op.name(),
            self.generation,
            self.document.node_count(),
        )
    }

    /// `Null` を許す ID 引数（`insertBefore` の基準ノード）。
    fn optional_node_arg(
        &self,
        op: DomOp,
        args: &[JsValue],
        index: usize,
    ) -> Result<Option<NodeId>, DomBridgeError> {
        match args.get(index.saturating_sub(1)) {
            Some(JsValue::Null) => Ok(None),
            other => decode_node_id(
                other,
                index,
                op.name(),
                self.generation,
                self.document.node_count(),
            )
            .map(Some),
        }
    }

    fn encode(&self, id: NodeId) -> Result<JsValue, DomBridgeError> {
        encode_node_id(self.generation, id).map(JsValue::Number)
    }

    fn encode_optional(&self, id: Option<NodeId>) -> Result<JsValue, DomBridgeError> {
        match id {
            Some(id) => self.encode(id),
            None => Ok(JsValue::Null),
        }
    }

    /// 新ノードを払い出す前に、ID に載る添字の余地があることを確認する。
    fn ensure_index_space(&self) -> Result<(), DomBridgeError> {
        if u32::try_from(self.document.node_count()).is_err() {
            return Err(DomBridgeError::NodeIndexOutOfRange);
        }
        Ok(())
    }

    /// `html` 要素の子から `name` の要素を探す（`body` / `head`）。
    fn html_child(&self, name: &str) -> Option<NodeId> {
        let root = self.document.root();
        let html = self
            .document
            .children(root)
            .find(|&c| self.document.local_name(c) == Some("html"))?;
        self.document
            .children(html)
            .find(|&c| self.document.local_name(c) == Some(name))
    }
}

/// 数値引数を要求 ID（1 以上の u32 に収まる整数）として検証する。範囲外は `None`。
fn decode_net_id(value: Option<&JsValue>) -> Option<u32> {
    match value {
        Some(JsValue::Number(raw))
            if raw.is_finite()
                && raw.fract() == 0.0
                && *raw >= 1.0
                && *raw <= f64::from(u32::MAX) =>
        {
            // 範囲を検証済みのため切り詰めなく変換できる。
            Some(*raw as u32)
        }
        _ => None,
    }
}

fn string_arg<'a>(
    op: DomOp,
    args: &'a [JsValue],
    index: usize,
    limits: &DomBridgeLimits,
) -> Result<&'a str, DomBridgeError> {
    let Some(JsValue::String(s)) = args.get(index.saturating_sub(1)) else {
        return Err(DomBridgeError::TypeMismatch {
            operation: op.name(),
            index,
            expected: "a string",
        });
    };
    check_string_len(s, index, limits)?;
    Ok(s)
}

fn check_string_len(s: &str, index: usize, limits: &DomBridgeLimits) -> Result<(), DomBridgeError> {
    if s.len() > limits.max_string_arg_bytes {
        return Err(DomBridgeError::StringTooLong {
            index,
            len: s.len(),
            limit: limits.max_string_arg_bytes,
        });
    }
    Ok(())
}

fn check_response_len(len: usize, limits: &DomBridgeLimits) -> Result<(), DomBridgeError> {
    if len > limits.max_response_bytes {
        return Err(DomBridgeError::ResponseTooLarge {
            len,
            limit: limits.max_response_bytes,
        });
    }
    Ok(())
}

/// `getTextContent` の結果バイト数を、文字列を連結せずに数えて応答上限と比較する。
/// 上限を超えた時点で走査を打ち切る。対象外のノード種別（`text_content` が `None`）は何もしない。
fn check_text_content_len(
    document: &Document,
    node: NodeId,
    limits: &DomBridgeLimits,
) -> Result<(), DomBridgeError> {
    let mut total: usize = 0;
    let mut add = |n: usize| -> Result<(), DomBridgeError> {
        total = total.saturating_add(n);
        check_response_len(total, limits)
    };
    match document.node_data(node) {
        Some(NodeData::Text { contents } | NodeData::Comment { contents }) => add(contents.len()),
        Some(NodeData::ProcessingInstruction { data, .. }) => add(data.len()),
        Some(NodeData::Element { .. } | NodeData::DocumentFragment) => {
            for d in document.descendants(node) {
                if let Some(NodeData::Text { contents }) = document.node_data(d) {
                    add(contents.len())?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn string_response(s: String, limits: &DomBridgeLimits) -> Result<JsValue, DomBridgeError> {
    check_response_len(s.len(), limits)?;
    Ok(JsValue::String(s))
}

/// `Document::set_attribute` が保存する属性名（HTML 要素は ASCII 小文字、それ以外は原文）。
fn normalized_attribute_name(doc: &Document, element: NodeId, name: &str) -> String {
    if doc.namespace_url(element) == Some(crate::dom::HTML_NAMESPACE_URI) {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

/// 検証済みの `op` を実行する。各操作は引数の検証を終えてから `Document` を変更する。
fn execute(
    op: DomOp,
    args: &[JsValue],
    page: &mut PageState,
    limits: &DomBridgeLimits,
) -> Result<JsValue, DomBridgeError> {
    match op {
        DomOp::DocumentRoot => page.encode(page.document.root()),
        DomOp::ReadyState => Ok(JsValue::String(page.ready_state.as_str().to_owned())),
        DomOp::CurrentScript => page.encode_optional(page.current_script),
        DomOp::NavigatorUserAgent => Ok(JsValue::String(crate::fetch::USER_AGENT.to_owned())),
        DomOp::GetLocation => {
            let name = string_arg(op, args, 1, limits)?;
            let member = LocationMember::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known location member",
            })?;
            string_response(location_member(page.location.as_ref(), member), limits)
        }
        DomOp::SetLocationHash => {
            let value = string_arg(op, args, 1, limits)?;
            match page.location.as_mut() {
                Some(url) => {
                    if !apply_hash(url, value) {
                        // 結果が URL 上限を超える代入は適用せず、無視した事実だけ残す。
                        page.diagnostics
                            .record_location_change(LocationChangeKind::Hash, value.len());
                    }
                }
                // about:blank には書き換え先が無い。無視した事実だけ残す。
                None => page
                    .diagnostics
                    .record_location_change(LocationChangeKind::Hash, value.len()),
            }
            Ok(JsValue::Undefined)
        }
        // history.pushState / replaceState の URL 書き換え（#1171）。同一オリジンの検査は
        // ここ（Rust 側）で行い、理由と URL は返さない。遷移も取得もしない。
        DomOp::HistorySetUrl => {
            let value = string_arg(op, args, 1, limits)?;
            match rewrite_same_origin(page.location.as_ref(), value) {
                Some(url) => {
                    page.location = Some(url);
                    Ok(JsValue::Bool(true))
                }
                None => Ok(JsValue::Bool(false)),
            }
        }
        DomOp::IgnoreLocationChange => {
            let name = string_arg(op, args, 1, limits)?;
            let value = string_arg(op, args, 2, limits)?;
            let kind = LocationChangeKind::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known location change kind",
            })?;
            page.diagnostics.record_location_change(kind, value.len());
            Ok(JsValue::Undefined)
        }
        DomOp::ConsoleMessage => {
            let name = string_arg(op, args, 1, limits)?;
            let text = string_arg(op, args, 2, limits)?;
            let stack = string_arg(op, args, 3, limits)?;
            let level = ConsoleLevel::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known console level",
            })?;
            // stack は opt-in（`capture_timer_origin`）のときだけ保持する。ページが直接 op を
            // 呼んでも既定では残らない（Issue #1183・`SEC-2`）。
            let stack = if page.capture_timer_origin { stack } else { "" };
            page.diagnostics.record_console(level, text, stack);
            Ok(JsValue::Undefined)
        }
        // shim 注入中（Rust 側が立てたフラグ）だけ true。events.js が再 install 時の
        // ディスパッチャー差し替えを許すかの判定に使う（ページ JS はフラグを変えられない）。
        DomOp::LifecycleInstallOpen => Ok(JsValue::Bool(page.shim_installing)),
        // shim がリスナー保持件数の上限に達したことを通知する（`JS-6`）。ランナーが
        // 全スクリプトの実行後に確認し、リソース上限による打ち切りとして扱う。
        DomOp::LifecycleListenerLimit => {
            page.diagnostics.listener_limit_exceeded = true;
            Ok(JsValue::Undefined)
        }
        // タイマーの登録（`JS-7`）。遅延はページ由来の untrusted 入力で、ここで丸める。
        // 保留中の件数上限で拒否したときは 0 を返し、ランナーが確認できるフラグを立てる（`JS-6`）。
        DomOp::TimerSet => {
            let Some(JsValue::Number(delay)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a delay number",
                });
            };
            let Some(JsValue::Bool(repeat)) = args.get(1) else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 2,
                    expected: "a boolean",
                });
            };
            let delay_ms = normalize_delay(*delay);
            match page
                .timers
                .register(delay_ms, *repeat, &page.event_loop_limits)
            {
                Ok(id) => Ok(JsValue::Number(f64::from(id.get()))),
                Err(_) => Ok(JsValue::Number(0.0)),
            }
        }
        // 非有限・非整数・範囲外・未知の ID は HTML の作法どおり黙って無視する。
        DomOp::TimerClear => {
            let Some(JsValue::Number(raw)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a timer id number",
                });
            };
            if raw.is_finite() && raw.fract() == 0.0 && *raw >= 1.0 && *raw <= f64::from(u32::MAX) {
                // 範囲を検証済みのため切り詰めなく変換できる。
                page.timers.clear(TimerId::from_raw(*raw as u32));
            }
            Ok(JsValue::Undefined)
        }
        // ランナーが `next_timer` で選んだ発火を、shim のディスパッチャーが一度だけ消費する（`JS-6`）。
        // ページが `__fandheRunTimer(id)` を直接呼んでも、許可（armed）が無ければ false になり実行されない。
        DomOp::TimerConsume => {
            let Some(JsValue::Number(raw)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a timer id number",
                });
            };
            let ok = raw.is_finite()
                && raw.fract() == 0.0
                && *raw >= 1.0
                && *raw <= f64::from(u32::MAX)
                // 範囲を検証済みのため切り詰めなく変換できる。
                && page.timers.consume_armed(TimerId::from_raw(*raw as u32));
            Ok(JsValue::Bool(ok))
        }
        // shim が JS 側に保持しているコールバックのうち、Rust 側で既に無効な ID を回収するための照会
        // （`JS-6`）。`__dom.op('timerClear', id)` の直接呼びで Rust 側だけ解放されても、shim が
        // 定期的にこの op で掃除するため、JS 側の保持件数は Rust 側の登録上限の範囲に収まる。
        DomOp::TimerPending => {
            let Some(JsValue::Number(raw)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a timer id number",
                });
            };
            let alive = raw.is_finite()
                && raw.fract() == 0.0
                && *raw >= 1.0
                && *raw <= f64::from(u32::MAX)
                // 範囲を検証済みのため切り詰めなく変換できる。
                && page.timers.is_alive(TimerId::from_raw(*raw as u32));
            Ok(JsValue::Bool(alive))
        }
        // 登録元 stack を捕捉するか（読み取り専用の bool。既定は常に false で、ページが読んでも
        // 識別面にならない。Issue #1176）。
        DomOp::TimerOriginCapture => Ok(JsValue::Bool(page.capture_timer_origin)),
        // 登録直後のタイマーの登録元 stack を保存する（診断用。Issue #1176）。フラグが false なら
        // 何もしない（ページが直接呼んでも保持させられない）。ID の検証は `timerPending` と同じ。
        DomOp::TimerOrigin => {
            let Some(JsValue::Number(raw)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a timer id number",
                });
            };
            let stack = string_arg(op, args, 2, limits)?;
            if page.capture_timer_origin
                && raw.is_finite()
                && raw.fract() == 0.0
                && *raw >= 1.0
                && *raw <= f64::from(u32::MAX)
            {
                // 範囲を検証済みのため切り詰めなく変換できる。
                page.timers
                    .set_origin(TimerId::from_raw(*raw as u32), stack);
            }
            Ok(JsValue::Undefined)
        }
        // microtask の保留数が shim の上限を超えたことの通知。通常は予算外で処理済み（下記）。
        DomOp::MicrotaskLimit => {
            page.diagnostics.microtask_limit_exceeded = true;
            Ok(JsValue::Undefined)
        }
        // MutationObserver の保留レコード数・登録数が shim の上限を超えたことの通知。
        // 通常は予算外で処理済み（下記）。
        DomOp::MutationObserverLimit => {
            page.diagnostics.mutation_observer_limit_exceeded = true;
            Ok(JsValue::Undefined)
        }
        // `observe` の options のうち実装せず無視したもの（`attributeOldValue` 等）の件数（`REPAIR-3`）。
        DomOp::MutationOptionsIgnored => {
            let Some(JsValue::Number(count)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a count number",
                });
            };
            if count.is_finite() && *count >= 1.0 {
                // 1 回の通知で数える上限（巨大な値で飽和させない）。
                let add = count.min(64.0) as u64;
                page.diagnostics.mutation_options_ignored = page
                    .diagnostics
                    .mutation_options_ignored
                    .saturating_add(add);
            }
            Ok(JsValue::Undefined)
        }
        // `new URL(input, base)` 等の解析（#1144）。第 2 引数は `Null`（base なし）か文字列。
        // 入力長は既存の文字列引数の上限で解析より前に検証する（`MAX_LOCATION_URL_BYTES` は
        // URL をページ状態に保持して複製する `set_location` 用で、何も保持しないこの op には使わない）。
        // 解析失敗は `Null`（shim が `TypeError` にする）。診断は記録せず、ネットワークにも出ない。
        DomOp::UrlParse => {
            let input = string_arg(op, args, 1, limits)?;
            let base = match args.get(1) {
                Some(JsValue::Null) => None,
                _ => Some(string_arg(op, args, 2, limits).map_err(|e| match e {
                    DomBridgeError::TypeMismatch {
                        operation, index, ..
                    } => DomBridgeError::TypeMismatch {
                        operation,
                        index,
                        expected: "a string or null",
                    },
                    other => other,
                })?),
            };
            match url_parse::parse_url_components(input, base) {
                Some(joined) => string_response(joined, limits),
                None => Ok(JsValue::Null),
            }
        }
        // `ancestor` が `node` 自身またはその祖先か（MutationObserver の `subtree` 判定用。
        // 祖先の探索量は `Document::ancestors` が `node_count` 歩で打ち切る）。
        DomOp::IsInclusiveAncestor => {
            let ancestor = page.node_arg(op, args, 1)?;
            let node = page.node_arg(op, args, 2)?;
            let found = ancestor == node || page.document.ancestors(node).any(|a| a == ancestor);
            Ok(JsValue::Bool(found))
        }
        // 要素が `focus()` でフォーカスできるか（`document.activeElement` の追跡用。#1185・簡略判定は
        // `focus.rs`、REPAIR-3）。読み取り専用。
        DomOp::IsFocusable => {
            let node = page.node_arg(op, args, 1)?;
            Ok(JsValue::Bool(focus::is_focusable(&page.document, node)))
        }
        // HTML 名前空間の要素の local name（ASCII 小文字）。SVG・MathML 要素・テキスト・文書は null
        // （dom.js が input / button 等の HTML の規則を SVG の同名要素へ当てないため。#1186・JS-5）。
        // 読み取り専用で、応答は上限（JS-6）に従う。
        DomOp::HtmlLocalName => {
            let node = page.node_arg(op, args, 1)?;
            if page.document.is_element(node)
                && page.document.namespace_url(node) == Some(crate::dom::HTML_NAMESPACE_URI)
                && let Some(name) = page.document.local_name(node)
            {
                return string_response(name.to_string(), limits);
            }
            Ok(JsValue::Null)
        }
        DomOp::TimerCallbackError => {
            let name = string_arg(op, args, 1, limits)?;
            // 例外の種類の名前（`error.name`。空文字可）。語彙外は要約側で `other` に落とす。
            let error_name = string_arg(op, args, 2, limits)?;
            let message = string_arg(op, args, 3, limits)?;
            // `error.stack`（空文字可）。計測 example の stderr 専用で、opt-in が無ければ page_runner が破棄する。
            let stack = string_arg(op, args, 4, limits)?;
            let source = TimerCallbackSource::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known timer callback source",
            })?;
            page.diagnostics
                .record_timer_error(source, error_name, message, stack);
            Ok(JsValue::Undefined)
        }
        DomOp::TimerStringHandlerIgnored => {
            page.diagnostics.timer_string_handlers_ignored = page
                .diagnostics
                .timer_string_handlers_ignored
                .saturating_add(1);
            Ok(JsValue::Undefined)
        }
        // ページ内 fetch / XHR の要求の受理（`JS-8`・`JS-6`）。受理したら要求 ID、reject なら 0。
        // reject の理由は診断に残す。取得そのものはランナーが Fetcher 経由で行う。
        DomOp::NetRequest => {
            let kind = string_arg(op, args, 1, limits)?;
            let method = string_arg(op, args, 2, limits)?;
            let Some(JsValue::Bool(has_body)) = args.get(2) else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 3,
                    expected: "a boolean",
                });
            };
            let url = string_arg(op, args, 4, limits)?;
            let (request, result) = page
                .net
                .request(page.location.as_ref(), kind, method, *has_body, url)
                .ok_or(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a known request kind",
                })?;
            match result {
                Ok(id) => Ok(JsValue::Number(f64::from(id))),
                Err(reason) => {
                    page.diagnostics.record_net_rejection(request, reason);
                    Ok(JsValue::Number(0.0))
                }
            }
        }
        // 配送中の ID に対する一回限りの許可。status（失敗は 0）か、不許可の -1 を返す。
        DomOp::NetConsume => {
            let Some(id) = decode_net_id(args.first()) else {
                return Ok(JsValue::Number(-1.0));
            };
            Ok(JsValue::Number(f64::from(page.net.consume(id))))
        }
        DomOp::NetReadBody => {
            // UTF-8 の 1 文字（最大 4 バイト）より小さい上限では、未読本文があっても空文字しか
            // 返せず終端と誤認される。上限を超えて返すこともできないため、明示的なエラーにする。
            if limits.max_response_bytes < 4 {
                return Err(DomBridgeError::ResponseTooLarge {
                    len: 4,
                    limit: limits.max_response_bytes,
                });
            }
            let Some(id) = decode_net_id(args.first()) else {
                return Ok(JsValue::String(String::new()));
            };
            let max = NET_BODY_CHUNK_BYTES.min(limits.max_response_bytes);
            string_response(page.net.read_chunk(id, max), limits)
        }
        DomOp::NetFinalUrl => {
            let Some(id) = decode_net_id(args.first()) else {
                return Ok(JsValue::String(String::new()));
            };
            string_response(page.net.final_url(id), limits)
        }
        DomOp::NetCallbackError => {
            let kind = string_arg(op, args, 1, limits)?;
            // `error.name`（空文字可）・メッセージ・`error.stack`（空文字可。opt-in が無ければ page_runner が破棄）。
            let error_name = string_arg(op, args, 2, limits)?;
            let message = string_arg(op, args, 3, limits)?;
            let stack = string_arg(op, args, 4, limits)?;
            let request = match kind {
                "fetch" => NetRequestKind::Fetch,
                "xhr" => NetRequestKind::Xhr,
                _ => {
                    return Err(DomBridgeError::TypeMismatch {
                        operation: op.name(),
                        index: 1,
                        expected: "a known request kind",
                    });
                }
            };
            page.diagnostics
                .record_net_callback_error(request, error_name, message, stack);
            Ok(JsValue::Undefined)
        }
        // XHR の abort() / 再 open() による未送信要求の取り消し。取得前なら要求キューから除外し、
        // 取得に着手済み・配送済み・未知の ID は何もしない（旧世代の応答は shim 側で破棄する）。
        DomOp::NetCancel => {
            if let Some(id) = decode_net_id(args.first()) {
                page.net.cancel(id);
            }
            Ok(JsValue::Undefined)
        }
        // `fetch` の init で無視したオプション（headers / credentials 等）の件数。
        DomOp::NetOptionsIgnored => {
            let Some(JsValue::Number(count)) = args.first() else {
                return Err(DomBridgeError::TypeMismatch {
                    operation: op.name(),
                    index: 1,
                    expected: "a count number",
                });
            };
            if count.is_finite() && *count >= 1.0 {
                // 1 回の通知で数える上限（巨大な値で飽和させない）。
                let add = count.min(64.0) as u64;
                page.diagnostics.net_options_ignored =
                    page.diagnostics.net_options_ignored.saturating_add(add);
            }
            Ok(JsValue::Undefined)
        }
        DomOp::LifecycleListenerError => {
            let name = string_arg(op, args, 1, limits)?;
            // `error.name`（空文字可）・メッセージ・`error.stack`（空文字可。opt-in が無ければ page_runner が破棄）。
            let error_name = string_arg(op, args, 2, limits)?;
            let message = string_arg(op, args, 3, limits)?;
            let stack = string_arg(op, args, 4, limits)?;
            let event = LifecycleEvent::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known lifecycle event",
            })?;
            page.diagnostics
                .record_listener_error(event, error_name, message, stack);
            Ok(JsValue::Undefined)
        }
        DomOp::Body => page.encode_optional(page.html_child("body")),
        DomOp::Head => page.encode_optional(page.html_child("head")),
        DomOp::CreateElement => {
            let name = string_arg(op, args, 1, limits)?;
            page.ensure_index_space()?;
            let id = page.document.create_element(name)?;
            page.encode(id)
        }
        DomOp::CreateTextNode => {
            let data = string_arg(op, args, 1, limits)?;
            page.ensure_index_space()?;
            let id = page.document.create_text_node(data)?;
            page.encode(id)
        }
        DomOp::GetElementById => {
            let id = string_arg(op, args, 1, limits)?;
            if id.is_empty() {
                return Ok(JsValue::Null);
            }
            // CSS の ID セレクタ照合は Quirks 文書で大文字小文字を無視するため使わず、
            // DOM の `getElementById` どおり id 属性値を完全一致で文書順に探す。
            let doc = &page.document;
            let found = doc
                .descendants(doc.root())
                .find(|&n| doc.attribute(n, "id") == Some(id));
            page.encode_optional(found)
        }
        DomOp::QuerySelector => {
            let scope = page.node_arg(op, args, 1)?;
            let selector = string_arg(op, args, 2, limits)?;
            let found = query_selector_str(&page.document, scope, selector)?;
            page.encode_optional(found)
        }
        DomOp::QuerySelectorAll => {
            let scope = page.node_arg(op, args, 1)?;
            let selector = string_arg(op, args, 2, limits)?;
            // 各 ID は 1 桁以上 + 区切り 1 バイトのため、N 件の応答は 2N-1 バイト以上。
            // 応答上限から導く件数を超えた時点で走査を打ち切り、全件 Vec を確保しない。
            let max_results = limits.max_response_bytes / 2 + 1;
            let Some(found) =
                query_selector_all_str_bounded(&page.document, scope, selector, max_results)?
            else {
                return Err(DomBridgeError::ResponseTooLarge {
                    len: limits.max_response_bytes.saturating_add(1),
                    limit: limits.max_response_bytes,
                });
            };
            let mut out = String::new();
            for (i, id) in found.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let raw = encode_node_id(page.generation, id)?;
                // raw は 0 以上 2^53 未満の整数のため切り詰めは起きない。
                out.push_str(&(raw as u64).to_string());
                // 巨大な応答を作り切る前に打ち切る。
                check_response_len(out.len(), limits)?;
            }
            string_response(out, limits)
        }
        DomOp::AppendChild => {
            let parent = page.node_arg(op, args, 1)?;
            let child = page.node_arg(op, args, 2)?;
            let id = page.document.append_child(parent, child)?;
            page.encode(id)
        }
        DomOp::InsertBefore => {
            let parent = page.node_arg(op, args, 1)?;
            let child = page.node_arg(op, args, 2)?;
            let reference = page.optional_node_arg(op, args, 3)?;
            let id = page.document.insert_before(parent, child, reference)?;
            page.encode(id)
        }
        DomOp::RemoveChild => {
            let parent = page.node_arg(op, args, 1)?;
            let child = page.node_arg(op, args, 2)?;
            let id = page.document.remove_child(parent, child)?;
            page.encode(id)
        }
        DomOp::SetAttribute => {
            let element = page.node_arg(op, args, 1)?;
            let name = string_arg(op, args, 2, limits)?;
            let value = string_arg(op, args, 3, limits)?;
            page.document.set_attribute(element, name, value)?;
            // 実際に保存された（HTML 要素では ASCII 小文字化後の）属性名を返す。
            // shim が MutationObserver の attributeName・attributeFilter 照合に使う（JS-5・JS-7）。
            Ok(JsValue::String(normalized_attribute_name(
                &page.document,
                element,
                name,
            )))
        }
        DomOp::RemoveAttribute => {
            let element = page.node_arg(op, args, 1)?;
            let name = string_arg(op, args, 2, limits)?;
            // 実際に属性を削除した場合だけ、その正規化後の属性名を返し、削除しなかったときは null
            // （shim が MutationObserver の通知要否と属性名に使う。JS-5・JS-7）。
            let removed = page.document.remove_attribute(element, name)?;
            if removed.is_some() {
                Ok(JsValue::String(normalized_attribute_name(
                    &page.document,
                    element,
                    name,
                )))
            } else {
                Ok(JsValue::Null)
            }
        }
        DomOp::GetAttribute => {
            let element = page.node_arg(op, args, 1)?;
            let name = string_arg(op, args, 2, limits)?;
            // 読み取り専用（JS-5）。名前は Document 側で検証し、値は文字列を確保する前に
            // 応答上限（JS-6）を検査する。属性が無ければ null。
            match page.document.attribute_checked(element, name)? {
                Some(v) => {
                    check_response_len(v.len(), limits)?;
                    Ok(JsValue::String(v.to_owned()))
                }
                None => Ok(JsValue::Null),
            }
        }
        DomOp::GetTextContent => {
            let node = page.node_arg(op, args, 1)?;
            // 連結結果を確保する前に、テキスト総バイト数が応答上限内かを非確保で検査する。
            check_text_content_len(&page.document, node, limits)?;
            match page.document.text_content(node) {
                Some(text) => string_response(text, limits),
                None => Ok(JsValue::Null),
            }
        }
        DomOp::SetTextContent => {
            let node = page.node_arg(op, args, 1)?;
            let text = string_arg(op, args, 2, limits)?;
            page.ensure_index_space()?;
            page.document.set_text_content(node, text)?;
            Ok(JsValue::Undefined)
        }
        DomOp::GetInnerHtml => {
            let node = page.node_arg(op, args, 1)?;
            let html = match page.document.serialize_node_limited(
                node,
                SerializeScope::ChildrenOnly,
                limits.max_response_bytes,
            ) {
                Ok(result) => result.into_html(),
                // 出力バッファが応答上限に達した時点で打ち切られている。
                Err(Error::Dom(DomError::SerializedOutputTooLarge { limit }))
                    if limit == limits.max_response_bytes =>
                {
                    return Err(DomBridgeError::ResponseTooLarge {
                        len: limit.saturating_add(1),
                        limit,
                    });
                }
                Err(e) => return Err(e.into()),
            };
            string_response(html, limits)
        }
        DomOp::SetInnerHtml => {
            let node = page.node_arg(op, args, 1)?;
            let html = string_arg(op, args, 2, limits)?;
            page.ensure_index_space()?;
            // 解析は core のフラグメントパーサー（JS 側にパーサーは持たない）。
            // ノード数・保持バイト数の上限は Document 側（DomLimits）が効き、Err では木は不変。
            page.document.set_inner_html(node, html)?;
            Ok(JsValue::Undefined)
        }
    }
}

impl DomBridge {
    /// 上限設定つきでブリッジを作る。`Document` は [`DomBridge::attach`] で取り付ける。
    pub fn new(limits: DomBridgeLimits) -> Self {
        Self {
            state: Arc::new(Mutex::new(BridgeState {
                page: None,
                next_generation: 1,
                limits,
            })),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, BridgeState>, DomBridgeError> {
        self.state.lock().map_err(|_| DomBridgeError::StatePoisoned)
    }

    /// 新しいページの `Document` を取り付ける。新しい世代を割り当て、操作回数を 0 に戻す。
    /// 既存のページは破棄され、その ID はすべて世代不一致になる。
    pub fn attach(&self, document: Document) -> Result<(), DomBridgeError> {
        let mut state = self.lock()?;
        let generation = state.next_generation;
        if generation >= GENERATION_LIMIT {
            return Err(DomBridgeError::GenerationExhausted);
        }
        state.next_generation = generation.saturating_add(1);
        state.page = Some(PageState {
            document,
            ready_state: DocumentReadyState::Loading,
            current_script: None,
            location: None,
            diagnostics: BridgeDiagnostics::default(),
            generation,
            op_count: 0,
            shim_installing: false,
            op_limit_tripped: false,
            timers: TimerQueue::new(),
            capture_timer_origin: false,
            event_loop_limits: EventLoopLimits::default(),
            net: NetQueue::new(PageNetLimits::default()),
        });
        Ok(())
    }

    /// 変更後の `Document` を取り出す。以後の操作は [`DomBridgeError::Detached`]。
    pub fn detach(&self) -> Result<Option<Document>, DomBridgeError> {
        Ok(self.lock()?.page.take().map(|p| p.document))
    }

    /// 取り付け中の `Document` を読み取る（テスト・ランナー用）。未取り付けなら `None`。
    pub fn with_document<R>(
        &self,
        f: impl FnOnce(&Document) -> R,
    ) -> Result<Option<R>, DomBridgeError> {
        Ok(self.lock()?.page.as_ref().map(|p| f(&p.document)))
    }

    /// 取り付け中のページ向けの ID を符号化する（ランナー・shim 初期化用）。
    pub fn encode_node_id(&self, id: NodeId) -> Result<f64, DomBridgeError> {
        let state = self.lock()?;
        let page = state.page.as_ref().ok_or(DomBridgeError::Detached)?;
        encode_node_id(page.generation, id)
    }

    /// `document.readyState` を更新する（ページ実行ランナー用。TASK-109）。
    /// 未取り付けなら [`DomBridgeError::Detached`]。
    pub fn set_ready_state(&self, state: DocumentReadyState) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        page.ready_state = state;
        Ok(())
    }

    /// `document.currentScript` を更新する（実行中の `<script>` 要素。`None` で `null`）。
    /// 未取り付けなら [`DomBridgeError::Detached`]、`script` が現在の文書に無ければ
    /// [`DomBridgeError::InvalidNodeId`]。
    pub fn set_current_script(&self, script: Option<NodeId>) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        if let Some(id) = script
            && id.index() >= page.document.node_count()
        {
            return Err(DomBridgeError::InvalidNodeId {
                index: 0,
                reason: "no such node",
            });
        }
        page.current_script = script;
        Ok(())
    }

    /// `window.location` の URL を設定する（ページ実行ランナー用。TASK-109・TASK-112）。
    /// 取得結果の最終 URL を渡す想定。http / https 以外・解析不能は
    /// [`DomBridgeError::InvalidLocation`]（URL は反響しない）、userinfo は除去する。
    /// [`MAX_LOCATION_URL_BYTES`] を超える URL は解析前に拒否する（`SEC-2`）。
    /// 未取り付けなら [`DomBridgeError::Detached`]。
    pub fn set_location(&self, url: &str) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        let parsed =
            parse_location(url).map_err(|reason| DomBridgeError::InvalidLocation { reason })?;
        page.location = Some(parsed);
        Ok(())
    }

    /// ライフサイクルのリスナー保持件数が上限を超えたか（診断は消費しない。ランナー用）。
    /// 未取り付けなら [`DomBridgeError::Detached`]。
    pub fn listener_limit_exceeded(&self) -> Result<bool, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(page.diagnostics.listener_limit_exceeded)
    }

    /// リソース上限（リスナー保持件数・保留中タイマー数）に達したかを返す（ランナー用。
    /// 診断は消費しない）。未取り付けなら [`DomBridgeError::Detached`]。
    pub fn resource_limit_tripped(&self) -> Result<Option<ResourceLimitKind>, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(if page.diagnostics.listener_limit_exceeded {
            Some(ResourceLimitKind::Listeners)
        } else if page.diagnostics.microtask_limit_exceeded {
            Some(ResourceLimitKind::Microtasks)
        } else if page.diagnostics.mutation_observer_limit_exceeded {
            Some(ResourceLimitKind::MutationObservers)
        } else if page.timers.registration_refused() {
            Some(ResourceLimitKind::RegisteredTimers)
        } else {
            None
        })
    }

    /// 操作回数の予算（`max_ops_per_page`）を使い切ったかを返す（ランナー用。`JS-6`）。
    ///
    /// shim は `timerConsume` 等の op のエラーを握りつぶすため、予算切れはコールバックが
    /// 黙って実行されない形で現れる。ランナーはタイマー評価後にこれを確認して打ち切る。
    /// 未取り付けなら [`DomBridgeError::Detached`]。
    pub fn op_limit_tripped(&self) -> Result<bool, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(page.op_limit_tripped)
    }

    /// イベントループの上限を設定する（ランナー用。`attach` のたびに既定へ戻る）。
    pub fn set_event_loop_limits(&self, limits: EventLoopLimits) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        page.event_loop_limits = limits;
        Ok(())
    }

    /// 次に発火すべきタイマーを選ぶ（ランナー用。仮想時計を進め、発火回数に数える）。
    pub fn next_timer(&self) -> Result<NextStep, DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        Ok(page.timers.next(&page.event_loop_limits))
    }

    /// コールバック評価後に interval を積み直す（ランナー用）。
    pub fn rearm_timer(&self, id: TimerId) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        page.timers.rearm_if_alive(id);
        Ok(())
    }

    /// イベントループの現況 `(仮想経過, 発火回数, 保留中)`（ランナー用）。
    pub fn event_loop_state(&self) -> Result<(Duration, usize, usize), DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok((
            page.timers.virtual_now(),
            page.timers.fired(),
            page.timers.pending(),
        ))
    }

    /// タイマーの登録元 stack の捕捉を設定する（ランナー用。`attach` のたびに false へ戻る。
    /// shim の install より前に呼ぶこと。Issue #1176）。同じフラグが console.error / warn の
    /// Error の stack 保持も有効にする（生の診断情報を取るかの単一フラグ。Issue #1183）。
    pub fn set_capture_timer_origin(&self, capture: bool) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        page.capture_timer_origin = capture;
        Ok(())
    }

    /// 保留中タイマーを期限順に先頭から最大 `limit` 件返す（ランナー用。診断。Issue #1176）。
    pub fn pending_timers(&self, limit: usize) -> Result<Vec<PendingEntry>, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(page.timers.pending_entries(limit))
    }

    /// ページ内 fetch / XHR の上限を設定する（ランナー用。`attach` のたびに既定へ戻る）。
    pub fn set_net_limits(&self, limits: PageNetLimits) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        page.net.set_limits(limits);
        Ok(())
    }

    /// 保留中のネットワーク要求の件数（ランナー用）。
    pub(crate) fn net_pending(&self) -> Result<usize, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(page.net.pending_len())
    }

    /// 次に取得すべき要求を取り出す（ランナー用。受理順）。
    pub(crate) fn take_next_net_request(&self) -> Result<Option<PendingRequest>, DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        Ok(page.net.pop_pending())
    }

    /// 累計バイト上限までの残り（ランナー用。到達済みは 0）。
    pub(crate) fn net_remaining_bytes(&self) -> Result<u64, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(page.net.remaining_bytes())
    }

    /// 取得結果の配送を開始する（ランナー用）。reject は診断に残す（URL は残さない）。
    pub(crate) fn begin_net_delivery(
        &self,
        request: &PendingRequest,
        outcome: NetOutcome,
    ) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        if let NetOutcome::Rejected(kind) = &outcome {
            page.diagnostics.record_net_rejection(request.kind, *kind);
        }
        page.net.begin_delivery(request.id, outcome);
        Ok(())
    }

    /// 配送を終え、保持していた本文を解放する（ランナー用）。
    pub(crate) fn finish_net_delivery(&self) -> Result<(), DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        page.net.finish_delivery();
        Ok(())
    }

    /// ネットワーク要求の実行結果（ランナー用）。
    pub(crate) fn net_report(&self) -> Result<NetReport, DomBridgeError> {
        let guard = self.lock()?;
        let page = guard.page.as_ref().ok_or(DomBridgeError::Detached)?;
        Ok(page.net.report())
    }

    /// 集まった診断（無視した `location` 操作・`console`）を取り出して空にする
    /// （ランナー用）。未取り付けなら [`DomBridgeError::Detached`]。
    pub fn take_diagnostics(&self) -> Result<BridgeDiagnostics, DomBridgeError> {
        let mut guard = self.lock()?;
        let page = guard.page.as_mut().ok_or(DomBridgeError::Detached)?;
        Ok(std::mem::take(&mut page.diagnostics))
    }

    /// shim 注入中フラグを設定する。`JsRuntime::install_dom_shim*` が注入の前後で呼ぶ。
    /// ページが未 attach なら何もしない。
    pub(crate) fn set_shim_installing(&self, installing: bool) {
        if let Ok(mut guard) = self.lock()
            && let Some(page) = guard.page.as_mut()
        {
            page.shim_installing = installing;
        }
    }

    /// 同じ内部状態を共有するブリッジか（`JsRuntime::install_dom_shim` が束縛済みの
    /// ブリッジと一致するかの判定に使う）。
    pub(crate) fn same_instance(&self, other: &DomBridge) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    /// `__dom.op` の本体。`args[0]` が操作名、残りが操作の引数。エンジン無しでも呼べる。
    pub fn dispatch(&self, args: &[JsValue]) -> Result<JsValue, DomBridgeError> {
        let mut guard = self.lock()?;
        let state = &mut *guard;
        let limits = &state.limits;
        let page = state.page.as_mut().ok_or(DomBridgeError::Detached)?;

        // リスナー上限超過の通知は操作予算の対象外にする（`JS-6`）。ページが予算を使い切った後に
        // 上限を超えて登録しても、フラグが立たず打ち切りを逃れることを防ぐ。冪等なフラグ設定のみで
        // 副作用が無く、アロケーションも伴わないため、予算外でも DoS 経路にならない。
        if matches!(args, [JsValue::String(name)] if name == "lifecycleListenerLimit") {
            page.diagnostics.listener_limit_exceeded = true;
            return Ok(JsValue::Undefined);
        }

        // microtask 上限超過の通知も同様に予算外で処理する（`JS-6`。冪等なフラグ設定のみ）。
        if matches!(args, [JsValue::String(name)] if name == "microtaskLimit") {
            page.diagnostics.microtask_limit_exceeded = true;
            return Ok(JsValue::Undefined);
        }

        // MutationObserver 上限超過の通知も同様に予算外で処理する（`JS-6`。冪等なフラグ設定のみ）。
        if matches!(args, [JsValue::String(name)] if name == "mutationObserverLimit") {
            page.diagnostics.mutation_observer_limit_exceeded = true;
            return Ok(JsValue::Undefined);
        }

        // 検証より前に数える（shim を迂回した不正呼び出しの連打も対象）。
        if page.op_limit_tripped || page.op_count >= limits.max_ops_per_page {
            page.op_limit_tripped = true;
            return Err(DomBridgeError::OpLimitExceeded {
                limit: limits.max_ops_per_page,
            });
        }
        page.op_count = page.op_count.saturating_add(1);

        let name = match args.first() {
            None => return Err(DomBridgeError::MissingOperationName),
            Some(JsValue::String(name)) => name,
            Some(_) => {
                return Err(DomBridgeError::TypeMismatch {
                    operation: "dom.op",
                    index: 0,
                    expected: "an operation name string",
                });
            }
        };
        check_string_len(name, 0, limits)?;
        let op = DomOp::parse(name).ok_or(DomBridgeError::UnknownOperation { len: name.len() })?;
        let rest = args.get(1..).unwrap_or(&[]);
        if rest.len() != op.arity() {
            return Err(DomBridgeError::ArityMismatch {
                operation: op.name(),
                expected: op.arity(),
                actual: rest.len(),
            });
        }
        execute(op, rest, page, limits)
    }

    /// `dispatch` を呼ぶ [`NativeFn`] を作る（エラーは [`JsEngineError`] へ変換）。
    pub fn native_fn(&self) -> NativeFn {
        let bridge = self.clone();
        Box::new(move |args| bridge.dispatch(args).map_err(JsEngineError::from))
    }

    /// `__dom.op` をエンジンへ登録する（`bind_dom_like_object`。モジュール doc 参照）。
    pub fn register(&self, engine: &mut dyn JsEngine) -> Result<(), JsEngineError> {
        engine.bind_dom_like_object(
            DOM_BRIDGE_OBJECT_NAME,
            vec![(DOM_BRIDGE_METHOD_NAME.to_owned(), self.native_fn())],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exception_summary::ExceptionKind;
    use crate::page_net::NetRejectKind;
    use crate::parse::{ParseOptions, parse_document};

    const HTML: &str = "<html><head></head><body><div id=\"a\"></div></body></html>";

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn bridge_with(limits: DomBridgeLimits) -> DomBridge {
        let b = DomBridge::new(limits);
        b.attach(parse(HTML)).expect("attach");
        b
    }

    fn bridge() -> DomBridge {
        bridge_with(DomBridgeLimits::default())
    }

    fn s(v: &str) -> JsValue {
        JsValue::String(v.to_owned())
    }

    fn n(v: f64) -> JsValue {
        JsValue::Number(v)
    }

    fn call(b: &DomBridge, args: &[JsValue]) -> Result<JsValue, DomBridgeError> {
        b.dispatch(args)
    }

    fn num(v: Result<JsValue, DomBridgeError>) -> f64 {
        match v.expect("成功するはず") {
            JsValue::Number(x) => x,
            other => panic!("Number を期待: {other:?}"),
        }
    }

    type Snap = Vec<(Option<NodeId>, Vec<NodeId>, Vec<(String, String)>)>;

    fn snapshot(b: &DomBridge) -> Snap {
        b.with_document(|doc| {
            (0..doc.node_count())
                .map(|i| {
                    let id = NodeId::new(i);
                    (
                        doc.parent(id),
                        doc.children(id).collect(),
                        doc.attributes(id)
                            .iter()
                            .map(|a| (a.name.local.to_string(), a.value.clone()))
                            .collect(),
                    )
                })
                .collect()
        })
        .expect("lock")
        .expect("attached")
    }

    #[test]
    fn js_5_defaults_match_spec() {
        let l = DomBridgeLimits::default();
        assert_eq!(l.max_string_arg_bytes(), 64 * 1024);
        assert_eq!(l.max_response_bytes(), 1024 * 1024);
        assert_eq!(l.max_ops_per_page(), 100_000);
    }

    #[test]
    fn js_5_create_append_and_get_element_by_id() {
        let b = bridge();
        let body = num(call(&b, &[s("body")]));
        let div = num(call(&b, &[s("createElement"), s("div")]));
        call(&b, &[s("setAttribute"), n(div), s("id"), s("x")]).expect("set");
        assert_eq!(num(call(&b, &[s("appendChild"), n(body), n(div)])), div);
        assert_eq!(num(call(&b, &[s("getElementById"), s("x")])), div);
        assert_eq!(
            call(&b, &[s("getElementById"), s("missing")]).expect("ok"),
            JsValue::Null
        );
        // セレクタ注入にならず、文字列どおりに照合する。
        assert_eq!(
            call(&b, &[s("getElementById"), s("x, div")]).expect("ok"),
            JsValue::Null
        );
        assert_eq!(
            call(&b, &[s("getElementById"), s("")]).expect("ok"),
            JsValue::Null
        );
    }

    #[test]
    fn js_5_query_selector_all_returns_comma_separated_ids() {
        let b = bridge();
        let root = num(call(&b, &[s("documentRoot")]));
        let body = num(call(&b, &[s("body")]));
        let div = num(call(&b, &[s("createElement"), s("div")]));
        call(&b, &[s("appendChild"), n(body), n(div)]).expect("append");
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let got = call(&b, &[s("querySelectorAll"), n(root), s("div")]).expect("ok");
        assert_eq!(got, s(&format!("{},{}", a as u64, div as u64)));
        let none = call(&b, &[s("querySelectorAll"), n(root), s("span")]).expect("ok");
        assert_eq!(none, s(""));
        assert_eq!(num(call(&b, &[s("querySelector"), n(root), s("#a")])), a);
    }

    #[test]
    fn js_5_text_and_inner_html_roundtrip() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        call(&b, &[s("setTextContent"), n(a), s("hi")]).expect("set");
        assert_eq!(call(&b, &[s("getTextContent"), n(a)]).expect("ok"), s("hi"));
        assert_eq!(call(&b, &[s("getInnerHTML"), n(a)]).expect("ok"), s("hi"));
        let t = num(call(&b, &[s("createTextNode"), s("x")]));
        call(&b, &[s("insertBefore"), n(a), n(t), JsValue::Null]).expect("insert");
        assert_eq!(
            call(&b, &[s("getTextContent"), n(a)]).expect("ok"),
            s("hix")
        );
        assert_eq!(num(call(&b, &[s("removeChild"), n(a), n(t)])), t);
        call(&b, &[s("removeAttribute"), n(a), s("id")]).expect("rm");
        assert_eq!(
            call(&b, &[s("getElementById"), s("a")]).expect("ok"),
            JsValue::Null
        );
    }

    #[test]
    fn js_5_set_inner_html_replaces_children() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let r = call(
            &b,
            &[s("setInnerHTML"), n(a), s("<p id=\"x\">t</p><b>u</b>")],
        );
        assert!(matches!(r, Ok(JsValue::Undefined)));
        let html = b
            .with_document(|d| d.serialize_html().expect("serialize").into_html())
            .expect("lock")
            .expect("attached");
        assert_eq!(
            html,
            "<html><head></head><body><div id=\"a\"><p id=\"x\">t</p><b>u</b></div></body></html>"
        );
    }

    #[test]
    fn js_6_set_inner_html_over_node_limit_keeps_dom_unchanged() {
        let b = bridge();
        b.with_document(|_| ()).expect("lock");
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        // 上限を現在のノード数ちょうどに絞る。
        {
            let mut guard = b.state.lock().expect("lock");
            let page = guard.page.as_mut().expect("attached");
            let count = page.document.node_count();
            page.document
                .set_limits(crate::DomLimits::default().with_max_nodes(count + 1));
        }
        let before = snapshot(&b);
        let err = call(&b, &[s("setInnerHTML"), n(a), s("<i></i><i></i>")]).expect_err("上限超過");
        assert!(matches!(
            err,
            DomBridgeError::Core(Error::Dom(DomError::NodeLimitExceeded { .. }))
        ));
        assert_eq!(snapshot(&b), before);
    }

    #[test]
    fn js_5_set_inner_html_rejects_non_string() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let err = call(&b, &[s("setInnerHTML"), n(a), n(1.0)]).expect_err("型違反");
        assert!(matches!(err, DomBridgeError::TypeMismatch { .. }));
    }

    #[test]
    fn js_5_ready_state_and_current_script_are_rust_side_state() {
        let b = bridge();
        assert_eq!(
            call(&b, &[s("readyState")]).expect("ok"),
            JsValue::String("loading".to_owned())
        );
        b.set_ready_state(DocumentReadyState::Complete)
            .expect("set");
        assert_eq!(
            call(&b, &[s("readyState")]).expect("ok"),
            JsValue::String("complete".to_owned())
        );
        assert_eq!(call(&b, &[s("currentScript")]).expect("ok"), JsValue::Null);
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let a_id = NodeId::new((a as u64 & 0xFFFF_FFFF) as usize);
        b.set_current_script(Some(a_id)).expect("set");
        assert_eq!(num(call(&b, &[s("currentScript")])), a);
        b.set_current_script(None).expect("set");
        assert_eq!(call(&b, &[s("currentScript")]).expect("ok"), JsValue::Null);
        let err = b
            .set_current_script(Some(NodeId::new(1_000_000)))
            .expect_err("範囲外");
        assert!(matches!(err, DomBridgeError::InvalidNodeId { .. }));
        // 引数を渡すと arity 違反。
        let err = call(&b, &[s("readyState"), n(1.0)]).expect_err("arity");
        assert!(matches!(err, DomBridgeError::ArityMismatch { .. }));
    }

    #[test]
    fn js_5_ready_state_resets_on_attach_and_setters_require_page() {
        let b = bridge();
        b.set_ready_state(DocumentReadyState::Interactive)
            .expect("set");
        b.attach(parse(HTML)).expect("attach");
        assert_eq!(
            call(&b, &[s("readyState")]).expect("ok"),
            JsValue::String("loading".to_owned())
        );
        b.detach().expect("detach");
        assert!(matches!(
            b.set_ready_state(DocumentReadyState::Complete),
            Err(DomBridgeError::Detached)
        ));
        assert!(matches!(
            b.set_current_script(None),
            Err(DomBridgeError::Detached)
        ));
    }

    #[test]
    fn js_5_invalid_node_ids_are_rejected_without_mutation() {
        let b = bridge();
        let body = num(call(&b, &[s("body")]));
        let before = snapshot(&b);
        let bad = [
            (-1.0, "negative"),
            (1.5, "not an integer"),
            (f64::NAN, "not finite"),
            (f64::INFINITY, "not finite"),
            (f64::NEG_INFINITY, "not finite"),
            (1e300, "out of range"),
            (9_007_199_254_740_992.0, "out of range"),
        ];
        for (value, reason) in bad {
            let err = call(&b, &[s("appendChild"), n(value), n(body)]).expect_err("拒否");
            assert!(
                matches!(err, DomBridgeError::InvalidNodeId { index: 1, reason: r } if r == reason),
                "{value}: {err:?}"
            );
        }
        // 世代 0 の生の添字は偽造できない。
        let err = call(&b, &[s("appendChild"), n(0.0), n(body)]).expect_err("拒否");
        assert!(matches!(err, DomBridgeError::StaleGeneration { index: 1 }));
        // 世代は合っているが存在しない添字。
        let count = b.with_document(|d| d.node_count()).expect("l").expect("a");
        let gen_base = body - (body % 4_294_967_296.0);
        let err =
            call(&b, &[s("appendChild"), n(gen_base + count as f64), n(body)]).expect_err("拒否");
        assert!(matches!(
            err,
            DomBridgeError::InvalidNodeId {
                reason: "no such node",
                ..
            }
        ));
        // 文字列は ID として使えない。
        let err = call(&b, &[s("appendChild"), s("1"), n(body)]).expect_err("拒否");
        assert!(matches!(err, DomBridgeError::TypeMismatch { index: 1, .. }));
        assert_eq!(snapshot(&b), before);
    }

    #[test]
    fn js_5_generation_isolates_pages() {
        let b = bridge();
        let old_body = num(call(&b, &[s("body")]));
        b.attach(parse(HTML)).expect("attach");
        let err = call(&b, &[s("appendChild"), n(old_body), n(old_body)]).expect_err("旧世代");
        assert!(matches!(err, DomBridgeError::StaleGeneration { index: 1 }));
        let new_body = num(call(&b, &[s("body")]));
        assert_ne!(old_body, new_body);
    }

    #[test]
    fn js_5_detach_returns_mutated_document_and_blocks_ops() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        call(&b, &[s("setAttribute"), n(a), s("class"), s("k")]).expect("set");
        let doc = b.detach().expect("lock").expect("attached");
        let found = query_selector_str(&doc, doc.root(), "#a")
            .expect("q")
            .expect("found");
        assert_eq!(doc.attribute(found, "class"), Some("k"));
        let err = call(&b, &[s("body")]).expect_err("detach 後");
        assert!(matches!(err, DomBridgeError::Detached));
    }

    #[test]
    fn js_5_operation_name_validation() {
        let b = bridge();
        let e = call(&b, &[s("eval"), s("1")]).expect_err("未知");
        assert!(matches!(e, DomBridgeError::UnknownOperation { len: 4 }));
        let e = call(&b, &[s("")]).expect_err("未知");
        assert!(matches!(e, DomBridgeError::UnknownOperation { len: 0 }));
        let e = call(&b, &[]).expect_err("欠落");
        assert!(matches!(e, DomBridgeError::MissingOperationName));
        let e = call(&b, &[n(1.0)]).expect_err("型");
        assert!(matches!(e, DomBridgeError::TypeMismatch { index: 0, .. }));
        let long = "a".repeat(DEFAULT_MAX_STRING_ARG_BYTES + 1);
        let e = call(&b, &[s(&long)]).expect_err("長い");
        assert!(matches!(e, DomBridgeError::StringTooLong { index: 0, .. }));
    }

    #[test]
    fn js_5_arity_is_exact() {
        let b = bridge();
        let e = call(&b, &[s("createElement")]).expect_err("不足");
        assert!(matches!(
            e,
            DomBridgeError::ArityMismatch {
                expected: 1,
                actual: 0,
                ..
            }
        ));
        let e = call(&b, &[s("createElement"), s("a"), s("b")]).expect_err("過剰");
        assert!(matches!(
            e,
            DomBridgeError::ArityMismatch {
                expected: 1,
                actual: 2,
                ..
            }
        ));
        let e = call(&b, &[s("body"), n(1.0)]).expect_err("過剰");
        assert!(matches!(
            e,
            DomBridgeError::ArityMismatch { expected: 0, .. }
        ));
    }

    #[test]
    fn js_6_string_arg_limit_is_utf8_bytes() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let before = snapshot(&b);
        let ok = "x".repeat(DEFAULT_MAX_STRING_ARG_BYTES);
        call(&b, &[s("setAttribute"), n(a), s("title"), s(&ok)]).expect("上限ちょうど");
        let after_ok = snapshot(&b);
        assert_ne!(after_ok, before);
        let over = "x".repeat(DEFAULT_MAX_STRING_ARG_BYTES + 1);
        let e = call(&b, &[s("setAttribute"), n(a), s("title"), s(&over)]).expect_err("超過");
        assert!(matches!(
            e,
            DomBridgeError::StringTooLong { index: 3, len, limit }
                if len == DEFAULT_MAX_STRING_ARG_BYTES + 1 && limit == DEFAULT_MAX_STRING_ARG_BYTES
        ));
        assert_eq!(snapshot(&b), after_ok);
        // マルチバイトは文字数でなくバイト数で数える（"あ" は 3 バイト）。
        let multibyte = "あ".repeat(DEFAULT_MAX_STRING_ARG_BYTES / 3 + 1);
        let e = call(&b, &[s("setTextContent"), n(a), s(&multibyte)]).expect_err("超過");
        assert!(matches!(e, DomBridgeError::StringTooLong { index: 2, .. }));
        assert_eq!(snapshot(&b), after_ok);
    }

    #[test]
    fn js_5_get_attribute_returns_value_or_null() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let before = snapshot(&b);
        assert_eq!(
            call(&b, &[s("getAttribute"), n(a), s("id")]).expect("ok"),
            s("a")
        );
        assert_eq!(
            call(&b, &[s("getAttribute"), n(a), s("ID")]).expect("ok"),
            s("a")
        );
        assert_eq!(
            call(&b, &[s("getAttribute"), n(a), s("nope")]).expect("ok"),
            JsValue::Null
        );
        assert_eq!(snapshot(&b), before);
        call(&b, &[s("setAttribute"), n(a), s("title"), s("t")]).expect("set");
        assert_eq!(
            call(&b, &[s("getAttribute"), n(a), s("title")]).expect("ok"),
            s("t")
        );
    }

    #[test]
    fn js_5_get_attribute_rejects_invalid_args() {
        let b = bridge();
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let before = snapshot(&b);
        // 文字種の不正な名前は例外にせず null（DOM の契約）。
        for name in ["", "1x", "a b"] {
            assert_eq!(
                call(&b, &[s("getAttribute"), n(a), s(name)]).expect("ok"),
                JsValue::Null
            );
        }
        let long = "x".repeat(DEFAULT_MAX_STRING_ARG_BYTES + 1);
        let e = call(&b, &[s("getAttribute"), n(a), s(&long)]).expect_err("超過");
        assert!(matches!(e, DomBridgeError::StringTooLong { index: 2, .. }));
        let e = call(&b, &[s("getAttribute"), n(a)]).expect_err("不足");
        assert!(matches!(
            e,
            DomBridgeError::ArityMismatch { expected: 2, .. }
        ));
        let e = call(&b, &[s("getAttribute"), n(a), n(1.0)]).expect_err("型");
        assert!(matches!(e, DomBridgeError::TypeMismatch { index: 2, .. }));
        assert_eq!(snapshot(&b), before);
    }

    #[test]
    fn js_6_get_attribute_response_limit() {
        let b = bridge_with(DomBridgeLimits::default().with_max_response_bytes(4));
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        call(&b, &[s("setAttribute"), n(a), s("title"), s("1234")]).expect("set");
        assert_eq!(
            call(&b, &[s("getAttribute"), n(a), s("title")]).expect("ok"),
            s("1234")
        );
        call(&b, &[s("setAttribute"), n(a), s("title"), s("12345")]).expect("set");
        let e = call(&b, &[s("getAttribute"), n(a), s("title")]).expect_err("超過");
        assert!(matches!(
            e,
            DomBridgeError::ResponseTooLarge { len: 5, limit: 4 }
        ));
    }

    #[test]
    fn js_6_response_limit_applies_to_each_string_response() {
        let b = bridge_with(DomBridgeLimits::default().with_max_response_bytes(4));
        let root = num(call(&b, &[s("documentRoot")]));
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        call(&b, &[s("setTextContent"), n(a), s("abcd")]).expect("set");
        // 上限ちょうどは成功。
        assert_eq!(
            call(&b, &[s("getTextContent"), n(a)]).expect("ok"),
            s("abcd")
        );
        call(&b, &[s("setTextContent"), n(a), s("abcde")]).expect("set");
        let e = call(&b, &[s("getTextContent"), n(a)]).expect_err("超過");
        assert!(matches!(
            e,
            DomBridgeError::ResponseTooLarge { len: 5, limit: 4 }
        ));
        let e = call(&b, &[s("getInnerHTML"), n(a)]).expect_err("超過");
        assert!(matches!(e, DomBridgeError::ResponseTooLarge { .. }));
        let e = call(&b, &[s("querySelectorAll"), n(root), s("*")]);
        assert!(e.is_err());
        let e = call(&b, &[s("querySelectorAll"), n(root), s("div")]).expect_err("超過");
        assert!(matches!(e, DomBridgeError::ResponseTooLarge { .. }));
    }

    #[test]
    fn js_6_op_limit_is_sticky_and_resets_on_attach() {
        let b = bridge_with(DomBridgeLimits::default().with_max_ops_per_page(3));
        for _ in 0..3 {
            call(&b, &[s("body")]).expect("上限内");
        }
        for _ in 0..3 {
            let e = call(&b, &[s("body")]).expect_err("上限超過");
            assert!(matches!(e, DomBridgeError::OpLimitExceeded { limit: 3 }));
        }
        b.attach(parse(HTML)).expect("attach");
        // 不正な呼び出しも数える。
        let _ = call(&b, &[s("eval")]);
        let _ = call(&b, &[s("eval")]);
        let _ = call(&b, &[s("eval")]);
        let e = call(&b, &[s("body")]).expect_err("上限超過");
        assert!(matches!(e, DomBridgeError::OpLimitExceeded { .. }));
    }

    #[test]
    fn js_6_generation_exhaustion_is_fail_closed() {
        let b = bridge();
        b.state.lock().expect("lock").next_generation = GENERATION_LIMIT;
        let e = b.attach(parse(HTML)).expect_err("枯渇");
        assert!(matches!(e, DomBridgeError::GenerationExhausted));
        assert!(encode_node_id(GENERATION_LIMIT, NodeId::new(0)).is_err());
        assert!(encode_node_id(0, NodeId::new(0)).is_err());
        let max = encode_node_id(GENERATION_LIMIT - 1, NodeId::new(u32::MAX as usize))
            .expect("最大値は載る");
        assert!(max < MAX_SAFE_INTEGER_EXCLUSIVE);
        #[cfg(target_pointer_width = "64")]
        assert!(matches!(
            encode_node_id(1, NodeId::new(u32::MAX as usize + 1)),
            Err(DomBridgeError::NodeIndexOutOfRange)
        ));
    }

    #[test]
    fn js_5_error_messages_do_not_echo_untrusted_strings() {
        let b = bridge();
        let root = num(call(&b, &[s("documentRoot")]));
        let secret = "SECRET_TOKEN";
        let errs = [
            call(&b, &[s(secret)]).expect_err("未知"),
            call(&b, &[s("querySelector"), n(root), s("a[")]).expect_err("不正"),
            call(&b, &[s("createElement"), s("bad name")]).expect_err("不正名"),
        ];
        for e in errs {
            assert!(!e.to_string().contains(secret));
            let js: JsEngineError = e.into();
            assert!(matches!(js, JsEngineError::EvaluationFailed(_)));
        }
        let e = call(&b, &[s("querySelector"), n(root), s("a[SECRET")]).expect_err("不正");
        assert!(!e.to_string().contains("SECRET"));
    }

    #[test]
    fn js_5_head_and_body_lookup() {
        let b = bridge();
        let head = num(call(&b, &[s("head")]));
        let body = num(call(&b, &[s("body")]));
        assert_ne!(head, body);
        let root = num(call(&b, &[s("documentRoot")]));
        assert_eq!(
            num(call(&b, &[s("querySelector"), n(root), s("body")])),
            body
        );
    }
    #[test]
    fn js_5_get_element_by_id_is_case_sensitive_even_in_quirks_mode() {
        // DOCTYPE なし = Quirks。CSS の ID 照合は大文字小文字を無視するが getElementById は完全一致。
        let b = DomBridge::new(DomBridgeLimits::default());
        b.attach(parse("<div id=\"A\">x</div><p id=\"a\">y</p>"))
            .expect("attach");
        let lower = num(call(&b, &[s("getElementById"), s("a")]));
        let upper = num(call(&b, &[s("getElementById"), s("A")]));
        assert_ne!(lower, upper);
        let got = call(&b, &[s("getTextContent"), n(lower)]).expect("text");
        assert_eq!(got, s("y"));
        assert_eq!(
            call(&b, &[s("getElementById"), s("b")]).expect("none"),
            JsValue::Null
        );
    }

    #[test]
    fn js_6_oversized_responses_are_rejected_before_allocation() {
        let b = bridge_with(DomBridgeLimits::default().with_max_response_bytes(8));
        let root = num(call(&b, &[s("documentRoot")]));
        let e = call(&b, &[s("getTextContent"), n(root)]);
        // Document は None（Null）。要素側は上限内/超過を既存テストで確認済み。
        assert_eq!(e.expect("null"), JsValue::Null);
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        call(&b, &[s("setTextContent"), n(a), s("123456789")]).expect("set");
        let body = num(call(&b, &[s("body")]));
        assert!(matches!(
            call(&b, &[s("getTextContent"), n(body)]).expect_err("超過"),
            DomBridgeError::ResponseTooLarge { limit: 8, .. }
        ));
        assert!(matches!(
            call(&b, &[s("getInnerHTML"), n(body)]).expect_err("超過"),
            DomBridgeError::ResponseTooLarge { limit: 8, .. }
        ));
    }

    /// `REPAIR-9`: getInnerHTML は成功・失敗とも 1 呼び出し 1 件が記録される。
    #[test]
    fn repair_9_get_inner_html_is_recorded_once_per_call() {
        use crate::{InMemoryRecorder, OperationKind, OperationOutcome};
        use std::sync::Arc;

        let rec = Arc::new(InMemoryRecorder::with_capacity(16));
        let doc = parse_document(HTML, &ParseOptions::default().with_recorder(rec.clone()))
            .expect("parse")
            .document;
        let limits = DomBridgeLimits {
            max_response_bytes: 8,
            ..DomBridgeLimits::default()
        };
        let b = DomBridge::new(limits);
        b.attach(doc).expect("attach");
        let a = num(call(&b, &[s("getElementById"), s("a")]));
        let base = rec.records().len();
        call(&b, &[s("getInnerHTML"), n(a)]).expect("ok");
        let records = rec.records();
        assert_eq!(records.len(), base + 1);
        let last = records[base];
        assert_eq!(last.operation(), OperationKind::Dom);
        assert_eq!(last.outcome(), OperationOutcome::Success);

        let body = num(call(&b, &[s("body")]));
        call(&b, &[s("setTextContent"), n(a), s("0123456789")]).expect("set");
        let before = rec.records().len();
        call(&b, &[s("getInnerHTML"), n(body)]).expect_err("超過");
        let records = rec.records();
        assert_eq!(records.len(), before + 1);
        assert!(matches!(
            records[before].outcome(),
            OperationOutcome::Failure { .. }
        ));
    }

    fn text(v: Result<JsValue, DomBridgeError>) -> String {
        match v.expect("成功するはず") {
            JsValue::String(x) => x,
            other => panic!("String を期待: {other:?}"),
        }
    }

    fn loc(b: &DomBridge, member: &str) -> String {
        text(call(b, &[s("getLocation"), s(member)]))
    }

    #[test]
    fn js_5_location_defaults_to_about_blank_and_follows_set_location() {
        let b = bridge();
        assert_eq!(loc(&b, "href"), "about:blank");
        assert_eq!(loc(&b, "origin"), "null");
        assert_eq!(loc(&b, "pathname"), "blank");
        b.set_location("https://user:pw@example.com:8443/a/b?q=1#f")
            .expect("set");
        assert_eq!(loc(&b, "href"), "https://example.com:8443/a/b?q=1#f");
        assert_eq!(loc(&b, "origin"), "https://example.com:8443");
        assert_eq!(loc(&b, "port"), "8443");
        assert_eq!(loc(&b, "search"), "?q=1");
        assert_eq!(loc(&b, "hash"), "#f");
        b.set_location("https://example.com/").expect("set");
        assert_eq!(loc(&b, "port"), "");
        assert_eq!(loc(&b, "search"), "");
        assert_eq!(loc(&b, "hash"), "");
        // attach で about:blank に戻る。
        b.attach(parse(HTML)).expect("attach");
        assert_eq!(loc(&b, "href"), "about:blank");
    }

    #[test]
    fn js_5_set_location_rejects_bad_urls_without_echo() {
        let b = bridge();
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "ftp://h/",
            "about:blank",
            "::garbage::",
        ] {
            let e = b.set_location(bad).expect_err(bad);
            assert!(matches!(e, DomBridgeError::InvalidLocation { .. }), "{e}");
            assert!(!e.to_string().contains("passwd"), "{e}");
        }
        // 上限超過は解析前に拒否し、URL 本文を反響しない。
        let long = format!("https://example.com/{}", "SECRETQ".repeat(2000));
        let e = b.set_location(&long).expect_err("too long");
        assert!(matches!(e, DomBridgeError::InvalidLocation { .. }), "{e}");
        assert!(!e.to_string().contains("SECRETQ"), "{e}");
        assert_eq!(loc(&b, "href"), "about:blank");
        let detached = DomBridge::new(DomBridgeLimits::default());
        assert!(matches!(
            detached.set_location("https://example.com/"),
            Err(DomBridgeError::Detached)
        ));
    }

    #[test]
    fn js_5_set_location_hash_strips_one_hash_and_clears_when_empty() {
        let b = bridge();
        b.set_location("https://example.com/p?x=1").expect("set");
        call(&b, &[s("setLocationHash"), s("n")]).expect("ok");
        assert_eq!(loc(&b, "href"), "https://example.com/p?x=1#n");
        call(&b, &[s("setLocationHash"), s("#m")]).expect("ok");
        assert_eq!(loc(&b, "hash"), "#m");
        call(&b, &[s("setLocationHash"), s("")]).expect("ok");
        assert_eq!(loc(&b, "href"), "https://example.com/p?x=1");
        // about:blank は書き換えず診断に残す。
        let blank = bridge();
        call(&blank, &[s("setLocationHash"), s("z")]).expect("ok");
        assert_eq!(loc(&blank, "href"), "about:blank");
        let d = blank.take_diagnostics().expect("diag");
        assert_eq!(
            d.location_changes_ignored,
            vec![IgnoredLocationChange {
                kind: LocationChangeKind::Hash,
                value_len: 1
            }]
        );
    }

    #[test]
    fn js_5_ignore_location_change_records_kind_and_length_only() {
        let b = bridge();
        call(
            &b,
            &[
                s("ignoreLocationChange"),
                s("href"),
                s("https://secret.test/?t=1"),
            ],
        )
        .expect("ok");
        call(&b, &[s("ignoreLocationChange"), s("nope"), s("x")]).expect_err("許可リスト外");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.location_changes_ignored.len(), 1);
        assert_eq!(d.location_changes_ignored[0].value_len, 24);
        assert!(!format!("{d:?}").contains("secret.test"));
        assert_eq!(
            b.take_diagnostics().expect("empty"),
            BridgeDiagnostics::default()
        );
    }

    #[test]
    fn js_5_console_message_truncates_on_char_boundary_and_caps_count() {
        let b = bridge();
        let long = "あ".repeat(2000);
        call(&b, &[s("consoleMessage"), s("log"), s(&long), s("")]).expect("ok");
        call(&b, &[s("consoleMessage"), s("trace"), s("x"), s("")]).expect_err("許可リスト外");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.console_messages.len(), 1);
        assert_eq!(d.console_messages[0].text.len(), 4095);
        assert!(d.console_messages[0].truncated);
        // Debug 出力に本文を含めないこと（診断フィールドの追加で長さは緩やかに増えるため余裕を持たせる）。
        assert!(format!("{d:?}").len() < 500);
        for _ in 0..(MAX_BRIDGE_DIAGNOSTICS + 3) {
            call(&b, &[s("consoleMessage"), s("warn"), s("m"), s("")]).expect("ok");
        }
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.console_messages.len(), MAX_BRIDGE_DIAGNOSTICS);
        assert_eq!(d.dropped, 3);
        call(&b, &[s("consoleMessage"), s("info"), s("keep"), s("")]).expect("ok");
        b.attach(parse(HTML)).expect("attach");
        assert_eq!(
            b.take_diagnostics().expect("empty"),
            BridgeDiagnostics::default()
        );
    }

    #[test]
    fn js_6_lifecycle_listener_limit_op_is_exempt_from_op_budget() {
        let b = bridge_with(DomBridgeLimits::default().with_max_ops_per_page(1));
        call(&b, &[s("body")]).expect("budget consumed");
        call(&b, &[s("body")]).expect_err("over budget");
        call(&b, &[s("lifecycleListenerLimit")]).expect("exempt");
        assert!(b.listener_limit_exceeded().expect("flag"));
    }

    #[test]
    fn js_6_lifecycle_listener_limit_op_sets_flag() {
        let b = bridge();
        assert!(!b.listener_limit_exceeded().expect("flag"));
        call(&b, &[s("lifecycleListenerLimit"), s("x")]).expect_err("arity");
        call(&b, &[s("lifecycleListenerLimit")]).expect("ok");
        assert!(b.listener_limit_exceeded().expect("flag"));
        assert!(b.take_diagnostics().expect("diag").listener_limit_exceeded);
    }

    #[test]
    fn js_4_lifecycle_listener_error_op_validates_event_name() {
        let b = bridge();
        let stack = "x".repeat(1025);
        call(
            &b,
            &[
                s("lifecycleListenerError"),
                s("load"),
                s("TypeError"),
                s("foo.bar is not a function"),
                s(&stack),
            ],
        )
        .expect("ok");
        call(
            &b,
            &[
                s("lifecycleListenerError"),
                s("click"),
                s(""),
                s("x"),
                s(""),
            ],
        )
        .expect_err("許可リスト外");
        call(&b, &[s("lifecycleListenerError"), s("load")]).expect_err("arity");
        // 旧形式（種別・メッセージの 2 引数）と 3 引数は拒否する（Issue #1172）。
        call(&b, &[s("lifecycleListenerError"), s("load"), s("boom")]).expect_err("旧形式");
        call(
            &b,
            &[s("lifecycleListenerError"), s("load"), s(""), s("boom")],
        )
        .expect_err("arity 3");
        call(
            &b,
            &[s("lifecycleListenerError"), s("load"), s(""), n(1.0), s("")],
        )
        .expect_err("type");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.listener_errors.len(), 1);
        assert_eq!(d.listener_errors[0].event, LifecycleEvent::Load);
        assert_eq!(d.listener_errors[0].message, "foo.bar is not a function");
        assert_eq!(
            d.listener_errors[0].summary.kind(),
            ExceptionKind::TypeError
        );
        assert_eq!(d.listener_errors[0].summary.identifier(), Some("foo.bar"));
        assert_eq!(d.listener_errors[0].raw_stack().map(str::len), Some(1024));
    }

    #[test]
    fn js_5_navigator_user_agent_matches_fetcher() {
        let b = bridge();
        assert_eq!(
            text(call(&b, &[s("navigatorUserAgent")])),
            crate::fetch::USER_AGENT
        );
    }

    #[test]
    fn js_5_history_set_url_rewrites_only_same_origin() {
        let b = bridge();
        b.set_location("https://example.com:8443/a/b?q=1#f")
            .expect("set_location");
        let href = || match call(&b, &[s("getLocation"), s("href")]).expect("href") {
            JsValue::String(v) => v,
            other => panic!("String を期待: {other:?}"),
        };
        assert!(matches!(
            call(&b, &[s("historySetUrl"), s("c?x=1")]),
            Ok(JsValue::Bool(true))
        ));
        assert_eq!(href(), "https://example.com:8443/a/c?x=1");
        for bad in [
            "https://evil.test/secret?token=abc",
            "javascript:1",
            "file:///x",
        ] {
            assert!(matches!(
                call(&b, &[s("historySetUrl"), s(bad)]),
                Ok(JsValue::Bool(false))
            ));
        }
        assert_eq!(href(), "https://example.com:8443/a/c?x=1");
        let diag = b.take_diagnostics().expect("diagnostics");
        assert!(!format!("{diag:?}").contains("evil.test"));
    }

    #[test]
    fn js_5_history_set_url_rejects_on_about_blank() {
        let b = bridge();
        assert!(matches!(
            call(&b, &[s("historySetUrl"), s("/x")]),
            Ok(JsValue::Bool(false))
        ));
    }

    #[test]
    fn js_5_page_env_ops_check_arity_and_types() {
        let b = bridge();
        for (args, label) in [
            (vec![s("navigatorUserAgent"), s("x")], "ua arity"),
            (vec![s("getLocation")], "get arity"),
            (vec![s("getLocation"), n(1.0)], "get type"),
            (vec![s("getLocation"), s("nope")], "get member"),
            (vec![s("setLocationHash"), n(1.0)], "hash type"),
            (vec![s("ignoreLocationChange"), s("href")], "ignore arity"),
            (vec![s("historySetUrl")], "history arity"),
            (vec![s("historySetUrl"), n(1.0)], "history type"),
            (
                vec![s("consoleMessage"), s("log"), s("a"), n(1.0)],
                "console stack type",
            ),
            (vec![s("consoleMessage"), s("log"), s("a")], "console arity"),
        ] {
            call(&b, &args).expect_err(label);
        }
    }

    /// JS-5: console の stack は opt-in 時だけ 1024 バイトへ切り詰めて保持し、Debug に本文を出さない
    /// （Issue #1183・`SEC-2`）。
    #[test]
    fn js_5_console_stack_is_opt_in_and_bounded() {
        let b = bridge();
        let long = "a".repeat(2000);
        call(&b, &[s("consoleMessage"), s("error"), s("m"), s(&long)]).expect("ok");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.console_messages[0].raw_stack(), None);

        b.set_capture_timer_origin(true).expect("flag");
        call(&b, &[s("consoleMessage"), s("error"), s("m"), s(&long)]).expect("ok");
        call(&b, &[s("consoleMessage"), s("error"), s("m"), s("")]).expect("ok");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.console_messages[0].raw_stack().map(str::len), Some(1024));
        assert_eq!(d.console_messages[1].raw_stack(), None);
        assert!(!format!("{d:?}").contains("aaaa"));
    }

    fn b_bool(v: bool) -> JsValue {
        JsValue::Bool(v)
    }

    /// JS-7: `timerSet` は遅延を丸めて登録し、型の不一致・arity は拒否する。
    #[test]
    fn js_7_timer_set_op_validates_and_registers() {
        let b = bridge();
        assert_eq!(num(call(&b, &[s("timerSet"), n(-5.0), b_bool(false)])), 1.0);
        assert_eq!(
            num(call(&b, &[s("timerSet"), n(f64::NAN), b_bool(true)])),
            2.0
        );
        call(&b, &[s("timerSet"), s("10"), b_bool(false)]).expect_err("type");
        call(&b, &[s("timerSet"), n(10.0), n(1.0)]).expect_err("type");
        call(&b, &[s("timerSet"), n(10.0)]).expect_err("arity");
        assert_eq!(b.event_loop_state().expect("state").2, 2);
    }

    /// JS-6: 保留中の件数の上限を超えると 0 を返し、ランナー向けフラグが立つ。
    #[test]
    fn js_6_timer_set_op_flags_registered_limit() {
        let b = bridge();
        b.set_event_loop_limits(EventLoopLimits::default().with_max_registered_timers(2))
            .expect("limits");
        assert_eq!(b.resource_limit_tripped().expect("flag"), None);
        assert_eq!(num(call(&b, &[s("timerSet"), n(1.0), b_bool(false)])), 1.0);
        assert_eq!(num(call(&b, &[s("timerSet"), n(1.0), b_bool(false)])), 2.0);
        assert_eq!(num(call(&b, &[s("timerSet"), n(1.0), b_bool(false)])), 0.0);
        assert_eq!(
            b.resource_limit_tripped().expect("flag"),
            Some(ResourceLimitKind::RegisteredTimers)
        );
        // attach し直すと状態も上限も初期化される。
        b.attach(parse(HTML)).expect("attach");
        assert_eq!(b.resource_limit_tripped().expect("flag"), None);
        assert_eq!(b.event_loop_state().expect("state").2, 0);
    }

    /// JS-7: `timerClear` は不正な ID を黙って無視し、有効な ID を取り消す。
    #[test]
    fn js_7_timer_clear_op_ignores_invalid_ids() {
        let b = bridge();
        call(&b, &[s("timerSet"), n(1.0), b_bool(false)]).expect("set");
        for bad in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1.5, 1e300] {
            call(&b, &[s("timerClear"), n(bad)]).expect("ignored");
        }
        call(&b, &[s("timerClear"), s("1")]).expect_err("type");
        assert_eq!(b.event_loop_state().expect("state").2, 1);
        call(&b, &[s("timerClear"), n(1.0)]).expect("clear");
        assert_eq!(b.event_loop_state().expect("state").2, 0);
    }

    /// JS-6: `timerPending` は登録中の ID だけ true を返し、取り消し済み・不正な ID は false。
    #[test]
    fn js_6_timer_pending_op_reports_liveness() {
        let b = bridge();
        call(&b, &[s("timerSet"), n(1.0), b_bool(false)]).expect("set");
        assert_eq!(
            call(&b, &[s("timerPending"), n(1.0)]).expect("p"),
            JsValue::Bool(true)
        );
        assert_eq!(
            call(&b, &[s("timerPending"), n(2.0)]).expect("p"),
            JsValue::Bool(false)
        );
        assert_eq!(
            call(&b, &[s("timerPending"), n(f64::NAN)]).expect("p"),
            JsValue::Bool(false)
        );
        call(&b, &[s("timerPending"), s("1")]).expect_err("type");
        call(&b, &[s("timerClear"), n(1.0)]).expect("clear");
        assert_eq!(
            call(&b, &[s("timerPending"), n(1.0)]).expect("p"),
            JsValue::Bool(false)
        );
    }

    /// JS-6: 登録元 stack の op は既定で無視し、opt-in 時だけ切り詰めて保存する。型・arity は拒否する（Issue #1176）。
    #[test]
    fn js_6_timer_origin_ops_are_opt_in_and_bounded() {
        let b = bridge();
        call(&b, &[s("timerSet"), n(5.0), b_bool(false)]).expect("set");
        assert_eq!(
            call(&b, &[s("timerOriginCapture")]).expect("c"),
            JsValue::Bool(false)
        );
        call(&b, &[s("timerOrigin"), n(1.0), s("at a (u:1:1)")]).expect("ignored");
        assert_eq!(b.pending_timers(4).expect("p")[0].origin, None);

        b.set_capture_timer_origin(true).expect("flag");
        assert_eq!(
            call(&b, &[s("timerOriginCapture")]).expect("c"),
            JsValue::Bool(true)
        );
        call(&b, &[s("timerOrigin"), n(1.0), s("at a (u:1:1)")]).expect("kept");
        let got = b.pending_timers(4).expect("p");
        assert_eq!(got[0].origin.as_deref(), Some("at a (u:1:1)"));
        assert_eq!((got[0].delay_ms, got[0].due_ms), (5, 5));
        // 不正な ID・保留中でない ID は黙って無視する。
        for bad in [f64::NAN, 0.5, -1.0, 9.0] {
            call(&b, &[s("timerOrigin"), n(bad), s("x")]).expect("ignored");
        }
        let long = "z".repeat(5000);
        call(&b, &[s("timerOrigin"), n(1.0), s(&long)]).expect("trunc");
        assert_eq!(
            b.pending_timers(4).expect("p")[0]
                .origin
                .as_deref()
                .map(str::len),
            Some(crate::event_loop::MAX_TIMER_ORIGIN_BYTES)
        );
        call(&b, &[s("timerOrigin"), s("1"), s("x")]).expect_err("type");
        call(&b, &[s("timerOrigin"), n(1.0), n(1.0)]).expect_err("type");
        call(&b, &[s("timerOrigin"), n(1.0)]).expect_err("arity");
        call(&b, &[s("timerOriginCapture"), n(1.0)]).expect_err("arity");
    }

    fn url_parts(v: Result<JsValue, DomBridgeError>) -> Vec<String> {
        match v.expect("成功するはず") {
            JsValue::String(joined) => joined.split('\n').map(str::to_owned).collect(),
            other => panic!("String を期待: {other:?}"),
        }
    }

    /// JS-5: `urlParse` は絶対 URL・相対 URL と base・失敗（Null）を返す。
    #[test]
    fn js_5_url_parse_op_resolves_and_fails_with_null() {
        let b = bridge();
        let parts = url_parts(call(
            &b,
            &[
                s("urlParse"),
                s("https://user:pw@example.com:8080/a/b?x=1#h"),
                JsValue::Null,
            ],
        ));
        assert_eq!(parts.len(), 11);
        assert_eq!(parts[0], "https://user:pw@example.com:8080/a/b?x=1#h");
        assert_eq!(parts[3], "user");
        assert_eq!(parts[4], "pw");
        assert_eq!(parts[10], "#h");
        let parts = url_parts(call(
            &b,
            &[s("urlParse"), s("../c?q"), s("https://example.com/a/b/")],
        ));
        assert_eq!(parts[0], "https://example.com/a/c?q");
        assert_eq!(
            call(&b, &[s("urlParse"), s("not a url"), JsValue::Null]).expect("ok"),
            JsValue::Null
        );
        assert_eq!(
            call(&b, &[s("urlParse"), s("/x"), s("also bad")]).expect("ok"),
            JsValue::Null
        );
        let parts = url_parts(call(
            &b,
            &[
                s("urlParse"),
                s("https://ex\nample.com/a\tb"),
                JsValue::Null,
            ],
        ));
        assert_eq!(parts.len(), 11);
        assert_eq!(parts[0], "https://example.com/ab");
        assert_eq!(
            b.take_diagnostics().expect("diag"),
            BridgeDiagnostics::default()
        );
    }

    /// JS-6: `urlParse` は型・arity 違反を拒否し、入力長は解析の前に検証する。
    #[test]
    fn js_6_url_parse_op_validates_args_and_length() {
        let b = bridge();
        assert!(matches!(
            call(&b, &[s("urlParse"), s("https://a/"), n(1.0)]).expect_err("型"),
            DomBridgeError::TypeMismatch { index: 2, .. }
        ));
        assert!(matches!(
            call(&b, &[s("urlParse"), s("https://a/")]).expect_err("arity"),
            DomBridgeError::ArityMismatch { .. }
        ));
        let over = "a".repeat(DEFAULT_MAX_STRING_ARG_BYTES + 1);
        assert!(matches!(
            call(&b, &[s("urlParse"), s(&over), JsValue::Null]).expect_err("超過"),
            DomBridgeError::StringTooLong { index: 1, .. }
        ));
        assert!(matches!(
            call(&b, &[s("urlParse"), s("/x"), s(&over)]).expect_err("超過"),
            DomBridgeError::StringTooLong { index: 2, .. }
        ));
        let prefix = "https://a/";
        let exact = format!(
            "{prefix}{}",
            "x".repeat(DEFAULT_MAX_STRING_ARG_BYTES - prefix.len())
        );
        let parts = url_parts(call(&b, &[s("urlParse"), s(&exact), JsValue::Null]));
        assert_eq!(parts[0].len(), DEFAULT_MAX_STRING_ARG_BYTES);
        let small = bridge_with(DomBridgeLimits::default().with_max_response_bytes(8));
        assert!(matches!(
            call(&small, &[s("urlParse"), s("https://a/"), JsValue::Null]).expect_err("応答"),
            DomBridgeError::ResponseTooLarge { .. }
        ));
    }

    /// JS-6: `mutationObserverLimit` は操作予算を使い切った後でもフラグを立て、
    /// `resource_limit_tripped` が `MutationObservers` を返す。
    #[test]
    fn js_6_mutation_observer_limit_op_is_budget_exempt_and_trips() {
        let b = bridge_with(DomBridgeLimits::default().with_max_ops_per_page(1));
        call(&b, &[s("body")]).expect("budget");
        call(&b, &[s("body")]).expect_err("budget exhausted");
        assert_eq!(b.resource_limit_tripped().expect("flag"), None);
        call(&b, &[s("mutationObserverLimit")]).expect("exempt");
        assert_eq!(
            b.resource_limit_tripped().expect("flag"),
            Some(ResourceLimitKind::MutationObservers)
        );
        assert!(
            b.take_diagnostics()
                .expect("diag")
                .mutation_observer_limit_exceeded
        );
    }

    /// REPAIR-3: `mutationOptionsIgnored` は 1 回の加算を 64 で頭打ちにし、NaN・負値は無視する。
    #[test]
    fn repair_3_mutation_options_ignored_op_is_bounded() {
        let b = bridge();
        call(&b, &[s("mutationOptionsIgnored"), n(2.0)]).expect("ok");
        call(&b, &[s("mutationOptionsIgnored"), n(f64::INFINITY)]).expect("ok");
        call(&b, &[s("mutationOptionsIgnored"), n(f64::NAN)]).expect("ok");
        call(&b, &[s("mutationOptionsIgnored"), n(-3.0)]).expect("ok");
        call(&b, &[s("mutationOptionsIgnored"), n(1e9)]).expect("ok");
        call(&b, &[s("mutationOptionsIgnored"), s("1")]).expect_err("type");
        call(&b, &[s("mutationOptionsIgnored")]).expect_err("arity");
        assert_eq!(
            b.take_diagnostics().expect("diag").mutation_options_ignored,
            2 + 64
        );
    }

    /// JS-5: `isInclusiveAncestor` は自身・祖先で true、無関係・切り離しノードで false、
    /// 不正 ID・型違いはエラー。
    #[test]
    fn js_5_is_inclusive_ancestor_op() {
        let b = bridge();
        let body = num(call(&b, &[s("body")]));
        let div = num(call(&b, &[s("getElementById"), s("a")]));
        let head = num(call(&b, &[s("head")]));
        let loose = num(call(&b, &[s("createElement"), s("p")]));
        let is = |a: f64, x: f64| call(&b, &[s("isInclusiveAncestor"), n(a), n(x)]).expect("ok");
        assert_eq!(is(div, div), JsValue::Bool(true));
        assert_eq!(is(body, div), JsValue::Bool(true));
        assert_eq!(is(div, body), JsValue::Bool(false));
        assert_eq!(is(head, div), JsValue::Bool(false));
        assert_eq!(is(body, loose), JsValue::Bool(false));
        call(&b, &[s("isInclusiveAncestor"), n(1e12), n(div)]).expect_err("invalid id");
        call(&b, &[s("isInclusiveAncestor"), s("1"), n(div)]).expect_err("type");
        call(&b, &[s("isInclusiveAncestor"), n(div)]).expect_err("arity");
    }

    /// JS-5: `isFocusable` は接続した input で true、div・切り離した要素で false、不正入力はエラー。
    #[test]
    fn js_5_is_focusable_op() {
        let b = bridge();
        let body = num(call(&b, &[s("body")]));
        let div = num(call(&b, &[s("getElementById"), s("a")]));
        let input = num(call(&b, &[s("createElement"), s("input")]));
        let is = |x: f64| call(&b, &[s("isFocusable"), n(x)]).expect("ok");
        assert_eq!(is(input), JsValue::Bool(false));
        call(&b, &[s("appendChild"), n(div), n(input)]).expect("append");
        assert_eq!(is(input), JsValue::Bool(true));
        assert_eq!(is(div), JsValue::Bool(false));
        call(&b, &[s("removeChild"), n(div), n(input)]).expect("remove");
        assert_eq!(is(input), JsValue::Bool(false));
        assert_eq!(is(body), JsValue::Bool(false));
        call(&b, &[s("isFocusable"), n(1e12)]).expect_err("invalid id");
        call(&b, &[s("isFocusable"), s("1")]).expect_err("type");
        call(&b, &[s("isFocusable")]).expect_err("arity");
        call(&b, &[s("isFocusable"), n(div), n(div)]).expect_err("arity");
    }

    /// JS-5: `htmlLocalName` は HTML 要素の小文字名を返し、SVG 要素・テキスト・文書は null、不正入力はエラー（#1186）。
    #[test]
    fn js_5_html_local_name_op() {
        let b = bridge();
        let body = num(call(&b, &[s("body")]));
        let root = num(call(&b, &[s("documentRoot")]));
        let input = num(call(&b, &[s("createElement"), s("INPUT")]));
        let text = num(call(&b, &[s("createTextNode"), s("t")]));
        let name = |x: f64| call(&b, &[s("htmlLocalName"), n(x)]).expect("ok");
        assert_eq!(name(input), JsValue::String("input".to_string()));
        assert_eq!(name(text), JsValue::Null);
        assert_eq!(name(root), JsValue::Null);
        call(
            &b,
            &[
                s("setInnerHTML"),
                n(body),
                s("<svg><a id=\"sa\"></a></svg>"),
            ],
        )
        .expect("svg");
        let svg_a = num(call(&b, &[s("getElementById"), s("sa")]));
        assert_eq!(name(svg_a), JsValue::Null);
        call(&b, &[s("htmlLocalName"), n(1e12)]).expect_err("invalid id");
        call(&b, &[s("htmlLocalName"), s("1")]).expect_err("type");
        call(&b, &[s("htmlLocalName")]).expect_err("arity");
        call(&b, &[s("htmlLocalName"), n(input), n(input)]).expect_err("arity");
    }

    /// JS-7: `timerCallbackError` は種別を許可リストで検証し、メッセージを切り詰めて件数を頭打ちにする。
    #[test]
    fn js_7_timer_callback_error_op_is_bounded() {
        let b = bridge();
        call(
            &b,
            &[
                s("timerCallbackError"),
                s("click"),
                s("Error"),
                s("x"),
                s(""),
            ],
        )
        .expect_err("許可リスト外");
        call(&b, &[s("timerCallbackError"), s("timeout"), s("x")]).expect_err("arity");
        call(
            &b,
            &[s("timerCallbackError"), s("timeout"), s("Error"), s("x")],
        )
        .expect_err("arity 旧形式");
        call(
            &b,
            &[
                s("timerCallbackError"),
                s("timeout"),
                s("Error"),
                n(1.0),
                s(""),
            ],
        )
        .expect_err("type");
        let big = "x".repeat(MAX_CONSOLE_MESSAGE_BYTES + 1);
        call(
            &b,
            &[
                s("timerCallbackError"),
                s("interval"),
                s("Error"),
                s(&big),
                s(""),
            ],
        )
        .expect("ok");
        for _ in 0..(MAX_BRIDGE_DIAGNOSTICS + 3) {
            call(
                &b,
                &[
                    s("timerCallbackError"),
                    s("microtask"),
                    s("EvilError"),
                    s("e"),
                    s("at f (https://h.example/a.js:1:2)"),
                ],
            )
            .expect("ok");
        }
        call(&b, &[s("timerStringHandlerIgnored")]).expect("ok");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.timer_errors.len(), MAX_BRIDGE_DIAGNOSTICS);
        assert_eq!(d.timer_errors[0].source, TimerCallbackSource::Interval);
        assert_eq!(d.timer_errors[0].message.len(), MAX_CONSOLE_MESSAGE_BYTES);
        assert!(d.timer_errors[0].truncated);
        assert_eq!(d.timer_errors[0].summary.kind(), ExceptionKind::Error);
        // 語彙外の name は Other に落ちる。
        assert_eq!(d.timer_errors[1].summary.kind(), ExceptionKind::Other);
        assert_eq!(d.dropped, 4);
        assert_eq!(d.timer_string_handlers_ignored, 1);
    }

    fn net_bridge() -> DomBridge {
        let b = bridge();
        b.set_location("https://example.test/dir/page.html")
            .expect("location");
        b
    }

    fn net_request(b: &DomBridge, kind: &str, method: &str, body: bool, url: &str) -> f64 {
        num(call(
            b,
            &[s("netRequest"), s(kind), s(method), b_bool(body), s(url)],
        ))
    }

    /// JS-8: `netRequest` は相対 URL を location 基準で解決して受理し、拒否は 0 と診断で表す。
    #[test]
    fn js_8_net_request_op_validates_and_records_rejections() {
        let b = net_bridge();
        assert_eq!(net_request(&b, "fetch", "GET", false, "a.json"), 1.0);
        assert_eq!(net_request(&b, "xhr", "get", false, "../b"), 2.0);
        assert_eq!(net_request(&b, "fetch", "POST", false, "a"), 0.0);
        assert_eq!(net_request(&b, "xhr", "GET", true, "a"), 0.0);
        assert_eq!(net_request(&b, "xhr_sync", "GET", false, "a"), 0.0);
        assert_eq!(
            net_request(&b, "fetch", "GET", false, "file:///etc/passwd"),
            0.0
        );
        assert_eq!(
            net_request(&b, "fetch", "GET", false, "https://u:p@example.test/"),
            0.0
        );
        // 未知の種別・型の不一致・arity は拒否する。
        call(
            &b,
            &[
                s("netRequest"),
                s("beacon"),
                s("GET"),
                b_bool(false),
                s("a"),
            ],
        )
        .expect_err("kind");
        call(
            &b,
            &[s("netRequest"), s("fetch"), s("GET"), s("no"), s("a")],
        )
        .expect_err("type");
        call(&b, &[s("netRequest"), s("fetch"), s("GET"), b_bool(false)]).expect_err("arity");
        let kinds: Vec<_> = b
            .take_diagnostics()
            .expect("diag")
            .net_rejections
            .iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                NetRejectKind::UnsupportedMethod,
                NetRejectKind::RequestBody,
                NetRejectKind::SyncXhr,
                NetRejectKind::InvalidUrl,
                NetRejectKind::Credentials,
            ]
        );
    }

    /// JS-8: 応答上限が 4 バイト未満なら netReadBody は明示的なエラーにし、上限超過の文字列を返さない。
    #[test]
    fn js_8_net_read_body_rejects_tiny_response_limit() {
        let b = bridge_with(DomBridgeLimits {
            max_response_bytes: 3,
            ..DomBridgeLimits::default()
        });
        let err = call(&b, &[s("netReadBody"), n(1.0)]).expect_err("tiny limit");
        assert!(matches!(
            err,
            DomBridgeError::ResponseTooLarge { len: 4, limit: 3 }
        ));
    }

    /// JS-8: 未配送の ID は消費も本文の読み出しもできない（配送の偽造を許さない）。
    #[test]
    fn js_8_net_consume_and_read_require_runner_delivery() {
        let b = net_bridge();
        assert_eq!(net_request(&b, "fetch", "GET", false, "a"), 1.0);
        assert_eq!(num(call(&b, &[s("netConsume"), n(1.0)])), -1.0);
        assert_eq!(num(call(&b, &[s("netConsume"), n(f64::NAN)])), -1.0);
        assert_eq!(
            call(&b, &[s("netReadBody"), n(1.0)]).expect("read"),
            JsValue::String(String::new())
        );
        let req = b.take_next_net_request().expect("take").expect("pending");
        b.begin_net_delivery(
            &req,
            NetOutcome::Response {
                status: 200,
                body: "hi".to_string(),
                final_url: "https://example.test/dir/a".to_string(),
            },
        )
        .expect("begin");
        assert_eq!(num(call(&b, &[s("netConsume"), n(2.0)])), -1.0);
        assert_eq!(num(call(&b, &[s("netConsume"), n(1.0)])), 200.0);
        assert_eq!(num(call(&b, &[s("netConsume"), n(1.0)])), -1.0);
        assert_eq!(
            call(&b, &[s("netReadBody"), n(1.0)]).expect("read"),
            JsValue::String("hi".to_string())
        );
        b.finish_net_delivery().expect("finish");
        assert_eq!(
            call(&b, &[s("netReadBody"), n(1.0)]).expect("read"),
            JsValue::String(String::new())
        );
    }

    /// JS-8: コールバック例外は種別を許可リストで検証し、メッセージを切り詰めて保持する。
    #[test]
    fn js_8_net_callback_error_and_ignored_options_are_bounded() {
        let b = net_bridge();
        call(
            &b,
            &[s("netCallbackError"), s("beacon"), s(""), s("x"), s("")],
        )
        .expect_err("kind");
        // 旧形式（種別・メッセージの 2 引数）は拒否する（Issue #1172）。
        call(&b, &[s("netCallbackError"), s("xhr"), s("x")]).expect_err("旧形式");
        let big = "e".repeat(MAX_CONSOLE_MESSAGE_BYTES + 10);
        call(
            &b,
            &[
                s("netCallbackError"),
                s("xhr"),
                s("EvilError"),
                s(&big),
                s(""),
            ],
        )
        .expect("ok");
        call(&b, &[s("netOptionsIgnored"), n(2.0)]).expect("ok");
        call(&b, &[s("netOptionsIgnored"), n(f64::INFINITY)]).expect("ok");
        call(&b, &[s("netOptionsIgnored"), s("1")]).expect_err("type");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.net_callback_errors.len(), 1);
        assert_eq!(d.net_callback_errors[0].request, NetRequestKind::Xhr);
        assert_eq!(
            d.net_callback_errors[0].message.len(),
            MAX_CONSOLE_MESSAGE_BYTES
        );
        assert!(d.net_callback_errors[0].truncated);
        assert_eq!(d.net_callback_errors[0].raw_stack(), None);
        assert_eq!(d.net_options_ignored, 2);
    }
}

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
//! `hash`（`setLocationHash`）だけで、それ以外の遷移系の代入・呼び出しは実行せず
//! `ignoreLocationChange` として診断に記録する（`JS-5`）。`console` のメッセージも
//! `consoleMessage` で診断に溜める。診断は [`DomBridge::take_diagnostics`] で回収し、
//! `attach` ごとに空へ戻る。診断に URL 本文は保存しない（種別と長さのみ）。
//! `navigatorUserAgent` は [`crate::fetch::USER_AGENT`] を返し、実ブラウザを装わない（`SEC-2`）。
//!
//! # 未実装（REPAIR-3）
//!
//! - `hashchange` / `popstate` イベントの発火、`document.location` / `document.URL`、
//!   `location` 代入による実際の遷移は未実装（後続 issue）。
//! - #771 のスパイクで判明する追加操作（`getAttribute`・`parentNode` 等）は後続 issue で足す。

use std::sync::{Arc, Mutex, MutexGuard};

use reqwest::Url;

use fandhe_browser_js::{JsEngine, JsEngineError, JsValue, NativeFn};

use crate::dom::NodeData;
use crate::dom::{Document, NodeId, SerializeScope};
use crate::error::DomError;
use crate::error::Error;
use crate::query::{query_selector_all_str_bounded, query_selector_str};

mod page_env;
pub use page_env::{
    BridgeDiagnostics, ConsoleLevel, ConsoleMessage, IgnoredLocationChange, LifecycleEvent,
    ListenerError, LocationChangeKind, LocationMember, MAX_BRIDGE_DIAGNOSTICS,
    MAX_CONSOLE_MESSAGE_BYTES, MAX_LOCATION_URL_BYTES,
};
use page_env::{apply_hash, location_member};
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
    GetTextContent,
    SetTextContent,
    GetInnerHtml,
    SetInnerHtml,
    ReadyState,
    CurrentScript,
    GetLocation,
    SetLocationHash,
    IgnoreLocationChange,
    NavigatorUserAgent,
    ConsoleMessage,
    LifecycleListenerError,
    LifecycleInstallOpen,
    LifecycleListenerLimit,
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
            "getTextContent" => Self::GetTextContent,
            "setTextContent" => Self::SetTextContent,
            "getInnerHTML" => Self::GetInnerHtml,
            "setInnerHTML" => Self::SetInnerHtml,
            "readyState" => Self::ReadyState,
            "currentScript" => Self::CurrentScript,
            "getLocation" => Self::GetLocation,
            "setLocationHash" => Self::SetLocationHash,
            "ignoreLocationChange" => Self::IgnoreLocationChange,
            "navigatorUserAgent" => Self::NavigatorUserAgent,
            "consoleMessage" => Self::ConsoleMessage,
            "lifecycleListenerError" => Self::LifecycleListenerError,
            "lifecycleInstallOpen" => Self::LifecycleInstallOpen,
            "lifecycleListenerLimit" => Self::LifecycleListenerLimit,
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
            Self::GetTextContent => "getTextContent",
            Self::SetTextContent => "setTextContent",
            Self::GetInnerHtml => "getInnerHTML",
            Self::SetInnerHtml => "setInnerHTML",
            Self::ReadyState => "readyState",
            Self::CurrentScript => "currentScript",
            Self::GetLocation => "getLocation",
            Self::SetLocationHash => "setLocationHash",
            Self::IgnoreLocationChange => "ignoreLocationChange",
            Self::NavigatorUserAgent => "navigatorUserAgent",
            Self::ConsoleMessage => "consoleMessage",
            Self::LifecycleListenerError => "lifecycleListenerError",
            Self::LifecycleInstallOpen => "lifecycleInstallOpen",
            Self::LifecycleListenerLimit => "lifecycleListenerLimit",
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
            | Self::LifecycleListenerLimit => 0,
            Self::CreateElement
            | Self::CreateTextNode
            | Self::GetElementById
            | Self::GetTextContent
            | Self::GetInnerHtml
            | Self::GetLocation
            | Self::SetLocationHash => 1,
            Self::QuerySelector
            | Self::QuerySelectorAll
            | Self::AppendChild
            | Self::RemoveChild
            | Self::RemoveAttribute
            | Self::SetTextContent
            | Self::SetInnerHtml
            | Self::IgnoreLocationChange
            | Self::ConsoleMessage
            | Self::LifecycleListenerError => 2,
            Self::InsertBefore | Self::SetAttribute => 3,
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
            let level = ConsoleLevel::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known console level",
            })?;
            page.diagnostics.record_console(level, text);
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
        DomOp::LifecycleListenerError => {
            let name = string_arg(op, args, 1, limits)?;
            let message = string_arg(op, args, 2, limits)?;
            let event = LifecycleEvent::parse(name).ok_or(DomBridgeError::TypeMismatch {
                operation: op.name(),
                index: 1,
                expected: "a known lifecycle event",
            })?;
            page.diagnostics.record_listener_error(event, message);
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
            Ok(JsValue::Undefined)
        }
        DomOp::RemoveAttribute => {
            let element = page.node_arg(op, args, 1)?;
            let name = string_arg(op, args, 2, limits)?;
            page.document.remove_attribute(element, name)?;
            Ok(JsValue::Undefined)
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
        call(&b, &[s("consoleMessage"), s("log"), s(&long)]).expect("ok");
        call(&b, &[s("consoleMessage"), s("trace"), s("x")]).expect_err("許可リスト外");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.console_messages.len(), 1);
        assert_eq!(d.console_messages[0].text.len(), 4095);
        assert!(d.console_messages[0].truncated);
        assert!(format!("{d:?}").len() < 400);
        for _ in 0..(MAX_BRIDGE_DIAGNOSTICS + 3) {
            call(&b, &[s("consoleMessage"), s("warn"), s("m")]).expect("ok");
        }
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.console_messages.len(), MAX_BRIDGE_DIAGNOSTICS);
        assert_eq!(d.dropped, 3);
        call(&b, &[s("consoleMessage"), s("info"), s("keep")]).expect("ok");
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
        call(&b, &[s("lifecycleListenerError"), s("load"), s("boom")]).expect("ok");
        call(&b, &[s("lifecycleListenerError"), s("click"), s("x")]).expect_err("許可リスト外");
        call(&b, &[s("lifecycleListenerError"), s("load")]).expect_err("arity");
        call(&b, &[s("lifecycleListenerError"), s("load"), n(1.0)]).expect_err("type");
        let d = b.take_diagnostics().expect("diag");
        assert_eq!(d.listener_errors.len(), 1);
        assert_eq!(d.listener_errors[0].event, LifecycleEvent::Load);
        assert_eq!(d.listener_errors[0].message, "boom");
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
    fn js_5_page_env_ops_check_arity_and_types() {
        let b = bridge();
        for (args, label) in [
            (vec![s("navigatorUserAgent"), s("x")], "ua arity"),
            (vec![s("getLocation")], "get arity"),
            (vec![s("getLocation"), n(1.0)], "get type"),
            (vec![s("getLocation"), s("nope")], "get member"),
            (vec![s("setLocationHash"), n(1.0)], "hash type"),
            (vec![s("ignoreLocationChange"), s("href")], "ignore arity"),
            (vec![s("consoleMessage"), s("log"), n(1.0)], "console type"),
        ] {
            call(&b, &args).expect_err(label);
        }
    }
}

//! `DomBridge` が保持するページ環境（`location`・診断）の型と純粋関数
//! （TASK-108・Issue #779・ビヘイビア `JS-5`・`SEC-2`）。
//!
//! 親の [`super`]（`dom_bridge.rs`）が `getLocation` / `setLocationHash` /
//! `ignoreLocationChange` / `consoleMessage` の各 op から使う。JS からの文字列は
//! すべて許可リスト enum か長さ上限で絞り、診断には URL 本文を保存しない
//! （クエリにトークンを含み得るため。security.md「秘密情報の混入防止」）。

use reqwest::Url;

use crate::exception_summary::{ExceptionSummary, summarize_named_exception};
use crate::page_net::{NetRejectKind, NetRejection, NetRequestKind};

/// 種別ごとの診断の保持上限（件数）。超過分は [`BridgeDiagnostics::dropped`] に数える。
pub const MAX_BRIDGE_DIAGNOSTICS: usize = 64;
/// `console` で記録するページ由来文字列の上限（UTF-8 バイト。文字境界で切り詰める）。
pub const MAX_CONSOLE_MESSAGE_BYTES: usize = 4096;

/// `set_location` が受け付ける URL の上限（UTF-8 バイト。解析・保持・複製のコストを
/// 抑えるため解析前に検証する。`SEC-2`）。
pub const MAX_LOCATION_URL_BYTES: usize = 8192;

/// `location` の読み取り対象（許可リスト。`JS-5`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocationMember {
    /// `location.href`。
    Href,
    /// `location.origin`。
    Origin,
    /// `location.protocol`。
    Protocol,
    /// `location.host`。
    Host,
    /// `location.hostname`。
    Hostname,
    /// `location.port`。
    Port,
    /// `location.pathname`。
    Pathname,
    /// `location.search`。
    Search,
    /// `location.hash`。
    Hash,
}

impl LocationMember {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "href" => Self::Href,
            "origin" => Self::Origin,
            "protocol" => Self::Protocol,
            "host" => Self::Host,
            "hostname" => Self::Hostname,
            "port" => Self::Port,
            "pathname" => Self::Pathname,
            "search" => Self::Search,
            "hash" => Self::Hash,
            _ => return None,
        })
    }
}

/// 実行せず診断に記録する `location` の遷移系操作の種別（許可リスト。`JS-5`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocationChangeKind {
    /// `location.href = ...`。
    Href,
    /// `location.protocol = ...`。
    Protocol,
    /// `location.host = ...`。
    Host,
    /// `location.hostname = ...`。
    Hostname,
    /// `location.port = ...`。
    Port,
    /// `location.pathname = ...`。
    Pathname,
    /// `location.search = ...`。
    Search,
    /// `location.origin = ...`（実ブラウザでも無効な代入）。
    Origin,
    /// `window.location = ...`。
    AssignLocation,
    /// `location.assign(...)`。
    Assign,
    /// `location.replace(...)`。
    Replace,
    /// `location.reload()`。
    Reload,
    /// `about:blank` 上の `location.hash = ...`（書き換え先が無い）。
    Hash,
    /// `history.back()` の移動を実行できなかった記録（microtask の登録失敗・URL の書き換え失敗。
    /// 移動できた場合や範囲外は記録しない。#1177・`REPAIR-3`）。
    HistoryBack,
    /// `history.forward()` の移動を実行できなかった記録（同上）。
    HistoryForward,
    /// `history.go(delta)` の記録（`go(0)` は再読み込みに当たるため実行せず常に記録する。移動を実行できなかった
    /// 場合も同様。値の長さは変換後の整数の文字列長）。
    HistoryGo,
}

impl LocationChangeKind {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "href" => Self::Href,
            "protocol" => Self::Protocol,
            "host" => Self::Host,
            "hostname" => Self::Hostname,
            "port" => Self::Port,
            "pathname" => Self::Pathname,
            "search" => Self::Search,
            "origin" => Self::Origin,
            "assignLocation" => Self::AssignLocation,
            "assign" => Self::Assign,
            "replace" => Self::Replace,
            "reload" => Self::Reload,
            "historyBack" => Self::HistoryBack,
            "historyForward" => Self::HistoryForward,
            "historyGo" => Self::HistoryGo,
            _ => return None,
        })
    }
}

/// `console` の記録対象レベル（許可リスト）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConsoleLevel {
    /// `console.log` 系。
    Log,
    /// `console.info`。
    Info,
    /// `console.warn`。
    Warn,
    /// `console.error`。
    Error,
    /// `console.debug`。
    Debug,
}

impl ConsoleLevel {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "log" => Self::Log,
            "info" => Self::Info,
            "warn" => Self::Warn,
            "error" => Self::Error,
            "debug" => Self::Debug,
            _ => return None,
        })
    }
}

/// ページのライフサイクルイベント（許可リスト。`JS-4`・TASK-109・Issue #781）。
///
/// ランナーが発火する `DOMContentLoaded` / `load` と、shim の `lifecycleListenerError` op が
/// 受け取るイベント名の検証に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// `document` / `window` の `DOMContentLoaded`。
    DomContentLoaded,
    /// `window` の `load`。
    Load,
}

impl LifecycleEvent {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "DOMContentLoaded" => Self::DomContentLoaded,
            "load" => Self::Load,
            _ => return None,
        })
    }

    /// JS 側のイベント名（shim のディスパッチャーへ渡す文字列）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DomContentLoaded => "DOMContentLoaded",
            Self::Load => "load",
        }
    }
}

/// ライフサイクルイベントのリスナーが投げた例外 1 件（[`MAX_CONSOLE_MESSAGE_BYTES`] で切り詰め済み）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenerError {
    /// 発火中のイベント。
    pub event: LifecycleEvent,
    /// 例外メッセージ（ページ由来。切り詰め後）。
    pub message: String,
    /// 切り詰めが起きたか。
    pub truncated: bool,
    /// 例外の種類・メッセージ型・不足識別子の要約（`TimerError::summary` と同じ分類。
    /// Issue #1172・`REPAIR-4`）。
    pub summary: ExceptionSummary,
    /// `error.stack` の先頭 [`MAX_EXCEPTION_STACK_BYTES`] バイト（空なら `None`）。非公開で、
    /// opt-in が無い限り page_runner が破棄する（Issue #1172・`SEC-2`）。
    stack: Option<String>,
}

impl ListenerError {
    /// 切り詰め済みの `error.stack`（opt-in が有効なときだけ値がある）。計測 example の stderr 専用で、
    /// JSONL・要約には出さない（Issue #1172・`REPAIR-4`）。
    #[doc(hidden)]
    pub fn raw_stack(&self) -> Option<&str> {
        self.stack.as_deref()
    }

    /// stack を破棄する（opt-in が無効なとき page_runner が呼ぶ。fail-closed）。
    pub(crate) fn clear_stack(&mut self) {
        self.stack = None;
    }
}

// ページ由来の文字列は秘密を含み得るため、Debug には長さと要約だけを出す。
impl std::fmt::Debug for ListenerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenerError")
            .field("event", &self.event)
            .field("message_len", &self.message.len())
            .field("summary", &self.summary)
            .field("stack_len", &self.stack.as_ref().map(String::len))
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// タイマー / microtask のコールバックの種別（`timerCallbackError` op の許可リスト。`JS-7`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimerCallbackSource {
    /// `setTimeout`。
    Timeout,
    /// `setInterval`。
    Interval,
    /// `queueMicrotask`。
    Microtask,
}

impl TimerCallbackSource {
    pub(super) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "timeout" => Self::Timeout,
            "interval" => Self::Interval,
            "microtask" => Self::Microtask,
            _ => return None,
        })
    }

    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Interval => "interval",
            Self::Microtask => "microtask",
        }
    }
}

/// `TimerError`・`ListenerError`・`NetCallbackError`・`ConsoleMessage` が保持する `error.stack` の上限バイト数
/// （Issue #1168・#1172・#1183）。
const MAX_EXCEPTION_STACK_BYTES: usize = 1024;

/// `error.stack` を上限まで切って保持用に整える（空なら `None`）。3 種の callback 例外で共用する。
fn clip_stack(stack: &str) -> Option<String> {
    (!stack.is_empty()).then(|| truncate_utf8(stack, MAX_EXCEPTION_STACK_BYTES).0.to_owned())
}

/// タイマー / microtask のコールバックが投げた例外 1 件（[`MAX_CONSOLE_MESSAGE_BYTES`] で切り詰め済み）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TimerError {
    /// 例外を投げたコールバックの種別。
    pub source: TimerCallbackSource,
    /// 例外メッセージ（ページ由来。切り詰め後）。
    pub message: String,
    /// 切り詰めが起きたか。
    pub truncated: bool,
    /// 例外の種類・メッセージ型・不足識別子の要約（生メッセージを含まない固定語彙。
    /// module 評価失敗と同じ分類。Issue #1156・`JS-7`・`REPAIR-4`）。
    pub summary: ExceptionSummary,
    /// `error.stack` の先頭 [`MAX_EXCEPTION_STACK_BYTES`] バイト（空なら `None`）。URL・本文を含み得るため
    /// 非公開で、`PageRunOptions` の opt-in が無い限り page_runner が破棄する（Issue #1168・`SEC-2`）。
    stack: Option<String>,
}

impl TimerError {
    /// 切り詰め済みの `error.stack`（opt-in が有効なときだけ値がある）。計測 example の stderr 専用で、
    /// JSONL・要約には出さない（Issue #1168・`REPAIR-4`）。
    #[doc(hidden)]
    pub fn raw_stack(&self) -> Option<&str> {
        self.stack.as_deref()
    }

    /// stack を破棄する（opt-in が無効なとき page_runner が呼ぶ。fail-closed）。
    pub(crate) fn clear_stack(&mut self) {
        self.stack = None;
    }
}

// ページ由来の文字列は秘密を含み得るため、Debug には長さと要約だけを出す。
impl std::fmt::Debug for TimerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerError")
            .field("source", &self.source)
            .field("message_len", &self.message.len())
            .field("summary", &self.summary)
            .field("stack_len", &self.stack.as_ref().map(String::len))
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// `fetch` / `XMLHttpRequest` のコールバックが投げた例外 1 件（[`MAX_CONSOLE_MESSAGE_BYTES`] で切り詰め済み。`JS-8`）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetCallbackError {
    /// 例外を投げたコールバックの発行元 API。
    pub request: NetRequestKind,
    /// 例外メッセージ（ページ由来。切り詰め後）。
    pub message: String,
    /// 切り詰めが起きたか。
    pub truncated: bool,
    /// 例外の種類・メッセージ型・不足識別子の要約（`TimerError::summary` と同じ分類。
    /// Issue #1172・`REPAIR-4`）。
    pub summary: ExceptionSummary,
    /// `error.stack` の先頭 [`MAX_EXCEPTION_STACK_BYTES`] バイト（空なら `None`）。非公開で、
    /// opt-in が無い限り page_runner が破棄する（Issue #1172・`SEC-2`）。
    stack: Option<String>,
}

impl NetCallbackError {
    /// 切り詰め済みの `error.stack`（opt-in が有効なときだけ値がある）。計測 example の stderr 専用で、
    /// JSONL・要約には出さない（Issue #1172・`REPAIR-4`）。
    #[doc(hidden)]
    pub fn raw_stack(&self) -> Option<&str> {
        self.stack.as_deref()
    }

    /// stack を破棄する（opt-in が無効なとき page_runner が呼ぶ。fail-closed）。
    pub(crate) fn clear_stack(&mut self) {
        self.stack = None;
    }
}

// ページ由来の文字列は秘密を含み得るため、Debug には長さと要約だけを出す。
impl std::fmt::Debug for NetCallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetCallbackError")
            .field("request", &self.request)
            .field("message_len", &self.message.len())
            .field("summary", &self.summary)
            .field("stack_len", &self.stack.as_ref().map(String::len))
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// 無視した `location` の遷移系操作 1 件。URL 本文は持たず、種別と長さだけを残す。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct IgnoredLocationChange {
    /// 操作の種別。
    pub kind: LocationChangeKind,
    /// 代入された値のバイト長（本文は保存しない）。
    pub value_len: usize,
}

/// `console` が受け取ったメッセージ 1 件（[`MAX_CONSOLE_MESSAGE_BYTES`] で切り詰め済み）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsoleMessage {
    /// レベル。
    pub level: ConsoleLevel,
    /// ページ由来の文字列（切り詰め後）。
    pub text: String,
    /// 切り詰めが起きたか。
    pub truncated: bool,
    /// console.error / warn に渡された Error の `stack` 先頭 [`MAX_EXCEPTION_STACK_BYTES`] バイト
    /// （空なら `None`）。URL・本文を含み得るため非公開で、opt-in が無い限り bridge が保存せず、
    /// page_runner も破棄する。ページが偽造し得る診断専用の値（Issue #1183・`SEC-2`）。
    stack: Option<String>,
}

impl ConsoleMessage {
    /// 切り詰め済みの `error.stack`（opt-in が有効なときだけ値がある）。計測 example の stderr 専用で、
    /// JSONL・要約には出さない（Issue #1183・`REPAIR-4`）。
    #[doc(hidden)]
    pub fn raw_stack(&self) -> Option<&str> {
        self.stack.as_deref()
    }

    /// stack を破棄する（opt-in が無効なとき page_runner が呼ぶ。fail-closed）。
    pub(crate) fn clear_stack(&mut self) {
        self.stack = None;
    }
}

// ページ由来の文字列は秘密を含み得るため、Debug には長さだけを出す。
impl std::fmt::Debug for ConsoleMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleMessage")
            .field("level", &self.level)
            .field("text_len", &self.text.len())
            .field("truncated", &self.truncated)
            .field("stack_len", &self.stack.as_ref().map(String::len))
            .finish()
    }
}

/// ページ実行中にブリッジが集めた診断（`DomBridge::take_diagnostics` で回収する。REPAIR-4）。
///
/// `attach` ごとに空へ戻り、ページ間で持ち越さない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BridgeDiagnostics {
    /// 実行せず無視した `location` の遷移系操作（先頭から [`MAX_BRIDGE_DIAGNOSTICS`] 件）。
    pub location_changes_ignored: Vec<IgnoredLocationChange>,
    /// `console` のメッセージ（先頭から [`MAX_BRIDGE_DIAGNOSTICS`] 件）。
    pub console_messages: Vec<ConsoleMessage>,
    /// ライフサイクルイベントのリスナー例外（先頭から [`MAX_BRIDGE_DIAGNOSTICS`] 件）。
    pub listener_errors: Vec<ListenerError>,
    /// ライフサイクルのリスナー保持件数が shim の上限を超え、登録を無視したか（`JS-6`）。
    pub listener_limit_exceeded: bool,
    /// 保留中の microtask 数が shim の上限を超え、登録を無視したか（`JS-6`）。
    pub microtask_limit_exceeded: bool,
    /// MutationObserver の保留レコード数または登録数が shim の上限を超え、破棄したか（`JS-6`）。
    pub mutation_observer_limit_exceeded: bool,
    /// `MutationObserver.observe` の options のうち実装せず無視したもの
    /// （`attributeOldValue`・`characterDataOldValue`・`attributeFilter`）の件数（`REPAIR-3`）。
    pub mutation_options_ignored: u64,
    /// タイマー / microtask のコールバック例外（先頭から [`MAX_BRIDGE_DIAGNOSTICS`] 件。`JS-7`）。
    pub timer_errors: Vec<TimerError>,
    /// 文字列ハンドラ（`setTimeout("code", ...)`）として渡され、評価せず無視した件数（`JS-7`）。
    pub timer_string_handlers_ignored: u64,
    /// reject したページ内 fetch / XHR（先頭から [`MAX_BRIDGE_DIAGNOSTICS`] 件。URL は持たない。`JS-8`・`JS-6`）。
    pub net_rejections: Vec<NetRejection>,
    /// fetch / XHR のコールバック例外（先頭から [`MAX_BRIDGE_DIAGNOSTICS`] 件。`JS-8`）。
    pub net_callback_errors: Vec<NetCallbackError>,
    /// 累計レスポンス本文バイト数の上限に達したか（`JS-6`）。
    pub net_limit_exceeded: bool,
    /// `fetch` の init で無視したオプション（headers / credentials 等）の件数（`JS-8`）。
    pub net_options_ignored: u64,
    /// 件数上限で保存しなかった診断の数。
    pub dropped: u64,
}

impl BridgeDiagnostics {
    pub(super) fn record_location_change(&mut self, kind: LocationChangeKind, value_len: usize) {
        if self.location_changes_ignored.len() < MAX_BRIDGE_DIAGNOSTICS {
            self.location_changes_ignored
                .push(IgnoredLocationChange { kind, value_len });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub(super) fn record_listener_error(
        &mut self,
        event: LifecycleEvent,
        name: &str,
        message: &str,
        stack: &str,
    ) {
        if self.listener_errors.len() < MAX_BRIDGE_DIAGNOSTICS {
            let (kept, truncated) = truncate_utf8(message, MAX_CONSOLE_MESSAGE_BYTES);
            self.listener_errors.push(ListenerError {
                event,
                message: kept.to_owned(),
                truncated,
                summary: summarize_named_exception(name, message),
                stack: clip_stack(stack),
            });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub(super) fn record_timer_error(
        &mut self,
        source: TimerCallbackSource,
        name: &str,
        message: &str,
        stack: &str,
    ) {
        if self.timer_errors.len() < MAX_BRIDGE_DIAGNOSTICS {
            let (kept, truncated) = truncate_utf8(message, MAX_CONSOLE_MESSAGE_BYTES);
            self.timer_errors.push(TimerError {
                source,
                message: kept.to_owned(),
                truncated,
                summary: summarize_named_exception(name, message),
                stack: clip_stack(stack),
            });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub(super) fn record_net_rejection(&mut self, request: NetRequestKind, kind: NetRejectKind) {
        if matches!(kind, NetRejectKind::ResponseBytesLimit) {
            self.net_limit_exceeded = true;
        }
        if self.net_rejections.len() < MAX_BRIDGE_DIAGNOSTICS {
            self.net_rejections.push(NetRejection { request, kind });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub(super) fn record_net_callback_error(
        &mut self,
        request: NetRequestKind,
        name: &str,
        message: &str,
        stack: &str,
    ) {
        if self.net_callback_errors.len() < MAX_BRIDGE_DIAGNOSTICS {
            let (kept, truncated) = truncate_utf8(message, MAX_CONSOLE_MESSAGE_BYTES);
            self.net_callback_errors.push(NetCallbackError {
                request,
                message: kept.to_owned(),
                truncated,
                summary: summarize_named_exception(name, message),
                stack: clip_stack(stack),
            });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    pub(super) fn record_console(&mut self, level: ConsoleLevel, text: &str, stack: &str) {
        if self.console_messages.len() < MAX_BRIDGE_DIAGNOSTICS {
            let (kept, truncated) = truncate_utf8(text, MAX_CONSOLE_MESSAGE_BYTES);
            self.console_messages.push(ConsoleMessage {
                level,
                text: kept.to_owned(),
                truncated,
                stack: clip_stack(stack),
            });
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}

/// `s` を UTF-8 の文字境界を守って `max` バイト以下に切り詰める（切り詰めたら `true`）。
pub(crate) fn truncate_utf8(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s.get(..end).unwrap_or(""), true)
}

/// `set_location` の入力を検証して正規化する（http / https のみ。userinfo を除去）。
/// 失敗理由は静的文言で、入力 URL は反響しない。
pub(crate) fn parse_location(input: &str) -> Result<Url, &'static str> {
    // 解析（アロケーション）の前に長さを検証する。
    if input.len() > MAX_LOCATION_URL_BYTES {
        return Err("URL is too long");
    }
    let mut url = Url::parse(input).map_err(|_| "not a valid absolute URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("scheme must be http or https");
    }
    if url.host_str().is_none() {
        return Err("URL has no host");
    }
    // userinfo は公開値から常に取り除く（fetch の final_url と同じ扱い）。
    let _ = url.set_username("");
    let _ = url.set_password(None);
    // 正規化（パーセントエンコード等）で伸びた後の長さにも上限を適用する。
    if url.as_str().len() > MAX_LOCATION_URL_BYTES {
        return Err("URL is too long");
    }
    Ok(url)
}

/// `history.pushState` / `replaceState` の URL 引数を、現在の URL と同一オリジンの場合だけ
/// 受理して新しい `location` を返す（#1171・`JS-5`・`SEC-2`）。
///
/// 同一オリジンの検査はページ JS ではなくこの関数（Rust 側）で行う。`__dom.op` はページから
/// 直接呼べるため、JS 側の検査には頼れない。拒否理由と入力 URL は返さない（反響防止）。
/// `current` が `None`（`about:blank`。opaque origin）なら常に拒否する（簡略化。`REPAIR-3`）。
/// userinfo を持つ URL は黙って除去せず拒否する。
pub(super) fn rewrite_same_origin(current: Option<&Url>, input: &str) -> Option<Url> {
    let current = current?;
    // 解析（アロケーション）の前に長さを検証する。
    if input.len() > MAX_LOCATION_URL_BYTES {
        return None;
    }
    let joined = current.join(input).ok()?;
    if !joined.username().is_empty() || joined.password().is_some() {
        return None;
    }
    let normalized = parse_location(joined.as_str()).ok()?;
    (normalized.origin() == current.origin()).then_some(normalized)
}

/// `location` の `member` を WHATWG の形で文字列化する（`None` は `about:blank`）。
pub(super) fn location_member(url: Option<&Url>, member: LocationMember) -> String {
    let Some(url) = url else {
        return match member {
            LocationMember::Href => "about:blank",
            LocationMember::Origin => "null",
            LocationMember::Protocol => "about:",
            LocationMember::Pathname => "blank",
            _ => "",
        }
        .to_owned();
    };
    match member {
        LocationMember::Href => url.as_str().to_owned(),
        LocationMember::Origin => url.origin().ascii_serialization(),
        LocationMember::Protocol => format!("{}:", url.scheme()),
        LocationMember::Host => {
            let host = url.host_str().unwrap_or("");
            match url.port() {
                Some(p) => format!("{host}:{p}"),
                None => host.to_owned(),
            }
        }
        LocationMember::Hostname => url.host_str().unwrap_or("").to_owned(),
        LocationMember::Port => url.port().map(|p| p.to_string()).unwrap_or_default(),
        LocationMember::Pathname => url.path().to_owned(),
        LocationMember::Search => match url.query() {
            Some(q) if !q.is_empty() => format!("?{q}"),
            _ => String::new(),
        },
        LocationMember::Hash => match url.fragment() {
            Some(f) if !f.is_empty() => format!("#{f}"),
            _ => String::new(),
        },
    }
}

/// `location.hash = value` を適用する（先頭の `#` を 1 つ剥がし、空なら fragment を消す）。
/// 結果の URL が [`MAX_LOCATION_URL_BYTES`] を超え得る場合は適用せず `false` を返す。
pub(super) fn apply_hash(url: &mut Url, value: &str) -> bool {
    let v = value.strip_prefix('#').unwrap_or(value);
    if v.is_empty() {
        url.set_fragment(None);
        return true;
    }
    let base_len = url.as_str().len() - url.fragment().map_or(0, |f| f.len() + 1);
    // パーセントエンコードで最大 3 倍に膨らみ得るため、保守的に見積もる。
    if base_len.saturating_add(1 + v.len().saturating_mul(3)) > MAX_LOCATION_URL_BYTES {
        return false;
    }
    url.set_fragment(Some(v));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_5_truncate_utf8_respects_char_boundary() {
        let s = "あ".repeat(2000); // 6000 バイト
        let (kept, truncated) = truncate_utf8(&s, MAX_CONSOLE_MESSAGE_BYTES);
        assert!(truncated);
        assert_eq!(kept.len(), 4095);
        assert_eq!(truncate_utf8("abc", 3), ("abc", false));
        assert_eq!(truncate_utf8("あ", 2), ("", true));
    }

    #[test]
    fn js_4_lifecycle_event_allow_list() {
        assert_eq!(
            LifecycleEvent::parse("DOMContentLoaded"),
            Some(LifecycleEvent::DomContentLoaded)
        );
        assert_eq!(LifecycleEvent::parse("load"), Some(LifecycleEvent::Load));
        assert_eq!(LifecycleEvent::parse("click"), None);
        assert_eq!(LifecycleEvent::parse("Load"), None);
    }

    #[test]
    fn js_4_listener_errors_truncate_and_cap() {
        let mut d = BridgeDiagnostics::default();
        d.record_listener_error(LifecycleEvent::Load, "", &"x".repeat(4096), "");
        d.record_listener_error(LifecycleEvent::Load, "", &"x".repeat(4097), "");
        assert!(!d.listener_errors[0].truncated);
        assert_eq!(d.listener_errors[1].message.len(), 4096);
        assert!(d.listener_errors[1].truncated);
        for _ in 0..(MAX_BRIDGE_DIAGNOSTICS + 3) {
            d.record_listener_error(LifecycleEvent::DomContentLoaded, "", "e", "");
        }
        assert_eq!(d.listener_errors.len(), MAX_BRIDGE_DIAGNOSTICS);
        assert_eq!(d.dropped, 5);
    }

    #[test]
    fn js_5_parse_location_rejects_non_http_without_echo() {
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "ftp://h/",
            "about:blank",
            "not a url",
        ] {
            let e = parse_location(bad).expect_err(bad);
            assert!(!e.contains("passwd") && !e.contains("alert"), "{e}");
        }
        let url = parse_location("https://user:pw@example.com:8443/a/b?q=1#f").expect("ok");
        assert_eq!(url.as_str(), "https://example.com:8443/a/b?q=1#f");
    }

    #[test]
    fn js_5_parse_location_limits_length_before_parsing() {
        let prefix = "https://example.com/";
        let at_limit = format!(
            "{prefix}{}",
            "a".repeat(MAX_LOCATION_URL_BYTES - prefix.len())
        );
        assert_eq!(at_limit.len(), MAX_LOCATION_URL_BYTES);
        assert!(parse_location(&at_limit).is_ok());
        let over = format!("{at_limit}a");
        let e = parse_location(&over).expect_err("over");
        assert_eq!(e, "URL is too long");
    }

    #[test]
    fn js_5_parse_location_limits_length_after_normalization() {
        // 入力は 8192 バイト以下だが、パーセントエンコードで正規化後に上限を超える。
        let input = format!("https://example.com/{}", "あ".repeat(2000));
        assert!(input.len() <= MAX_LOCATION_URL_BYTES);
        let e = parse_location(&input).expect_err("normalized too long");
        assert_eq!(e, "URL is too long");
    }

    fn same_origin(current: &str, input: &str) -> Option<String> {
        let cur = Url::parse(current).expect("current");
        rewrite_same_origin(Some(&cur), input).map(|u| u.to_string())
    }

    #[test]
    fn js_5_rewrite_same_origin_accepts_relative_and_same_origin_urls() {
        let cur = "https://example.com:8443/a/b?q=1#f";
        let cases = [
            ("c?x=1", "https://example.com:8443/a/c?x=1"),
            ("/p/q#h", "https://example.com:8443/p/q#h"),
            ("?z=2", "https://example.com:8443/a/b?z=2"),
            ("#only", "https://example.com:8443/a/b?q=1#only"),
            ("", "https://example.com:8443/a/b?q=1"),
            (
                "https://example.com:8443/abs?k=v",
                "https://example.com:8443/abs?k=v",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(same_origin(cur, input).as_deref(), Some(want), "{input}");
        }
    }

    #[test]
    fn js_5_rewrite_same_origin_rejects_cross_origin_and_unsafe_urls() {
        let cur = "https://example.com:8443/a/b";
        for input in [
            "https://evil.test/",
            "https://example.com:9000/",
            "https://example.com/",
            "http://example.com:8443/",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,x",
            "https://user:pw@example.com:8443/",
            "https://user@example.com:8443/",
        ] {
            assert_eq!(same_origin(cur, input), None, "{input}");
        }
    }

    #[test]
    fn js_5_rewrite_same_origin_rejects_about_blank_and_oversized_input() {
        assert!(rewrite_same_origin(None, "/x").is_none());
        let cur = Url::parse("https://example.com/").expect("current");
        let long = format!("/{}", "a".repeat(MAX_LOCATION_URL_BYTES));
        assert!(rewrite_same_origin(Some(&cur), &long).is_none());
        // 正規化（パーセントエンコード）で上限を超える入力も拒否する。
        let grow = format!("/{}", " a".repeat(2500));
        assert!(rewrite_same_origin(Some(&cur), &grow).is_none());
    }

    #[test]
    fn js_5_history_location_change_kinds_parse() {
        assert_eq!(
            LocationChangeKind::parse("historyBack"),
            Some(LocationChangeKind::HistoryBack)
        );
        assert_eq!(
            LocationChangeKind::parse("historyForward"),
            Some(LocationChangeKind::HistoryForward)
        );
        assert_eq!(
            LocationChangeKind::parse("historyGo"),
            Some(LocationChangeKind::HistoryGo)
        );
    }

    #[test]
    fn js_5_apply_hash_rejects_oversized_result() {
        let mut u = parse_location("https://example.com/").expect("ok");
        assert!(!apply_hash(&mut u, &"h".repeat(MAX_LOCATION_URL_BYTES)));
        assert_eq!(u.as_str(), "https://example.com/");
        assert!(apply_hash(&mut u, &"h".repeat(1000)));
        assert_eq!(u.fragment().map(str::len), Some(1000));
    }

    #[test]
    fn js_5_location_members_follow_whatwg_shape() {
        let u = parse_location("https://example.com/").expect("ok");
        assert_eq!(location_member(Some(&u), LocationMember::Port), "");
        assert_eq!(location_member(Some(&u), LocationMember::Search), "");
        assert_eq!(location_member(Some(&u), LocationMember::Hash), "");
        assert_eq!(
            location_member(Some(&u), LocationMember::Host),
            "example.com"
        );
        let u = parse_location("http://example.com:8080/p?x=1#h").expect("ok");
        assert_eq!(
            location_member(Some(&u), LocationMember::Origin),
            "http://example.com:8080"
        );
        assert_eq!(
            location_member(Some(&u), LocationMember::Host),
            "example.com:8080"
        );
        assert_eq!(location_member(Some(&u), LocationMember::Protocol), "http:");
        assert_eq!(location_member(None, LocationMember::Origin), "null");
        assert_eq!(location_member(None, LocationMember::Pathname), "blank");
    }
}

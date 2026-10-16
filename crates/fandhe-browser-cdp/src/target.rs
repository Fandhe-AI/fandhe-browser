//! CDP のターゲット表・セッション表と ID 型（TASK-41（41.2）・#170、ビヘイビア `CDP-1`・MS-3）。
//!
//! 本モジュールは cdp crate 固有の状態（core の `AppState` とは別物）のうち、
//! 「どのターゲットが存在し、どのセッションがどのターゲットへ attach しているか」を
//! 保持する表本体を担う。`server` モジュールの `CdpState` が 1 つ内包し、
//! `/json/list`（TASK-41.3）・`/devtools/browser/{id}`（TASK-41.4）・`Target.*` ハンドラ
//! （TASK-42）から呼ばれる想定。HTTP・WebSocket・非同期ランタイムの型は含めない。
//!
//! # 設計上の注意
//!
//! - ID は識別子であって認証情報ではない。アクセス制御は `SEC-4`（ローカルホスト限定
//!   バインド等）と TASK-41.4・41.5 の責務であり、ID の推測困難性には依存しない。
//! - 全メソッドは同期で、ロックのガードを返さない（await をまたぐ保持を起こさない）。
//!   各メソッドは検証をすべて済ませてから表を変更するため、失敗時に表は不変。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::conn_state::DownloadSettings;

/// 同時に保持できるターゲット数の上限（無制限確保による DoS の防止）。
pub const MAX_TARGETS: usize = 64;
/// 既定 context を除く、同時に保持できる browser context 数の上限（`CDP-7`。無制限確保の防止）。
pub const MAX_BROWSER_CONTEXTS: usize = 64;
/// 同時に保持できるセッション数の上限。
pub const MAX_SESSIONS: usize = 256;
/// 外部入力から ID を組み立てる際の最大バイト長。
const MAX_ID_LEN: usize = 64;
/// ターゲット URL の最大バイト長（巨大 URL によるメモリ無制限消費の防止）。
pub const MAX_URL_LEN: usize = 8192;

/// cdp 状態型の操作エラー。表は失敗時に変更されない。
///
/// メッセージは英語で、入力 ID・URL を含めない（`security.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CdpStateError {
    /// 指定 ID のターゲットが存在しない。
    UnknownTarget,
    /// 指定 ID のセッションが存在しない。
    UnknownSession,
    /// ターゲット数が上限に達している。
    TooManyTargets {
        /// 上限値。
        limit: usize,
    },
    /// セッション数が上限に達している。
    TooManySessions {
        /// 上限値。
        limit: usize,
    },
    /// ID 用カウンタが枯渇した（wrap はしない）。
    IdExhausted,
    /// 外部入力の ID が空・長すぎる・許可外の文字を含む。
    InvalidId,
    /// ターゲット URL がバイト長の上限（[`MAX_URL_LEN`]）を超えている。
    UrlTooLong {
        /// 上限値（バイト）。
        limit: usize,
    },
    /// 指定 ID の browser context が存在しない。
    UnknownBrowserContext,
    /// browser context 数が上限（[`MAX_BROWSER_CONTEXTS`]）に達している。
    TooManyBrowserContexts {
        /// 上限値。
        limit: usize,
    },
    /// 既定 browser context は破棄できない。
    DefaultBrowserContext,
}

impl fmt::Display for CdpStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTarget => f.write_str("unknown target"),
            Self::UnknownSession => f.write_str("unknown session"),
            Self::TooManyTargets { limit } => write!(f, "too many targets (limit {limit})"),
            Self::TooManySessions { limit } => write!(f, "too many sessions (limit {limit})"),
            Self::IdExhausted => f.write_str("id counter exhausted"),
            Self::InvalidId => f.write_str("invalid id"),
            Self::UrlTooLong { limit } => write!(f, "url too long (limit {limit} bytes)"),
            Self::UnknownBrowserContext => f.write_str("unknown browser context"),
            Self::TooManyBrowserContexts { limit } => {
                write!(f, "too many browser contexts (limit {limit})")
            }
            Self::DefaultBrowserContext => {
                f.write_str("default browser context cannot be disposed")
            }
        }
    }
}

impl std::error::Error for CdpStateError {}

/// 外部入力の ID 文字列を検証する（長さ・文字種）。
fn validate_id(s: &str) -> Result<(), CdpStateError> {
    if s.is_empty() || s.len() > MAX_ID_LEN {
        return Err(CdpStateError::InvalidId);
    }
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(())
    } else {
        Err(CdpStateError::InvalidId)
    }
}

/// カウンタ値を Chrome の targetId と同じ見た目（32 桁大文字 16 進）にする。
fn format_id(n: u64) -> String {
    format!("{n:032X}")
}

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            /// 外部入力（URL パス・CDP メッセージ）の文字列から ID を組み立てる。
            ///
            /// 空・65 バイト以上・ASCII 英数字と `-` 以外を含む場合は
            /// [`CdpStateError::InvalidId`]。比較は大文字小文字を区別する。
            pub fn parse(s: &str) -> Result<Self, CdpStateError> {
                validate_id(s)?;
                Ok(Self(s.to_owned()))
            }

            /// ID の文字列表現。
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub(crate) fn from_counter(n: u64) -> Self {
                Self(format_id(n))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// ブラウザ（`/devtools/browser/{id}` の `{id}`）の識別子。`CdpState` が保持する。
    ///
    /// 識別子であって認証情報ではない（`SEC-4` は TASK-41.4・41.5 の責務）。
    BrowserId
);
id_type!(
    /// browser context（シークレットウィンドウ相当の分離単位）の識別子。`CDP-2`。
    ///
    /// 名前空間は [`TargetId`] 等とは別で、桁・書式が同じためカウンタ値が一致すれば文字列も一致し得るが、
    /// 機能上の意味は持たない。ブラウザ ID との衝突は、`CdpState::new` がブラウザ ID を別名前空間
    /// （最上位ビット）で払い出すため起きず、型も別なので `Target.createTarget`〔#910〕側の対応は不要。
    BrowserContextId
);
id_type!(
    /// ターゲット（ページ等）の識別子。`CDP-1`。
    TargetId
);
id_type!(
    /// CDP セッション（attach 単位）の識別子。`CDP-1`。
    SessionId
);

/// ターゲットの種別。CDP の `targetInfo.type` に対応する。
///
/// 現状は `Page` のみ。将来 `Browser`・`ServiceWorker` 等を追加する（TASK-42、`CDP-1`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetKind {
    /// ページ。
    Page,
}

impl TargetKind {
    /// CDP 上の文字列表現（`"page"`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Page => "page",
        }
    }
}

/// ターゲット 1 件の情報。`/json/list`（TASK-41.3）等が読み取る。
///
/// `Debug` は URL にトークンを含み得るため長さのみ出力する。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TargetInfo {
    target_id: TargetId,
    kind: TargetKind,
    url: String,
    browser_context_id: BrowserContextId,
}

impl TargetInfo {
    /// ターゲット ID。
    pub fn target_id(&self) -> &TargetId {
        &self.target_id
    }

    /// 種別。
    pub fn kind(&self) -> TargetKind {
        self.kind
    }

    /// 現在の URL（保持のみで解釈しない）。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 所属する browser context の ID（`targetInfo.browserContextId`。`CDP-2`）。
    pub fn browser_context_id(&self) -> &BrowserContextId {
        &self.browser_context_id
    }
}

impl fmt::Debug for TargetInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetInfo")
            .field("target_id", &self.target_id)
            .field("kind", &self.kind)
            .field("browser_context_id", &self.browser_context_id)
            .field("url_len", &self.url.len())
            .finish()
    }
}

/// browser context 1 件の状態。
#[derive(Debug, Default)]
struct ContextEntry {
    /// context 単位のダウンロード設定（保持のみ。実行系は未実装で `REPAIR-3`）。
    /// context の破棄で一緒に消える。
    download: Option<DownloadSettings>,
}

/// セッション単位で有効化できるドメイン（43.3j・#1038・`CDP-2`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDomain {
    Log,
    Runtime,
    Network,
}

/// ドメインごとの有効フラグ。イベント送出は未実装で、現時点では印を持つだけ（`REPAIR-3`）。
#[derive(Debug, Clone, Copy, Default)]
struct SessionDomains {
    log: bool,
    runtime: bool,
    network: bool,
}

impl SessionDomains {
    fn slot(&mut self, d: SessionDomain) -> &mut bool {
        match d {
            SessionDomain::Log => &mut self.log,
            SessionDomain::Runtime => &mut self.runtime,
            SessionDomain::Network => &mut self.network,
        }
    }
}

#[derive(Debug)]
struct Tables {
    targets: HashMap<TargetId, TargetInfo>,
    sessions: HashMap<SessionId, TargetId>,
    /// 既定 context を含む browser context 表。
    contexts: HashMap<BrowserContextId, ContextEntry>,
    default_context: BrowserContextId,
    /// `waitForDebuggerOnStart` で止めたターゲットと、止めた（再開を待つ）セッション（#910・`CDP-2`）。
    /// ターゲットの閉鎖・context の破棄で消える。件数はターゲット数以下。
    paused: HashMap<TargetId, SessionId>,
    /// `Page.enable` 済みのセッション（43.3i・#1037・`CDP-2`）。`Page.lifecycleEvent` 送出ゲートの片方
    /// （[`TargetRegistry::lifecycle_events_active`]。43.3k・#1039）。
    /// 寿命は `sessions` と同じで、削除と同じロック内で消す。件数は `MAX_SESSIONS` 以下（`CDP-7`）。
    page_enabled: HashSet<SessionId>,
    /// `Page.setLifecycleEventsEnabled{enabled:true}` 済みのセッション（43.3k・#1039・`CDP-2`）。
    /// 寿命は `sessions` と同じで削除と同じロック内で消す。件数は `MAX_SESSIONS` 以下（`CDP-7`）。
    lifecycle_events_enabled: HashSet<SessionId>,
    /// `Log.enable`・`Runtime.enable`・`Network.enable` 済みのセッション（43.3j・#1038・`CDP-2`）。
    /// 寿命は `sessions` と同じで削除と同じロック内で消す。件数は `MAX_SESSIONS` 以下（`CDP-7`）。
    domains_enabled: HashMap<SessionId, SessionDomains>,
    /// `Emulation.setFocusEmulationEnabled`・`setEmulatedMedia` で受理した検証済みの値（43.3m・#1041・`CDP-2`）。
    /// どの API からも読まない（読み取りは `#[cfg(test)]` のみ）。寿命は `sessions` と同じで削除と同じロック内で
    /// 消す。件数は `MAX_SESSIONS` 以下（`CDP-7`）。
    emulation: HashMap<SessionId, SessionEmulation>,
    next_target: u64,
    next_session: u64,
    next_context: u64,
}

/// セッションごとの Emulation 保持値（43.3m・#1041・`CDP-2`）。
///
/// `Emulation.setFocusEmulationEnabled`・`setEmulatedMedia`（Playwright 既定値のみ）の検証済みの値を持つだけで、
/// どの API も読まない。画面・UA・locale・タイムゾーンは含めない（`SEC-2`・`CDP-6`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionEmulation {
    /// `setFocusEmulationEnabled` の直近の値。
    pub(crate) focus: Option<bool>,
    /// `setEmulatedMedia` で既定値を受理済みか。
    pub(crate) media_default: bool,
}

/// ターゲット表とセッション表。単一 `Mutex` で保護し、
/// 「未知ターゲットへの attach 失敗」「close 時の配下セッション一括削除」を原子的にする。
#[derive(Debug)]
pub struct TargetRegistry {
    inner: Mutex<Tables>,
}

impl Default for TargetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TargetRegistry {
    /// 空の表を作る。
    pub fn new() -> Self {
        Self::with_counters(1, 1, 2)
    }

    /// `next_context` は既定 context（カウンタ 1 固定）の次から払い出す値。
    fn with_counters(next_target: u64, next_session: u64, next_context: u64) -> Self {
        let default_context = BrowserContextId::from_counter(1);
        let mut contexts = HashMap::new();
        contexts.insert(default_context.clone(), ContextEntry::default());
        Self {
            inner: Mutex::new(Tables {
                targets: HashMap::new(),
                sessions: HashMap::new(),
                contexts,
                default_context,
                paused: HashMap::new(),
                page_enabled: HashSet::new(),
                lifecycle_events_enabled: HashSet::new(),
                domains_enabled: HashMap::new(),
                emulation: HashMap::new(),
                next_target,
                next_session,
                next_context,
            }),
        }
    }

    /// poison は回復して使う（ライブラリで panic しない）。
    fn lock(&self) -> MutexGuard<'_, Tables> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 既定 browser context にターゲットを作成して ID を返す。件数・URL 長の上限超過・ID 枯渇時は
    /// 表を変更しない。
    pub fn create_target(&self, kind: TargetKind, url: &str) -> Result<TargetId, CdpStateError> {
        self.create_target_impl(None, kind, url)
    }

    /// 指定 browser context にターゲットを作成する（`Target.createTarget`〔#910〕が接続スコープ経由で呼ぶ）。
    /// 未知の context は [`CdpStateError::UnknownBrowserContext`]。上限は全 context 共通で、
    /// 失敗時は表を変更しない。
    pub fn create_target_in(
        &self,
        context: &BrowserContextId,
        kind: TargetKind,
        url: &str,
    ) -> Result<TargetId, CdpStateError> {
        self.create_target_impl(Some(context), kind, url)
    }

    fn create_target_impl(
        &self,
        context: Option<&BrowserContextId>,
        kind: TargetKind,
        url: &str,
    ) -> Result<TargetId, CdpStateError> {
        // 借用したまま長さを検証し、超過入力は所有 String へ複製する前に拒否する（巨大入力による DoS 対策）。
        if url.len() > MAX_URL_LEN {
            return Err(CdpStateError::UrlTooLong { limit: MAX_URL_LEN });
        }
        let url = url.to_owned();
        let mut t = self.lock();
        let context = match context {
            Some(c) if t.contexts.contains_key(c) => c.clone(),
            Some(_) => return Err(CdpStateError::UnknownBrowserContext),
            None => t.default_context.clone(),
        };
        if t.targets.len() >= MAX_TARGETS {
            return Err(CdpStateError::TooManyTargets { limit: MAX_TARGETS });
        }
        let n = t.next_target;
        let next = n.checked_add(1).ok_or(CdpStateError::IdExhausted)?;
        let id = TargetId::from_counter(n);
        let info = TargetInfo {
            target_id: id.clone(),
            kind,
            url,
            browser_context_id: context,
        };
        t.next_target = next;
        t.targets.insert(id.clone(), info);
        Ok(id)
    }

    /// ターゲット情報を返す。
    pub fn target(&self, id: &TargetId) -> Option<TargetInfo> {
        self.lock().targets.get(id).cloned()
    }

    /// ターゲット情報と、セッションが 1 つ以上 attach 中か（`Target.getTargetInfo` の `attached`）を
    /// 1 回のロックで返す。
    pub(crate) fn target_with_attached(&self, id: &TargetId) -> Option<(TargetInfo, bool)> {
        let t = self.lock();
        let info = t.targets.get(id)?.clone();
        let attached = t.sessions.values().any(|tid| tid == id);
        Some((info, attached))
    }

    /// 全ターゲットを ID 昇順（= 作成順）で返す。
    pub fn targets(&self) -> Vec<TargetInfo> {
        let mut v: Vec<TargetInfo> = self.lock().targets.values().cloned().collect();
        v.sort_by(|a, b| a.target_id.as_str().cmp(b.target_id.as_str()));
        v
    }

    /// ターゲットの現在 URL を更新する（`Page.navigate` の成功後に cdp の page ハンドラから呼ぶ。
    /// `/json/list` の URL と共有状態の整合を保つ。`CDP-1`）。
    ///
    /// 未知ターゲット・URL 長超過では表を変更しない。
    pub fn set_target_url(&self, id: &TargetId, url: &str) -> Result<(), CdpStateError> {
        if url.len() > MAX_URL_LEN {
            return Err(CdpStateError::UrlTooLong { limit: MAX_URL_LEN });
        }
        let mut t = self.lock();
        let info = t.targets.get_mut(id).ok_or(CdpStateError::UnknownTarget)?;
        info.url = url.to_owned();
        Ok(())
    }

    /// ターゲットを所有判定なしで閉じ、配下セッションを削除して、その SessionId を昇順で返す。
    /// 接続の権限を確認する経路（`Target.closeTarget`）は [`Self::close_target_in_contexts`] を使う。
    pub fn close_target(&self, id: &TargetId) -> Result<Vec<SessionId>, CdpStateError> {
        let mut t = self.lock();
        Self::remove_target(&mut t, id)
    }

    /// 閉じてよい context に属するときだけターゲットを閉じる（`Target.closeTarget`・TASK-43.3h・`CDP-7`・`SEC-2`）。
    ///
    /// 許可されるのは既定 context か `allowed`（呼び出し接続が所有する context）に属するターゲット。
    /// 判定と削除は 1 回のロックで行い（TOCTOU 回避）、未知・許可外はどちらも
    /// [`CdpStateError::UnknownTarget`] にして存在を漏らさず、表は変えない。
    /// `Target.createTarget` は作成先を既定 context か自接続の context に限る（[`crate::conn_state`]）ため、
    /// ターゲット単位の所有を持たず context 単位の判定で足りる。
    pub(crate) fn close_target_in_contexts(
        &self,
        id: &TargetId,
        allowed: &[BrowserContextId],
    ) -> Result<Vec<SessionId>, CdpStateError> {
        let mut t = self.lock();
        let permitted = t.targets.get(id).is_some_and(|info| {
            info.browser_context_id == t.default_context
                || allowed.contains(&info.browser_context_id)
        });
        if !permitted {
            return Err(CdpStateError::UnknownTarget);
        }
        Self::remove_target(&mut t, id)
    }

    fn remove_target(t: &mut Tables, id: &TargetId) -> Result<Vec<SessionId>, CdpStateError> {
        if t.targets.remove(id).is_none() {
            return Err(CdpStateError::UnknownTarget);
        }
        t.paused.remove(id);
        let mut removed: Vec<SessionId> = t
            .sessions
            .iter()
            .filter(|(_, tid)| *tid == id)
            .map(|(sid, _)| sid.clone())
            .collect();
        removed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for sid in &removed {
            t.sessions.remove(sid);
            t.page_enabled.remove(sid);
            t.lifecycle_events_enabled.remove(sid);
            t.domains_enabled.remove(sid);
            t.emulation.remove(sid);
        }
        Ok(removed)
    }

    /// ターゲットへ attach し、新しいセッション ID を返す。
    pub fn attach(&self, id: &TargetId) -> Result<SessionId, CdpStateError> {
        let mut t = self.lock();
        if !t.targets.contains_key(id) {
            return Err(CdpStateError::UnknownTarget);
        }
        if t.sessions.len() >= MAX_SESSIONS {
            return Err(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS,
            });
        }
        let n = t.next_session;
        let next = n.checked_add(1).ok_or(CdpStateError::IdExhausted)?;
        let sid = SessionId::from_counter(n);
        t.next_session = next;
        t.sessions.insert(sid.clone(), id.clone());
        Ok(sid)
    }

    /// セッションを切り離し、attach 先のターゲット ID を返す。
    pub fn detach(&self, id: &SessionId) -> Result<TargetId, CdpStateError> {
        let mut t = self.lock();
        t.page_enabled.remove(id);
        t.lifecycle_events_enabled.remove(id);
        t.domains_enabled.remove(id);
        t.emulation.remove(id);
        t.sessions.remove(id).ok_or(CdpStateError::UnknownSession)
    }

    /// セッションに `Page.enable` 済みの印を付ける（冪等。43.3i・#1037・`CDP-2`）。
    ///
    /// 存在確認と設定を 1 回のロックで行う。フラグを `Tables` に置くのは、セッション削除と同じロック内で
    /// 消せて TOCTOU が起きず、`conn_state` と違い自動 attach 由来でないセッションも扱えるため。
    pub(crate) fn enable_page(&self, sid: &SessionId) -> Result<(), CdpStateError> {
        let mut t = self.lock();
        if !t.sessions.contains_key(sid) {
            return Err(CdpStateError::UnknownSession);
        }
        t.page_enabled.insert(sid.clone());
        Ok(())
    }

    /// `Page.enable` 済みか（テスト用。本番の送出ゲートは [`Self::lifecycle_events_active`]）。
    #[cfg(test)]
    pub(crate) fn is_page_enabled(&self, sid: &SessionId) -> bool {
        self.lock().page_enabled.contains(sid)
    }

    /// セッションの lifecycle 有効フラグを設定する（`true` で insert・`false` で remove。冪等。
    /// 43.3k・#1039・`CDP-2`）。存在確認と設定を 1 回のロックで行う。置き場所の理由は [`Self::enable_page`]。
    pub(crate) fn set_lifecycle_events(
        &self,
        sid: &SessionId,
        enabled: bool,
    ) -> Result<(), CdpStateError> {
        let mut t = self.lock();
        if !t.sessions.contains_key(sid) {
            return Err(CdpStateError::UnknownSession);
        }
        if enabled {
            t.lifecycle_events_enabled.insert(sid.clone());
        } else {
            t.lifecycle_events_enabled.remove(sid);
        }
        Ok(())
    }

    /// `Page.lifecycleEvent` を送ってよいか（`Page.enable` 済みかつ lifecycle 有効）を 1 回のロックで返す。
    /// `Page.navigate` の送出ゲートが読む（43.3k・#1039）。
    pub(crate) fn lifecycle_events_active(&self, sid: &SessionId) -> bool {
        let t = self.lock();
        t.page_enabled.contains(sid) && t.lifecycle_events_enabled.contains(sid)
    }

    /// lifecycle 有効フラグのみを返す（テスト用）。
    #[cfg(test)]
    pub(crate) fn is_lifecycle_events_enabled(&self, sid: &SessionId) -> bool {
        self.lock().lifecycle_events_enabled.contains(sid)
    }

    /// `Emulation.setFocusEmulationEnabled` の検証済みの値を保持する（冪等。43.3m・#1041・`CDP-2`）。
    ///
    /// 保持値は読まれず、今は観測できる効果が無い（`REPAIR-3`。将来は render の境界ボックス・フォーカス管理と接続）。
    /// 存在確認と設定を 1 回のロックで行う。置き場所の理由は [`Self::enable_page`]。
    pub(crate) fn set_focus_emulation(
        &self,
        sid: &SessionId,
        enabled: bool,
    ) -> Result<(), CdpStateError> {
        let mut t = self.lock();
        if !t.sessions.contains_key(sid) {
            return Err(CdpStateError::UnknownSession);
        }
        t.emulation.entry(sid.clone()).or_default().focus = Some(enabled);
        Ok(())
    }

    /// `Emulation.setEmulatedMedia` で Playwright 既定値を受理した印を付ける（冪等。43.3m・#1041・`CDP-2`）。
    ///
    /// 既定値以外は呼び出し側が拒否するため、保持するのは「既定値を受理済み」の印だけ。読まれない（`REPAIR-3`）。
    pub(crate) fn set_emulated_media_default(&self, sid: &SessionId) -> Result<(), CdpStateError> {
        let mut t = self.lock();
        if !t.sessions.contains_key(sid) {
            return Err(CdpStateError::UnknownSession);
        }
        t.emulation.entry(sid.clone()).or_default().media_default = true;
        Ok(())
    }

    /// セッションの Emulation 保持値（テスト用。本番コードからは読まない）。
    #[cfg(test)]
    pub(crate) fn emulation_of(&self, sid: &SessionId) -> Option<SessionEmulation> {
        self.lock().emulation.get(sid).copied()
    }

    /// セッションに `Log`・`Runtime`・`Network` のいずれかの有効印を付ける（冪等。43.3j・#1038・`CDP-2`）。
    ///
    /// 存在確認と設定を 1 回のロックで行う。置き場所の理由は [`Self::enable_page`] と同じ。
    pub(crate) fn enable_domain(
        &self,
        sid: &SessionId,
        d: SessionDomain,
    ) -> Result<(), CdpStateError> {
        let mut t = self.lock();
        if !t.sessions.contains_key(sid) {
            return Err(CdpStateError::UnknownSession);
        }
        *t.domains_enabled.entry(sid.clone()).or_default().slot(d) = true;
        Ok(())
    }

    /// ドメインが有効か。本番コードでは読む箇所がない（イベント送出の実装時に読む）。
    #[cfg(test)]
    pub(crate) fn is_domain_enabled(&self, sid: &SessionId, d: SessionDomain) -> bool {
        self.lock()
            .domains_enabled
            .get_mut(sid)
            .is_some_and(|x| *x.slot(d))
    }

    /// セッションの attach 先ターゲットを返す。
    pub fn session_target(&self, id: &SessionId) -> Option<TargetId> {
        self.lock().sessions.get(id).cloned()
    }

    /// ターゲットを作成して即 attach し、必要なら一時停止として登録する（1 回のロック。`CDP-7`）。
    ///
    /// セッション枠が無ければターゲットも作らず表を変更しない（原子性）。`pause` が true なら
    /// 新しいセッションを「再開を待つセッション」として記録する（`waitForDebuggerOnStart`。`CDP-2`）。
    /// `Target.createTarget`（[`crate::conn_state::ConnectionScoped::create_target`]）から呼ばれる。
    pub(crate) fn create_target_attached(
        &self,
        context: Option<&BrowserContextId>,
        kind: TargetKind,
        url: &str,
        pause: bool,
    ) -> Result<(TargetId, SessionId), CdpStateError> {
        if url.len() > MAX_URL_LEN {
            return Err(CdpStateError::UrlTooLong { limit: MAX_URL_LEN });
        }
        let mut t = self.lock();
        if t.sessions.len() >= MAX_SESSIONS {
            return Err(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS,
            });
        }
        let context = match context {
            Some(c) if t.contexts.contains_key(c) => c.clone(),
            Some(_) => return Err(CdpStateError::UnknownBrowserContext),
            None => t.default_context.clone(),
        };
        if t.targets.len() >= MAX_TARGETS {
            return Err(CdpStateError::TooManyTargets { limit: MAX_TARGETS });
        }
        let tn = t.next_target;
        let next_t = tn.checked_add(1).ok_or(CdpStateError::IdExhausted)?;
        let sn = t.next_session;
        let next_s = sn.checked_add(1).ok_or(CdpStateError::IdExhausted)?;
        let tid = TargetId::from_counter(tn);
        let sid = SessionId::from_counter(sn);
        t.next_target = next_t;
        t.next_session = next_s;
        t.targets.insert(
            tid.clone(),
            TargetInfo {
                target_id: tid.clone(),
                kind,
                url: url.to_owned(),
                browser_context_id: context,
            },
        );
        t.sessions.insert(sid.clone(), tid.clone());
        if pause {
            t.paused.insert(tid.clone(), sid.clone());
        }
        Ok((tid, sid))
    }

    /// ターゲットが `waitForDebuggerOnStart` で止まったままか（`Page.navigate` の拒否判定用）。
    pub(crate) fn is_paused(&self, id: &TargetId) -> bool {
        self.lock().paused.contains_key(id)
    }

    /// `session` が止めたターゲットを再開して返す。止めたセッション以外・止まっていないターゲットは
    /// `None`（`Runtime.runIfWaitingForDebugger`。Chromium と同じ実際の no-op）。
    pub(crate) fn resume_by_session(&self, session: &SessionId) -> Option<TargetId> {
        let mut t = self.lock();
        let tid = t.sessions.get(session)?.clone();
        if t.paused.get(&tid) == Some(session) {
            t.paused.remove(&tid);
            Some(tid)
        } else {
            None
        }
    }

    /// 止めたセッションが外れた（detach・切断）ターゲットを再開対象として取り出す。
    /// 永久停止で資源が残らないようにする（`CDP-7`）。呼び出し側が初期読み込みを行う。
    pub(crate) fn take_orphaned_paused(&self) -> Vec<TargetId> {
        let mut t = self.lock();
        let orphaned: Vec<TargetId> = t
            .paused
            .iter()
            .filter(|(_, s)| !t.sessions.contains_key(*s))
            .map(|(tid, _)| tid.clone())
            .collect();
        for tid in &orphaned {
            t.paused.remove(tid);
        }
        orphaned
    }

    /// 既定 browser context の ID。既存ページの `targetInfo.browserContextId` に使う。
    pub fn default_browser_context(&self) -> BrowserContextId {
        self.lock().default_context.clone()
    }

    /// 指定 ID の browser context が存在するか。
    pub fn has_browser_context(&self, id: &BrowserContextId) -> bool {
        self.lock().contexts.contains_key(id)
    }

    /// 新しい browser context を作り ID を返す（既定 context は含まない件数で上限検証）。
    /// 上限超過・ID 枯渇時は表を変更しない。`Target.createBrowserContext` から呼ばれる（`CDP-2`・`CDP-7`）。
    pub fn create_browser_context(&self) -> Result<BrowserContextId, CdpStateError> {
        let mut t = self.lock();
        // 既定 context は上限の外。
        if t.contexts.len() > MAX_BROWSER_CONTEXTS {
            return Err(CdpStateError::TooManyBrowserContexts {
                limit: MAX_BROWSER_CONTEXTS,
            });
        }
        let n = t.next_context;
        let next = n.checked_add(1).ok_or(CdpStateError::IdExhausted)?;
        let id = BrowserContextId::from_counter(n);
        t.next_context = next;
        t.contexts.insert(id.clone(), ContextEntry::default());
        Ok(id)
    }

    /// browser context を破棄し、所属ターゲットと配下セッションも原子的に削除する。
    /// 閉じた `(targetId, 配下 SessionId)` を targetId 昇順で返す。既定 context は
    /// [`CdpStateError::DefaultBrowserContext`]、未知は [`CdpStateError::UnknownBrowserContext`]。
    pub fn dispose_browser_context(
        &self,
        id: &BrowserContextId,
    ) -> Result<Vec<(TargetId, Vec<SessionId>)>, CdpStateError> {
        let mut t = self.lock();
        if *id == t.default_context {
            return Err(CdpStateError::DefaultBrowserContext);
        }
        if t.contexts.remove(id).is_none() {
            return Err(CdpStateError::UnknownBrowserContext);
        }
        let mut targets: Vec<TargetId> = t
            .targets
            .values()
            .filter(|i| i.browser_context_id == *id)
            .map(|i| i.target_id.clone())
            .collect();
        targets.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let mut closed = Vec::with_capacity(targets.len());
        for tid in targets {
            t.targets.remove(&tid);
            t.paused.remove(&tid);
            let mut sids: Vec<SessionId> = t
                .sessions
                .iter()
                .filter(|(_, x)| **x == tid)
                .map(|(s, _)| s.clone())
                .collect();
            sids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            for s in &sids {
                t.sessions.remove(s);
                t.page_enabled.remove(s);
                t.lifecycle_events_enabled.remove(s);
                t.domains_enabled.remove(s);
                t.emulation.remove(s);
            }
            closed.push((tid, sids));
        }
        Ok(closed)
    }

    /// context 単位のダウンロード設定を置き換える（存在確認と設定を 1 回のロックで行う）。
    pub(crate) fn set_context_download(
        &self,
        id: &BrowserContextId,
        settings: DownloadSettings,
    ) -> Result<(), CdpStateError> {
        let mut t = self.lock();
        let c = t
            .contexts
            .get_mut(id)
            .ok_or(CdpStateError::UnknownBrowserContext)?;
        c.download = Some(settings);
        Ok(())
    }

    /// context のダウンロード設定（実行系が未実装のためテストのみが読む）。
    #[cfg(test)]
    pub(crate) fn context_download(&self, id: &BrowserContextId) -> Option<DownloadSettings> {
        self.lock()
            .contexts
            .get(id)
            .and_then(|c| c.download.clone())
    }

    /// 既定 context を除く browser context 数（テスト用）。
    #[cfg(test)]
    pub(crate) fn browser_context_count(&self) -> usize {
        self.lock().contexts.len().saturating_sub(1)
    }

    /// ターゲット数。
    pub fn target_count(&self) -> usize {
        self.lock().targets.len()
    }

    /// セッション数。
    pub fn session_count(&self) -> usize {
        self.lock().sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(r: &TargetRegistry) -> TargetId {
        r.create_target(TargetKind::Page, "about:blank").unwrap()
    }

    #[test]
    fn cdp1_create_target_assigns_distinct_ids() {
        let r = TargetRegistry::new();
        let a = page(&r);
        let b = page(&r);
        assert_eq!(a.as_str(), "00000000000000000000000000000001");
        assert_eq!(b.as_str(), "00000000000000000000000000000002");
        assert_eq!(r.target_count(), 2);
    }

    #[test]
    fn cdp1_attach_unknown_target_fails() {
        let r = TargetRegistry::new();
        let ghost = TargetId::parse("ABC").unwrap();
        assert_eq!(r.attach(&ghost), Err(CdpStateError::UnknownTarget));
        assert_eq!(r.session_count(), 0);
    }

    #[test]
    fn cdp1_attach_detach_roundtrip() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let s = r.attach(&t).unwrap();
        assert_eq!(r.session_target(&s), Some(t.clone()));
        assert_eq!(r.detach(&s), Ok(t));
        assert_eq!(r.session_target(&s), None);
        assert_eq!(r.detach(&s), Err(CdpStateError::UnknownSession));
    }

    #[test]
    fn cdp7_close_target_in_contexts_checks_ownership() {
        let r = TargetRegistry::new();
        let own = r.create_browser_context().unwrap();
        let other = r.create_browser_context().unwrap();
        let d = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let a = r
            .create_target_in(&own, TargetKind::Page, "about:blank")
            .unwrap();
        let b = r
            .create_target_in(&other, TargetKind::Page, "about:blank")
            .unwrap();
        let sb = r.attach(&b).unwrap();
        let sd = r.attach(&d).unwrap();
        assert_eq!(r.close_target_in_contexts(&d, &[]), Ok(vec![sd]));
        assert_eq!(
            r.close_target_in_contexts(&a, std::slice::from_ref(&own)),
            Ok(vec![])
        );
        assert_eq!(
            r.close_target_in_contexts(&b, std::slice::from_ref(&own)),
            Err(CdpStateError::UnknownTarget)
        );
        assert!(r.target(&b).is_some());
        assert_eq!(r.session_target(&sb), Some(b.clone()));
        let ghost = TargetId::from_counter(9999);
        assert_eq!(
            r.close_target_in_contexts(&ghost, &[]),
            Err(CdpStateError::UnknownTarget)
        );
    }

    #[test]
    fn cdp1_close_target_removes_attached_sessions() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let other = page(&r);
        let s1 = r.attach(&t).unwrap();
        let s2 = r.attach(&t).unwrap();
        let s3 = r.attach(&other).unwrap();
        assert_eq!(r.close_target(&t), Ok(vec![s1, s2]));
        assert_eq!(r.session_count(), 1);
        assert_eq!(r.session_target(&s3), Some(other));
        assert!(r.target(&t).is_none());
        assert_eq!(r.close_target(&t), Err(CdpStateError::UnknownTarget));
    }

    #[test]
    fn cdp1_url_length_limit_rejects_oversized_url() {
        let r = TargetRegistry::new();
        let at_limit = "a".repeat(MAX_URL_LEN);
        let id = r
            .create_target(TargetKind::Page, at_limit.as_str())
            .unwrap();
        assert_eq!(r.target(&id).unwrap().url().len(), MAX_URL_LEN);
        let over = "a".repeat(MAX_URL_LEN + 1);
        assert_eq!(
            r.create_target(TargetKind::Page, &over),
            Err(CdpStateError::UrlTooLong { limit: MAX_URL_LEN })
        );
        assert_eq!(r.target_count(), 1);
    }

    #[test]
    fn cdp1_target_limit_leaves_table_unchanged() {
        let r = TargetRegistry::new();
        for _ in 0..MAX_TARGETS {
            page(&r);
        }
        assert_eq!(
            r.create_target(TargetKind::Page, "about:blank"),
            Err(CdpStateError::TooManyTargets { limit: MAX_TARGETS })
        );
        assert_eq!(r.target_count(), MAX_TARGETS);
    }

    #[test]
    fn cdp1_session_limit_leaves_table_unchanged() {
        let r = TargetRegistry::new();
        let t = page(&r);
        for _ in 0..MAX_SESSIONS {
            r.attach(&t).unwrap();
        }
        assert_eq!(
            r.attach(&t),
            Err(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            })
        );
        assert_eq!(r.session_count(), MAX_SESSIONS);
    }

    #[test]
    fn cdp1_id_counter_exhaustion_does_not_wrap() {
        let r = TargetRegistry::with_counters(u64::MAX, u64::MAX, 2);
        assert_eq!(
            r.create_target(TargetKind::Page, "about:blank"),
            Err(CdpStateError::IdExhausted)
        );
        assert_eq!(r.target_count(), 0);

        let r = TargetRegistry::with_counters(1, u64::MAX, 2);
        let t = page(&r);
        assert_eq!(r.attach(&t), Err(CdpStateError::IdExhausted));
        assert_eq!(r.session_count(), 0);
    }

    #[test]
    fn cdp1_parse_rejects_invalid_ids() {
        let long = "a".repeat(65);
        for bad in ["", long.as_str(), "../x", "a b", "日本語"] {
            assert_eq!(TargetId::parse(bad), Err(CdpStateError::InvalidId), "{bad}");
        }
        assert_eq!(
            TargetId::parse("ABCDEF0123").unwrap().as_str(),
            "ABCDEF0123"
        );
        assert_eq!(SessionId::parse("a-b").unwrap().as_str(), "a-b");
        assert_eq!(
            BrowserContextId::parse("../x"),
            Err(CdpStateError::InvalidId)
        );
        assert_eq!(BrowserContextId::parse(""), Err(CdpStateError::InvalidId));
        assert_eq!(BrowserContextId::parse("C-1").unwrap().as_str(), "C-1");
        assert_eq!(
            BrowserId::parse(&"a".repeat(64)).unwrap().as_str().len(),
            64
        );
        assert_ne!(
            TargetId::parse("abc").unwrap(),
            TargetId::parse("ABC").unwrap()
        );
    }

    #[test]
    fn cdp1_target_info_debug_hides_url() {
        let r = TargetRegistry::new();
        let url = "https://example.test/?token=secret";
        let id = r.create_target(TargetKind::Page, url).unwrap();
        let dbg = format!("{:?}", r.target(&id).unwrap());
        assert!(!dbg.contains("secret") && !dbg.contains("example"));
        assert!(dbg.contains(&format!("url_len: {}", url.len())));
    }

    #[test]
    fn cdp1_targets_listing_is_deterministic() {
        let r = TargetRegistry::new();
        let ids: Vec<TargetId> = (0..5).map(|_| page(&r)).collect();
        let listed: Vec<TargetId> = r.targets().iter().map(|i| i.target_id().clone()).collect();
        assert_eq!(listed, ids);
    }

    #[test]
    fn cdp2_target_with_attached_reflects_sessions() {
        let r = TargetRegistry::new();
        let id = page(&r);
        let (info, attached) = r.target_with_attached(&id).unwrap();
        assert_eq!(info.url(), "about:blank");
        assert!(!attached);
        let sid = r.attach(&id).unwrap();
        assert!(r.target_with_attached(&id).unwrap().1);
        r.detach(&sid).unwrap();
        assert!(!r.target_with_attached(&id).unwrap().1);
        assert!(
            r.target_with_attached(&TargetId::parse("nope").unwrap())
                .is_none()
        );
    }

    #[test]
    fn cdp2_targets_get_default_browser_context() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let d = r.default_browser_context();
        assert_eq!(d.as_str(), "00000000000000000000000000000001");
        assert_eq!(r.target(&t).unwrap().browser_context_id(), &d);
        assert!(r.has_browser_context(&d));
        assert_eq!(r.browser_context_count(), 0);
    }

    #[test]
    fn cdp2_create_browser_context_assigns_distinct_deterministic_ids() {
        let r = TargetRegistry::new();
        let a = r.create_browser_context().unwrap();
        let b = r.create_browser_context().unwrap();
        assert_eq!(a.as_str(), "00000000000000000000000000000002");
        assert_eq!(b.as_str(), "00000000000000000000000000000003");
        assert_eq!(r.browser_context_count(), 2);
    }

    #[test]
    fn cdp2_create_target_in_context_and_unknown_context_rejected() {
        let r = TargetRegistry::new();
        let c = r.create_browser_context().unwrap();
        let t = r
            .create_target_in(&c, TargetKind::Page, "about:blank")
            .unwrap();
        assert_eq!(r.target(&t).unwrap().browser_context_id(), &c);
        let ghost = BrowserContextId::parse("GHOST").unwrap();
        assert_eq!(
            r.create_target_in(&ghost, TargetKind::Page, "about:blank"),
            Err(CdpStateError::UnknownBrowserContext)
        );
        assert_eq!(r.target_count(), 1);
    }

    #[test]
    fn cdp7_browser_context_limit_leaves_table_unchanged() {
        let r = TargetRegistry::new();
        for _ in 0..MAX_BROWSER_CONTEXTS {
            r.create_browser_context().unwrap();
        }
        assert_eq!(
            r.create_browser_context(),
            Err(CdpStateError::TooManyBrowserContexts {
                limit: MAX_BROWSER_CONTEXTS
            })
        );
        assert_eq!(r.browser_context_count(), MAX_BROWSER_CONTEXTS);
    }

    #[test]
    fn cdp7_target_limit_applies_across_contexts() {
        let r = TargetRegistry::new();
        let c = r.create_browser_context().unwrap();
        for i in 0..MAX_TARGETS {
            if i % 2 == 0 {
                page(&r);
            } else {
                r.create_target_in(&c, TargetKind::Page, "about:blank")
                    .unwrap();
            }
        }
        assert_eq!(
            r.create_target(TargetKind::Page, "about:blank"),
            Err(CdpStateError::TooManyTargets { limit: MAX_TARGETS })
        );
        assert_eq!(
            r.create_target_in(&c, TargetKind::Page, "about:blank"),
            Err(CdpStateError::TooManyTargets { limit: MAX_TARGETS })
        );
        assert_eq!(r.target_count(), MAX_TARGETS);
    }

    #[test]
    fn cdp7_session_limit_unchanged_with_contexts() {
        let r = TargetRegistry::new();
        let c = r.create_browser_context().unwrap();
        let t = r
            .create_target_in(&c, TargetKind::Page, "about:blank")
            .unwrap();
        for _ in 0..MAX_SESSIONS {
            r.attach(&t).unwrap();
        }
        assert_eq!(
            r.attach(&t),
            Err(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            })
        );
    }

    #[test]
    fn cdp7_dispose_browser_context_closes_its_targets_and_sessions_only() {
        let r = TargetRegistry::new();
        let keep = page(&r);
        let keep_s = r.attach(&keep).unwrap();
        let c = r.create_browser_context().unwrap();
        let other = r.create_browser_context().unwrap();
        let t1 = r
            .create_target_in(&c, TargetKind::Page, "about:blank")
            .unwrap();
        let t2 = r
            .create_target_in(&c, TargetKind::Page, "about:blank")
            .unwrap();
        let o = r
            .create_target_in(&other, TargetKind::Page, "about:blank")
            .unwrap();
        let s1 = r.attach(&t1).unwrap();
        let s2 = r.attach(&t1).unwrap();
        assert_eq!(
            r.dispose_browser_context(&c),
            Ok(vec![(t1.clone(), vec![s1, s2]), (t2, vec![])])
        );
        assert!(!r.has_browser_context(&c));
        assert!(r.target(&t1).is_none());
        assert_eq!(r.target_count(), 2);
        assert!(r.target(&o).is_some() && r.has_browser_context(&other));
        assert_eq!(r.session_target(&keep_s), Some(keep));
        assert_eq!(r.session_count(), 1);
        assert_eq!(
            r.dispose_browser_context(&c),
            Err(CdpStateError::UnknownBrowserContext)
        );
        assert_eq!(
            r.dispose_browser_context(&r.default_browser_context()),
            Err(CdpStateError::DefaultBrowserContext)
        );
    }

    #[test]
    fn cdp7_context_id_counter_exhaustion_does_not_wrap() {
        let r = TargetRegistry::with_counters(1, 1, u64::MAX);
        assert_eq!(r.create_browser_context(), Err(CdpStateError::IdExhausted));
        assert_eq!(r.browser_context_count(), 0);
    }

    #[test]
    fn cdp2_set_context_download_unknown_id_and_dropped_with_context() {
        use crate::conn_state::{DownloadBehavior, DownloadSettings};
        let settings = || DownloadSettings {
            behavior: DownloadBehavior::Deny,
            download_path: None,
            events_enabled: true,
        };
        let r = TargetRegistry::new();
        let ghost = BrowserContextId::parse("GHOST").unwrap();
        assert_eq!(
            r.set_context_download(&ghost, settings()),
            Err(CdpStateError::UnknownBrowserContext)
        );
        let c = r.create_browser_context().unwrap();
        assert!(r.context_download(&c).is_none());
        r.set_context_download(&c, settings()).unwrap();
        assert_eq!(r.context_download(&c), Some(settings()));
        r.dispose_browser_context(&c).unwrap();
        assert!(r.context_download(&c).is_none());
    }

    #[test]
    fn cdp2_create_target_attached_pauses_and_resumes_by_owning_session() {
        let r = TargetRegistry::new();
        let (t, s) = r
            .create_target_attached(None, TargetKind::Page, "about:blank", true)
            .unwrap();
        assert_eq!(r.session_target(&s), Some(t.clone()));
        assert!(r.is_paused(&t));
        // 別セッションは再開できない。
        let other = r.attach(&t).unwrap();
        assert_eq!(r.resume_by_session(&other), None);
        assert!(r.is_paused(&t));
        assert_eq!(r.resume_by_session(&s), Some(t.clone()));
        assert!(!r.is_paused(&t));
        assert_eq!(r.resume_by_session(&s), None);
        // 止めない作成は記録しない。
        let (t2, _) = r
            .create_target_attached(None, TargetKind::Page, "about:blank", false)
            .unwrap();
        assert!(!r.is_paused(&t2));
    }

    #[test]
    fn cdp7_paused_record_is_dropped_on_close_dispose_and_orphaned_by_detach() {
        let r = TargetRegistry::new();
        let (t, s) = r
            .create_target_attached(None, TargetKind::Page, "about:blank", true)
            .unwrap();
        assert!(r.take_orphaned_paused().is_empty());
        r.detach(&s).unwrap();
        assert_eq!(r.take_orphaned_paused(), vec![t.clone()]);
        assert!(!r.is_paused(&t));
        // 閉鎖で消える。
        let (t2, _) = r
            .create_target_attached(None, TargetKind::Page, "about:blank", true)
            .unwrap();
        r.close_target(&t2).unwrap();
        assert!(!r.is_paused(&t2));
        assert!(r.take_orphaned_paused().is_empty());
        // context の破棄で消える。
        let c = r.create_browser_context().unwrap();
        let (t3, _) = r
            .create_target_attached(Some(&c), TargetKind::Page, "about:blank", true)
            .unwrap();
        r.dispose_browser_context(&c).unwrap();
        assert!(!r.is_paused(&t3));
        assert!(r.take_orphaned_paused().is_empty());
    }

    #[test]
    fn cdp2_enable_page_is_idempotent_and_rejects_unknown_session() {
        let r = TargetRegistry::new();
        let t = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s = r.attach(&t).unwrap();
        assert!(!r.is_page_enabled(&s));
        r.enable_page(&s).unwrap();
        r.enable_page(&s).unwrap();
        assert!(r.is_page_enabled(&s));
        let ghost = SessionId::parse("ghost-1").unwrap();
        assert_eq!(r.enable_page(&ghost), Err(CdpStateError::UnknownSession));
        assert!(!r.is_page_enabled(&ghost));
    }

    #[test]
    fn cdp7_page_enabled_flag_is_dropped_on_detach_close_and_dispose() {
        let r = TargetRegistry::new();
        let t1 = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s1 = r.attach(&t1).unwrap();
        r.enable_page(&s1).unwrap();
        r.detach(&s1).unwrap();
        assert!(!r.is_page_enabled(&s1));
        let s1b = r.attach(&t1).unwrap();
        r.enable_page(&s1b).unwrap();
        r.close_target(&t1).unwrap();
        assert!(!r.is_page_enabled(&s1b));
        let c = r.create_browser_context().unwrap();
        let (_t3, s3) = r
            .create_target_attached(Some(&c), TargetKind::Page, "about:blank", false)
            .unwrap();
        r.enable_page(&s3).unwrap();
        r.dispose_browser_context(&c).unwrap();
        assert!(!r.is_page_enabled(&s3));
    }

    #[test]
    fn cdp2_set_lifecycle_events_toggles_and_rejects_unknown_session() {
        let r = TargetRegistry::new();
        let t = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s = r.attach(&t).unwrap();
        assert!(!r.is_lifecycle_events_enabled(&s));
        r.set_lifecycle_events(&s, true).unwrap();
        r.set_lifecycle_events(&s, true).unwrap();
        assert!(r.is_lifecycle_events_enabled(&s));
        r.set_lifecycle_events(&s, false).unwrap();
        r.set_lifecycle_events(&s, false).unwrap();
        assert!(!r.is_lifecycle_events_enabled(&s));
        let ghost = SessionId::parse("ghost-1").unwrap();
        assert_eq!(
            r.set_lifecycle_events(&ghost, true),
            Err(CdpStateError::UnknownSession)
        );
        assert!(!r.is_lifecycle_events_enabled(&ghost));
    }

    #[test]
    fn cdp2_lifecycle_events_active_requires_page_enable_and_flag() {
        let r = TargetRegistry::new();
        let t = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s = r.attach(&t).unwrap();
        r.set_lifecycle_events(&s, true).unwrap();
        assert!(!r.lifecycle_events_active(&s));
        r.enable_page(&s).unwrap();
        assert!(r.lifecycle_events_active(&s));
        r.set_lifecycle_events(&s, false).unwrap();
        assert!(!r.lifecycle_events_active(&s));
        let s2 = r.attach(&t).unwrap();
        r.enable_page(&s2).unwrap();
        assert!(!r.lifecycle_events_active(&s2));
    }

    #[test]
    fn cdp7_lifecycle_flag_is_dropped_on_detach_close_and_dispose() {
        let r = TargetRegistry::new();
        let t1 = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s1 = r.attach(&t1).unwrap();
        r.set_lifecycle_events(&s1, true).unwrap();
        r.detach(&s1).unwrap();
        assert!(!r.is_lifecycle_events_enabled(&s1));
        let s1b = r.attach(&t1).unwrap();
        r.set_lifecycle_events(&s1b, true).unwrap();
        r.close_target(&t1).unwrap();
        assert!(!r.is_lifecycle_events_enabled(&s1b));
        let c = r.create_browser_context().unwrap();
        let (_t3, s3) = r
            .create_target_attached(Some(&c), TargetKind::Page, "about:blank", false)
            .unwrap();
        r.set_lifecycle_events(&s3, true).unwrap();
        r.dispose_browser_context(&c).unwrap();
        assert!(!r.is_lifecycle_events_enabled(&s3));
    }

    #[test]
    fn cdp2_emulation_state_is_idempotent_independent_and_rejects_unknown_session() {
        let r = TargetRegistry::new();
        let t = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s = r.attach(&t).unwrap();
        assert_eq!(r.emulation_of(&s), None);
        r.set_focus_emulation(&s, true).unwrap();
        r.set_focus_emulation(&s, true).unwrap();
        assert_eq!(
            r.emulation_of(&s),
            Some(SessionEmulation {
                focus: Some(true),
                media_default: false
            })
        );
        r.set_emulated_media_default(&s).unwrap();
        r.set_focus_emulation(&s, false).unwrap();
        assert_eq!(
            r.emulation_of(&s),
            Some(SessionEmulation {
                focus: Some(false),
                media_default: true
            })
        );
        // 他のフラグとは独立。
        assert!(!r.is_page_enabled(&s));
        assert!(!r.is_lifecycle_events_enabled(&s));
        let ghost = SessionId::parse("ghost-1").unwrap();
        assert_eq!(
            r.set_focus_emulation(&ghost, true),
            Err(CdpStateError::UnknownSession)
        );
        assert_eq!(
            r.set_emulated_media_default(&ghost),
            Err(CdpStateError::UnknownSession)
        );
        assert_eq!(r.emulation_of(&ghost), None);
    }

    #[test]
    fn cdp7_emulation_state_is_dropped_on_detach_close_and_dispose() {
        let r = TargetRegistry::new();
        let t1 = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s1 = r.attach(&t1).unwrap();
        r.set_focus_emulation(&s1, true).unwrap();
        r.detach(&s1).unwrap();
        assert_eq!(r.emulation_of(&s1), None);
        let s1b = r.attach(&t1).unwrap();
        r.set_emulated_media_default(&s1b).unwrap();
        r.close_target(&t1).unwrap();
        assert_eq!(r.emulation_of(&s1b), None);
        let c = r.create_browser_context().unwrap();
        let (_t3, s3) = r
            .create_target_attached(Some(&c), TargetKind::Page, "about:blank", false)
            .unwrap();
        r.set_focus_emulation(&s3, true).unwrap();
        r.dispose_browser_context(&c).unwrap();
        assert_eq!(r.emulation_of(&s3), None);
    }

    #[test]
    fn cdp2_enable_domain_is_idempotent_per_domain_and_rejects_unknown_session() {
        use SessionDomain::{Log, Network, Runtime};
        let r = TargetRegistry::new();
        let t = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s = r.attach(&t).unwrap();
        for d in [Log, Runtime, Network] {
            assert!(!r.is_domain_enabled(&s, d));
            r.enable_domain(&s, d).unwrap();
            r.enable_domain(&s, d).unwrap();
            assert!(r.is_domain_enabled(&s, d));
        }
        let s2 = r.attach(&t).unwrap();
        r.enable_domain(&s2, Log).unwrap();
        assert!(r.is_domain_enabled(&s2, Log));
        assert!(!r.is_domain_enabled(&s2, Runtime));
        assert!(!r.is_domain_enabled(&s2, Network));
        assert!(!r.is_page_enabled(&s2));
        let ghost = SessionId::parse("ghost-1").unwrap();
        assert_eq!(
            r.enable_domain(&ghost, Log),
            Err(CdpStateError::UnknownSession)
        );
        assert!(!r.is_domain_enabled(&ghost, Log));
    }

    #[test]
    fn cdp7_domain_flags_are_dropped_on_detach_close_and_dispose() {
        use SessionDomain::{Log, Network, Runtime};
        let all = |r: &TargetRegistry, s: &SessionId| {
            for d in [Log, Runtime, Network] {
                r.enable_domain(s, d).unwrap();
            }
        };
        let none = |r: &TargetRegistry, s: &SessionId| {
            for d in [Log, Runtime, Network] {
                assert!(!r.is_domain_enabled(s, d));
            }
        };
        let r = TargetRegistry::new();
        let t1 = r.create_target(TargetKind::Page, "about:blank").unwrap();
        let s1 = r.attach(&t1).unwrap();
        all(&r, &s1);
        r.detach(&s1).unwrap();
        none(&r, &s1);
        let s1b = r.attach(&t1).unwrap();
        all(&r, &s1b);
        r.close_target(&t1).unwrap();
        none(&r, &s1b);
        let c = r.create_browser_context().unwrap();
        let (_t3, s3) = r
            .create_target_attached(Some(&c), TargetKind::Page, "about:blank", false)
            .unwrap();
        all(&r, &s3);
        r.dispose_browser_context(&c).unwrap();
        none(&r, &s3);
    }

    #[test]
    fn cdp7_create_target_attached_rolls_back_on_session_limit() {
        let r = TargetRegistry::new();
        let t = r.create_target(TargetKind::Page, "about:blank").unwrap();
        while r.session_count() < MAX_SESSIONS {
            r.attach(&t).unwrap();
        }
        let before = r.target_count();
        assert_eq!(
            r.create_target_attached(None, TargetKind::Page, "about:blank", true)
                .unwrap_err(),
            CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            }
        );
        assert_eq!(r.target_count(), before);
    }
}

//! WebSocket 接続スコープの CDP 状態（`Target.setAutoAttach` の自動 attach セッション所有と
//! `Browser.setDownloadBehavior` の設定。TASK-43.3c・#908、ビヘイビア `CDP-2`・`CDP-7`・MS-4）。
//!
//! `CdpState`（`server.rs`）が 1 つ内包し、`playwright_compat` のハンドラが読み書きする。
//! 接続の開始・終了は `crate::ws` が `CdpState::open_connection` / `release_connection` で通知する。
//! 構造は `dom::IssuedDocuments` と同じく「生存中の接続のみ記録を受け付ける」表で、切断との競合で
//! 閉じた接続の状態が残らないようにする。
//!
//! `Target.createBrowserContext`（`disposeOnDetach:true`。TASK-43.3d・#909）で作った browser context も
//! 接続が所有し、切断で [`ConnectionScoped::release`] が返す ID を呼び出し側が
//! `TargetRegistry::dispose_browser_context` して破棄する（永続 context は `Target.disposeBrowserContext`
//! 未実装のため作らせない。`CDP-7`）。
//!
//! # 資源の所有と解放（`CDP-7`）
//!
//! [`crate::target::TargetRegistry`] のセッション表は全接続共有で [`crate::target::MAX_SESSIONS`] 件の
//! 上限を持つ。自動 attach で払い出したセッションは本表が接続ごとに所有し、切断で
//! [`ConnectionScoped::release`] が返す ID を呼び出し側が `detach` して必ず解放する
//! （解放漏れはセッション枠の枯渇、すなわち DoS になる）。ロック順序は常に「本表 → registry」。
//! Puppeteer 経路（#1334・`CDP-3`）では、ページの代わりに tab 擬似ターゲットのセッションと、その子のページの
//! セッションを同じ規則で所有する（切断では子 → tab の順で解放し、消失は 子 → tab の順で通知する）。
//!
//! # スタブについて（実装済みを装わない。`REPAIR-3`）
//!
//! ダウンロード設定は検証して保持するだけ（受理は `deny` と `default` のみ。`deny` では
//! `downloadPath` を保持しない）で、保存処理・`Browser.downloadWillBegin` 等の送出は
//! 未実装（将来 `CDP-2` の実行系で参照する）。サーバーは `downloadPath` に対して一切の
//! ファイル操作をしない。実行系で書き込む際は、プロファイル境界（`PROF`）を踏まえた別途の
//! セキュリティ設計を要する。

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::protocol::ConnId;
use crate::target::{
    BrowserContextId, CdpStateError, SessionId, TargetId, TargetInfo, TargetKind, TargetRegistry,
    is_tab_id,
};

/// `downloadPath` の最大バイト長（巨大入力による無制限消費の防止）。
pub(crate) const MAX_DOWNLOAD_PATH_LEN: usize = 4096;

/// `Browser.setDownloadBehavior` の `behavior`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadBehavior {
    Deny,
    Allow,
    AllowAndName,
    Default,
}

impl DownloadBehavior {
    /// CDP の文字列から変換する。未知の値は `None`。
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "deny" => Some(Self::Deny),
            "allow" => Some(Self::Allow),
            "allowAndName" => Some(Self::AllowAndName),
            "default" => Some(Self::Default),
            _ => None,
        }
    }
}

/// 検証済みのダウンロード設定。保持のみで実行系は未実装（モジュールドキュメント参照）。
///
/// `Debug` はパスを出さず長さのみにする。
// 実行系（未実装）が参照するまでフィールドは読まれない。
#[allow(dead_code)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DownloadSettings {
    pub behavior: DownloadBehavior,
    pub download_path: Option<PathBuf>,
    pub events_enabled: bool,
}

impl fmt::Debug for DownloadSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DownloadSettings")
            .field("behavior", &self.behavior)
            .field(
                "download_path_len",
                &self.download_path.as_ref().map(|p| p.as_os_str().len()),
            )
            .field("events_enabled", &self.events_enabled)
            .finish()
    }
}

/// `downloadPath` を検証する。長さ上限・NUL 不在・絶対パス・`..` 成分なしを要求する。
/// パスは組み立てるだけで、存在確認を含む一切のファイル操作をしない。
pub(crate) fn validate_download_path(s: &str) -> Option<PathBuf> {
    if s.is_empty() || s.len() > MAX_DOWNLOAD_PATH_LEN || s.contains('\0') {
        return None;
    }
    let p = Path::new(s);
    if !p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    Some(p.to_path_buf())
}

/// `Target.setAutoAttach`（root）が attach する対象の種別（#1334・`CDP-3`）。有効の間だけ意味を持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AutoAttachKind {
    /// ページを root から直接 attach する（Playwright 経路。`filter` 省略）。
    #[default]
    Pages,
    /// tab 擬似ターゲットを root から attach し、ページは tab のセッションの子にする
    /// （Puppeteer 経路。`filter:[{type:"page",exclude:true},{}]`）。
    Tabs,
}

/// tab 擬似ターゲットへの attach 1 件と、その子のページのセッション（#1334・`CDP-3`・`CDP-7`）。
#[derive(Debug, Clone)]
struct TabAttachment {
    page: TargetId,
    /// 払い出し時点の tab ID（ページが消えた後の通知に使う）。
    tab_id: TargetId,
    tab_session: SessionId,
    /// tab のセッションの `Target.setAutoAttach` で attach したページのセッション。
    child: Option<SessionId>,
}

/// 消えた（または自分で外した）tab の attach の通知内容（#1334）。通知の順は 子 → tab（`CDP-7`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabDetach {
    /// tab 擬似ターゲットの ID。
    pub tab_id: TargetId,
    /// tab のセッション（root の `Target.detachedFromTarget` の `sessionId`。子の通知のエンベロープでもある）。
    pub tab_session: SessionId,
    /// 子のページの `(targetId, sessionId)`。あれば tab のセッション上で先に通知する。
    pub child: Option<(TargetId, SessionId)>,
}

/// discover の差分（#1324・#1334）。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DiscoverDiff {
    /// 新規に通知すべきページ `(info, attached)`。
    pub created: Vec<(TargetInfo, bool)>,
    /// 新規に通知すべき tab `(ページの info, attached)`。discover が tab を含むときだけ。
    pub created_tabs: Vec<(TargetInfo, bool)>,
    /// 消滅したターゲット（ページ → tab の順）。
    pub destroyed: Vec<TargetId>,
}

/// [`ConnectionScoped::set_tab_child_auto_attach`] の結果（#1334）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TabChildChange {
    /// 変化なし（既に attach 済み、または既に外れている）。
    None,
    /// 子のページを attach した（`Target.attachedToTarget` を tab のセッション上で送る）。
    Attached {
        info: TargetInfo,
        session: SessionId,
        waiting_for_debugger: bool,
    },
    /// 子のページを外した（`Target.detachedFromTarget` を tab のセッション上で送る）。
    Detached { page: TargetId, session: SessionId },
}

/// 接続 1 本分の状態。
#[derive(Default)]
struct Entry {
    /// 自動 attach が払い出したセッション（ターゲット, セッション）。
    auto_attached: Vec<(TargetId, SessionId)>,
    /// `Target.setAutoAttach` が有効か（有効の間、後から現れたターゲットも attach 対象にする）。
    auto_attach_enabled: bool,
    /// `Target.setAutoAttach` の `waitForDebuggerOnStart`（`autoAttach:true` の間だけ有効）。
    /// 以後に attach される新しいターゲットを一時停止する指示で、既存ターゲットには適用しない。
    /// 新しいターゲットの停止は `Target.createTarget`（[`ConnectionScoped::create_target`]。#910）が行う。
    wait_for_debugger_on_start: bool,
    /// registry 側で消えたため所有記録から落としたが、まだ `Target.detachedFromTarget` を送出していないもの。
    pending_detached: Vec<(TargetId, SessionId)>,
    /// root の自動 attach が有効な間、何を attach するか（#1334）。
    auto_attach_kind: AutoAttachKind,
    /// tab を attach している記録（`Tabs` の間。件数は `MAX_TARGETS` 以下）。
    tab_attached: Vec<TabAttachment>,
    /// registry 側で消えた tab の attach で、まだ通知していないもの。
    pending_tab_detached: Vec<TabDetach>,
    /// `Target.setDiscoverTargets` の通知対象に tab を含めるか（`filter:[{}]`。#1334）。
    discover_tabs: bool,
    download: Option<DownloadSettings>,
    /// この接続が `Target.createBrowserContext` で作った browser context（切断で破棄する）。
    /// 件数は registry 側の `MAX_BROWSER_CONTEXTS` で抑えられる。
    owned_contexts: Vec<BrowserContextId>,
    /// `Target.setDiscoverTargets` が有効か（#1324・`CDP-3`）。
    discover_enabled: bool,
    /// この接続へ `Target.targetCreated` を通知済みで、まだ `Target.targetDestroyed` を通知していない
    /// ターゲット（ID 昇順。ページと、`discover_tabs` のときは tab）。discover 有効の間、enable・reconcile のたびに
    /// registry の可視ターゲット（既定 context と自接続所有 context のページ。件数は `MAX_TARGETS` 以下）と同期するため、
    /// 件数は `2 * MAX_TARGETS` に 2 件（`create_target` が直後の reconcile までに足す分）を足した値で抑えられる。
    /// TargetId は単調増加で再利用されないので、「記録 − 現在の可視集合」の差分が消滅の通知対象になる。
    discovered: Vec<TargetId>,
}

/// [`ConnectionScoped::release`] が返す、切断時に解放すべき資源。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Released {
    /// 自動 attach が払い出したセッション（tab 経路では子のページ → tab のセッションの順）。
    pub sessions: Vec<SessionId>,
    /// この接続が所有していた browser context。
    pub contexts: Vec<BrowserContextId>,
}

/// [`ConnectionScoped::create_target`] の結果。
#[derive(Debug)]
pub(crate) struct CreatedTarget {
    /// 作成したターゲット。
    pub info: TargetInfo,
    /// 呼び出し接続の自動 attach が払い出したセッション（自動 attach 無効なら `None`）。
    /// `tab` が true のときは tab のセッション（ページのセッションは tab のセッションの
    /// `Target.setAutoAttach` で子として払い出す。#1334）。
    pub session: Option<SessionId>,
    /// `session` が tab のセッションか（Puppeteer 経路。#1334）。
    pub tab: bool,
    /// 呼び出し接続が discover 有効で tab を含み、`Target.targetCreated`（type:"tab"）を送出すべきか（#1334）。
    pub announce_tab_created: bool,
    /// `waitForDebuggerOnStart` で止めたか（`Target.attachedToTarget` の `waitingForDebugger`）。
    pub waiting_for_debugger: bool,
    /// 呼び出し接続が discover 有効で、`Target.targetCreated` を送出すべきか（#1324・`CDP-3`）。
    pub announce_created: bool,
}

/// [`ConnectionScoped::close_target`] の結果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ClosedTarget {
    /// 自接続の自動 attach から外したセッション（ID 昇順。`Target.detachedFromTarget` の送出対象）。
    pub sessions: Vec<SessionId>,
    /// 呼び出し接続が discover 有効でこのターゲットを通知済みか（`Target.targetDestroyed` の送出要否。#1324）。
    pub announce_destroyed: bool,
    /// 自接続が tab として attach していた分の通知内容（子 → tab の順。#1334）。
    pub tabs: Vec<TabDetach>,
    /// 呼び出し接続が tab を通知済みか（tab の `Target.targetDestroyed` の送出要否。#1334）。
    pub announce_tab_destroyed: bool,
    /// 閉じたページの tab ID（`announce_tab_destroyed` で使う）。
    pub tab_id: Option<TargetId>,
}

/// 接続操作の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnStateError {
    /// 接続が生存中でない（切断済み・未登録）。
    NotLive,
    /// セッション枠の枯渇など registry 側の失敗。
    Registry(CdpStateError),
    /// 有効な自動 attach と別の種別（ページ / tab）で `Target.setAutoAttach` が来た（先に無効化が要る。#1334）。
    KindConflict,
}

/// [`ConnectionScoped::reconcile_auto_attach`] の結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Reconciled {
    /// 新たに attach した `(targetInfo, sessionId)`（`Target.attachedToTarget` の送出対象）。
    pub attached: Vec<(TargetInfo, SessionId)>,
    /// 消失した `(targetId, sessionId)`（`Target.detachedFromTarget` の送出対象）。
    pub detached: Vec<(TargetId, SessionId)>,
    /// tab 経路で新たに attach した `(ページの targetInfo, tab のセッション)`（root の `Target.attachedToTarget`。#1334）。
    pub tab_attached: Vec<(TargetInfo, SessionId)>,
    /// tab 経路で消失した attach（子 → tab の順で通知。#1334）。
    pub tab_detached: Vec<TabDetach>,
    /// discover 有効な接続へ新たに通知すべき `(targetInfo, attached)`（`Target.targetCreated`。#1324・`CDP-3`）。
    /// `attached` は通知時点で何らかのセッションが attach 中か（この呼び出しの自動 attach より前の実値）。
    pub created: Vec<(TargetInfo, bool)>,
    /// discover が tab を含むときの、新たに通知すべき tab `(ページの targetInfo, attached)`（#1334）。
    pub created_tabs: Vec<(TargetInfo, bool)>,
    /// discover 有効な接続へ通知すべき消滅ターゲット（`Target.targetDestroyed`。#1324）。
    pub destroyed: Vec<TargetId>,
    /// attach の途中失敗（セッション枠の枯渇等）。
    pub error: Option<ConnStateError>,
}

/// 接続別の状態表。
pub(crate) struct ConnectionScoped {
    inner: Mutex<HashMap<ConnId, Entry>>,
}

impl ConnectionScoped {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ConnId, Entry>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 接続を生存中として登録する。
    pub(crate) fn open(&self, conn: ConnId) {
        self.lock().entry(conn).or_default();
    }

    /// 接続の状態を取り除き、所有していた自動 attach のセッション ID と browser context を返す。
    /// 呼び出し側（`CdpState::release_connection`）が registry で `detach`・`dispose_browser_context` する。
    pub(crate) fn release(&self, conn: ConnId) -> Released {
        self.lock()
            .remove(&conn)
            .map(|e| {
                let mut sessions: Vec<SessionId> =
                    e.auto_attached.into_iter().map(|(_, s)| s).collect();
                // 子のページ → tab の順（`CDP-7`）。registry の `detach` は両方の表に対応する。
                for ta in e.tab_attached {
                    sessions.extend(ta.child);
                    sessions.push(ta.tab_session);
                }
                Released {
                    sessions,
                    contexts: e.owned_contexts,
                }
            })
            .unwrap_or_default()
    }

    /// 生存中の接続の所有として browser context を作る。接続が生存中でなければ作らない
    /// （切断との競合で、誰も破棄しない context が残らないようにする。`CDP-7`）。
    pub(crate) fn create_owned_browser_context(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<BrowserContextId, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        let id = registry
            .create_browser_context()
            .map_err(ConnStateError::Registry)?;
        entry.owned_contexts.push(id.clone());
        Ok(id)
    }

    /// `Target.createTarget` の本体（#910・`CDP-2`・`CDP-7`）。生存確認・作成先 context の許可判定・
    /// 作成・自動 attach・一時停止の登録を 1 回のロック（本表 → registry）で行う。
    ///
    /// 作成先は既定 context か自接続が所有する context に限る。未知の context と他接続の context は
    /// どちらも `Registry(UnknownBrowserContext)` にして存在を漏らさない。自動 attach が有効なら
    /// 同時に attach して記録し（後段の reconcile で二重 attach しない）、`waitForDebuggerOnStart` が
    /// 有効ならそのセッションが再開するまで止める。セッション枠の枯渇ではターゲットも作らない（原子性）。
    pub(crate) fn create_target(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        context: Option<&BrowserContextId>,
        url: &str,
    ) -> Result<CreatedTarget, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        if let Some(c) = context
            && *c != registry.default_browser_context()
            && !entry.owned_contexts.contains(c)
        {
            return Err(ConnStateError::Registry(
                CdpStateError::UnknownBrowserContext,
            ));
        }
        if entry.auto_attach_enabled && entry.auto_attach_kind == AutoAttachKind::Tabs {
            // Puppeteer 経路: ページは tab へ attach する。ページのセッションは作らない（#1334）。
            let pause = entry.wait_for_debugger_on_start;
            let (tid, sid) = registry
                .create_target_tab_attached(context, TargetKind::Page, url, pause)
                .map_err(ConnStateError::Registry)?;
            let info = registry
                .target(&tid)
                .ok_or(ConnStateError::Registry(CdpStateError::UnknownTarget))?;
            entry.tab_attached.push(TabAttachment {
                page: tid,
                tab_id: info.tab_id().clone(),
                tab_session: sid.clone(),
                child: None,
            });
            let announce_created = Self::record_discovered(entry, &info);
            let announce_tab_created = Self::record_discovered_tab(entry, &info);
            return Ok(CreatedTarget {
                info,
                session: Some(sid),
                tab: true,
                announce_tab_created,
                waiting_for_debugger: pause,
                announce_created,
            });
        }
        if entry.auto_attach_enabled {
            let pause = entry.wait_for_debugger_on_start;
            let (tid, sid) = registry
                .create_target_attached(context, TargetKind::Page, url, pause)
                .map_err(ConnStateError::Registry)?;
            let info = registry
                .target(&tid)
                .ok_or(ConnStateError::Registry(CdpStateError::UnknownTarget))?;
            entry.auto_attached.push((tid, sid.clone()));
            let announce_created = Self::record_discovered(entry, &info);
            // discover が tab を含むなら tab の作成通知も先に送る（通知順序は tab → ページ → attach。`CDP-3`）。
            let announce_tab_created = Self::record_discovered_tab(entry, &info);
            Ok(CreatedTarget {
                info,
                session: Some(sid),
                tab: false,
                announce_tab_created,
                waiting_for_debugger: pause,
                announce_created,
            })
        } else {
            let tid = match context {
                Some(c) => registry.create_target_in(c, TargetKind::Page, url),
                None => registry.create_target(TargetKind::Page, url),
            }
            .map_err(ConnStateError::Registry)?;
            let info = registry
                .target(&tid)
                .ok_or(ConnStateError::Registry(CdpStateError::UnknownTarget))?;
            let announce_created = Self::record_discovered(entry, &info);
            let announce_tab_created = Self::record_discovered_tab(entry, &info);
            Ok(CreatedTarget {
                info,
                session: None,
                tab: false,
                announce_tab_created,
                waiting_for_debugger: false,
                announce_created,
            })
        }
    }

    /// 自動 attach を有効にし、未 attach の既存ターゲットすべてへ attach して
    /// `(targetInfo, sessionId)` を昇順で返す。
    /// 途中で失敗したらこの呼び出しの attach を巻き戻して `Err`（部分成功を残さず、有効化もしない。
    /// `wait_for_debugger_on_start` も更新しない。`CDP-7`）。成功時は値を置き換えて保持する。
    pub(crate) fn enable_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        wait_for_debugger_on_start: bool,
    ) -> Result<Vec<(TargetInfo, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        if entry.auto_attach_enabled && entry.auto_attach_kind != AutoAttachKind::Pages {
            return Err(ConnStateError::KindConflict);
        }
        let (added, err) = Self::attach_missing(entry, registry, true, true);
        if let Some(e) = err {
            return Err(e);
        }
        entry.auto_attach_enabled = true;
        entry.auto_attach_kind = AutoAttachKind::Pages;
        entry.wait_for_debugger_on_start = wait_for_debugger_on_start;
        Ok(added)
    }

    /// root の `Target.setAutoAttach` を tab 経路で有効にし、未 attach の可視ページの tab へ attach して
    /// `(ページの targetInfo, tab のセッション)` を昇順で返す（Puppeteer 経路。#1334・`CDP-3`・`CDP-7`）。
    /// 途中で失敗したらこの呼び出しの attach を巻き戻して `Err`（有効化もしない）。ページ経路が有効な間は
    /// `KindConflict`（状態は変えない）。対象は既定 context と自接続所有 context のページだけ。
    pub(crate) fn enable_tab_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        wait_for_debugger_on_start: bool,
    ) -> Result<Vec<(TargetInfo, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        if entry.auto_attach_enabled && entry.auto_attach_kind != AutoAttachKind::Tabs {
            return Err(ConnStateError::KindConflict);
        }
        let (added, err) = Self::attach_missing_tabs(entry, registry, true, true);
        if let Some(e) = err {
            return Err(e);
        }
        entry.auto_attach_enabled = true;
        entry.auto_attach_kind = AutoAttachKind::Tabs;
        entry.wait_for_debugger_on_start = wait_for_debugger_on_start;
        Ok(added)
    }

    /// 接続の自動 attach の種別（有効でなければ `None`）。
    pub(crate) fn auto_attach_kind(&self, conn: ConnId) -> Option<AutoAttachKind> {
        self.lock()
            .get(&conn)
            .filter(|e| e.auto_attach_enabled)
            .map(|e| e.auto_attach_kind)
    }

    /// この接続が払い出した tab のセッションか（tab のセッション宛ての `Target.setAutoAttach`・
    /// `Runtime.runIfWaitingForDebugger` の所有確認。他接続の ID を使わせない。#1334・`CDP-7`）。
    pub(crate) fn owns_tab_session(&self, conn: ConnId, sid: &SessionId) -> bool {
        self.lock()
            .get(&conn)
            .is_some_and(|e| e.tab_attached.iter().any(|t| t.tab_session == *sid))
    }

    /// tab のセッション宛ての `Target.setAutoAttach` の本体（#1334・`CDP-3`・`CDP-7`）。
    /// `auto_attach` が true なら子のページを 1 つ attach し（既に attach 済みなら変化なし）、false なら外す。
    /// `wait_for_debugger_on_start` は、tab が止めていたページの停止を子へ移すかの指示。
    /// この接続の tab のセッションでなければ `Registry(UnknownSession)`。ロック順序は本表 → registry。
    pub(crate) fn set_tab_child_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        tab_session: &SessionId,
        auto_attach: bool,
        wait_for_debugger_on_start: bool,
    ) -> Result<TabChildChange, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        let unknown = ConnStateError::Registry(CdpStateError::UnknownSession);
        let ta = entry
            .tab_attached
            .iter_mut()
            .find(|t| t.tab_session == *tab_session)
            .ok_or(unknown)?;
        if !registry.is_tab_session(tab_session) {
            return Err(ConnStateError::Registry(CdpStateError::UnknownSession));
        }
        if auto_attach {
            if ta.child.is_some() {
                return Ok(TabChildChange::None);
            }
            let (page, session, waiting) = registry
                .attach_child_page(tab_session, wait_for_debugger_on_start)
                .map_err(ConnStateError::Registry)?;
            let Some(info) = registry.target(&page) else {
                let _ = registry.detach(&session);
                return Err(ConnStateError::Registry(CdpStateError::UnknownTarget));
            };
            ta.child = Some(session.clone());
            Ok(TabChildChange::Attached {
                info,
                session,
                waiting_for_debugger: waiting,
            })
        } else {
            match ta.child.take() {
                Some(session) => {
                    let _ = registry.detach(&session);
                    Ok(TabChildChange::Detached {
                        page: ta.page.clone(),
                        session,
                    })
                }
                None => Ok(TabChildChange::None),
            }
        }
    }

    /// 自動 attach が有効な接続について、有効化後に registry へ現れたターゲットへ attach し、
    /// ターゲットの close 等で消えたセッションを回収する。無効・未登録の接続は空の結果を返す。
    /// ディスパッチ後（成功・失敗を問わず）に呼ばれる（push 型の通知経路が無いため、
    /// イベントは当該接続の次のコマンド応答に続けて送出される）。
    /// セッション枠の枯渇等で失敗しても、そこまでに attach できた分は残して `error` に失敗を載せる
    /// （全件ロールバックすると枠が空くまで永久に attach できないため。次回に残りを再試行する）。
    /// `attach_new` が false のときは新規 attach を行わず回収だけする（コマンドがエラーで終わった要求で
    /// 原子的 attach の巻き戻し直後に部分 attach し直さないため。後続要求で再試行する。`CDP-7`）。
    pub(crate) fn reconcile_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        attach_new: bool,
    ) -> Reconciled {
        let mut map = self.lock();
        let Some(entry) = map.get_mut(&conn) else {
            return Reconciled::default();
        };
        // discover の差分は attach より前に計算する（`attached` を自動 attach 前の実値にするため。#1324）。
        let diff = Self::discover_diff(entry, registry);
        let mut rec = Reconciled {
            created: diff.created,
            created_tabs: diff.created_tabs,
            destroyed: diff.destroyed,
            ..Reconciled::default()
        };
        if entry.auto_attach_enabled {
            match entry.auto_attach_kind {
                AutoAttachKind::Pages => {
                    let (attached, error) =
                        Self::attach_missing(entry, registry, false, attach_new);
                    rec.attached = attached;
                    rec.error = error;
                }
                AutoAttachKind::Tabs => {
                    let (attached, error) =
                        Self::attach_missing_tabs(entry, registry, false, attach_new);
                    rec.tab_attached = attached;
                    rec.error = error;
                }
            }
        }
        rec.detached = std::mem::take(&mut entry.pending_detached);
        rec.tab_detached = std::mem::take(&mut entry.pending_tab_detached);
        rec
    }

    /// `Target.setDiscoverTargets {discover:true}` の本体（#1324・`CDP-3`）。discover を有効にし、
    /// まだ通知していない可視ターゲット（既定 context と自接続所有 context のページ）を ID 昇順で
    /// `(targetInfo, attached)` として返す。記録済みで既に消えたターゲットは第 2 要素で返す。冪等（2 回目以降は新規分のみ）。
    /// 他接続の context のターゲットは通知対象にしない（`Target.getBrowserContexts` と同じ接続間分離。`CDP-7`）。
    ///
    /// `include_tabs` が true（`filter:[{}]`）なら tab 擬似ターゲットも通知対象にする（#1334）。false に変えた場合、
    /// 通知済みの tab の記録は `targetDestroyed` を出さずに捨てる（フィルタ変更であって消滅ではない）。
    pub(crate) fn enable_discover(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        include_tabs: bool,
    ) -> Result<DiscoverDiff, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        entry.discover_enabled = true;
        entry.discover_tabs = include_tabs;
        // 消滅分も返す（再送時に捨てると、記録が可視集合へ同期されて `targetDestroyed` が永久に欠落する）。
        Ok(Self::discover_diff(entry, registry))
    }

    /// `Target.setDiscoverTargets {discover:false}` の本体。通知済みの記録を捨てる
    /// （Chromium も無効化時に `Target.targetDestroyed` は送らない）。再度有効にすると全件を通知し直す。
    pub(crate) fn disable_discover(&self, conn: ConnId) -> Result<(), ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        entry.discover_enabled = false;
        entry.discover_tabs = false;
        entry.discovered.clear();
        Ok(())
    }

    /// ターゲットが接続から見えるか（既定 context か自接続所有 context。`Target.createTarget` の作成先規則と同じ）。
    fn is_visible(entry: &Entry, registry: &TargetRegistry, info: &TargetInfo) -> bool {
        let c = info.browser_context_id();
        *c == registry.default_browser_context() || entry.owned_contexts.contains(c)
    }

    /// discover 有効なら、記録と registry の可視ターゲットの差分を返して記録を同期する。
    /// 戻りは `(新規 (info, attached), 消滅 ID)`。無効なら空（記録も持たない）。
    fn discover_diff(entry: &mut Entry, registry: &TargetRegistry) -> DiscoverDiff {
        if !entry.discover_enabled {
            return DiscoverDiff::default();
        }
        if !entry.discover_tabs {
            // フィルタが tab を含まない: 通知済みの tab の記録は `targetDestroyed` を出さずに捨てる。
            entry.discovered.retain(|t| !is_tab_id(t));
        }
        let visible: Vec<(TargetInfo, bool)> = registry
            .targets_with_attached()
            .into_iter()
            .filter(|(i, _)| Self::is_visible(entry, registry, i))
            .collect();
        let visible_tabs: Vec<(TargetInfo, bool)> = if entry.discover_tabs {
            registry
                .tabs_with_attached()
                .into_iter()
                .filter(|(i, _)| Self::is_visible(entry, registry, i))
                .collect()
        } else {
            Vec::new()
        };
        let created: Vec<(TargetInfo, bool)> = visible
            .iter()
            .filter(|(i, _)| !entry.discovered.contains(i.target_id()))
            .cloned()
            .collect();
        let created_tabs: Vec<(TargetInfo, bool)> = visible_tabs
            .iter()
            .filter(|(i, _)| !entry.discovered.contains(i.tab_id()))
            .cloned()
            .collect();
        let mut now: Vec<TargetId> = visible.iter().map(|(i, _)| i.target_id().clone()).collect();
        now.extend(visible_tabs.iter().map(|(i, _)| i.tab_id().clone()));
        now.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        // 記録は ID 昇順（ページ → tab）なので、消滅もページ → tab の順になる。
        let destroyed: Vec<TargetId> = entry
            .discovered
            .iter()
            .filter(|t| !now.contains(t))
            .cloned()
            .collect();
        entry.discovered = now;
        DiscoverDiff {
            created,
            created_tabs,
            destroyed,
        }
    }

    /// `create_target` が作った tab を通知済みとして記録する。discover が無効か tab を含まなければ記録せず `false`。
    fn record_discovered_tab(entry: &mut Entry, info: &TargetInfo) -> bool {
        if !entry.discover_enabled || !entry.discover_tabs {
            return false;
        }
        if !entry.discovered.contains(info.tab_id()) {
            entry.discovered.push(info.tab_id().clone());
            entry.discovered.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        }
        true
    }

    /// `create_target` が作ったターゲットを通知済みとして記録する。discover 無効なら記録せず `false`。
    fn record_discovered(entry: &mut Entry, info: &TargetInfo) -> bool {
        if !entry.discover_enabled {
            return false;
        }
        if !entry.discovered.contains(info.target_id()) {
            entry.discovered.push(info.target_id().clone());
            entry.discovered.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        }
        true
    }

    /// 未 attach のターゲットへ attach する。`atomic` なら失敗時にこの呼び出しの attach を全て巻き戻して
    /// 空を返し、そうでなければ成功分を残す。registry 側で消えたセッションは所有記録から落として
    /// `pending_detached` に積む（通知は呼び出し側。`CDP-7`）。
    fn attach_missing(
        entry: &mut Entry,
        registry: &TargetRegistry,
        atomic: bool,
        attach_new: bool,
    ) -> (Vec<(TargetInfo, SessionId)>, Option<ConnStateError>) {
        let owned = std::mem::take(&mut entry.auto_attached);
        for (t, s) in owned {
            if registry.session_target(&s).is_some() {
                entry.auto_attached.push((t, s));
            } else {
                entry.pending_detached.push((t, s));
            }
        }
        let mut added: Vec<(TargetInfo, SessionId)> = Vec::new();
        let mut error = None;
        // attach_new が false なら新規 attach はせず消失セッションの回収だけ行う（`CDP-7`）。
        let candidates = if attach_new {
            registry.targets()
        } else {
            Vec::new()
        };
        for info in candidates {
            if entry
                .auto_attached
                .iter()
                .any(|(t, _)| t == info.target_id())
            {
                continue;
            }
            match registry.attach(info.target_id()) {
                Ok(sid) => added.push((info, sid)),
                Err(e) => {
                    error = Some(ConnStateError::Registry(e));
                    break;
                }
            }
        }
        if error.is_some() && atomic {
            for (_, sid) in &added {
                let _ = registry.detach(sid);
            }
            return (Vec::new(), error);
        }
        for (info, sid) in &added {
            entry
                .auto_attached
                .push((info.target_id().clone(), sid.clone()));
        }
        (added, error)
    }

    /// [`Self::attach_missing`] の tab 版（#1334）。registry 側で消えた tab の attach は
    /// `pending_tab_detached` に積み（通知は呼び出し側。`CDP-7`）、未 attach の可視ページの tab へ attach する。
    fn attach_missing_tabs(
        entry: &mut Entry,
        registry: &TargetRegistry,
        atomic: bool,
        attach_new: bool,
    ) -> (Vec<(TargetInfo, SessionId)>, Option<ConnStateError>) {
        let owned = std::mem::take(&mut entry.tab_attached);
        for ta in owned {
            if registry.is_tab_session(&ta.tab_session) {
                entry.tab_attached.push(ta);
            } else {
                entry.pending_tab_detached.push(TabDetach {
                    tab_id: ta.tab_id,
                    tab_session: ta.tab_session,
                    child: ta.child.map(|c| (ta.page, c)),
                });
            }
        }
        let mut added: Vec<(TargetInfo, SessionId)> = Vec::new();
        let mut error = None;
        let candidates = if attach_new {
            registry.targets()
        } else {
            Vec::new()
        };
        for info in candidates {
            if !Self::is_visible(entry, registry, &info)
                || entry
                    .tab_attached
                    .iter()
                    .any(|t| t.page == *info.target_id())
            {
                continue;
            }
            match registry.attach_tab(info.target_id()) {
                Ok(sid) => added.push((info, sid)),
                Err(e) => {
                    error = Some(ConnStateError::Registry(e));
                    break;
                }
            }
        }
        if error.is_some() && atomic {
            for (_, sid) in &added {
                let _ = registry.detach(sid);
            }
            return (Vec::new(), error);
        }
        for (info, sid) in &added {
            entry.tab_attached.push(TabAttachment {
                page: info.target_id().clone(),
                tab_id: info.tab_id().clone(),
                tab_session: sid.clone(),
                child: None,
            });
        }
        (added, error)
    }

    /// tab 経路の自動 attach を外し、通知すべき attach（子 → tab の順）を返す（#1334・`CDP-7`）。
    /// registry 側で既に消えていた分も通知対象に含める。種別が `Tabs` でなければ何も外さない。
    pub(crate) fn disable_tab_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<Vec<TabDetach>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        if entry.auto_attach_kind != AutoAttachKind::Tabs {
            return Ok(Vec::new());
        }
        entry.auto_attach_enabled = false;
        entry.auto_attach_kind = AutoAttachKind::Pages;
        entry.wait_for_debugger_on_start = false;
        let mut out = std::mem::take(&mut entry.pending_tab_detached);
        for ta in std::mem::take(&mut entry.tab_attached) {
            if let Some(c) = &ta.child {
                let _ = registry.detach(c);
            }
            let _ = registry.detach(&ta.tab_session);
            out.push(TabDetach {
                tab_id: ta.tab_id,
                tab_session: ta.tab_session,
                child: ta.child.map(|c| (ta.page, c)),
            });
        }
        Ok(out)
    }

    /// この接続が自動 attach したセッションを detach し、通知すべき `(targetId, sessionId)` を返す。
    /// registry 側で既に消えていて `detach` が失敗したセッションも通知対象に含める（`CDP-7`）。
    pub(crate) fn disable_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<Vec<(TargetId, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        if entry.auto_attach_kind == AutoAttachKind::Tabs {
            // tab 経路は `disable_tab_auto_attach` が外す（種別を取り違えて黙って何もしないのを防ぐ）。
            return Ok(Vec::new());
        }
        entry.auto_attach_enabled = false;
        entry.wait_for_debugger_on_start = false;
        let mut out = std::mem::take(&mut entry.pending_detached);
        for (t, s) in std::mem::take(&mut entry.auto_attached) {
            let _ = registry.detach(&s);
            out.push((t, s));
        }
        Ok(out)
    }

    /// ダウンロード設定を保持する（接続あたり 1 件。置き換え）。
    pub(crate) fn set_download(
        &self,
        conn: ConnId,
        settings: DownloadSettings,
    ) -> Result<(), ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        entry.download = Some(settings);
        Ok(())
    }

    /// 保持中の `waitForDebuggerOnStart`（テスト用。本番は [`Self::create_target`] が内部で参照する）。
    #[cfg(test)]
    pub(crate) fn wait_for_debugger_on_start(&self, conn: ConnId) -> bool {
        self.lock()
            .get(&conn)
            .is_some_and(|e| e.wait_for_debugger_on_start)
    }

    /// 保持中のダウンロード設定（実行系が未実装のためテストのみが読む）。
    #[cfg(test)]
    pub(crate) fn download(&self, conn: ConnId) -> Option<DownloadSettings> {
        self.lock().get(&conn).and_then(|e| e.download.clone())
    }

    /// 接続が閉じてよいターゲットを閉じ、自接続の自動 attach 記録から該当セッションを落として
    /// その SessionId を昇順で返す。discover 有効で通知済みなら `announce_destroyed` も返す（`Target.closeTarget`・TASK-43.3h・`CDP-2`・`CDP-7`）。
    /// 落とした分は `pending_detached` に積まない（`Target.detachedFromTarget` の送出は呼び出し側の
    /// ハンドラに一本化し、続く `reconcile_auto_attach` での二重送出を防ぐ）。他接続のセッションは
    /// その接続の次の reconcile で通知される。ロック順序は本表 → registry。
    pub(crate) fn close_target(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
        tid: &TargetId,
    ) -> Result<ClosedTarget, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        let tab_id = registry.target(tid).map(|i| i.tab_id().clone());
        registry
            .close_target_in_contexts(tid, &entry.owned_contexts)
            .map_err(ConnStateError::Registry)?;
        // 自接続が tab として attach していた分は、子 → tab の順で通知させる（#1334）。
        let mut tabs: Vec<TabDetach> = Vec::new();
        entry.tab_attached.retain(|ta| {
            if ta.page == *tid {
                tabs.push(TabDetach {
                    tab_id: ta.tab_id.clone(),
                    tab_session: ta.tab_session.clone(),
                    child: ta.child.clone().map(|c| (ta.page.clone(), c)),
                });
                false
            } else {
                true
            }
        });
        let mut own: Vec<SessionId> = Vec::new();
        entry.auto_attached.retain(|(t, s)| {
            if t == tid {
                own.push(s.clone());
                false
            } else {
                true
            }
        });
        own.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        // 通知済みなら記録から外し、続く reconcile で `targetDestroyed` を二重に送らない（#1324）。
        let before = entry.discovered.len();
        entry.discovered.retain(|t| t != tid);
        let announce_destroyed = entry.discovered.len() != before;
        let before_tab = entry.discovered.len();
        if let Some(t) = &tab_id {
            entry.discovered.retain(|d| d != t);
        }
        Ok(ClosedTarget {
            sessions: own,
            announce_destroyed,
            tabs,
            announce_tab_destroyed: entry.discovered.len() != before_tab,
            tab_id,
        })
    }

    /// この接続が所有する非既定 browser context を作成順（ID 昇順）で返す。
    ///
    /// `Target.getBrowserContexts`（#1323・`CDP-3`・`CDP-7`）から呼ばれる。既定 context は含まない。
    /// 本表だけを引き registry には触れない（ロック順序は本表 → registry のまま）。
    /// 未登録・切断済みの接続は `NotLive`（`release` と競合した場合も fail-closed）。
    pub(crate) fn owned_contexts(
        &self,
        conn: ConnId,
    ) -> Result<Vec<BrowserContextId>, ConnStateError> {
        self.lock()
            .get(&conn)
            .map(|e| e.owned_contexts.clone())
            .ok_or(ConnStateError::NotLive)
    }

    /// 生存中の接続数（テスト用）。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{MAX_SESSIONS, TargetKind};

    fn page(r: &TargetRegistry) -> TargetId {
        r.create_target(TargetKind::Page, "about:blank").unwrap()
    }

    #[test]
    fn cdp7_enable_auto_attach_is_idempotent_and_ordered() {
        let r = TargetRegistry::new();
        let (a, b) = (page(&r), page(&r));
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let added = cs.enable_auto_attach(c, &r, false).unwrap();
        let ids: Vec<&TargetId> = added.iter().map(|(i, _)| i.target_id()).collect();
        assert_eq!(ids, vec![&a, &b]);
        assert_eq!(r.session_count(), 2);
        assert!(cs.enable_auto_attach(c, &r, false).unwrap().is_empty());
        assert_eq!(r.session_count(), 2);
    }

    #[test]
    fn cdp7_auto_attach_state_persists_for_targets_created_later() {
        let r = TargetRegistry::new();
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        // 無効の間は何も attach しない。
        let _early = page(&r);
        assert_eq!(cs.reconcile_auto_attach(c, &r, true), Reconciled::default());
        assert_eq!(r.session_count(), 0);
        // 空でない registry で有効化した後に現れたターゲットも attach される。
        assert_eq!(cs.enable_auto_attach(c, &r, false).unwrap().len(), 1);
        let later = page(&r);
        let added = cs.reconcile_auto_attach(c, &r, true).attached;
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].0.target_id(), &later);
        assert_eq!(cs.reconcile_auto_attach(c, &r, true), Reconciled::default());
        // 無効化後は追従しない。
        cs.disable_auto_attach(c, &r).unwrap();
        let _third = page(&r);
        assert_eq!(cs.reconcile_auto_attach(c, &r, true), Reconciled::default());
        assert_eq!(r.session_count(), 0);
    }

    #[test]
    fn cdp7_reconcile_reports_vanished_sessions_once() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let sid = cs.enable_auto_attach(c, &r, false).unwrap().remove(0).1;
        r.close_target(&t).unwrap();
        let rec = cs.reconcile_auto_attach(c, &r, true);
        assert!(rec.attached.is_empty());
        assert_eq!(rec.detached, vec![(t, sid)]);
        assert_eq!(rec.error, None);
        assert_eq!(cs.reconcile_auto_attach(c, &r, true), Reconciled::default());
        assert!(cs.release(c).sessions.is_empty());
    }

    #[test]
    fn cdp7_reconcile_keeps_partial_progress_on_session_limit() {
        let r = TargetRegistry::new();
        let t1 = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        cs.enable_auto_attach(c, &r, false).unwrap();
        let _t2 = page(&r);
        let _t3 = page(&r);
        // 残り枠を 1 に絞る。
        while r.session_count() < MAX_SESSIONS - 1 {
            r.attach(&t1).unwrap();
        }
        let rec = cs.reconcile_auto_attach(c, &r, true);
        assert_eq!(rec.attached.len(), 1);
        assert_eq!(
            rec.error,
            Some(ConnStateError::Registry(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            }))
        );
        assert_eq!(r.session_count(), MAX_SESSIONS);
    }

    /// 2 ターゲット・空き枠 1 で再度 enable して巻き戻された直後、同じ要求内の
    /// `attach_new=false` の回収では部分 attach が残らない（`CDP-7`）。
    #[test]
    fn cdp7_reconcile_without_attach_new_leaves_no_partial_attach() {
        let r = TargetRegistry::new();
        let t1 = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        cs.enable_auto_attach(c, &r, false).unwrap();
        let _t2 = page(&r);
        let _t3 = page(&r);
        while r.session_count() < MAX_SESSIONS - 1 {
            r.attach(&t1).unwrap();
        }
        let before = r.session_count();
        assert!(cs.enable_auto_attach(c, &r, false).is_err());
        assert_eq!(r.session_count(), before);
        let rec = cs.reconcile_auto_attach(c, &r, false);
        assert!(rec.attached.is_empty());
        assert_eq!(rec.error, None);
        assert_eq!(r.session_count(), before);
    }

    #[test]
    fn cdp7_enable_auto_attach_rolls_back_on_session_limit() {
        let r = TargetRegistry::new();
        let t1 = page(&r);
        let _t2 = page(&r);
        for _ in 0..(MAX_SESSIONS - 1) {
            r.attach(&t1).unwrap();
        }
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let before = r.session_count();
        assert_eq!(
            cs.enable_auto_attach(c, &r, false),
            Err(ConnStateError::Registry(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            }))
        );
        assert_eq!(r.session_count(), before);
        assert!(cs.release(c).sessions.is_empty());
    }

    #[test]
    fn cdp7_release_returns_owned_sessions_only() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let cs = ConnectionScoped::new();
        let (c1, c2) = (ConnId::new(1), ConnId::new(2));
        cs.open(c1);
        cs.open(c2);
        let s1 = cs.enable_auto_attach(c1, &r, false).unwrap();
        assert_eq!(s1.len(), 1);
        // 別接続は同じターゲットへ独立に attach する。
        assert_eq!(cs.enable_auto_attach(c2, &r, false).unwrap().len(), 1);
        assert_eq!(r.session_count(), 2);
        assert_eq!(
            cs.release(c1),
            Released {
                sessions: vec![s1[0].1.clone()],
                contexts: vec![]
            }
        );
        assert_eq!(cs.len(), 1);
        assert_eq!(
            cs.enable_auto_attach(c1, &r, false),
            Err(ConnStateError::NotLive)
        );
        let _ = t;
    }

    #[test]
    fn cdp7_disable_auto_attach_detaches_only_owned() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let foreign = r.attach(&t).unwrap();
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let added = cs.enable_auto_attach(c, &r, false).unwrap();
        let out = cs.disable_auto_attach(c, &r).unwrap();
        assert_eq!(out, vec![(t.clone(), added[0].1.clone())]);
        assert_eq!(r.session_count(), 1);
        assert_eq!(r.session_target(&foreign), Some(t));
    }

    #[test]
    fn cdp7_disable_auto_attach_notifies_already_vanished_sessions() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let sid = cs.enable_auto_attach(c, &r, false).unwrap().remove(0).1;
        r.close_target(&t).unwrap();
        assert_eq!(cs.disable_auto_attach(c, &r).unwrap(), vec![(t, sid)]);
    }

    #[test]
    fn cdp7_close_target_drops_own_sessions_without_pending_and_notifies_others_once() {
        let r = TargetRegistry::new();
        let c = ConnectionScoped::new();
        let (a, b) = (ConnId::new(1), ConnId::new(2));
        c.open(a);
        c.open(b);
        let t = page(&r);
        c.enable_auto_attach(a, &r, false).unwrap();
        c.enable_auto_attach(b, &r, false).unwrap();
        let sa = c.close_target(a, &r, &t).unwrap().sessions;
        assert_eq!(sa.len(), 1);
        assert!(c.reconcile_auto_attach(a, &r, true).detached.is_empty());
        assert_eq!(c.reconcile_auto_attach(b, &r, true).detached.len(), 1);
        assert!(c.reconcile_auto_attach(b, &r, true).detached.is_empty());
    }

    #[test]
    fn cdp7_close_target_rejects_foreign_context_and_unregistered_connection() {
        let r = TargetRegistry::new();
        let c = ConnectionScoped::new();
        let (a, b) = (ConnId::new(1), ConnId::new(2));
        c.open(a);
        c.open(b);
        let ctx = c.create_owned_browser_context(a, &r).unwrap();
        let t = r
            .create_target_in(&ctx, TargetKind::Page, "about:blank")
            .unwrap();
        assert_eq!(
            c.close_target(b, &r, &t),
            Err(ConnStateError::Registry(CdpStateError::UnknownTarget))
        );
        assert!(r.target(&t).is_some());
        assert_eq!(
            c.close_target(ConnId::new(9), &r, &t),
            Err(ConnStateError::NotLive)
        );
        assert_eq!(c.close_target(a, &r, &t).unwrap().sessions, vec![]);
    }

    #[test]
    fn cdp7_owned_contexts_requires_live_connection_and_keeps_creation_order() {
        let r = TargetRegistry::new();
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        assert_eq!(cs.owned_contexts(c), Err(ConnStateError::NotLive));
        cs.open(c);
        assert_eq!(cs.owned_contexts(c), Ok(vec![]));
        let a = cs.create_owned_browser_context(c, &r).unwrap();
        let b = cs.create_owned_browser_context(c, &r).unwrap();
        assert_eq!(cs.owned_contexts(c), Ok(vec![a, b]));
        cs.release(c);
        assert_eq!(cs.owned_contexts(c), Err(ConnStateError::NotLive));
    }

    #[test]
    fn cdp7_owned_browser_contexts_are_returned_on_release_only_for_owner() {
        let r = TargetRegistry::new();
        let cs = ConnectionScoped::new();
        let (c1, c2) = (ConnId::new(1), ConnId::new(2));
        cs.open(c1);
        cs.open(c2);
        let a = cs.create_owned_browser_context(c1, &r).unwrap();
        let b = cs.create_owned_browser_context(c2, &r).unwrap();
        assert_eq!(cs.owned_contexts(c1), Ok(vec![a.clone()]));
        let rel = cs.release(c1);
        assert_eq!(rel.contexts, vec![a]);
        assert!(rel.sessions.is_empty());
        assert_eq!(cs.owned_contexts(c2), Ok(vec![b]));
        assert_eq!(cs.owned_contexts(c1), Err(ConnStateError::NotLive));
    }

    #[test]
    fn cdp7_create_owned_browser_context_requires_live_connection() {
        let r = TargetRegistry::new();
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        assert_eq!(
            cs.create_owned_browser_context(c, &r),
            Err(ConnStateError::NotLive)
        );
        cs.open(c);
        cs.release(c);
        assert_eq!(
            cs.create_owned_browser_context(c, &r),
            Err(ConnStateError::NotLive)
        );
        assert_eq!(r.browser_context_count(), 0);
    }

    #[test]
    fn cdp2_download_path_validation() {
        let abs = std::env::temp_dir();
        let abs_s = abs.to_str().unwrap().to_owned();
        assert_eq!(validate_download_path(&abs_s), Some(abs.clone()));
        let dotdot = abs.join("..").join("x");
        for bad in [
            "",
            "relative/dir",
            "./x",
            "a\0b",
            dotdot.to_str().unwrap(),
            &"/".repeat(MAX_DOWNLOAD_PATH_LEN + 1),
        ] {
            assert_eq!(validate_download_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cdp2_download_settings_debug_hides_path() {
        let s = DownloadSettings {
            behavior: DownloadBehavior::AllowAndName,
            download_path: Some(PathBuf::from("/secret/dir")),
            events_enabled: true,
        };
        let d = format!("{s:?}");
        assert!(!d.contains("secret"));
        assert!(d.contains("download_path_len: Some(11)"));
    }
    // ---- tab 経路（#1334・`CDP-3`・`CDP-7`） ----

    #[test]
    fn cdp7_enable_tab_auto_attach_rolls_back_on_session_limit_and_rejects_kind_switch() {
        let r = TargetRegistry::new();
        let t1 = page(&r);
        let _t2 = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        // 残り枠を 1 に絞ると、2 ページ分の tab attach は巻き戻って何も残らない。
        while r.session_count() < MAX_SESSIONS - 1 {
            r.attach(&t1).unwrap();
        }
        let before = r.session_count();
        assert_eq!(
            cs.enable_tab_auto_attach(c, &r, false),
            Err(ConnStateError::Registry(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            }))
        );
        assert_eq!(r.session_count(), before);
        assert_eq!(cs.auto_attach_kind(c), None);
    }

    #[test]
    fn cdp3_page_auto_attach_create_announces_discovered_tab_first() {
        let r = TargetRegistry::new();
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        cs.enable_discover(c, &r, true).unwrap();
        cs.enable_auto_attach(c, &r, false).unwrap();
        let created = cs.create_target(c, &r, None, "about:blank").unwrap();
        assert!(!created.tab);
        assert!(created.announce_tab_created);
        assert!(created.announce_created);
    }

    #[test]
    fn cdp7_tab_and_page_kinds_conflict_and_release_frees_child_then_tab() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let added = cs.enable_tab_auto_attach(c, &r, true).unwrap();
        let tab = added[0].1.clone();
        assert_eq!(cs.auto_attach_kind(c), Some(AutoAttachKind::Tabs));
        assert!(cs.owns_tab_session(c, &tab));
        assert!(!cs.owns_tab_session(ConnId::new(2), &tab));
        assert_eq!(
            cs.enable_auto_attach(c, &r, false),
            Err(ConnStateError::KindConflict)
        );
        let TabChildChange::Attached { session: child, .. } = cs
            .set_tab_child_auto_attach(c, &r, &tab, true, false)
            .unwrap()
        else {
            panic!("child must attach");
        };
        // 2 回目は変化なし。
        assert_eq!(
            cs.set_tab_child_auto_attach(c, &r, &tab, true, false),
            Ok(TabChildChange::None)
        );
        assert_eq!(r.session_count(), 2);
        let released = cs.release(c);
        assert_eq!(released.sessions, vec![child, tab]);
        for s in &released.sessions {
            r.detach(s).unwrap();
        }
        assert_eq!(r.session_count(), 0);
        assert!(r.target(&t).is_some());
    }
}

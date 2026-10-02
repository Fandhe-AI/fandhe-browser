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

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// 同時に保持できるターゲット数の上限（無制限確保による DoS の防止）。
pub const MAX_TARGETS: usize = 64;
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
}

impl fmt::Debug for TargetInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetInfo")
            .field("target_id", &self.target_id)
            .field("kind", &self.kind)
            .field("url_len", &self.url.len())
            .finish()
    }
}

#[derive(Debug)]
struct Tables {
    targets: HashMap<TargetId, TargetInfo>,
    sessions: HashMap<SessionId, TargetId>,
    next_target: u64,
    next_session: u64,
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
        Self::with_counters(1, 1)
    }

    fn with_counters(next_target: u64, next_session: u64) -> Self {
        Self {
            inner: Mutex::new(Tables {
                targets: HashMap::new(),
                sessions: HashMap::new(),
                next_target,
                next_session,
            }),
        }
    }

    /// poison は回復して使う（ライブラリで panic しない）。
    fn lock(&self) -> MutexGuard<'_, Tables> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// ターゲットを作成して ID を返す。件数・URL 長の上限超過・ID 枯渇時は表を変更しない。
    pub fn create_target(&self, kind: TargetKind, url: &str) -> Result<TargetId, CdpStateError> {
        // 借用したまま長さを検証し、超過入力は所有 String へ複製する前に拒否する（巨大入力による DoS 対策）。
        if url.len() > MAX_URL_LEN {
            return Err(CdpStateError::UrlTooLong { limit: MAX_URL_LEN });
        }
        let url = url.to_owned();
        let mut t = self.lock();
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
        };
        t.next_target = next;
        t.targets.insert(id.clone(), info);
        Ok(id)
    }

    /// ターゲット情報を返す。
    pub fn target(&self, id: &TargetId) -> Option<TargetInfo> {
        self.lock().targets.get(id).cloned()
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

    /// ターゲットを閉じ、配下セッションを削除して、その SessionId を返す
    /// （将来 `Target.detachedFromTarget` の送出に使う）。
    pub fn close_target(&self, id: &TargetId) -> Result<Vec<SessionId>, CdpStateError> {
        let mut t = self.lock();
        if t.targets.remove(id).is_none() {
            return Err(CdpStateError::UnknownTarget);
        }
        let mut removed: Vec<SessionId> = t
            .sessions
            .iter()
            .filter(|(_, tid)| *tid == id)
            .map(|(sid, _)| sid.clone())
            .collect();
        removed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for sid in &removed {
            t.sessions.remove(sid);
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
        self.lock()
            .sessions
            .remove(id)
            .ok_or(CdpStateError::UnknownSession)
    }

    /// セッションの attach 先ターゲットを返す。
    pub fn session_target(&self, id: &SessionId) -> Option<TargetId> {
        self.lock().sessions.get(id).cloned()
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
        let r = TargetRegistry::with_counters(u64::MAX, u64::MAX);
        assert_eq!(
            r.create_target(TargetKind::Page, "about:blank"),
            Err(CdpStateError::IdExhausted)
        );
        assert_eq!(r.target_count(), 0);

        let r = TargetRegistry::with_counters(1, u64::MAX);
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
}

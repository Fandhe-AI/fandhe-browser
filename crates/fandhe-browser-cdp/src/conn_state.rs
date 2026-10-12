//! WebSocket 接続スコープの CDP 状態（`Target.setAutoAttach` の自動 attach セッション所有と
//! `Browser.setDownloadBehavior` の設定。TASK-43.3c・#908、ビヘイビア `CDP-2`・`CDP-7`・MS-4）。
//!
//! `CdpState`（`server.rs`）が 1 つ内包し、`playwright_compat` のハンドラが読み書きする。
//! 接続の開始・終了は `crate::ws` が `CdpState::open_connection` / `release_connection` で通知する。
//! 構造は `dom::IssuedDocuments` と同じく「生存中の接続のみ記録を受け付ける」表で、切断との競合で
//! 閉じた接続の状態が残らないようにする。
//!
//! # 資源の所有と解放（`CDP-7`）
//!
//! [`crate::target::TargetRegistry`] のセッション表は全接続共有で [`crate::target::MAX_SESSIONS`] 件の
//! 上限を持つ。自動 attach で払い出したセッションは本表が接続ごとに所有し、切断で
//! [`ConnectionScoped::release`] が返す ID を呼び出し側が `detach` して必ず解放する
//! （解放漏れはセッション枠の枯渇、すなわち DoS になる）。ロック順序は常に「本表 → registry」。
//!
//! # スタブについて（実装済みを装わない。`REPAIR-3`）
//!
//! ダウンロード設定は検証して保持するだけで、保存処理・`Browser.downloadWillBegin` 等の送出は
//! 未実装（将来 `CDP-2` の実行系で参照する）。サーバーは `downloadPath` に対して一切の
//! ファイル操作をしない。実行系で書き込む際は、プロファイル境界（`PROF`）を踏まえた別途の
//! セキュリティ設計を要する。

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::protocol::ConnId;
use crate::target::{CdpStateError, SessionId, TargetId, TargetInfo, TargetRegistry};

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

    /// 保存先ディレクトリを要する挙動か。
    pub(crate) fn needs_path(self) -> bool {
        matches!(self, Self::Allow | Self::AllowAndName)
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

/// 接続 1 本分の状態。
#[derive(Default)]
struct Entry {
    /// 自動 attach が払い出したセッション（ターゲット, セッション）。
    auto_attached: Vec<(TargetId, SessionId)>,
    download: Option<DownloadSettings>,
}

/// 接続操作の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnStateError {
    /// 接続が生存中でない（切断済み・未登録）。
    NotLive,
    /// セッション枠の枯渇など registry 側の失敗。
    Registry(CdpStateError),
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

    /// 接続の状態を取り除き、所有していた自動 attach のセッション ID を返す。
    /// 呼び出し側（`CdpState::release_connection`）が registry で `detach` する。
    pub(crate) fn release(&self, conn: ConnId) -> Vec<SessionId> {
        self.lock()
            .remove(&conn)
            .map(|e| e.auto_attached.into_iter().map(|(_, s)| s).collect())
            .unwrap_or_default()
    }

    /// 未 attach の既存ターゲットすべてへ attach し、`(targetInfo, sessionId)` を昇順で返す。
    /// 途中で失敗したらこの呼び出しの attach を巻き戻して `Err`（部分成功を残さない。`CDP-7`）。
    pub(crate) fn enable_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<Vec<(TargetInfo, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        // ターゲットが閉じられて registry 側で消えたセッションは所有記録からも落とす。
        entry
            .auto_attached
            .retain(|(_, s)| registry.session_target(s).is_some());
        let mut added: Vec<(TargetInfo, SessionId)> = Vec::new();
        for info in registry.targets() {
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
                    for (_, sid) in &added {
                        let _ = registry.detach(sid);
                    }
                    return Err(ConnStateError::Registry(e));
                }
            }
        }
        for (info, sid) in &added {
            entry
                .auto_attached
                .push((info.target_id().clone(), sid.clone()));
        }
        Ok(added)
    }

    /// この接続が自動 attach したセッションを detach し、実際に外れた `(targetId, sessionId)` を返す。
    pub(crate) fn disable_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<Vec<(TargetId, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        let owned = std::mem::take(&mut entry.auto_attached);
        Ok(owned
            .into_iter()
            .filter(|(_, s)| registry.detach(s).is_ok())
            .collect())
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

    /// 保持中のダウンロード設定（実行系が未実装のためテストのみが読む）。
    #[cfg(test)]
    pub(crate) fn download(&self, conn: ConnId) -> Option<DownloadSettings> {
        self.lock().get(&conn).and_then(|e| e.download.clone())
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
        let added = cs.enable_auto_attach(c, &r).unwrap();
        let ids: Vec<&TargetId> = added.iter().map(|(i, _)| i.target_id()).collect();
        assert_eq!(ids, vec![&a, &b]);
        assert_eq!(r.session_count(), 2);
        assert!(cs.enable_auto_attach(c, &r).unwrap().is_empty());
        assert_eq!(r.session_count(), 2);
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
            cs.enable_auto_attach(c, &r),
            Err(ConnStateError::Registry(CdpStateError::TooManySessions {
                limit: MAX_SESSIONS
            }))
        );
        assert_eq!(r.session_count(), before);
        assert!(cs.release(c).is_empty());
    }

    #[test]
    fn cdp7_release_returns_owned_sessions_only() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let cs = ConnectionScoped::new();
        let (c1, c2) = (ConnId::new(1), ConnId::new(2));
        cs.open(c1);
        cs.open(c2);
        let s1 = cs.enable_auto_attach(c1, &r).unwrap();
        assert_eq!(s1.len(), 1);
        // 別接続は同じターゲットへ独立に attach する。
        assert_eq!(cs.enable_auto_attach(c2, &r).unwrap().len(), 1);
        assert_eq!(r.session_count(), 2);
        assert_eq!(cs.release(c1), vec![s1[0].1.clone()]);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs.enable_auto_attach(c1, &r), Err(ConnStateError::NotLive));
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
        let added = cs.enable_auto_attach(c, &r).unwrap();
        let out = cs.disable_auto_attach(c, &r).unwrap();
        assert_eq!(out, vec![(t.clone(), added[0].1.clone())]);
        assert_eq!(r.session_count(), 1);
        assert_eq!(r.session_target(&foreign), Some(t));
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
}

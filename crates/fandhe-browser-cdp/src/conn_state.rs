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
    /// `Target.setAutoAttach` が有効か（有効の間、後から現れたターゲットも attach 対象にする）。
    auto_attach_enabled: bool,
    /// registry 側で消えたため所有記録から落としたが、まだ `Target.detachedFromTarget` を送出していないもの。
    pending_detached: Vec<(TargetId, SessionId)>,
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

/// [`ConnectionScoped::reconcile_auto_attach`] の結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Reconciled {
    /// 新たに attach した `(targetInfo, sessionId)`（`Target.attachedToTarget` の送出対象）。
    pub attached: Vec<(TargetInfo, SessionId)>,
    /// 消失した `(targetId, sessionId)`（`Target.detachedFromTarget` の送出対象）。
    pub detached: Vec<(TargetId, SessionId)>,
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

    /// 接続の状態を取り除き、所有していた自動 attach のセッション ID を返す。
    /// 呼び出し側（`CdpState::release_connection`）が registry で `detach` する。
    pub(crate) fn release(&self, conn: ConnId) -> Vec<SessionId> {
        self.lock()
            .remove(&conn)
            .map(|e| e.auto_attached.into_iter().map(|(_, s)| s).collect())
            .unwrap_or_default()
    }

    /// 自動 attach を有効にし、未 attach の既存ターゲットすべてへ attach して
    /// `(targetInfo, sessionId)` を昇順で返す。
    /// 途中で失敗したらこの呼び出しの attach を巻き戻して `Err`（部分成功を残さず、有効化もしない。`CDP-7`）。
    pub(crate) fn enable_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<Vec<(TargetInfo, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        let (added, err) = Self::attach_missing(entry, registry, true, true);
        if let Some(e) = err {
            return Err(e);
        }
        entry.auto_attach_enabled = true;
        Ok(added)
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
        match map.get_mut(&conn) {
            Some(entry) if entry.auto_attach_enabled => {
                let (attached, error) = Self::attach_missing(entry, registry, false, attach_new);
                Reconciled {
                    attached,
                    detached: std::mem::take(&mut entry.pending_detached),
                    error,
                }
            }
            Some(entry) => Reconciled {
                detached: std::mem::take(&mut entry.pending_detached),
                ..Reconciled::default()
            },
            None => Reconciled::default(),
        }
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

    /// この接続が自動 attach したセッションを detach し、通知すべき `(targetId, sessionId)` を返す。
    /// registry 側で既に消えていて `detach` が失敗したセッションも通知対象に含める（`CDP-7`）。
    pub(crate) fn disable_auto_attach(
        &self,
        conn: ConnId,
        registry: &TargetRegistry,
    ) -> Result<Vec<(TargetId, SessionId)>, ConnStateError> {
        let mut map = self.lock();
        let entry = map.get_mut(&conn).ok_or(ConnStateError::NotLive)?;
        entry.auto_attach_enabled = false;
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

    /// 保持中のダウンロード設定（実行系が未実装のためテストのみが読む）。
    ///
    /// 読み手は `playwright_compat` の unix 限定ハンドラテスト（`CdpState` の構築に要る
    /// `Profile::open` が非 unix で `Unsupported` を返す）だけのため、非 unix では未使用になる。
    #[cfg(all(test, unix))]
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
        assert_eq!(cs.enable_auto_attach(c, &r).unwrap().len(), 1);
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
        let sid = cs.enable_auto_attach(c, &r).unwrap().remove(0).1;
        r.close_target(&t).unwrap();
        let rec = cs.reconcile_auto_attach(c, &r, true);
        assert!(rec.attached.is_empty());
        assert_eq!(rec.detached, vec![(t, sid)]);
        assert_eq!(rec.error, None);
        assert_eq!(cs.reconcile_auto_attach(c, &r, true), Reconciled::default());
        assert!(cs.release(c).is_empty());
    }

    #[test]
    fn cdp7_reconcile_keeps_partial_progress_on_session_limit() {
        let r = TargetRegistry::new();
        let t1 = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        cs.enable_auto_attach(c, &r).unwrap();
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
        cs.enable_auto_attach(c, &r).unwrap();
        let _t2 = page(&r);
        let _t3 = page(&r);
        while r.session_count() < MAX_SESSIONS - 1 {
            r.attach(&t1).unwrap();
        }
        let before = r.session_count();
        assert!(cs.enable_auto_attach(c, &r).is_err());
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
    fn cdp7_disable_auto_attach_notifies_already_vanished_sessions() {
        let r = TargetRegistry::new();
        let t = page(&r);
        let cs = ConnectionScoped::new();
        let c = ConnId::new(1);
        cs.open(c);
        let sid = cs.enable_auto_attach(c, &r).unwrap().remove(0).1;
        r.close_target(&t).unwrap();
        assert_eq!(cs.disable_auto_attach(c, &r).unwrap(), vec![(t, sid)]);
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

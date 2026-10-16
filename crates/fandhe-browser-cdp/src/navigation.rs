//! ターゲット別の遷移状態表 [`TargetNavigations`]（TASK-42（42.2・42.4）・#658、ビヘイビア `CDP-1`・`SEC-2`・MS-4）。
//!
//! `Page.navigate`（[`crate::page`]）が書き込み、`DOM.getDocument`（[`crate::dom`]）が
//! `sessionId` 付きで読む。ターゲット別の遷移状態の所有者をここ 1 か所にし、
//! `CdpState`（[`crate::server::CdpState`]）が保持する。crate 内部専用で公開 API は変えない。
//!
//! 保持件数はレジストリに登録済みのターゲット数以下（上限 `MAX_TARGETS`）。閉じたターゲットの
//! 状態は次の [`TargetNavigations::get_or_create`] で破棄し、作成・遷移・閉鎖の繰り返しで
//! 取得済み HTML が無制限に蓄積しないようにする（`SEC-2`）。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use fandhe_browser_core::NavigationState;

use crate::target::{TargetId, TargetRegistry};

/// `Page.getFrameTree` が返すメインフレームの現在状態（43.3i・#1037。将来の拡張に備え構造体にする。`REPAIR-4`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MainFrameSnapshot {
    /// 現在の文書 URL。
    pub(crate) url: String,
    /// 現在の文書の `loaderId`（世代の hex。`Page.navigate`・`Page.frameNavigated` と同じ書式）。
    pub(crate) loader_id: String,
}

/// 初期 `about:blank` を確定済みの新規状態を作る（begin + clear）。
/// `commit_initial_blank` と初期 `loaderId` の導出が同じ手順を共有し、値の食い違いを防ぐ。
fn new_committed_blank_state() -> Option<NavigationState> {
    let nav = NavigationState::new();
    nav.begin_navigation()
        .and_then(|g| nav.clear_navigation(g))
        .ok()?;
    Some(nav)
}

/// 初期 `about:blank` 確定後の `loaderId`（まだ状態を持たない止めたターゲットの `Page.getFrameTree` 用）。
fn initial_blank_loader_id() -> Option<String> {
    let nav = new_committed_blank_state()?;
    Some(format!("{:x}", nav.current_generation().get()))
}

/// ターゲット別の遷移状態表（`CDP-1` のターゲット境界）。
///
/// 別ターゲットへの `Page.navigate` が他ターゲットの HTML・URL を上書きしないよう分離する。
pub(crate) struct TargetNavigations {
    inner: Mutex<HashMap<TargetId, Arc<NavigationState>>>,
}

impl TargetNavigations {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// 閉じたターゲットの状態を破棄したうえで、`tid` の状態を返す（なければ作る）。
    ///
    /// `tid` がレジストリに無い場合は表へ挿入せず、表に載らない切り離された状態を返す
    /// （セッション解決と挿入の間にターゲットが閉じられても、件数が生存ターゲット数を超えない）。
    pub(crate) fn get_or_create(
        &self,
        registry: &TargetRegistry,
        tid: &TargetId,
    ) -> Arc<NavigationState> {
        let mut m = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        m.retain(|id, _| registry.target(id).is_some());
        if registry.target(tid).is_none() {
            return Arc::new(NavigationState::new());
        }
        Arc::clone(
            m.entry(tid.clone())
                .or_insert_with(|| Arc::new(NavigationState::new())),
        )
    }

    /// 新規ターゲットの初期 `about:blank` 文書を確定する（`Target.createTarget`・`Runtime.runIfWaitingForDebugger`
    /// から呼ばれる。#910・`CDP-2`）。取得を伴わない。
    ///
    /// 登録済みのターゲットで、遷移状態がまだ無いときだけ確定して `true` を返す。既に状態がある
    /// （`Page.navigate` が先に走った等）・未登録のターゲットは何もせず `false`（遷移を中断しない。表にも入れない）。
    pub(crate) fn commit_initial_blank(&self, registry: &TargetRegistry, tid: &TargetId) -> bool {
        let mut m = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        m.retain(|id, _| registry.target(id).is_some());
        if registry.target(tid).is_none() || m.contains_key(tid) {
            return false;
        }
        match new_committed_blank_state() {
            Some(nav) => {
                m.insert(tid.clone(), Arc::new(nav));
                true
            }
            None => false,
        }
    }

    /// `tid` のメインフレームの現在状態を読み取り専用で返す（`Page.getFrameTree`。43.3i・#1037・`CDP-2`）。
    ///
    /// 未登録のターゲットは `None`（呼び出し側が `invalid params` に写す）。状態を作らず
    /// `commit_initial_blank` も呼ばない。`url` は registry の値（止めたターゲットでは `about:blank`）、
    /// `loader_id` は遷移状態の現在世代の hex。状態が無い（止めた）ターゲットは
    /// 再開時に `commit_initial_blank` が作る世代と同じ値を返し、再開の前後で変わらない。
    ///
    /// 簡易実装（`REPAIR-3`）: 遷移中（begin 後・確定前）や取得失敗後は「進行中または失敗した世代」を返し、
    /// `url` も registry の値（直前の URL、失敗時は試行した URL）になる。Chromium は確定済みの直前文書を返す。
    /// また `url` と世代は別々に読むため、確定の瞬間に食い違う組を一瞬観測し得る。
    pub(crate) fn frame_snapshot(
        &self,
        registry: &TargetRegistry,
        tid: &TargetId,
    ) -> Option<MainFrameSnapshot> {
        let url = registry.target(tid)?.url().to_string();
        let loader_id = match self.get(tid) {
            Some(nav) => format!("{:x}", nav.current_generation().get()),
            None => initial_blank_loader_id()?,
        };
        Some(MainFrameSnapshot { url, loader_id })
    }

    /// 読み取り専用。表に無ければ `None`（作成しない）。
    pub(crate) fn get(&self, tid: &TargetId) -> Option<Arc<NavigationState>> {
        let m = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        m.get(tid).map(Arc::clone)
    }

    /// 閉じられたターゲットの状態を破棄する。
    pub(crate) fn forget(&self, tid: &TargetId) {
        let mut m = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        m.remove(tid);
    }

    /// 保持しているターゲット数。
    pub(crate) fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

impl fmt::Debug for TargetNavigations {
    /// HTML・URL は出さず件数だけを出す。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetNavigations")
            .field("targets", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{MAX_TARGETS, TargetKind};

    #[test]
    fn cdp2_commit_initial_blank_commits_once_and_skips_unknown_or_existing() {
        let reg = TargetRegistry::new();
        let t = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let n = TargetNavigations::new();
        assert!(n.commit_initial_blank(&reg, &t));
        assert!(n.get(&t).unwrap().latest().is_some());
        // 2 回目は何もしない。
        assert!(!n.commit_initial_blank(&reg, &t));
        // 既に状態がある（get_or_create 済み）ターゲットは中断しない。
        let t2 = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let st = n.get_or_create(&reg, &t2);
        assert!(!n.commit_initial_blank(&reg, &t2));
        assert!(st.latest().is_none());
        // 未登録のターゲットは表へ入れない。
        let ghost = TargetId::parse("ghost-1").unwrap();
        assert!(!n.commit_initial_blank(&reg, &ghost));
        assert_eq!(n.len(), 2);
    }

    #[test]
    fn cdp1_get_does_not_create_and_get_or_create_is_stable() {
        let reg = TargetRegistry::new();
        let t = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let n = TargetNavigations::new();
        assert!(n.get(&t).is_none());
        assert_eq!(n.len(), 0);
        let a = n.get_or_create(&reg, &t);
        let b = n.get_or_create(&reg, &t);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a, &n.get(&t).unwrap()));
        assert_eq!(n.len(), 1);
    }

    #[test]
    fn cdp1_closed_target_state_is_pruned() {
        let reg = TargetRegistry::new();
        let t1 = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let t2 = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let n = TargetNavigations::new();
        n.get_or_create(&reg, &t1);
        n.get_or_create(&reg, &t2);
        assert_eq!(n.len(), 2);
        reg.close_target(&t1).unwrap();
        n.get_or_create(&reg, &t2);
        assert_eq!(n.len(), 1);
        assert!(n.get(&t1).is_none());
        assert!(n.get(&t2).is_some());
    }

    #[test]
    fn cdp1_unregistered_target_is_not_inserted() {
        let reg = TargetRegistry::new();
        let t = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        reg.close_target(&t).unwrap();
        let n = TargetNavigations::new();
        let _detached = n.get_or_create(&reg, &t);
        assert_eq!(n.len(), 0);
        assert!(n.get(&t).is_none());
    }

    #[test]
    fn sec2_len_is_bounded_by_max_targets_across_churn() {
        let reg = TargetRegistry::new();
        let n = TargetNavigations::new();
        for _ in 0..3 {
            let ids: Vec<TargetId> = (0..MAX_TARGETS)
                .map(|_| reg.create_target(TargetKind::Page, "about:blank").unwrap())
                .collect();
            for id in &ids {
                n.get_or_create(&reg, id);
            }
            assert_eq!(n.len(), MAX_TARGETS);
            for id in &ids {
                reg.close_target(id).unwrap();
            }
        }
        let t = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        n.get_or_create(&reg, &t);
        assert_eq!(n.len(), 1);
    }

    #[test]
    fn cdp2_initial_blank_loader_id_matches_commit_initial_blank() {
        let reg = TargetRegistry::new();
        let t = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let n = TargetNavigations::new();
        assert!(n.commit_initial_blank(&reg, &t));
        let committed = format!("{:x}", n.get(&t).unwrap().current_generation().get());
        assert_eq!(committed, "2");
        assert_eq!(initial_blank_loader_id().as_deref(), Some("2"));
    }

    #[test]
    fn cdp2_frame_snapshot_uses_registry_url_and_generation() {
        use fandhe_browser_core::NavigationResult;
        let reg = TargetRegistry::new();
        let t = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let n = TargetNavigations::new();
        // 状態なし（止めたターゲット相当）。状態は作らない。
        let snap = n.frame_snapshot(&reg, &t).unwrap();
        assert_eq!(
            snap,
            MainFrameSnapshot {
                url: "about:blank".into(),
                loader_id: "2".into()
            }
        );
        assert_eq!(n.len(), 0);
        // 確定後も同じ値。
        assert!(n.commit_initial_blank(&reg, &t));
        assert_eq!(n.frame_snapshot(&reg, &t).unwrap().loader_id, "2");
        // 取得を伴う遷移の確定後は、その URL と世代。
        let t2 = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let st = n.get_or_create(&reg, &t2);
        let g = st.begin_navigation().unwrap();
        st.commit_navigation(g, NavigationResult::new("https://a.test/", ""))
            .unwrap();
        reg.set_target_url(&t2, "https://a.test/").unwrap();
        assert_eq!(
            n.frame_snapshot(&reg, &t2).unwrap(),
            MainFrameSnapshot {
                url: "https://a.test/".into(),
                loader_id: "1".into()
            }
        );
        // 未登録のターゲットは None。
        let ghost = TargetId::parse("ghost-1").unwrap();
        assert!(n.frame_snapshot(&reg, &ghost).is_none());
        assert_eq!(n.len(), 2);
    }

    #[test]
    fn cdp1_debug_shows_count_only() {
        let n = TargetNavigations::new();
        assert_eq!(format!("{n:?}"), "TargetNavigations { targets: 0 }");
    }
}

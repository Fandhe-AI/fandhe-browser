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
    fn cdp1_debug_shows_count_only() {
        let n = TargetNavigations::new();
        assert_eq!(format!("{n:?}"), "TargetNavigations { targets: 0 }");
    }
}

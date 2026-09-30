//! cdp crate 固有の状態型 `CdpState`（TASK-41（41.2）・#170、ビヘイビア `CDP-1`・`AISNAP-6`・MS-3）。
//!
//! core の共通 `AppState`（TASK-41.1）を変更せず `Arc` で内包し、その上に CDP 固有の
//! ターゲット表・セッション表（[`crate::target`]）とブラウザ ID を載せる。
//! `AppState` は cli（TASK-41.5）が生成して ai と同一インスタンスを共有する
//! （`AISNAP-6`）。本型は HTTP・WebSocket・非同期ランタイムの型を含まない。
//!
//! # スタブについて
//!
//! `/json/*` ルータ（TASK-41.3）と `/devtools/browser/{id}` 受け口（TASK-41.4）は
//! 後続タスクで本ファイルに追加する（REPAIR-3）。初期ターゲット（`about:blank`）の
//! 自動作成は行わず、後続タスクの判断に委ねる。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fandhe_browser_core::AppState;

use crate::target::{BrowserId, TargetRegistry};

/// `CdpState::new` が払い出すブラウザ ID 用のプロセス内連番。
static BROWSER_SEQ: AtomicU64 = AtomicU64::new(1);

/// CDP サーバーのルータ・ハンドラが共有する状態。共有は `Arc<CdpState>` で行う（`Clone` なし）。
///
/// ブラウザ ID は識別子であって認証情報ではない。アクセス制御は `SEC-4`
/// （ローカルホスト限定バインド等）と TASK-41.4・41.5 の責務。
pub struct CdpState {
    app: Arc<AppState>,
    browser_id: BrowserId,
    registry: TargetRegistry,
}

impl CdpState {
    /// 決定的（プロセス内連番）なブラウザ ID で構築する。
    pub fn new(app: Arc<AppState>) -> Self {
        let n = BROWSER_SEQ.fetch_add(1, Ordering::Relaxed);
        Self::with_browser_id(app, BrowserId::from_counter(n))
    }

    /// ブラウザ ID を指定して構築する（テスト・cli からの注入用）。
    pub fn with_browser_id(app: Arc<AppState>, browser_id: BrowserId) -> Self {
        Self {
            app,
            browser_id,
            registry: TargetRegistry::new(),
        }
    }

    /// 内包する core の共通状態。
    pub fn app_state(&self) -> &Arc<AppState> {
        &self.app
    }

    /// `/devtools/browser/{id}` の ID。
    pub fn browser_id(&self) -> &BrowserId {
        &self.browser_id
    }

    /// ターゲット・セッション表。
    pub fn registry(&self) -> &TargetRegistry {
        &self.registry
    }
}

impl fmt::Debug for CdpState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CdpState")
            .field("browser_id", &self.browser_id)
            .field("targets", &self.registry.target_count())
            .field("sessions", &self.registry.session_count())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp1_cdp_state_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CdpState>();
    }
}

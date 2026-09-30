//! `fandhe_browser_cdp` の状態型の公開 API を通した結合テスト
//! （TASK-41（41.2）・#170、ビヘイビア `CDP-1`・`AISNAP-6`・`PROF-1`）。
//!
//! `TargetRegistry` のテストは 3 OS 共通。`CdpState` の構築には `AppState`、すなわち
//! `Profile::open` が必要で、これは非 unix では `ProfileError::Unsupported` を返す仕様
//! （`XOS-7`〜`XOS-10` 未実装）のため、構築を伴うテストは `#[cfg(unix)]` とする。
//! 任意の skip ではなく、非 unix では `AppState` を構築する手段自体が無いためである。

use fandhe_browser_cdp::{CdpStateError, TargetKind, TargetRegistry};

#[test]
fn cdp1_public_api_create_attach_close_flow() {
    let r = TargetRegistry::new();
    let t = r
        .create_target(TargetKind::Page, "https://a.test/")
        .unwrap();
    let s = r.attach(&t).unwrap();
    assert_eq!(r.target(&t).unwrap().kind().as_str(), "page");
    assert_eq!(r.close_target(&t), Ok(vec![s.clone()]));
    assert_eq!(r.session_target(&s), None);
    assert_eq!((r.target_count(), r.session_count()), (0, 0));
}

#[test]
fn cdp1_error_display_is_english_and_hides_input() {
    assert_eq!(CdpStateError::UnknownTarget.to_string(), "unknown target");
    assert_eq!(
        CdpStateError::TooManyTargets { limit: 64 }.to_string(),
        "too many targets (limit 64)"
    );
}

#[cfg(unix)]
mod cdp_state {
    use fandhe_browser_cdp::{BrowserId, CdpState, TargetKind};
    use fandhe_browser_core::AppState;
    use fandhe_browser_profile::Profile;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// 一時ディレクトリ（drop で再帰削除。外部依存は追加しない）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            Self(base.join(format!("fandhe-cdp-state-test-{}-{n}", std::process::id())))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn app(dir: &TempDir) -> Arc<AppState> {
        let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
        Arc::new(AppState::with_disabled_renderer(profile))
    }

    #[test]
    fn cdp1_cdp_state_wraps_same_app_state_instance() {
        let dir = TempDir::new();
        let app = app(&dir);
        let cdp = CdpState::new(Arc::clone(&app));
        assert!(Arc::ptr_eq(cdp.app_state(), &app));

        let t = cdp
            .registry()
            .create_target(TargetKind::Page, "https://a.test/")
            .unwrap();
        cdp.registry().attach(&t).unwrap();
        assert_eq!(app.navigation().current_generation().get(), 0);
    }

    #[test]
    fn cdp1_browser_id_injection_and_uniqueness() {
        let dir = TempDir::new();
        let app = app(&dir);
        let a = CdpState::new(Arc::clone(&app));
        let b = CdpState::new(Arc::clone(&app));
        assert_ne!(a.browser_id(), b.browser_id());
        let c = CdpState::with_browser_id(app, BrowserId::parse("fixed-1").unwrap());
        assert_eq!(c.browser_id().as_str(), "fixed-1");
    }

    #[test]
    fn cdp1_debug_hides_url() {
        let dir = TempDir::new();
        let cdp = CdpState::new(app(&dir));
        cdp.registry()
            .create_target(TargetKind::Page, "https://secret.test/?token=x")
            .unwrap();
        let dbg = format!("{cdp:?}");
        assert!(!dbg.contains("secret") && dbg.contains("targets: 1"));
    }
}

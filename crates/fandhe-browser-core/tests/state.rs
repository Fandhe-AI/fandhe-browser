//! `fandhe_browser_core::state` の公開 API を通した結合テスト
//! （TASK-41（41.1）・#169、ビヘイビア `CDP-1`・`AISNAP-6`・`RENDER-1`・`PROF-1`）。
//!
//! `NavigationState` に関するテストは 3 OS 共通。`AppState` の構築には `Profile::open`
//! が必要で、これは非 unix では `ProfileError::Unsupported` を返す仕様
//! （`XOS-7`〜`XOS-10` 未実装）のため、構築を伴うテストは `#[cfg(unix)]` とする。
//! 任意の skip ではなく、非 unix では `AppState` を構築する手段自体が無いためである。

use fandhe_browser_core::{NavigationResult, NavigationState, StateError};

#[test]
fn cdp1_public_api_supersede_flow() {
    let s = NavigationState::new();
    let g1 = s.begin_navigation().unwrap();
    let g2 = s.begin_navigation().unwrap();
    assert_eq!((g1.get(), g2.get()), (1, 2));
    assert_eq!(
        s.commit_navigation(g1, NavigationResult::new("https://old/", "old")),
        Err(StateError::Superseded {
            attempted: g1,
            current: g2
        })
    );
    s.commit_navigation(g2, NavigationResult::new("https://new/", "<b>new</b>"))
        .unwrap();
    let r = s.latest().unwrap();
    assert_eq!((r.url(), r.html()), ("https://new/", "<b>new</b>"));
}

#[cfg(unix)]
mod app_state {
    use fandhe_browser_core::render::{
        BoundingBox, ElementRef, ElementVisibility, RenderError, Renderer, Screenshot,
        ScreenshotOptions,
    };
    use fandhe_browser_core::{AppState, NavigationResult};
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
            // macOS の `/var` symlink を避けるため正規化した基点から組み立てる。
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            Self(base.join(format!("fandhe-core-state-test-{}-{n}", std::process::id())))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 注入経路を再現するための fake。`bounding_box` だけ成功を返す。
    struct FakeRenderer;

    impl Renderer for FakeRenderer {
        fn capture_screenshot(&self, _: &ScreenshotOptions) -> Result<Screenshot, RenderError> {
            Err(RenderError::RenderingDisabled)
        }
        fn element_visibility(&self, _: &ElementRef) -> Result<ElementVisibility, RenderError> {
            Err(RenderError::RenderingDisabled)
        }
        fn bounding_box(&self, _: &ElementRef) -> Result<BoundingBox, RenderError> {
            Ok(BoundingBox::new(1.0, 2.0, 3.0, 4.0))
        }
    }

    fn open(dir: &TempDir) -> Arc<Profile> {
        Arc::new(Profile::open(&dir.0).expect("profile open"))
    }

    #[test]
    fn render1_with_disabled_renderer_returns_rendering_disabled() {
        let dir = TempDir::new();
        let state = AppState::with_disabled_renderer(open(&dir));
        let err = state
            .renderer()
            .capture_screenshot(&ScreenshotOptions::new())
            .unwrap_err();
        assert!(matches!(err, RenderError::RenderingDisabled));
    }

    #[test]
    fn render1_injected_renderer_is_used() {
        let dir = TempDir::new();
        let state = AppState::new(open(&dir), Arc::new(FakeRenderer));
        let bb = state
            .renderer()
            .bounding_box(&ElementRef::Selector("p".into()))
            .unwrap();
        assert_eq!(bb, BoundingBox::new(1.0, 2.0, 3.0, 4.0));
    }

    #[test]
    fn aisnap6_shared_arc_app_state_sees_same_navigation() {
        let dir = TempDir::new();
        let profile = open(&dir);
        let state = Arc::new(AppState::with_disabled_renderer(Arc::clone(&profile)));
        assert!(Arc::ptr_eq(state.profile(), &profile));

        let cdp = Arc::clone(&state);
        std::thread::spawn(move || {
            let g = cdp.navigation().begin_navigation().unwrap();
            cdp.navigation()
                .commit_navigation(g, NavigationResult::new("https://a.example/", "<p>a</p>"))
                .unwrap();
        })
        .join()
        .unwrap();

        let ai = Arc::clone(&state);
        let r = ai.navigation().latest().unwrap();
        assert_eq!((r.url(), r.html()), ("https://a.example/", "<p>a</p>"));
    }

    #[test]
    fn prof1_profile_root_is_exposed_via_profile_only() {
        let dir = TempDir::new();
        let state = AppState::with_disabled_renderer(open(&dir));
        assert_eq!(state.profile().root(), dir.0.as_path());
        assert!(format!("{state:?}").contains("generation"));
    }
}

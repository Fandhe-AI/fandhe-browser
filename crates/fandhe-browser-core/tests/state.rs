//! `fandhe_browser_core::state` の公開 API を通した結合テスト
//! （TASK-41（41.1）・#169、ビヘイビア `CDP-1`・`AISNAP-6`・`RENDER-1`・`PROF-1`）。
//!
//! `NavigationState` に関するテストも `AppState` 構築を伴うテストも 3 OS 共通。`AppState` の構築
//! には `Profile::open` が必要で、Windows でも `%LOCALAPPDATA%` 配下（一時ディレクトリを含む）
//! なら成功する（Issue #955）。

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

/// TASK-112（`JS-4`）: 遷移系列の識別子はインスタンスごとに異なり、インスタンスの生存中は不変。
#[test]
fn js4_navigation_scope_is_unique_per_state_and_stable() {
    let a = NavigationState::new();
    let b = NavigationState::new();
    assert_ne!(a.scope(), b.scope());
    let before = a.scope();
    a.begin_navigation().unwrap();
    assert_eq!(a.scope(), before);
}

mod app_state {
    use fandhe_browser_core::render::{
        BoundingBox, ElementRef, ElementVisibility, PageLoad, PageRef, RenderError, Renderer,
        Screenshot, ScreenshotOptions,
    };
    use fandhe_browser_core::{
        AppState, Config, NavigationResult, PageScriptExecutor, PageScriptHandle,
        ScriptContextHandle,
    };
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
            let base = fandhe_browser_profile::test_base_dir();
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

    /// `load` の呼び出し回数を数える fake（`RENDER-3`・TASK-38）。
    struct LoadCountingRenderer(Arc<AtomicUsize>);

    impl Renderer for LoadCountingRenderer {
        fn load(&self, page: &PageRef) -> Result<PageLoad, RenderError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PageLoad::new(page.url()))
        }
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

    #[test]
    fn render3_app_state_loads_only_when_generation_changes() {
        let dir = TempDir::new();
        let loads = Arc::new(AtomicUsize::new(0));
        let state = AppState::new(open(&dir), Arc::new(LoadCountingRenderer(loads.clone())));
        let el = ElementRef::Selector("p".into());

        // ページが無い間は load も描画も行わない。
        assert!(matches!(
            state.bounding_box(&el),
            Err(RenderError::PageNotLoaded)
        ));
        assert_eq!(loads.load(Ordering::SeqCst), 0);

        let g = state.navigation().begin_navigation().unwrap();
        state
            .navigation()
            .commit_navigation(g, NavigationResult::new("https://a.example/", "<p>a</p>"))
            .unwrap();
        let bb = BoundingBox::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(state.bounding_box(&el).unwrap(), bb);
        assert_eq!(state.bounding_box(&el).unwrap(), bb);
        assert!(matches!(
            state.capture_screenshot(&ScreenshotOptions::new()),
            Err(RenderError::RenderingDisabled)
        ));
        assert_eq!(loads.load(Ordering::SeqCst), 1);

        let g = state.navigation().begin_navigation().unwrap();
        state
            .navigation()
            .commit_navigation(g, NavigationResult::new("https://b.example/", "<p>b</p>"))
            .unwrap();
        assert_eq!(state.bounding_box(&el).unwrap(), bb);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
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

    /// TASK-112（`JS-1`）: 既定の `AppState` は無効なハンドルを持ち、スレッドを起動しない。
    #[test]
    fn js1_default_app_state_has_disabled_page_scripts() {
        let dir = TempDir::new();
        let state = AppState::with_disabled_renderer(open(&dir));
        assert!(!state.page_scripts().is_enabled());
        assert_eq!(state.page_scripts().stats().worker_spawns(), 0);
        assert!(format!("{state:?}").contains("page_scripts_enabled: false"));
    }

    /// 実行器を呼ばない（呼ばれたら `Err` を返す）テスト用の実行器。
    struct NeverExecutor;

    impl PageScriptExecutor for NeverExecutor {
        fn block_on<'a>(
            &self,
            _: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
        ) -> std::io::Result<()> {
            Err(std::io::Error::other("never"))
        }
    }

    /// TASK-112（`JS-1`・`JS-4`）: `with_page_scripts` で渡したハンドルが `AppState` から見える。
    /// エンジンなしビルドでは `true` の設定自体がエラーになるため、その分岐も検査する。
    #[test]
    fn js4_with_page_scripts_exposes_the_given_handle() {
        let dir = TempDir::new();
        let result = Config::from_toml_str("[js]\nexecute_page_scripts = true\n");
        if fandhe_browser_core::bundled_engines().is_empty() {
            assert!(result.is_err());
            return;
        }
        let config = result.expect("エンジンありなら受理");
        let handle = PageScriptHandle::new(config.js(), Arc::new(NeverExecutor));
        assert!(handle.is_enabled());
        let state = AppState::with_disabled_renderer(open(&dir)).with_page_scripts(handle);
        assert!(state.page_scripts().is_enabled());
        assert_eq!(state.page_scripts().stats().worker_spawns(), 0);
        assert!(format!("{state:?}").contains("page_scripts_enabled: true"));
    }
    /// TASK-44.5（`JS-1`・`PERF-7`）: 既定の `AppState` は無効な評価コンテキストを持ち、スレッドを起動しない。
    #[test]
    fn perf7_default_app_state_has_disabled_script_contexts() {
        let dir = TempDir::new();
        let state = AppState::with_disabled_renderer(open(&dir));
        assert!(!state.script_contexts().is_enabled());
        assert_eq!(state.script_contexts().stats().worker_spawns(), 0);
        assert!(format!("{state:?}").contains("script_contexts_enabled: false"));
    }

    /// TASK-44.5（`CDP-2`）: `with_script_contexts` で渡したハンドルが `AppState` から見え、構築だけでは
    /// ワーカーを起動しない。エンジンなし・生成不能の構成では無効のまま（成功を装わない）。
    #[test]
    fn cdp2_with_script_contexts_exposes_the_given_handle() {
        let dir = TempDir::new();
        let handle = ScriptContextHandle::new(Config::default().js());
        let expected = handle.is_enabled();
        let state = AppState::with_disabled_renderer(open(&dir)).with_script_contexts(handle);
        assert_eq!(state.script_contexts().is_enabled(), expected);
        assert_eq!(state.script_contexts().stats().worker_spawns(), 0);
        assert!(format!("{state:?}").contains(&format!("script_contexts_enabled: {expected}")));
    }
}

//! プラグインマニフェスト型の公開 API 結合テスト（`PLUG-2`・TASK-92.1・Issue #353、TASK-92.2・Issue #354）。

use fandhe_browser_ai::plugin_api::{ManifestError, PluginManifest, PluginTransport};
use serde_json::json;

#[test]
fn plug2_reference_plugin_manifest_round_trips_from_bytes() {
    let bytes = br#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio",
        "tools":["fetch","snapshot"],"permissions":["network.fetch"]}"#;
    let m = PluginManifest::from_slice(bytes).unwrap();
    assert_eq!(m.transport(), PluginTransport::Stdio);
    assert_eq!(
        m.to_value(),
        json!({"id":"mcp-ref","version":"0.1.0","transport":"stdio",
               "tools":["fetch","snapshot"],"permissions":["network.fetch"],
               "protocolVersion":"unspecified"})
    );
}

#[test]
fn plug2_invalid_manifest_bytes_are_rejected() {
    let bytes = br#"{"id":"x","version":"1.0.0","transport":"stdio","tools":["t"],"extra":1}"#;
    assert_eq!(
        PluginManifest::from_slice(bytes).unwrap_err(),
        ManifestError::UnknownField
    );
}

/// `AppState` の構築に `Profile::open` が必要で、非 unix では `Unsupported` を返す仕様のため
/// unix 限定（任意の skip ではなく構築手段自体が無いため）。純粋部は `src/plugin_api.rs` の
/// ユニットテストが 3 OS で検証する。
#[cfg(unix)]
mod ai_state {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use fandhe_browser_ai::plugin_api::{AiState, PluginManifest};
    use fandhe_browser_core::AppState;
    use fandhe_browser_profile::Profile;
    use serde_json::json;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            Self(base.join(format!("fandhe-ai-plugin-test-{}-{n}", std::process::id())))
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

    fn manifest(id: &str) -> PluginManifest {
        PluginManifest::from_value(
            &json!({"id": id, "version": "1.0.0", "transport": "stdio", "tools": ["t"]}),
        )
        .unwrap()
    }

    #[test]
    fn plug2_ai_state_registers_and_lists_plugins() {
        let dir = TempDir::new();
        let app = app(&dir);
        let state = AiState::new(app.clone());
        state.plugins().register(manifest("one")).unwrap();
        state.plugins().register(manifest("two")).unwrap();
        let ids: Vec<String> = state
            .plugins()
            .list()
            .iter()
            .map(|m| m.id().to_owned())
            .collect();
        assert_eq!(ids, ["one", "two"]);
        assert!(Arc::ptr_eq(state.app(), &app));
    }

    #[test]
    fn plug2_ai_state_leaves_navigation_untouched() {
        let dir = TempDir::new();
        let app = app(&dir);
        let state = AiState::new(app);
        let before = state.app().navigation().current_generation().get();
        state.plugins().register(manifest("one")).unwrap();
        assert_eq!(state.app().navigation().current_generation().get(), before);
    }

    #[test]
    fn plug2_ai_states_do_not_share_registries() {
        let (d1, d2) = (TempDir::new(), TempDir::new());
        let s1 = AiState::new(app(&d1));
        let s2 = AiState::new(app(&d2));
        s1.plugins().register(manifest("one")).unwrap();
        assert_eq!(s1.plugins().len(), 1);
        assert_eq!(s2.plugins().len(), 0);
    }
}

//! プラグインマニフェスト型の公開 API 結合テスト（`PLUG-2`・TASK-92.1・Issue #353、TASK-92.2・Issue #354）と、
//! 登録 → 一覧取得の通し結合テスト（`PLUG-2`・TASK-92.6・Issue #358・`MS-9`）。
//!
//! 各エンドポイント単体の詳細は `plugin_register.rs`・`plugin_list.rs` が担い、本ファイルは
//! `POST /ai/plugins/register` で登録した内容が `GET /ai/plugins` に現れる一連の流れ
//! （`mod http_flow`）と、それを `Profile` 非依存で 3 OS 検証する純粋部の通しテストを持つ。

use fandhe_browser_ai::api::{plugins_body, register_plugin};
use fandhe_browser_ai::plugin_api::{
    ManifestError, PluginManifest, PluginRegistry, PluginTransport,
};
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
               "protocolVersion":"unspecified","runtime":"unspecified","language":"unspecified"})
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

/// 登録 → 一覧取得の通し（純粋部。`Profile` 非依存のため 3 OS で実行する。`PLUG-2`・TASK-92.6）。
#[test]
fn plug2_register_then_list_round_trips_without_http() {
    let registry = PluginRegistry::new();
    let first = br#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio","tools":["a","b"],
        "permissions":["network.fetch"],"protocolVersion":"1"}"#;
    let second = br#"{"id":"second","version":"2.0.0","transport":"tcp","tools":["t"]}"#;
    assert_eq!(register_plugin(&registry, first).unwrap().id(), "mcp-ref");
    assert_eq!(register_plugin(&registry, second).unwrap().id(), "second");
    let listed: serde_json::Value = serde_json::from_slice(&plugins_body(&registry)).unwrap();
    assert_eq!(
        listed,
        json!({"plugins": [
            {"id": "mcp-ref", "version": "0.1.0", "transport": "stdio", "tools": ["a", "b"],
             "permissions": ["network.fetch"], "protocolVersion": "1", "runtime": "unspecified", "language": "unspecified"},
            {"id": "second", "version": "2.0.0", "transport": "tcp", "tools": ["t"],
             "permissions": [], "protocolVersion": "unspecified", "runtime": "unspecified", "language": "unspecified"},
        ]})
    );
}

/// `runtime`・`language` が登録 → 一覧で受け渡される（`PLUG-6`・TASK-98.1・Issue #389。純粋部で 3 OS 実行）。
#[test]
fn plug6_runtime_and_language_pass_through_register_and_list() {
    let registry = PluginRegistry::new();
    let declared = br#"{"id":"js-plugin","version":"1.0.0","transport":"stdio","tools":["t"],
        "runtime":"node","language":"javascript"}"#;
    let omitted = br#"{"id":"plain","version":"1.0.0","transport":"stdio","tools":["t"]}"#;
    register_plugin(&registry, declared).unwrap();
    register_plugin(&registry, omitted).unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&plugins_body(&registry)).unwrap();
    assert_eq!(listed["plugins"][0]["runtime"], "node");
    assert_eq!(listed["plugins"][0]["language"], "javascript");
    assert_eq!(listed["plugins"][1]["runtime"], "unspecified");
    assert_eq!(listed["plugins"][1]["language"], "unspecified");
}

/// 不正な `language` は登録されず、エラーは固定コードで入力値を含まない（`PLUG-6`）。
#[test]
fn plug6_register_rejects_invalid_language() {
    let registry = PluginRegistry::new();
    let body = br#"{"id":"bad","version":"1.0.0","transport":"stdio","tools":["t"],
        "language":"Rust"}"#;
    let e = register_plugin(&registry, body).unwrap_err();
    let text = e.to_string();
    assert_eq!(text, "manifest language is invalid");
    assert!(!text.contains("Rust"));
    assert_eq!(plugins_body(&registry), b"{\"plugins\":[]}");
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

/// HTTP 経由（`POST /ai/plugins/register` → `GET /ai/plugins`）の通しテスト。TCP bind はせず
/// `Router::dispatch` へ直接渡す。`AppState` 構築に `Profile::open` が必要で非 unix では
/// `Unsupported` のため unix 限定（任意の skip ではなく構築手段自体が無いため）。
#[cfg(unix)]
mod http_flow {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
    use fandhe_backend_http::response::Response;
    use fandhe_backend_routes::Router;
    use fandhe_browser_ai::api::{router, router_with_state};
    use fandhe_browser_ai::plugin_api::{AiState, MAX_PLUGINS};
    use fandhe_browser_core::{AppState, NavigationResult};
    use fandhe_browser_profile::Profile;
    use serde_json::{Value, json};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    const HOST: &str = "127.0.0.1:9222";
    const JSON_CT: &str = "application/json; charset=UTF-8";

    /// 一時ディレクトリ（drop で再帰削除）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap_or_else(|_| std::env::temp_dir());
            Self(base.join(format!(
                "fandhe-ai-plugin-flow-test-{}-{n}",
                std::process::id()
            )))
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

    fn navigate(app: &AppState, url: &str, html: &str) {
        let nav = app.navigation();
        let g = nav.begin_navigation().expect("begin");
        nav.commit_navigation(g, NavigationResult::new(url, html))
            .expect("commit");
    }

    async fn send(
        r: &Router,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Response {
        let mut raw = format!("{method} {path} HTTP/1.1\r\n");
        for (k, v) in headers {
            raw.push_str(&format!("{k}: {v}\r\n"));
        }
        raw.push_str("\r\n");
        let head = match parse_request_head(raw.as_bytes()).expect("parse") {
            ParseOutcome::Complete { head, .. } => head,
            ParseOutcome::Incomplete => panic!("incomplete request head"),
        };
        r.dispatch(&head, body.as_bytes()).await
    }

    async fn post_with(r: &Router, extra: &[(&str, &str)], body: &str) -> Response {
        let mut h = vec![("Host", HOST), ("Content-Type", "application/json")];
        h.extend_from_slice(extra);
        send(r, "POST", "/ai/plugins/register", &h, body).await
    }

    async fn post(r: &Router, body: &str) -> Response {
        post_with(r, &[], body).await
    }

    async fn get(r: &Router, path: &str) -> Response {
        send(r, "GET", path, &[("Host", HOST)], "").await
    }

    fn json_of(res: &Response) -> Value {
        serde_json::from_slice(&res.body).expect("json")
    }

    fn body_of(id: &str, version: &str) -> String {
        json!({"id": id, "version": version, "transport": "stdio", "tools": ["t"]}).to_string()
    }

    const FIRST: &str = r#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio","tools":["a","b"],
        "permissions":["network.fetch"],"protocolVersion":"1"}"#;
    const SECOND: &str = r#"{"id":"second","version":"2.0.0","transport":"tcp","tools":["t"]}"#;

    fn expected_two() -> Value {
        json!({"plugins": [
            {"id": "mcp-ref", "version": "0.1.0", "transport": "stdio", "tools": ["a", "b"],
             "permissions": ["network.fetch"], "protocolVersion": "1", "runtime": "unspecified", "language": "unspecified"},
            {"id": "second", "version": "2.0.0", "transport": "tcp", "tools": ["t"],
             "permissions": [], "protocolVersion": "unspecified", "runtime": "unspecified", "language": "unspecified"},
        ]})
    }

    #[tokio::test]
    async fn plug2_register_then_list_returns_posted_manifests_in_order() {
        let dir = TempDir::new();
        let r = router_with_state(Arc::new(AiState::new(app(&dir))));
        assert_eq!(get(&r, "/ai/plugins").await.body, b"{\"plugins\":[]}");
        let res = post(&r, FIRST).await;
        assert_eq!(res.status, 200);
        assert_eq!(json_of(&res), json!({"id": "mcp-ref", "ok": true}));
        let res = post(&r, SECOND).await;
        assert_eq!(res.status, 200);
        assert_eq!(json_of(&res), json!({"id": "second", "ok": true}));
        let res = get(&r, "/ai/plugins").await;
        assert_eq!(res.status, 200);
        assert_eq!(res.header("content-type"), Some(JSON_CT));
        assert_eq!(json_of(&res), expected_two());
    }

    #[tokio::test]
    async fn plug2_default_router_registers_then_lists() {
        let dir = TempDir::new();
        let r = router(app(&dir));
        assert_eq!(post(&r, SECOND).await.status, 200);
        let res = get(&r, "/ai/plugins").await;
        assert_eq!(res.status, 200);
        assert_eq!(
            json_of(&res),
            json!({"plugins": [
                {"id": "second", "version": "2.0.0", "transport": "tcp", "tools": ["t"],
                 "permissions": [], "protocolVersion": "unspecified", "runtime": "unspecified", "language": "unspecified"},
            ]})
        );
    }

    #[tokio::test]
    async fn plug2_rejected_registrations_do_not_appear_in_list() {
        let dir = TempDir::new();
        let r = router_with_state(Arc::new(AiState::new(app(&dir))));
        assert_eq!(post(&r, &body_of("keep", "1.0.0")).await.status, 200);
        let before = get(&r, "/ai/plugins").await.body;

        let unknown = r#"{"id":"evil","version":"1.0.0","transport":"stdio","tools":["t"],"x":1}"#;
        let res = post(&r, unknown).await;
        assert_eq!(res.status, 400);
        assert_eq!(json_of(&res)["code"], "unknown_field");

        let res = post(&r, &body_of("keep", "9.9.9")).await;
        assert_eq!(res.status, 409);
        assert_eq!(json_of(&res)["code"], "duplicate_plugin_id");

        let res = post_with(
            &r,
            &[("Origin", "https://evil.example")],
            &body_of("o", "1.0.0"),
        )
        .await;
        assert_eq!(res.status, 403);

        let h = [("Host", HOST), ("Content-Type", "text/plain")];
        let res = send(
            &r,
            "POST",
            "/ai/plugins/register",
            &h,
            &body_of("ct", "1.0.0"),
        )
        .await;
        assert_eq!(res.status, 415);

        let h = [
            ("Host", "evil.example"),
            ("Content-Type", "application/json"),
        ];
        let res = send(
            &r,
            "POST",
            "/ai/plugins/register",
            &h,
            &body_of("host", "1.0.0"),
        )
        .await;
        assert_eq!(res.status, 403);

        let after = get(&r, "/ai/plugins").await;
        assert_eq!(after.body, before);
        assert_eq!(
            json_of(&after),
            json!({"plugins": [
                {"id": "keep", "version": "1.0.0", "transport": "stdio", "tools": ["t"],
                 "permissions": [], "protocolVersion": "unspecified", "runtime": "unspecified", "language": "unspecified"},
            ]})
        );
    }

    #[tokio::test]
    async fn plug2_registry_limit_is_enforced_end_to_end() {
        let dir = TempDir::new();
        let r = router_with_state(Arc::new(AiState::new(app(&dir))));
        for i in 0..MAX_PLUGINS {
            let res = post(&r, &body_of(&format!("p{i}"), "1.0.0")).await;
            assert_eq!(res.status, 200, "i={i}");
        }
        let over = format!("p{MAX_PLUGINS}");
        let res = post(&r, &body_of(&over, "1.0.0")).await;
        assert_eq!(res.status, 429);
        assert_eq!(json_of(&res)["code"], "plugin_registry_full");
        let listed = json_of(&get(&r, "/ai/plugins").await);
        let ids: Vec<&str> = listed["plugins"]
            .as_array()
            .expect("array")
            .iter()
            .map(|p| p["id"].as_str().expect("id"))
            .collect();
        let last = format!("p{}", MAX_PLUGINS - 1);
        assert_eq!(ids.len(), MAX_PLUGINS);
        assert_eq!(ids.first().copied(), Some("p0"));
        assert_eq!(ids.last().copied(), Some(last.as_str()));
        assert!(!ids.contains(&over.as_str()));
    }

    #[tokio::test]
    async fn plug2_register_and_list_leave_snapshot_and_navigation_unchanged() {
        let dir = TempDir::new();
        let a = app(&dir);
        let r = router_with_state(Arc::new(AiState::new(Arc::clone(&a))));
        navigate(
            &a,
            "https://example.com/",
            "<title>Example Domain</title><h1>Example Domain</h1>",
        );
        let snap_before = get(&r, "/ai/snapshot").await;
        assert_eq!(snap_before.status, 200);
        let gen_before = a.navigation().current_generation();
        let latest = a.navigation().latest().expect("latest");
        let (url_before, html_before) = (latest.url().to_owned(), latest.html().to_owned());

        assert_eq!(post(&r, FIRST).await.status, 200);
        assert_eq!(get(&r, "/ai/plugins").await.status, 200);

        let snap_after = get(&r, "/ai/snapshot").await;
        assert_eq!(snap_after.status, 200);
        assert_eq!(snap_after.body, snap_before.body);
        assert_eq!(a.navigation().current_generation(), gen_before);
        let latest = a.navigation().latest().expect("latest");
        assert_eq!(latest.url(), url_before);
        assert_eq!(latest.html(), html_before);
    }

    #[tokio::test]
    async fn plug2_registered_plugins_persist_across_navigation() {
        let dir = TempDir::new();
        let a = app(&dir);
        let r = router_with_state(Arc::new(AiState::new(Arc::clone(&a))));
        assert_eq!(post(&r, FIRST).await.status, 200);
        assert_eq!(post(&r, SECOND).await.status, 200);
        let registered = get(&r, "/ai/plugins").await.body;
        navigate(&a, "https://a.example/", "<h1>A</h1>");
        assert_eq!(
            json_of(&get(&r, "/ai/snapshot").await)["url"],
            "https://a.example/"
        );
        assert_eq!(get(&r, "/ai/plugins").await.body, registered);
        navigate(&a, "https://b.example/", "<h1>B</h1>");
        assert_eq!(
            json_of(&get(&r, "/ai/snapshot").await)["url"],
            "https://b.example/"
        );
        assert_eq!(get(&r, "/ai/plugins").await.body, registered);
    }
}

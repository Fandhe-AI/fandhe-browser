//! `GET /ai/plugins` の結合テスト（`PLUG-2`・TASK-92.4・Issue #356・`MS-9`）。
//!
//! TCP bind はせず `Router::dispatch` へ直接渡す。`AppState` の構築に必要な `Profile::open` は
//! 非 unix で構築不能なため `#[cfg(unix)]`（`tests/api.rs` と同じ理由）。純粋部は
//! `src/api.rs` のユニットテストが 3 OS で検証する。

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_backend_routes::Router;
use fandhe_browser_ai::api::{router, router_with_state};
use fandhe_browser_ai::plugin_api::{AiState, PluginManifest};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::{Value, json};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!("fandhe-ai-plugins-test-{}-{n}", std::process::id())))
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

fn manifest(v: Value) -> PluginManifest {
    PluginManifest::from_value(&v).expect("manifest")
}

async fn get(router: &Router, path: &str, host: Option<&str>) -> Response {
    let mut raw = format!("GET {path} HTTP/1.1\r\n");
    if let Some(h) = host {
        raw.push_str(&format!("Host: {h}\r\n"));
    }
    raw.push_str("\r\n");
    let head = match parse_request_head(raw.as_bytes()).expect("parse") {
        ParseOutcome::Complete { head, .. } => head,
        ParseOutcome::Incomplete => panic!("incomplete request head"),
    };
    router.dispatch(&head, &[]).await
}

const HOST: Option<&str> = Some("127.0.0.1:9222");
const JSON_CT: &str = "application/json; charset=UTF-8";

fn json_of(res: &Response) -> Value {
    serde_json::from_slice(&res.body).expect("json")
}

#[tokio::test]
async fn plug2_get_plugins_returns_empty_array_when_unregistered() {
    let dir = TempDir::new();
    let r = router_with_state(Arc::new(AiState::new(app(&dir))));
    let res = get(&r, "/ai/plugins", HOST).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), Some(JSON_CT));
    assert_eq!(res.body, b"{\"plugins\":[]}");
}

#[tokio::test]
async fn plug2_get_plugins_returns_registered_manifests() {
    let dir = TempDir::new();
    let state = Arc::new(AiState::new(app(&dir)));
    state
        .plugins()
        .register(manifest(json!({
            "id": "mcp-ref", "version": "0.1.0", "transport": "stdio", "tools": ["a", "b"],
            "permissions": ["network.fetch"], "protocolVersion": "1"
        })))
        .expect("register");
    state
        .plugins()
        .register(manifest(
            json!({"id": "second", "version": "2.0.0", "transport": "tcp", "tools": ["t"]}),
        ))
        .expect("register");
    let r = router_with_state(state);
    let res = get(&r, "/ai/plugins", HOST).await;
    assert_eq!(res.status, 200);
    assert_eq!(
        json_of(&res),
        json!({"plugins": [
            {"id": "mcp-ref", "version": "0.1.0", "transport": "stdio", "tools": ["a", "b"],
             "permissions": ["network.fetch"], "protocolVersion": "1"},
            {"id": "second", "version": "2.0.0", "transport": "tcp", "tools": ["t"],
             "permissions": [], "protocolVersion": "unspecified"},
        ]})
    );
}

#[tokio::test]
async fn plug2_get_plugins_rejects_disallowed_host() {
    let dir = TempDir::new();
    let state = Arc::new(AiState::new(app(&dir)));
    state
        .plugins()
        .register(manifest(
            json!({"id": "secret-id", "version": "1.0.0", "transport": "stdio", "tools": ["t"]}),
        ))
        .expect("register");
    let r = router_with_state(state);
    let res = get(&r, "/ai/plugins", Some("evil.example")).await;
    assert_eq!(res.status, 403);
    assert_eq!(json_of(&res), json!({"code": "host_not_allowed"}));
    assert!(!String::from_utf8_lossy(&res.body).contains("secret-id"));
    let res = get(&r, "/ai/plugins", None).await;
    assert_eq!(res.status, 400);
    assert_eq!(json_of(&res), json!({"code": "invalid_host"}));
    assert!(!String::from_utf8_lossy(&res.body).contains("secret-id"));
}

#[tokio::test]
async fn plug2_get_plugins_does_not_change_snapshot() {
    let dir = TempDir::new();
    let a = app(&dir);
    let state = Arc::new(AiState::new(Arc::clone(&a)));
    state
        .plugins()
        .register(manifest(
            json!({"id": "p", "version": "1.0.0", "transport": "stdio", "tools": ["t"]}),
        ))
        .expect("register");
    let r = router_with_state(state);
    let before = a.navigation().current_generation();
    let res = get(&r, "/ai/plugins", HOST).await;
    assert_eq!(res.status, 200);
    let res = get(&r, "/ai/snapshot", HOST).await;
    assert_eq!(res.status, 409);
    assert_eq!(json_of(&res)["code"], "no_navigation");
    assert_eq!(a.navigation().current_generation(), before);
}

#[tokio::test]
async fn plug2_get_plugins_is_isolated_per_state() {
    let d1 = TempDir::new();
    let d2 = TempDir::new();
    let s1 = Arc::new(AiState::new(app(&d1)));
    s1.plugins()
        .register(manifest(
            json!({"id": "only-in-1", "version": "1.0.0", "transport": "stdio", "tools": ["t"]}),
        ))
        .expect("register");
    let r1 = router_with_state(s1);
    let r2 = router_with_state(Arc::new(AiState::new(app(&d2))));
    assert_eq!(get(&r1, "/ai/plugins", HOST).await.status, 200);
    let res = get(&r2, "/ai/plugins", HOST).await;
    assert_eq!(res.body, b"{\"plugins\":[]}");
}

#[tokio::test]
async fn plug2_get_plugins_via_default_router_is_empty() {
    let dir = TempDir::new();
    let r = router(app(&dir));
    let res = get(&r, "/ai/plugins", HOST).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, b"{\"plugins\":[]}");
}

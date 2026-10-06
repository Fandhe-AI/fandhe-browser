//! `fandhe_browser_ai::api::router` の `/ai/snapshot` 結合テスト
//! （TASK-19.1・Issue #223、TASK-19.2・Issue #224、ビヘイビア `AISNAP-6`・`AISNAP-14`・MS-4）。
//!
//! TCP bind はせず `Router::dispatch` へ直接渡す。`AppState` の構築には `Profile::open` が
//! 必要で、非 unix では `ProfileError::Unsupported` を返す仕様のため `#[cfg(unix)]` とする
//! （任意の skip ではなく構築手段自体が無いため）。純粋部は `src/api.rs` のユニットテストが
//! 3 OS で検証する。

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_ai::api::router;
use fandhe_browser_core::{AppState, NavigationResult};
use fandhe_browser_profile::Profile;
use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!("fandhe-ai-api-test-{}-{n}", std::process::id())))
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

async fn get(app: &Arc<AppState>, path: &str) -> Response {
    get_with_host(app, path, Some("127.0.0.1:9222")).await
}

/// `host`（None なら Host ヘッダ無し）付きで GET する。
async fn get_with_host(app: &Arc<AppState>, path: &str, host: Option<&str>) -> Response {
    let mut raw = format!("GET {path} HTTP/1.1\r\n");
    if let Some(h) = host {
        raw.push_str(&format!("Host: {h}\r\n"));
    }
    raw.push_str("\r\n");
    let head = match parse_request_head(raw.as_bytes()).expect("parse") {
        ParseOutcome::Complete { head, .. } => head,
        ParseOutcome::Incomplete => panic!("incomplete request head"),
    };
    router(Arc::clone(app)).dispatch(&head, &[]).await
}

const JSON_CT: &str = "application/json; charset=UTF-8";

#[tokio::test]
async fn aisnap6_get_snapshot_returns_json_of_latest_navigation() {
    let dir = TempDir::new();
    let app = app(&dir);
    navigate(
        &app,
        "https://example.com/",
        "<title>Example Domain</title><h1>Example Domain</h1>",
    );
    let res = get(&app, "/ai/snapshot").await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), Some(JSON_CT));
    let v: Value = serde_json::from_slice(&res.body).expect("json");
    assert_eq!(v["url"], "https://example.com/");
    assert_eq!(v["tree"]["role"], "document");
    assert_eq!(v["tree"]["name"], "Example Domain");
    assert_eq!(v["truncated"], false);
}

#[tokio::test]
async fn aisnap6_snapshot_follows_shared_state_updates() {
    let dir = TempDir::new();
    let app = app(&dir);
    navigate(&app, "https://a.example/", "<h1>A</h1>");
    let first: Value = serde_json::from_slice(&get(&app, "/ai/snapshot").await.body).expect("json");
    assert_eq!(first["url"], "https://a.example/");
    navigate(&app, "https://b.example/", "<h1>B</h1>");
    let second: Value =
        serde_json::from_slice(&get(&app, "/ai/snapshot").await.body).expect("json");
    assert_eq!(second["url"], "https://b.example/");
}

#[tokio::test]
async fn aisnap14_snapshot_before_navigation_is_409_no_navigation() {
    let dir = TempDir::new();
    let app = app(&dir);
    let res = get(&app, "/ai/snapshot").await;
    assert_eq!(res.status, 409);
    let v: Value = serde_json::from_slice(&res.body).expect("json");
    assert_eq!(v["code"], "no_navigation");
    assert_eq!(v["message"], "no navigation has been performed yet");
    // 409 の後にナビゲートすると同じ AppState で 200 になる
    navigate(&app, "https://example.com/", "<h1>x</h1>");
    assert_eq!(get(&app, "/ai/snapshot").await.status, 200);
}

#[tokio::test]
async fn aisnap14_host_check_precedes_navigation_check() {
    let dir = TempDir::new();
    let app = app(&dir);
    let res = get_with_host(&app, "/ai/snapshot", Some("evil.example:9222")).await;
    assert_eq!(res.status, 403);
    assert_eq!(res.body, br#"{"code":"host_not_allowed"}"#.to_vec());
}

#[tokio::test]
async fn aisnap6_snapshot_accepts_loopback_hosts() {
    let dir = TempDir::new();
    let app = app(&dir);
    navigate(&app, "https://example.com/", "<h1>x</h1>");
    for host in [
        "127.0.0.1:9222",
        "localhost:9222",
        "[::1]:9222",
        "localhost",
    ] {
        let res = get_with_host(&app, "/ai/snapshot", Some(host)).await;
        assert_eq!(res.status, 200, "host: {host}");
    }
}

#[tokio::test]
async fn aisnap6_snapshot_rejects_foreign_host_without_leaking_content() {
    let dir = TempDir::new();
    let app = app(&dir);
    navigate(&app, "https://secret.test/", "<h1>private</h1>");
    let res = get_with_host(&app, "/ai/snapshot", Some("evil.example:9222")).await;
    assert_eq!(res.status, 403);
    assert_eq!(res.body, br#"{"code":"host_not_allowed"}"#.to_vec());
    let text = String::from_utf8(res.body).expect("utf8");
    assert!(!text.contains("secret.test") && !text.contains("private"));
}

#[tokio::test]
async fn aisnap6_snapshot_rejects_missing_and_malformed_host() {
    let dir = TempDir::new();
    let app = app(&dir);
    navigate(&app, "https://example.com/", "<h1>x</h1>");
    for host in [None, Some("127.0.0.1:65536"), Some("[::1")] {
        let res = get_with_host(&app, "/ai/snapshot", host).await;
        assert_eq!(res.status, 400, "host: {host:?}");
        assert_eq!(res.body, br#"{"code":"invalid_host"}"#.to_vec());
    }
}

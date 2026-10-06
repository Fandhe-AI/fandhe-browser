//! `fandhe_browser_ai::api::router` の `/ai/snapshot` 結合テスト
//! （TASK-19.1・Issue #223、ビヘイビア `AISNAP-6`・MS-4）。
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
    let raw = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n\r\n");
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
async fn aisnap6_snapshot_before_navigation_is_409() {
    let dir = TempDir::new();
    let app = app(&dir);
    let res = get(&app, "/ai/snapshot").await;
    assert_eq!(res.status, 409);
    assert_eq!(res.body, br#"{"code":"no_navigation"}"#.to_vec());
}

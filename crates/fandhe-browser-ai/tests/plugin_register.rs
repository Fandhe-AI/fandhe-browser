//! `POST /ai/plugins/register` の結合テスト（`PLUG-2`・TASK-92.3・Issue #355・`MS-9`）。
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
use fandhe_backend_routes::Router;
use fandhe_browser_ai::api::{router, router_with_state};
use fandhe_browser_ai::plugin_api::AiState;
use fandhe_browser_core::AppState;
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

const JSON_CT: &str = "application/json; charset=UTF-8";
const VALID: &str = r#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio","tools":["t"]}"#;

/// `headers` をそのまま付けて `method path` を dispatch する。
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

async fn post(r: &Router, extra: &[(&str, &str)], body: &str) -> Response {
    let mut h = vec![
        ("Host", "127.0.0.1:9222"),
        ("Content-Type", "application/json"),
    ];
    h.extend_from_slice(extra);
    send(r, "POST", "/ai/plugins/register", &h, body).await
}

fn json_of(res: &Response) -> Value {
    serde_json::from_slice(&res.body).expect("json")
}

fn setup(dir: &TempDir) -> (Arc<AiState>, Router) {
    let state = Arc::new(AiState::new(app(dir)));
    let r = router_with_state(Arc::clone(&state));
    (state, r)
}

#[tokio::test]
async fn plug2_valid_manifest_is_registered() {
    let dir = TempDir::new();
    let (state, r) = setup(&dir);
    let res = post(&r, &[], VALID).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), Some(JSON_CT));
    assert_eq!(res.body, br#"{"id":"mcp-ref","ok":true}"#.to_vec());
    assert_eq!(state.plugins().len(), 1);
}

#[tokio::test]
async fn plug2_invalid_manifest_is_400_and_not_registered() {
    let dir = TempDir::new();
    let (state, r) = setup(&dir);
    let body =
        r#"{"id":"mcp-ref","version":"0.1.0","transport":"stdio","tools":["t"],"secretkey":1}"#;
    let res = post(&r, &[], body).await;
    assert_eq!(res.status, 400);
    let v = json_of(&res);
    assert_eq!(v["code"], "unknown_field");
    assert_eq!(v.as_object().expect("object").len(), 2);
    assert!(!String::from_utf8_lossy(&res.body).contains("secretkey"));
    assert_eq!(state.plugins().len(), 0);

    let res = post(&r, &[], "{").await;
    assert_eq!(res.status, 400);
    assert_eq!(json_of(&res)["code"], "invalid_json");

    let res = post(
        &r,
        &[],
        r#"{"id":"Bad_ID!","version":"0.1.0","transport":"stdio","tools":["t"]}"#,
    )
    .await;
    assert_eq!(res.status, 400);
    assert_eq!(json_of(&res)["code"], "invalid_id");
    assert!(!String::from_utf8_lossy(&res.body).contains("Bad_ID"));
    assert_eq!(state.plugins().len(), 0);
}

#[tokio::test]
async fn plug2_duplicate_id_is_409_and_first_is_kept() {
    let dir = TempDir::new();
    let (state, r) = setup(&dir);
    assert_eq!(post(&r, &[], VALID).await.status, 200);
    let again = VALID.replace("0.1.0", "9.9.9");
    let res = post(&r, &[], &again).await;
    assert_eq!(res.status, 409);
    assert_eq!(json_of(&res)["code"], "duplicate_plugin_id");
    assert_eq!(state.plugins().len(), 1);
    assert_eq!(state.plugins().list()[0].version(), "0.1.0");
}

#[tokio::test]
async fn plug2_origin_header_is_403() {
    let dir = TempDir::new();
    let (state, r) = setup(&dir);
    let res = post(&r, &[("Origin", "http://evil.example")], VALID).await;
    assert_eq!(res.status, 403);
    assert_eq!(json_of(&res)["code"], "origin_not_allowed");
    assert_eq!(state.plugins().len(), 0);
}

#[tokio::test]
async fn plug2_non_json_content_type_is_415() {
    let dir = TempDir::new();
    let (state, r) = setup(&dir);
    let h = [("Host", "127.0.0.1:9222"), ("Content-Type", "text/plain")];
    let res = send(&r, "POST", "/ai/plugins/register", &h, VALID).await;
    assert_eq!(res.status, 415);
    assert_eq!(json_of(&res)["code"], "unsupported_media_type");
    let h = [("Host", "127.0.0.1:9222")];
    let res = send(&r, "POST", "/ai/plugins/register", &h, VALID).await;
    assert_eq!(res.status, 415);
    assert_eq!(state.plugins().len(), 0);
}

#[tokio::test]
async fn plug2_bad_host_is_rejected() {
    let dir = TempDir::new();
    let (state, r) = setup(&dir);
    let h = [
        ("Host", "evil.example"),
        ("Content-Type", "application/json"),
    ];
    let res = send(&r, "POST", "/ai/plugins/register", &h, VALID).await;
    assert_eq!(res.status, 403);
    assert_eq!(json_of(&res)["code"], "host_not_allowed");
    let h = [("Content-Type", "application/json")];
    let res = send(&r, "POST", "/ai/plugins/register", &h, VALID).await;
    assert_eq!(res.status, 400);
    assert_eq!(json_of(&res)["code"], "invalid_host");
    assert_eq!(state.plugins().len(), 0);
}

#[tokio::test]
async fn plug2_default_router_entry_accepts_registration_and_keeps_snapshot() {
    let dir = TempDir::new();
    let r = router(app(&dir));
    let res = post(&r, &[], VALID).await;
    assert_eq!(res.status, 200);
    let h = [("Host", "127.0.0.1:9222")];
    let snap = send(&r, "GET", "/ai/snapshot", &h, "").await;
    assert_eq!(snap.status, 409);
    assert_eq!(json_of(&snap)["code"], "no_navigation");
}

//! `fandhe_browser_cdp::router` の `/json/*` 結合テスト
//! （TASK-41（41.3）・#172、ビヘイビア `CDP-1`・MS-3）。
//!
//! TCP bind はせず、`parse_request_head` で作ったリクエストヘッドを `Router::dispatch` へ
//! 直接渡す。`CdpState` の構築には `AppState`、すなわち `Profile::open` が必要で、これは
//! 非 unix では `ProfileError::Unsupported` を返す仕様（`XOS-7`〜`XOS-10` 未実装）のため
//! `#[cfg(unix)]` とする。任意の skip ではなく、非 unix では `AppState` を構築する手段自体が
//! 無いためである（`tests/state.rs` と同じ理由）。

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_cdp::{BrowserId, CdpState, TargetKind, router};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::{Value, json};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除。外部依存は追加しない）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!("fandhe-cdp-json-test-{}-{n}", std::process::id())))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state(dir: &TempDir) -> Arc<CdpState> {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    Arc::new(CdpState::with_browser_id(
        app,
        BrowserId::parse("fixed-1").unwrap(),
    ))
}

/// `method path` を `host`（None なら Host 無し）付きで dispatch する。
async fn request(state: &Arc<CdpState>, method: &str, path: &str, host: Option<&str>) -> Response {
    let mut raw = format!("{method} {path} HTTP/1.1\r\n");
    if let Some(h) = host {
        raw.push_str(&format!("Host: {h}\r\n"));
    }
    raw.push_str("\r\n");
    let head = match parse_request_head(raw.as_bytes()).expect("parse") {
        ParseOutcome::Complete { head, .. } => head,
        ParseOutcome::Incomplete => panic!("incomplete request head"),
    };
    router(Arc::clone(state)).dispatch(&head, &[]).await
}

fn json_of(res: &Response) -> Value {
    serde_json::from_slice(&res.body).expect("json body")
}

const JSON_CT: &str = "application/json; charset=UTF-8";

#[tokio::test]
async fn cdp1_json_version_has_ws_url() {
    let dir = TempDir::new();
    let st = state(&dir);
    let res = request(&st, "GET", "/json/version", Some("127.0.0.1:9222")).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("content-type"), Some(JSON_CT));
    let v = json_of(&res);
    assert_eq!(
        v["webSocketDebuggerUrl"],
        "ws://127.0.0.1:9222/devtools/browser/fixed-1"
    );
    assert!(v.get("V8-Version").is_none());
    let product = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
    assert_eq!(v["Browser"], product.as_str());
    assert_eq!(v["Protocol-Version"], "1.3");
}

#[tokio::test]
async fn cdp1_json_version_ws_url_brackets_ipv6_authority() {
    let dir = TempDir::new();
    let st = state(&dir);
    let res = request(&st, "GET", "/json/version", Some("[::1]:9222")).await;
    assert_eq!(res.status, 200);
    assert_eq!(
        json_of(&res)["webSocketDebuggerUrl"],
        "ws://[::1]:9222/devtools/browser/fixed-1"
    );
}

#[tokio::test]
async fn cdp1_json_list_reflects_registry() {
    let dir = TempDir::new();
    let st = state(&dir);
    let res = request(&st, "GET", "/json/list", Some("localhost:9222")).await;
    assert_eq!(res.status, 200);
    assert_eq!(json_of(&res), json!([]));

    let a = st
        .registry()
        .create_target(TargetKind::Page, "https://a.test/")
        .unwrap();
    let b = st
        .registry()
        .create_target(TargetKind::Page, "https://b.test/")
        .unwrap();
    let res = request(&st, "GET", "/json/list", Some("localhost:9222")).await;
    assert_eq!(res.header("content-type"), Some(JSON_CT));
    let v = json_of(&res);
    assert_eq!(v[0]["id"], a.as_str());
    assert_eq!(v[0]["type"], "page");
    assert_eq!(v[0]["url"], "https://a.test/");
    assert_eq!(v[1]["id"], b.as_str());
    assert_eq!(v[1]["url"], "https://b.test/");
    assert_eq!(v.as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn cdp1_json_alias_matches_json_list() {
    let dir = TempDir::new();
    let st = state(&dir);
    st.registry()
        .create_target(TargetKind::Page, "https://a.test/")
        .unwrap();
    let list = request(&st, "GET", "/json/list", Some("127.0.0.1:9222")).await;
    let alias = request(&st, "GET", "/json", Some("127.0.0.1:9222")).await;
    assert_eq!(alias.status, 200);
    assert_eq!(alias.body, list.body);
}

#[tokio::test]
async fn cdp1_json_rejects_foreign_and_missing_host() {
    let dir = TempDir::new();
    let st = state(&dir);
    for path in ["/json/version", "/json/list", "/json"] {
        let res = request(&st, "GET", path, Some("evil.example:9222")).await;
        assert_eq!(res.status, 403, "path {path}");
        assert_eq!(json_of(&res), json!({"error": "host not allowed"}));
        assert!(!String::from_utf8_lossy(&res.body).contains("evil"));

        let res = request(&st, "GET", path, None).await;
        assert_eq!(res.status, 400, "path {path}");
        assert_eq!(json_of(&res), json!({"error": "invalid host header"}));
    }
}

#[tokio::test]
async fn cdp1_json_non_get_is_405_and_unknown_path_is_404() {
    let dir = TempDir::new();
    let st = state(&dir);
    let res = request(&st, "POST", "/json/version", Some("127.0.0.1:9222")).await;
    assert_eq!(res.status, 405);
    let res = request(&st, "GET", "/json/nope", Some("127.0.0.1:9222")).await;
    assert_eq!(res.status, 404);
}

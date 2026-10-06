//! CDP がスタブ状態でも `GET /ai/snapshot` を単体で使えることを検証するための
//! テスト用サーバー組み立てヘルパー
//! （TASK-20.1・Issue #473、ビヘイビア `AISNAP-7`・MS-4）。
//! 後続の TASK-20.2（#474）・TASK-20.3（#475）が同じヘルパーへテストを足す。
//!
//! # 前提
//!
//! CDP 側の対象メソッド（`Page.captureScreenshot` 等）は未実装・スタブのままである。
//! 本ファイルの CDP 側ルータ（[`stub_cdp_router`]）は CDP の実装ではなく、固定の未実装応答
//! （JSON-RPC の `-32601` 相当）を返す代役であり、成功を装わない（`REPAIR-3`）。
//!
//! # 設計上の判断
//!
//! - 実 `fandhe-browser-cdp` は使わない: ai と cdp の相互依存は禁止
//!   （AGENTS.md「crate 間の許可依存」）。実 cdp との合成・`AppState` 共有は
//!   cli のテスト（TASK-19.3・19.4、`AISNAP-6`）が担う。
//! - TCP bind はしない: ai の依存に bind 手段が無く、追加は承認事項のため
//!   合成済み `Router` を `Router::dispatch` で直接駆動する（`tests/api.rs` と同方式）。
//! - `#![cfg(unix)]`: `Profile::open` が非 unix で `Unsupported` を返し `AppState` を
//!   作れないため（`tests/api.rs` と同じ。任意の skip ではない）。

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_backend_routes::Router;
use fandhe_browser_core::{AppState, NavigationResult};
use fandhe_browser_profile::Profile;
use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// スタブ CDP が返す固定の未実装応答本文。
const STUB_ERROR_BODY: &str = r#"{"error":{"code":-32601,"message":"method not implemented"}}"#;

/// 一時ディレクトリ（drop で再帰削除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!(
            "fandhe-ai-independence-test-{}-{n}",
            std::process::id()
        )))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// スタブ状態の CDP 側ルータ（代役）。CDP 側は未実装・スタブである前提で、
/// `GET /json/version` と CDP メソッド呼び出しの代役 `POST /cdp-stub/call` が
/// 固定の未実装応答（501）を返す。加えて、`AppState` 共有の検証用プローブ
/// `GET /cdp-stub/state` が共有ナビゲート状態の最新 URL を JSON で返す
/// （CDP の成功応答ではなくテスト専用の観測口。`url` は未ナビゲートなら `null`）。
fn stub_cdp_router(app: Arc<AppState>) -> Router {
    let version_app = Arc::clone(&app);
    let state_app = Arc::clone(&app);
    Router::new()
        .route("GET", "/json/version", move |_head, _body| {
            let _ = &version_app;
            stub_error()
        })
        .route("GET", "/cdp-stub/state", move |_head, _body| {
            let url = state_app.navigation().latest().map(|n| n.url().to_string());
            let body = serde_json::json!({ "url": url }).to_string();
            Response::new(200, body.into_bytes())
                .with_content_type("application/json; charset=UTF-8")
        })
        .route("POST", "/cdp-stub/call", move |_head, _body| {
            let _ = &app;
            stub_error()
        })
}

fn stub_error() -> Response {
    Response::new(501, STUB_ERROR_BODY.as_bytes().to_vec())
        .with_content_type("application/json; charset=UTF-8")
}

/// 同一 `AppState` を共有するスタブ CDP ルータと AI API ルータの合成サーバー。
struct StubServer {
    app: Arc<AppState>,
    router: Router,
    _dir: TempDir,
}

impl StubServer {
    /// `AppState` を 1 つ作り、スタブ CDP（未実装・スタブ前提）と
    /// `fandhe_browser_ai::api::router` を同一状態で合成する。
    fn start() -> Self {
        let dir = TempDir::new();
        let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
        let app = Arc::new(AppState::with_disabled_renderer(profile));
        let router = stub_cdp_router(Arc::clone(&app))
            .merge(fandhe_browser_ai::api::router(Arc::clone(&app)))
            .expect("route conflict");
        Self {
            app,
            router,
            _dir: dir,
        }
    }

    /// 共有 `AppState`（後続テストが要求の合間に状態を設定する用）。
    fn app(&self) -> &Arc<AppState> {
        &self.app
    }

    /// 共有 `AppState` へナビゲート結果を確定させる。
    fn navigate(&self, url: &str, html: &str) {
        let nav = self.app.navigation();
        let g = nav.begin_navigation().expect("begin");
        nav.commit_navigation(g, NavigationResult::new(url, html))
            .expect("commit");
    }

    /// `host`（None なら Host ヘッダ無し）付きでリクエストを合成ルータへ渡す。
    async fn request(&self, method: &str, path: &str, host: Option<&str>) -> Response {
        let mut raw = format!("{method} {path} HTTP/1.1\r\n");
        if let Some(h) = host {
            raw.push_str(&format!("Host: {h}\r\n"));
        }
        if method == "POST" {
            raw.push_str("Content-Length: 0\r\n");
        }
        raw.push_str("\r\n");
        let head = match parse_request_head(raw.as_bytes()).expect("parse") {
            ParseOutcome::Complete { head, .. } => head,
            ParseOutcome::Incomplete => panic!("incomplete request head"),
        };
        self.router.dispatch(&head, &[]).await
    }

    /// Host `127.0.0.1:9222` の GET。
    async fn get(&self, path: &str) -> Response {
        self.request("GET", path, Some("127.0.0.1:9222")).await
    }
}

#[tokio::test]
async fn aisnap7_stub_cdp_and_ai_routers_share_one_app_state() {
    let server = StubServer::start();

    let version = server.get("/json/version").await;
    assert_eq!(version.status, 501);
    assert_eq!(version.body, STUB_ERROR_BODY.as_bytes().to_vec());
    let call = server
        .request("POST", "/cdp-stub/call", Some("127.0.0.1:9222"))
        .await;
    assert_eq!(call.status, 501);
    assert_eq!(call.body, STUB_ERROR_BODY.as_bytes().to_vec());

    let snap = server.get("/ai/snapshot").await;
    assert_eq!(snap.status, 409);
    let v: Value = serde_json::from_slice(&snap.body).expect("json");
    assert_eq!(v["code"], "no_navigation");
    assert_eq!(v["message"], "no navigation has been performed yet");
}

/// スタブ側プローブと AI 側が同一 `AppState` を観測することを具体値で確認する
/// （別 `AppState` を渡すと CDP 側が `null` のままとなり失敗する）。
#[tokio::test]
async fn aisnap7_stub_probe_and_ai_observe_same_navigation_state() {
    let server = StubServer::start();

    let before = server.get("/cdp-stub/state").await;
    assert_eq!(before.status, 200);
    let b: Value = serde_json::from_slice(&before.body).expect("json");
    assert_eq!(b["url"], Value::Null);

    server.navigate("https://example.com/shared", "<h1>Shared</h1>");

    let cdp = server.get("/cdp-stub/state").await;
    let c: Value = serde_json::from_slice(&cdp.body).expect("json");
    assert_eq!(c["url"], "https://example.com/shared");
    let snap = server.get("/ai/snapshot").await;
    let a: Value = serde_json::from_slice(&snap.body).expect("json");
    assert_eq!(a["url"], c["url"]);
}

#[tokio::test]
async fn aisnap7_helper_navigate_is_visible_through_composed_router() {
    let server = StubServer::start();
    server.navigate(
        "https://example.com/",
        "<title>Example Domain</title><h1>Example Domain</h1>",
    );

    let snap = server.get("/ai/snapshot").await;
    assert_eq!(snap.status, 200);
    let v: Value = serde_json::from_slice(&snap.body).expect("json");
    assert_eq!(v["url"], "https://example.com/");
    assert_eq!(v["tree"]["role"], "document");
    let latest = server.app().navigation().latest().expect("latest");
    assert_eq!(latest.url(), "https://example.com/");

    let foreign = server
        .request("GET", "/ai/snapshot", Some("evil.example:9222"))
        .await;
    assert_eq!(foreign.status, 403);
    assert_eq!(foreign.body, br#"{"code":"host_not_allowed"}"#.to_vec());
}

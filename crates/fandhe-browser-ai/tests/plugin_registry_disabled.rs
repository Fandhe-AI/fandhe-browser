//! `plugin-registry` feature が off のとき、プラグイン系 2 ルートが存在せず 404 になることの
//! 結合テスト（`PERF-7`・`PLUG-2`・TASK-104・Issue #822）。
//!
//! 空配列や成功を返して実装済みを装わないこと（REPAIR-3）を確認し、ルータ自体は生きていること
//! （`/ai/snapshot`）を対照として確かめる。feature on のビルドでは本ファイルは空になる。
//! TCP bind はせず `Router::dispatch` へ直接渡す。

#![cfg(not(feature = "plugin-registry"))]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_ai::api::router;
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = fandhe_browser_profile::test_base_dir();
        Self(base.join(format!(
            "fandhe-ai-plugins-off-test-{}-{n}",
            std::process::id()
        )))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn dispatch(raw: &str, body: &[u8]) -> Response {
    let dir = TempDir::new();
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    let r = router(app);
    let head = match parse_request_head(raw.as_bytes()).expect("parse") {
        ParseOutcome::Complete { head, .. } => head,
        ParseOutcome::Incomplete => panic!("incomplete request head"),
    };
    r.dispatch(&head, body).await
}

/// PERF-7・PLUG-2: off では `GET /ai/plugins` は 404。
#[tokio::test]
async fn perf7_list_route_is_absent_when_registry_disabled() {
    let res = dispatch(
        "GET /ai/plugins HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n\r\n",
        &[],
    )
    .await;
    assert_eq!(res.status, 404);
}

/// PERF-7・PLUG-2: off では `POST /ai/plugins/register` も 404（正しいマニフェストでも登録されない）。
#[tokio::test]
async fn perf7_register_route_is_absent_when_registry_disabled() {
    let body = br#"{"id":"p","version":"1.0.0","transport":"stdio","tools":["t"]}"#;
    let raw = format!(
        "POST /ai/plugins/register HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let res = dispatch(&raw, body).await;
    assert_eq!(res.status, 404);
}

/// PERF-7: 対照。off でも `/ai/snapshot` は生きており、未ナビゲートは 409 `no_navigation`。
#[tokio::test]
async fn perf7_snapshot_route_still_served_when_registry_disabled() {
    let res = dispatch(
        "GET /ai/snapshot HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n\r\n",
        &[],
    )
    .await;
    assert_eq!(res.status, 409);
    let v: Value = serde_json::from_slice(&res.body).expect("json");
    assert_eq!(v["code"], "no_navigation");
}

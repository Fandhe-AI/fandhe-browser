//! 操作モードの `GET /ai/snapshot`（同期・最小間隔・`generation`・`mode`）を、偽の描画ホストで確かめる
//! 結合テスト（TASK-125・`AISNAP-20`・`AISNAP-21`）。
//!
//! 偽ホストは core の `Renderer` を実装し、`dom_snapshot` で番号付きの HTML を順に返す。操作モードの
//! 許可は core の `test-support`（dev-dependencies で有効にする）の入口だけで立てる。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_ai::api::router_with_state;
use fandhe_browser_ai::plugin_api::AiState;
use fandhe_browser_core::interactive::{
    HostDomSnapshot, InteractivePageId, build_interactive_document,
};
use fandhe_browser_core::render::{
    BoundingBox, ElementRef, ElementVisibility, RenderError, Renderer, Screenshot,
    ScreenshotOptions,
};
use fandhe_browser_core::{AppState, NavigationResult};
use fandhe_browser_profile::Profile;
use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(
            fandhe_browser_profile::test_base_dir()
                .join(format!("fandhe-ai-interactive-{}-{n}", std::process::id())),
        )
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `dom_snapshot` で用意した応答を順に返す偽ホスト。呼ばれた回数を数える。
#[derive(Default)]
struct FakeHost {
    replies: Mutex<VecDeque<Result<HostDomSnapshot, RenderError>>>,
    calls: AtomicUsize,
}

impl FakeHost {
    fn push(&self, reply: Result<HostDomSnapshot, RenderError>) {
        self.replies.lock().unwrap().push_back(reply);
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Renderer for FakeHost {
    fn capture_screenshot(&self, _: &ScreenshotOptions) -> Result<Screenshot, RenderError> {
        Err(RenderError::RenderingDisabled)
    }
    fn element_visibility(&self, _: &ElementRef) -> Result<ElementVisibility, RenderError> {
        Err(RenderError::RenderingDisabled)
    }
    fn bounding_box(&self, _: &ElementRef) -> Result<BoundingBox, RenderError> {
        Err(RenderError::RenderingDisabled)
    }
    fn dom_snapshot(&self) -> Result<HostDomSnapshot, RenderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(RenderError::SessionLost))
    }
}

fn host_snapshot(session: &str, doc_seq: u64, body: &str) -> HostDomSnapshot {
    let html = format!(
        r#"<html data-fandhe-n="0"><head data-fandhe-n="1"><title data-fandhe-n="2">Form</title></head><body data-fandhe-n="3">{body}</body></html>"#
    );
    HostDomSnapshot::new(
        InteractivePageId::new(session, doc_seq).unwrap(),
        "https://a.example/form",
        html,
        32,
        "snap",
    )
    .unwrap()
}

const FORM: &str = r#"<label data-fandhe-n="4">Agree <input type="checkbox" data-fandhe-n="5"></label><h1 data-fandhe-n="6">initial-heading</h1>"#;
const FILLED: &str = r#"<label data-fandhe-n="4">Agree <input type="checkbox" checked data-fandhe-n="5"></label><h1 data-fandhe-n="6">Saved</h1>"#;

/// 操作モードで確定済みの状態を作る（navigate の代わりに最初の同期結果を保存する）。
fn interactive_state(dir: &TempDir, host: Arc<FakeHost>) -> Arc<AiState> {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile"));
    let app = AppState::new(profile, host).allow_interactive_for_tests();
    let g = app.navigation().begin_navigation().unwrap();
    app.commit_interactive(
        g,
        build_interactive_document(host_snapshot("s1", 1, FORM)).unwrap(),
    )
    .unwrap();
    Arc::new(AiState::new(Arc::new(app)))
}

async fn get_snapshot(state: &Arc<AiState>) -> Response {
    let raw = "GET /ai/snapshot HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n\r\n";
    let head = match parse_request_head(raw.as_bytes()).expect("parse") {
        ParseOutcome::Complete { head, .. } => head,
        ParseOutcome::Incomplete => panic!("incomplete"),
    };
    router_with_state(Arc::clone(state))
        .dispatch(&head, &[])
        .await
}

fn json(res: &Response) -> Value {
    serde_json::from_slice(&res.body).expect("json")
}

/// `AISNAP-20`: 読み取りの前にホストの DOM を取り直し、操作の結果（チェック状態・変わった見出し）を静的モードと
/// 同じ形式で返す。応答は `generation`・`mode` を含む。1 秒以内の 2 回目は取り直さない（設計 §14.6）。
#[tokio::test]
async fn aisnap20_snapshot_reflects_host_dom_and_keeps_min_interval() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    host.push(Ok(host_snapshot("s1", 2, FILLED)));
    let state = interactive_state(&dir, Arc::clone(&host));

    let res = get_snapshot(&state).await;
    assert_eq!(res.status, 200);
    let v = json(&res);
    assert_eq!(
        (v["generation"].clone(), v["mode"].clone()),
        (1.into(), "interactive".into())
    );
    assert_eq!(v["url"], "https://a.example/form");
    let text = String::from_utf8(res.body.clone()).unwrap();
    assert!(text.contains(r#""checked":true"#), "{text}");
    assert!(text.contains("Saved"), "{text}");
    assert!(!text.contains("initial-heading"), "{text}");
    assert!(!text.contains("data-fandhe-n"), "{text}");
    let latest = state.app().navigation().latest().unwrap();
    assert_eq!(latest.interactive().unwrap().revision(), 2);

    // 直前の同期から 1 秒未満なので取り直さない（用意した応答が無くても成功する）。
    let again = get_snapshot(&state).await;
    assert_eq!(again.status, 200);
    assert_eq!(again.body, res.body);
    assert_eq!(host.calls(), 1);
}

/// `AISNAP-20`・`AISNAP-21`: 同期に失敗したら古い内容を返さずエラーにする。ホストが作り直された
/// （`session` が変わった）結果も差し替えず `session_lost` にする。
#[tokio::test]
async fn aisnap21_sync_failure_does_not_return_old_content() {
    for (reply, status, code) in [
        (Err(RenderError::SessionLost), 409, "session_lost"),
        (Ok(host_snapshot("s2", 1, FILLED)), 409, "session_lost"),
        (Err(RenderError::Timeout), 502, "sync_failed"),
        (
            Err(RenderError::RenderingDisabled),
            409,
            "rendering_disabled",
        ),
    ] {
        let dir = TempDir::new();
        let host = Arc::new(FakeHost::default());
        host.push(reply);
        let state = interactive_state(&dir, Arc::clone(&host));
        let res = get_snapshot(&state).await;
        assert_eq!(
            (res.status, json(&res)["code"].clone()),
            (status, code.into())
        );
        let text = String::from_utf8(res.body.clone()).unwrap();
        assert!(!text.contains("initial-heading"), "{text}");
        assert!(!text.contains("Saved"), "{text}");
        assert_eq!(
            state
                .app()
                .navigation()
                .latest()
                .unwrap()
                .interactive()
                .unwrap()
                .revision(),
            1
        );
    }
}

/// `AISNAP-20`: 静的モードのページは取り直さず、`mode` は `static`。
#[tokio::test]
async fn aisnap20_static_page_is_not_synced() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let profile = Arc::new(Profile::open(&dir.0).expect("profile"));
    let app = AppState::new(profile, host.clone()).allow_interactive_for_tests();
    let g = app.navigation().begin_navigation().unwrap();
    app.navigation()
        .commit_navigation(g, NavigationResult::new("https://a.example/", "<p>x</p>"))
        .unwrap();
    let state = Arc::new(AiState::new(Arc::new(app)));
    let v = json(&get_snapshot(&state).await);
    assert_eq!(
        (v["generation"].clone(), v["mode"].clone()),
        (1.into(), "static".into())
    );
    assert_eq!(host.calls(), 0);
}

//! `POST /ai/navigate`（操作モード）を、偽の描画ホストで HTTP の層から確かめる結合テスト
//! （TASK-127・`AISNAP-16`・`AISNAP-21`）。
//!
//! 操作モードの許可は core の `test-support`（dev-dependencies で有効にする）の入口だけで立てる。
//! 2026-11-19 のオーナー判断: `mode` の省略・`static` は 501、`navigator.webdriver` の確認の失敗は
//! `interactive_unavailable`（409）、HTTP ステータスの表。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_ai::api::router_with_state;
use fandhe_browser_ai::plugin_api::AiState;
use fandhe_browser_core::AppState;
use fandhe_browser_core::interactive::{HostDomSnapshot, InteractivePageId};
use fandhe_browser_core::render::{
    BoundingBox, ElementRef, ElementVisibility, InteractivePageLoad, PageLoad, PageRef,
    RenderError, Renderer, Screenshot, ScreenshotOptions,
};
use fandhe_browser_profile::Profile;
use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(
            fandhe_browser_profile::test_base_dir()
                .join(format!("fandhe-ai-navigate-{}-{n}", std::process::id())),
        )
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `load_interactive` と `dom_snapshot` の結果を差し替えられる偽ホスト。呼ばれた回数を数える。
#[derive(Default)]
struct FakeHost {
    load_error: Mutex<Option<RenderError>>,
    snapshot_error: Mutex<Option<RenderError>>,
    loads: AtomicUsize,
    snapshots: AtomicUsize,
}

impl FakeHost {
    fn calls(&self) -> (usize, usize) {
        (
            self.loads.load(Ordering::SeqCst),
            self.snapshots.load(Ordering::SeqCst),
        )
    }
}

const HTML: &str = r#"<html data-fandhe-n="0"><head data-fandhe-n="1"><title data-fandhe-n="2">T</title></head><body data-fandhe-n="3"><button data-fandhe-n="4">Go</button></body></html>"#;

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
    fn load_interactive(&self, page: &PageRef) -> Result<InteractivePageLoad, RenderError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.load_error.lock().unwrap().take() {
            return Err(e);
        }
        Ok(InteractivePageLoad::new(
            PageLoad::new(format!("{}#final", page.url())),
            InteractivePageId::new("s1", 1).unwrap(),
        ))
    }
    fn dom_snapshot(&self) -> Result<HostDomSnapshot, RenderError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.snapshot_error.lock().unwrap().take() {
            return Err(e);
        }
        Ok(HostDomSnapshot::new(
            InteractivePageId::new("s1", 1).unwrap(),
            "https://a.example/form",
            HTML.to_owned(),
            5,
            "snap",
        )
        .unwrap())
    }
}

fn state(dir: &TempDir, host: Arc<FakeHost>, allowed: bool) -> Arc<AiState> {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile"));
    let app = AppState::new(profile, host);
    let app = if allowed {
        app.allow_interactive_for_tests()
    } else {
        app
    };
    Arc::new(AiState::new(Arc::new(app)))
}

async fn send(state: &Arc<AiState>, raw_head: &str, body: &str) -> Response {
    let head = match parse_request_head(raw_head.as_bytes()).expect("parse") {
        ParseOutcome::Complete { head, .. } => head,
        ParseOutcome::Incomplete => panic!("incomplete"),
    };
    router_with_state(Arc::clone(state))
        .dispatch(&head, body.as_bytes())
        .await
}

async fn navigate(state: &Arc<AiState>, body: &str) -> Response {
    let head = format!(
        "POST /ai/navigate HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    send(state, &head, body).await
}

async fn snapshot(state: &Arc<AiState>) -> Response {
    send(
        state,
        "GET /ai/snapshot HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n\r\n",
        "",
    )
    .await
}

fn json(res: &Response) -> Value {
    serde_json::from_slice(&res.body).expect("json")
}

const OK_BODY: &str = r#"{"url":"https://a.example/form?token=secret","mode":"interactive"}"#;

/// `AISNAP-16`: 操作モードで読み込み、取得時の URL・世代・モードを返す。続く `/ai/snapshot` は同じ世代の
/// 操作モードの要約で、読み込みと同時に取った DOM を使う（すぐには取り直さない）。
#[tokio::test]
async fn aisnap16_navigate_interactive_then_snapshot() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host), true);
    let res = navigate(&s, OK_BODY).await;
    assert_eq!(res.status, 200);
    let v = json(&res);
    assert_eq!(v["ok"], true);
    // 読み込みの最終 URL（`#final`）ではなく、保存した文書の取得時の URL を返す。
    assert_eq!(v["url"], "https://a.example/form");
    assert_eq!(v["mode"], "interactive");
    let generation = v["generation"].as_u64().unwrap();
    assert_eq!(host.calls(), (1, 1));

    let res = snapshot(&s).await;
    assert_eq!(res.status, 200);
    let v = json(&res);
    assert_eq!(v["mode"], "interactive");
    assert_eq!(v["generation"].as_u64(), Some(generation));
    assert_eq!(v["url"], "https://a.example/form");
    assert!(v["tree"].to_string().contains("Go"));
    assert_eq!(host.calls(), (1, 1), "no re-sync right after navigate");
}

/// 前段の検査（Host・Origin・Content-Type）・本文の検査・501・未許可では、世代を進めず描画実装も呼ばない。
#[tokio::test]
async fn aisnap16_rejections_have_no_side_effects() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host), true);
    let generation = || s.app().navigation().current_generation().get();
    let before = generation();
    let cases: Vec<(String, &str, u16, &str)> = vec![
        (
            "POST /ai/navigate HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\n\r\n".into(),
            OK_BODY,
            403,
            "host_not_allowed",
        ),
        (
            "POST /ai/navigate HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nContent-Type: application/json\r\n\r\n".into(),
            OK_BODY,
            403,
            "origin_not_allowed",
        ),
        (
            "POST /ai/navigate HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: text/plain\r\n\r\n".into(),
            OK_BODY,
            415,
            "unsupported_media_type",
        ),
    ];
    for (head, body, status, code) in cases {
        let res = send(&s, &head, body).await;
        assert_eq!(
            (res.status, json(&res)["code"].as_str()),
            (status, Some(code))
        );
    }
    for (body, status, code) in [
        (r#"{"url":"https://a.example/"}"#, 501, "not_implemented"),
        (
            r#"{"url":"https://a.example/","mode":"static"}"#,
            501,
            "not_implemented",
        ),
        (
            r#"{"url":"file:///etc/passwd","mode":"interactive"}"#,
            400,
            "invalid_params",
        ),
        (
            r#"{"url":"http://127.0.0.1/","mode":"interactive"}"#,
            400,
            "invalid_params",
        ),
        (
            r#"{"url":"https://a.example/","mode":"interactive","x":1}"#,
            400,
            "invalid_params",
        ),
    ] {
        let res = navigate(&s, body).await;
        assert_eq!(
            (res.status, json(&res)["code"].as_str()),
            (status, Some(code)),
            "{body}"
        );
    }
    assert_eq!(generation(), before);
    assert_eq!(host.calls(), (0, 0));

    let dir2 = TempDir::new();
    let host2 = Arc::new(FakeHost::default());
    let off = state(&dir2, Arc::clone(&host2), false);
    let res = navigate(&off, OK_BODY).await;
    assert_eq!(
        (res.status, json(&res)["code"].as_str()),
        (409, Some("interactive_unavailable"))
    );
    assert_eq!(off.app().navigation().current_generation().get(), 0);
    assert_eq!(host2.calls(), (0, 0));
}

/// `AISNAP-21`・2026-11-19 の判断: 描画実装の失敗の写像。`navigator.webdriver` の確認の失敗は
/// `interactive_unavailable` で message で区別する。失敗の後は確定結果が無い（`/ai/snapshot` は
/// `no_navigation`）。エラー本文に URL（クエリ）を出さない。
#[tokio::test]
async fn aisnap21_navigate_error_mapping() {
    let cases = [
        (
            Some(RenderError::Unsupported),
            None,
            409,
            "interactive_unavailable",
            "page does not meet interactive mode requirements",
        ),
        (
            Some(RenderError::InteractiveUnavailable),
            None,
            409,
            "interactive_unavailable",
            "interactive mode is unavailable",
        ),
        (
            Some(RenderError::RenderingDisabled),
            None,
            409,
            "rendering_disabled",
            "rendering is disabled",
        ),
        (
            Some(RenderError::Timeout),
            None,
            502,
            "load_failed",
            "failed to load page in interactive mode",
        ),
        (
            None,
            Some(RenderError::Unsupported),
            409,
            "interactive_unavailable",
            "page does not meet interactive mode requirements",
        ),
        (
            None,
            Some(RenderError::SessionLost),
            409,
            "session_lost",
            "rendering host session was lost",
        ),
    ];
    for (load_error, snapshot_error, status, code, message) in cases {
        let dir = TempDir::new();
        let host = Arc::new(FakeHost::default());
        *host.load_error.lock().unwrap() = load_error;
        *host.snapshot_error.lock().unwrap() = snapshot_error;
        let s = state(&dir, Arc::clone(&host), true);
        let res = navigate(&s, OK_BODY).await;
        let v = json(&res);
        assert_eq!(
            (res.status, v["code"].as_str(), v["message"].as_str()),
            (status, Some(code), Some(message))
        );
        assert!(!String::from_utf8_lossy(&res.body).contains("secret"));
        let res = snapshot(&s).await;
        assert_eq!(
            (res.status, json(&res)["code"].as_str()),
            (409, Some("no_navigation"))
        );
    }
}

/// `AISNAP-20`・SEC-5: 同期でホストが `navigator.webdriver` の確認で断ったら、`interactive_unavailable` で返し、
/// 確定結果を無効にする（以後は新しい navigate まで `session_lost`）。
#[tokio::test]
async fn aisnap20_sync_unsupported_is_interactive_unavailable() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host), true);
    assert_eq!(navigate(&s, OK_BODY).await.status, 200);
    // 最小間隔（1 秒）を過ぎてから取り直させる。
    std::thread::sleep(fandhe_browser_ai::interactive::MIN_SYNC_INTERVAL);
    *host.snapshot_error.lock().unwrap() = Some(RenderError::Unsupported);
    let res = snapshot(&s).await;
    assert_eq!(
        (res.status, json(&res)["code"].as_str()),
        (409, Some("interactive_unavailable"))
    );
    let res = snapshot(&s).await;
    assert_eq!(
        (res.status, json(&res)["code"].as_str()),
        (409, Some("session_lost"))
    );
}

//! `POST /ai/wait`（操作モードの待機）を、偽の描画ホストで HTTP の層から確かめる結合テスト（TASK-129・`AISNAP-18`・
//! `AISNAP-21`・MS-11）。
//!
//! 偽ホストは `dom_snapshot` で固定の HTML と設定した文書番号を返し、`wait` の要求を記録する。操作モードの許可は core の
//! `test-support`（dev-dependencies で有効にする）の入口だけで立てる。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_ai::api::router_with_state;
use fandhe_browser_ai::plugin_api::AiState;
use fandhe_browser_core::interactive::{HostDomSnapshot, InteractivePageId};
use fandhe_browser_core::render::{
    ActOutcome, ActRequest, BoundingBox, ElementRef, ElementVisibility, InteractivePageLoad,
    PageLoad, PageRef, RenderError, Renderer, Screenshot, ScreenshotOptions, WaitCondition,
    WaitOutcome, WaitRequest,
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
                .join(format!("fandhe-ai-wait-{}-{n}", std::process::id())),
        )
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Gate = Option<(SyncSender<()>, Receiver<()>)>;

struct FakeHost {
    /// `dom_snapshot` が返すページの文書番号（ホスト側だけで進む遷移を表す）。
    doc_seq: AtomicU64,
    snapshots: AtomicUsize,
    waits: Mutex<Vec<WaitRequest>>,
    wait_error: Mutex<Option<RenderError>>,
    /// 置くと、次の `wait` は入ったことを知らせ、解放されるまで止まる（sleep に頼らず待機中を作る）。
    wait_gate: Mutex<Gate>,
    acts: AtomicUsize,
    act_gate: Mutex<Gate>,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            doc_seq: AtomicU64::new(1),
            snapshots: AtomicUsize::new(0),
            waits: Mutex::new(Vec::new()),
            wait_error: Mutex::new(None),
            wait_gate: Mutex::new(None),
            acts: AtomicUsize::new(0),
            act_gate: Mutex::new(None),
        }
    }
}

impl FakeHost {
    fn snapshots(&self) -> usize {
        self.snapshots.load(Ordering::SeqCst)
    }
    fn waits(&self) -> Vec<WaitRequest> {
        self.waits.lock().unwrap().clone()
    }
    fn page(&self) -> InteractivePageId {
        InteractivePageId::new("s1", self.doc_seq.load(Ordering::SeqCst)).unwrap()
    }
}

fn pass_gate(gate: &Mutex<Gate>) {
    let gate = gate.lock().unwrap().take();
    if let Some((entered, release)) = gate {
        entered.send(()).unwrap();
        release.recv().unwrap();
    }
}

const HTML: &str = r#"<html data-fandhe-n="0"><head data-fandhe-n="1"></head><body data-fandhe-n="2"><button id="go" data-fandhe-n="3">Go</button></body></html>"#;

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
        Ok(InteractivePageLoad::new(
            PageLoad::new(page.url()),
            self.page(),
        ))
    }
    fn dom_snapshot(&self) -> Result<HostDomSnapshot, RenderError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        Ok(HostDomSnapshot::new(
            self.page(),
            "https://a.example/",
            HTML.to_owned(),
            4,
            "snap",
        )
        .unwrap())
    }
    fn act(&self, _: &ActRequest) -> Result<ActOutcome, RenderError> {
        pass_gate(&self.act_gate);
        self.acts.fetch_add(1, Ordering::SeqCst);
        Ok(ActOutcome::new(self.page(), false))
    }
    fn wait(&self, request: &WaitRequest) -> Result<WaitOutcome, RenderError> {
        pass_gate(&self.wait_gate);
        self.waits.lock().unwrap().push(request.clone());
        if let Some(e) = self.wait_error.lock().unwrap().take() {
            return Err(e);
        }
        Ok(WaitOutcome::new(self.page()))
    }
}

fn state(dir: &TempDir, host: Arc<FakeHost>) -> Arc<AiState> {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile"));
    let app = AppState::new(profile, host).allow_interactive_for_tests();
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

async fn post(state: &Arc<AiState>, path: &str, body: &str) -> Response {
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
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

fn code(res: &Response) -> (u16, String) {
    (
        res.status,
        json(res)["code"].as_str().unwrap_or_default().to_owned(),
    )
}

async fn navigate(state: &Arc<AiState>) -> u64 {
    let res = post(
        state,
        "/ai/navigate",
        r#"{"url":"https://a.example/","mode":"interactive"}"#,
    )
    .await;
    assert_eq!(res.status, 200);
    json(&res)["generation"].as_u64().unwrap()
}

fn wait_body(g: u64, condition: &str, ms: u64) -> String {
    format!(r#"{{"generation":{g},"condition":{condition},"timeout_ms":{ms}}}"#)
}

const PRESENT: &str = r##"{"type":"selector_present","selector":"#done"}"##;

/// `AISNAP-18`: 待つ前に取り直し、いまの文書のページ識別子で待つ（ホスト側だけで進んだ遷移の後でも `stale` に
/// ならない）。成功は `{"ok":true,"generation"}`。待機の後の `/ai/snapshot` は最小間隔によらず取り直す。
#[tokio::test]
async fn aisnap18_wait_resyncs_and_returns_generation() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let g = navigate(&s).await;
    assert_eq!(host.snapshots(), 1);
    host.doc_seq.store(5, Ordering::SeqCst);

    let res = post(&s, "/ai/wait", &wait_body(g, PRESENT, 1500)).await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let v = json(&res);
    assert_eq!(
        (v["ok"].as_bool(), v["generation"].as_u64()),
        (Some(true), Some(g))
    );
    assert_eq!(host.snapshots(), 2, "re-synced right before the wait");
    let waits = host.waits();
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].page().doc_seq(), 5);
    assert_eq!(
        waits[0].condition(),
        &WaitCondition::SelectorPresent("#done".into())
    );
    assert_eq!(waits[0].timeout_ms(), 1500);

    assert_eq!(snapshot(&s).await.status, 200);
    assert_eq!(host.snapshots(), 3, "snapshot after wait re-syncs");
}

/// `AISNAP-18`・`AISNAP-21`: 失敗の写像（未ナビゲート・世代違い・本文の不正・時間切れ・文書の変化・ホストの喪失・
/// 静的モード）。エラー本文に条件の文字列を出さない。
#[tokio::test]
async fn aisnap21_wait_error_mapping() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let res = post(&s, "/ai/wait", &wait_body(1, PRESENT, 100)).await;
    assert_eq!(code(&res), (409, "no_navigation".into()));

    let g = navigate(&s).await;
    for (body, want) in [
        (wait_body(g + 1, PRESENT, 100), (409, "stale")),
        (wait_body(g, PRESENT, 60_001), (400, "invalid_params")),
        (
            wait_body(g, r#"{"type":"load_complete","x":1}"#, 100),
            (400, "invalid_params"),
        ),
    ] {
        let res = post(&s, "/ai/wait", &body).await;
        assert_eq!(code(&res), (want.0, want.1.into()), "{body}");
    }
    assert!(host.waits().is_empty());
    // 受け口の検査（Origin 付き・Content-Type 違い）は本文を見る前に断る。
    let body = wait_body(g, PRESENT, 100);
    for (extra, ct, status) in [
        ("Origin: https://evil.example\r\n", "application/json", 403),
        ("", "text/plain", 415),
    ] {
        let head = format!(
            "POST /ai/wait HTTP/1.1\r\nHost: 127.0.0.1:9222\r\n{extra}Content-Type: {ct}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        assert_eq!(send(&s, &head, &body).await.status, status, "{extra}{ct}");
    }
    assert!(host.waits().is_empty());

    let secret = r#"{"type":"text_present","text":"tok-secret"}"#;
    for (e, want) in [
        (RenderError::Timeout, (409, "timeout")),
        (RenderError::Stale, (409, "stale")),
        (RenderError::InvalidParams, (400, "invalid_params")),
        (RenderError::ProtocolMismatch, (502, "wait_failed")),
    ] {
        *host.wait_error.lock().unwrap() = Some(e);
        let res = post(&s, "/ai/wait", &wait_body(g, secret, 100)).await;
        assert_eq!(code(&res), (want.0, want.1.into()));
        assert!(!String::from_utf8_lossy(&res.body).contains("tok-secret"));
    }
    // 時間切れの後もページは保たれ、続けて待てる。
    assert_eq!(
        post(&s, "/ai/wait", &wait_body(g, PRESENT, 100))
            .await
            .status,
        200
    );

    // ホストが失われたら確定結果を無効にし、以後は新しい navigate まで `session_lost`。
    *host.wait_error.lock().unwrap() = Some(RenderError::SessionLost);
    let res = post(&s, "/ai/wait", &wait_body(g, PRESENT, 100)).await;
    assert_eq!(code(&res), (409, "session_lost".into()));
    assert_eq!(code(&snapshot(&s).await), (409, "session_lost".into()));
    assert_eq!(
        code(&post(&s, "/ai/wait", &wait_body(g, PRESENT, 100)).await),
        (409, "session_lost".into())
    );

    // 静的モードのページでの待機は `not_interactive`。
    let nav = s.app().navigation();
    let g2 = nav.begin_navigation().unwrap();
    nav.commit_navigation(g2, NavigationResult::new("https://a.example/", "<p>x</p>"))
        .unwrap();
    let res = post(&s, "/ai/wait", &wait_body(g2.get(), PRESENT, 100)).await;
    assert_eq!(code(&res), (409, "not_interactive".into()));
}

/// `AISNAP-18`: 同時に待てるのは 1 件。待機中の別の待機・操作・`/ai/snapshot` は待たせずに `busy`。操作の実行中の
/// 待機も `busy`。
#[tokio::test]
async fn aisnap18_busy_while_waiting() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let g = navigate(&s).await;
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    *host.wait_gate.lock().unwrap() = Some((entered_tx, release_rx));
    let running = {
        let s = Arc::clone(&s);
        let body = wait_body(g, PRESENT, 60_000);
        std::thread::spawn(move || fandhe_browser_ai::wait::wait(&s, body.as_bytes()))
    };
    entered_rx.recv().unwrap();
    assert_eq!(
        code(&post(&s, "/ai/wait", &wait_body(g, PRESENT, 100)).await),
        (409, "busy".into())
    );
    let click = format!(r##"{{"selector":"#go","generation":{g},"action":{{"type":"click"}}}}"##);
    assert_eq!(
        code(&post(&s, "/ai/act", &click).await),
        (409, "busy".into())
    );
    assert_eq!(code(&snapshot(&s).await), (409, "busy".into()));
    release_tx.send(()).unwrap();
    assert!(running.join().unwrap().is_ok());
    assert_eq!(host.waits().len(), 1);
    assert_eq!(host.acts.load(Ordering::SeqCst), 0);

    // 操作の実行中の待機も `busy`（同じ排他）。
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    *host.act_gate.lock().unwrap() = Some((entered_tx, release_rx));
    let running = {
        let s = Arc::clone(&s);
        std::thread::spawn(move || fandhe_browser_ai::act::act(&s, click.as_bytes()))
    };
    entered_rx.recv().unwrap();
    assert_eq!(
        code(&post(&s, "/ai/wait", &wait_body(g, PRESENT, 100)).await),
        (409, "busy".into())
    );
    release_tx.send(()).unwrap();
    assert!(running.join().unwrap().is_ok());
    assert_eq!(host.waits().len(), 1);
}

/// `AISNAP-18`・`AISNAP-20`: 待機の間に新しい navigate が始まったら（navigate は待機の排他を取らない）、古いページで
/// 条件が成立しても無効になった世代で成功を返さず `stale`。
#[tokio::test]
async fn aisnap18_wait_superseded_by_navigation_is_stale() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let g = navigate(&s).await;
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    *host.wait_gate.lock().unwrap() = Some((entered_tx, release_rx));
    let running = {
        let s = Arc::clone(&s);
        let body = wait_body(g, PRESENT, 60_000);
        std::thread::spawn(move || fandhe_browser_ai::wait::wait(&s, body.as_bytes()))
    };
    entered_rx.recv().unwrap();
    // navigate は描画層のゲートより前に世代を進める。その順序をここで再現する。
    let _ = s.app().navigation().begin_navigation().unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(
        running.join().unwrap(),
        Err(fandhe_browser_ai::wait::WaitError::Stale)
    );
    assert_eq!(host.waits().len(), 1, "the host did run the wait");
}

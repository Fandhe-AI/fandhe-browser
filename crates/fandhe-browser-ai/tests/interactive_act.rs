//! `POST /ai/act`（操作モードの click・fill）を、偽の描画ホストで HTTP の層から確かめる結合テスト
//! （TASK-128・`AISNAP-17`・`AISNAP-20`・`AISNAP-21`）。
//!
//! 偽ホストは `dom_snapshot` で用意した HTML を順に返し、`act` の要求を記録する。操作モードの許可は core の
//! `test-support`（dev-dependencies で有効にする）の入口だけで立てる。2026-11-19 のオーナー判断: `click`・`fill`
//! だけ、`sensitive` の操作があった世代はすべての入力値を伏せる、操作の実行中は `busy`。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fandhe_backend_http::request::{ParseOutcome, parse_request_head};
use fandhe_backend_http::response::Response;
use fandhe_browser_ai::api::router_with_state;
use fandhe_browser_ai::plugin_api::AiState;
use fandhe_browser_core::interactive::{HostDomSnapshot, InteractivePageId};
use fandhe_browser_core::render::{
    ActAction, ActOutcome, ActRequest, BoundingBox, ElementRef, ElementVisibility,
    InteractivePageLoad, PageLoad, PageRef, RenderError, Renderer, Screenshot, ScreenshotOptions,
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
                .join(format!("fandhe-ai-act-{}-{n}", std::process::id())),
        )
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 記録した `act` の要求（入力文字列を含むのでテストの中だけで見る）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    node: u32,
    tag: String,
    id: Option<String>,
    action: ActAction,
}

#[derive(Default)]
struct FakeHost {
    /// `dom_snapshot` が順に返す本文（最後の 1 件は繰り返す）。
    bodies: Mutex<VecDeque<String>>,
    snapshots: AtomicUsize,
    acts: Mutex<Vec<Seen>>,
    act_error: Mutex<Option<RenderError>>,
    act_delay: Mutex<Duration>,
}

impl FakeHost {
    fn push(&self, body: &str) {
        self.bodies.lock().unwrap().push_back(body.to_owned());
    }
    fn snapshots(&self) -> usize {
        self.snapshots.load(Ordering::SeqCst)
    }
    fn acts(&self) -> Vec<Seen> {
        self.acts.lock().unwrap().clone()
    }
}

const PAGE: &str = r#"<input id="q" name="q" data-fandhe-n="3"><input id="pw" type="password" data-fandhe-n="4"><button id="go" data-fandhe-n="5">Go</button><button class="dup" data-fandhe-n="6">A</button><button class="dup" data-fandhe-n="7">B</button>"#;

fn html(body: &str) -> String {
    format!(
        r#"<html data-fandhe-n="0"><head data-fandhe-n="1"></head><body data-fandhe-n="2">{body}</body></html>"#
    )
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
    fn load_interactive(&self, page: &PageRef) -> Result<InteractivePageLoad, RenderError> {
        Ok(InteractivePageLoad::new(
            PageLoad::new(page.url()),
            InteractivePageId::new("s1", 1).unwrap(),
        ))
    }
    fn dom_snapshot(&self) -> Result<HostDomSnapshot, RenderError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        let mut q = self.bodies.lock().unwrap();
        let body = if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            q.front().cloned().unwrap_or_else(|| PAGE.to_owned())
        };
        Ok(HostDomSnapshot::new(
            InteractivePageId::new("s1", 1).unwrap(),
            "https://a.example/form",
            html(&body),
            8,
            "snap",
        )
        .unwrap())
    }
    fn act(&self, request: &ActRequest) -> Result<ActOutcome, RenderError> {
        std::thread::sleep(*self.act_delay.lock().unwrap());
        self.acts.lock().unwrap().push(Seen {
            node: request.node(),
            tag: request.tag().to_owned(),
            id: request.id().map(str::to_owned),
            action: request.action().clone(),
        });
        if let Some(e) = self.act_error.lock().unwrap().take() {
            return Err(e);
        }
        Ok(ActOutcome::new(
            InteractivePageId::new("s1", 1).unwrap(),
            false,
        ))
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

/// 操作モードで navigate して世代を返す。
async fn navigate(state: &Arc<AiState>) -> u64 {
    let res = post(
        state,
        "/ai/navigate",
        r#"{"url":"https://a.example/form","mode":"interactive"}"#,
    )
    .await;
    assert_eq!(res.status, 200);
    json(&res)["generation"].as_u64().unwrap()
}

fn act_body(g: u64, target: &str, action: &str) -> String {
    format!(r#"{{{target},"generation":{g},"action":{action}}}"#)
}

/// `AISNAP-17`・`AISNAP-20`: 直前に取り直し、セレクタ・ref を一意に解決して番号と照合値で送る。操作の後の
/// `/ai/snapshot` は最小間隔によらず取り直し、操作の結果（入力値・変わった見出し）を返す。
#[tokio::test]
async fn aisnap17_click_and_fill_are_reflected_in_snapshot() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let g = navigate(&s).await;
    assert_eq!(host.snapshots(), 1);

    let res = post(
        &s,
        "/ai/act",
        &act_body(
            g,
            r##""selector":"#q""##,
            r#"{"type":"fill","text":"hello"}"#,
        ),
    )
    .await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let v = json(&res);
    assert_eq!(
        (
            v["ok"].as_bool(),
            v["generation"].as_u64(),
            v["navigated"].as_bool()
        ),
        (Some(true), Some(g), Some(false))
    );
    assert_eq!(host.snapshots(), 2, "re-synced right before the act");
    assert_eq!(
        host.acts(),
        vec![Seen {
            node: 3,
            tag: "input".into(),
            id: Some("q".into()),
            action: ActAction::Fill {
                text: "hello".into(),
                sensitive: false
            }
        }]
    );

    // ref で click する（要約の ref を使う）。
    host.push(
        &PAGE
            .replace("data-fandhe-n=\"3\"", "value=\"hello\" data-fandhe-n=\"3\"")
            .replace(">Go<", ">Saved<"),
    );
    let snap = json(&snapshot(&s).await);
    assert_eq!(host.snapshots(), 3, "snapshot after act re-syncs");
    // 入力値は要約の木には出ない（ラベルの中でなければ）。保存した写し（CDP の `DOM.*` も読む）で確かめる。
    assert!(
        saved_html(&s).contains("value=\"hello\""),
        "{}",
        saved_html(&s)
    );
    let go_ref = find_ref(&snap["tree"], "Saved").expect("ref");
    let res = post(
        &s,
        "/ai/act",
        &act_body(g, &format!(r#""ref":"{go_ref}""#), r#"{"type":"click"}"#),
    )
    .await;
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    assert_eq!(host.acts().last().unwrap().node, 5);
}

fn saved_html(s: &Arc<AiState>) -> String {
    s.app()
        .navigation()
        .latest()
        .map(|r| r.html().to_owned())
        .unwrap_or_default()
}

fn find_ref(v: &Value, name: &str) -> Option<String> {
    if v["name"].as_str() == Some(name) {
        return v["ref"].as_str().map(str::to_owned);
    }
    v["children"]
        .as_array()?
        .iter()
        .find_map(|c| find_ref(c, name))
}

/// `AISNAP-17`・2026-11-19 の判断: `sensitive` の `fill` と、伏せる対象（パスワード）への操作の後は、その世代の間
/// すべての入力値を要約に出さない。新しい navigate で外れる。
#[tokio::test]
async fn aisnap17_sensitive_redacts_all_values_for_the_generation() {
    for (target, action) in [
        (
            r##""selector":"#q""##,
            r#"{"type":"fill","text":"s3cret","sensitive":true}"#,
        ),
        (
            r##""selector":"#pw""##,
            r#"{"type":"fill","text":"s3cret"}"#,
        ),
    ] {
        let dir = TempDir::new();
        let host = Arc::new(FakeHost::default());
        let s = state(&dir, Arc::clone(&host));
        let filled = PAGE.replace(
            "data-fandhe-n=\"3\"",
            "value=\"visible-value\" data-fandhe-n=\"3\"",
        );
        host.push(PAGE);
        host.push(&filled);
        let g = navigate(&s).await;
        let res = post(&s, "/ai/act", &act_body(g, target, action)).await;
        assert_eq!(res.status, 200, "{target}");
        assert_eq!(snapshot(&s).await.status, 200);
        let body = saved_html(&s);
        assert!(!body.contains("visible-value"), "{target}: {body}");
        assert!(!body.contains("s3cret"), "{target}: {body}");
        // 新しい navigate の後は外れる（伏せる対象でない値は出る）。
        navigate(&s).await;
        std::thread::sleep(fandhe_browser_ai::interactive::MIN_SYNC_INTERVAL);
        assert_eq!(snapshot(&s).await.status, 200);
        let body = saved_html(&s);
        assert!(body.contains("visible-value"), "{target}: {body}");
    }
}

/// `AISNAP-17`・`AISNAP-21`: 失敗の写像（世代違い・静的・未ナビゲート・解決の失敗・ホストの失敗・501）。
/// エラー本文に入力文字列を出さない。
#[tokio::test]
async fn aisnap21_act_error_mapping() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let click = r#"{"type":"click"}"#;
    let res = post(&s, "/ai/act", &act_body(1, r##""selector":"#go""##, click)).await;
    assert_eq!(code(&res), (409, "no_navigation".into()));

    let g = navigate(&s).await;
    for (body, want) in [
        (
            act_body(g + 1, r##""selector":"#go""##, click),
            (409, "stale"),
        ),
        (
            act_body(g, r##""selector":"#none""##, click),
            (404, "not_found"),
        ),
        (
            act_body(g, r#""selector":".dup""#, click),
            (409, "ambiguous"),
        ),
        (
            act_body(g, r#""selector":"[[""#, click),
            (400, "invalid_params"),
        ),
        (
            act_body(
                g,
                r##""selector":"#go""##,
                r#"{"type":"press","key":"Enter"}"#,
            ),
            (501, "not_implemented"),
        ),
    ] {
        let res = post(&s, "/ai/act", &body).await;
        assert_eq!(code(&res), (want.0, want.1.into()), "{body}");
    }
    assert!(host.acts().is_empty());

    for (e, want) in [
        (RenderError::NotActionable, (409, "not_actionable")),
        (RenderError::Stale, (409, "stale")),
        (RenderError::Unsupported, (409, "unsupported")),
        (RenderError::InvalidState, (409, "not_actionable")),
        (RenderError::Timeout, (502, "act_failed")),
    ] {
        *host.act_error.lock().unwrap() = Some(e);
        let res = post(
            &s,
            "/ai/act",
            &act_body(
                g,
                r##""selector":"#q""##,
                r#"{"type":"fill","text":"tok-secret"}"#,
            ),
        )
        .await;
        assert_eq!(code(&res), (want.0, want.1.into()));
        assert!(!String::from_utf8_lossy(&res.body).contains("tok-secret"));
    }

    // ホストが失われたら確定結果を無効にし、以後は新しい navigate まで `session_lost`。
    *host.act_error.lock().unwrap() = Some(RenderError::SessionLost);
    let res = post(&s, "/ai/act", &act_body(g, r##""selector":"#go""##, click)).await;
    assert_eq!(code(&res), (409, "session_lost".into()));
    assert_eq!(code(&snapshot(&s).await), (409, "session_lost".into()));
    assert_eq!(
        code(&post(&s, "/ai/act", &act_body(g, r##""selector":"#go""##, click)).await),
        (409, "session_lost".into())
    );

    // 静的モードのページへの操作は `not_interactive`。
    let nav = s.app().navigation();
    let g2 = nav.begin_navigation().unwrap();
    nav.commit_navigation(g2, NavigationResult::new("https://a.example/", "<p>x</p>"))
        .unwrap();
    let res = post(
        &s,
        "/ai/act",
        &act_body(g2.get(), r#""selector":"p""#, click),
    )
    .await;
    assert_eq!(code(&res), (409, "not_interactive".into()));
}

/// `AISNAP-17`・2026-11-19 の判断: 操作の実行中は、操作モードの `/ai/snapshot` と別の操作を待たせずに `busy`。
/// 実行中でなければ `/ai/snapshot` は通る。
#[tokio::test]
async fn aisnap17_busy_while_acting() {
    let dir = TempDir::new();
    let host = Arc::new(FakeHost::default());
    let s = state(&dir, Arc::clone(&host));
    let g = navigate(&s).await;
    *host.act_delay.lock().unwrap() = Duration::from_millis(800);
    let running = {
        let s = Arc::clone(&s);
        let body = act_body(g, r##""selector":"#go""##, r#"{"type":"click"}"#);
        std::thread::spawn(move || fandhe_browser_ai::act::act(&s, body.as_bytes()))
    };
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(code(&snapshot(&s).await), (409, "busy".into()));
    let res = post(
        &s,
        "/ai/act",
        &act_body(g, r##""selector":"#go""##, r#"{"type":"click"}"#),
    )
    .await;
    assert_eq!(code(&res), (409, "busy".into()));
    assert!(running.join().unwrap().is_ok());
    assert_eq!(snapshot(&s).await.status, 200);
    assert_eq!(host.acts().len(), 1);
}

//! CDP がスタブ状態でも `GET /ai/snapshot` を単体で使えることを検証するための
//! テスト用サーバー組み立てヘルパー
//! （TASK-20.1・Issue #473、ビヘイビア `AISNAP-7`・MS-4）。
//! TASK-20.2（#474）は同じヘルパー上で、スタブ CDP 状態の `GET /ai/snapshot` の応答
//! （未ナビゲート時の `no_navigation`・ナビゲート後の簡約表現全体）を具体値で検証する。
//! TASK-20.3（#475）は同じヘルパー上で、スタブ CDP の未実装エラー応答（501・`-32601`）を
//! 繰り返し受けても `GET /ai/snapshot` の応答・共有ナビゲート状態が変わらないことを検証する。
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

// ---- TASK-20.2（Issue #474・`AISNAP-7`・MS-4）: スタブ CDP 状態での応答検証 ----

const JSON_CT: &str = "application/json; charset=UTF-8";

/// 記事ページ。`tests/snapshot.rs` の `ARTICLE` と同一（別テストバイナリの private const の
/// ため複製。ref の期待値も同ファイル `aisnap_1_article_snapshot_structure` の転記）。
const ARTICLE: &str = r#"<!DOCTYPE html><html><head><title>Rust 入門記事</title><style>p{}</style></head><body><header>サイトヘッダ</header><h1>はじめに</h1><p>詳細は<a href="https://example.com/docs">公式ドキュメント</a>を参照。</p><h2>要点</h2><ul><li>所有権</li><li>借用</li></ul><footer>連絡先</footer></body></html>"#;

/// フォームページ。`tests/snapshot.rs` の `FORM` と同一（hidden 値はダミー）。
const FORM: &str = r#"<!DOCTYPE html><html><head><title>登録フォーム</title></head><body><div><label for="user">ユーザー名</label><input type="text" id="user" name="user"></div><div><label><input type="checkbox" name="agree" checked>規約に同意</label></div><div><input type="radio" name="plan" aria-label="無料プラン"></div><select name="lang" aria-label="言語"></select><input type="hidden" name="token" value="dummy-hidden-value"><button type="submit">送信</button><button disabled>取消</button></body></html>"#;

/// ref 付きの葉ノード期待 JSON（`api.rs::node_json` の形）。
fn leaf(role: &str, name: &str, r: &str) -> Value {
    serde_json::json!({"role": role, "name": name, "ref": r, "children": []})
}

/// ref を持たない葉ノード期待 JSON（`ref` キーは省略される。TASK-23.4）。
fn leaf_no_ref(role: &str, name: &str) -> Value {
    serde_json::json!({"role": role, "name": name, "children": []})
}

/// ルート（`document`。`ref` キーは省略される）を包むエンベロープ期待 JSON。
fn envelope(url: &str, doc_name: &str, children: Vec<Value>) -> Value {
    serde_json::json!({
        "url": url,
        "tree": {"role": "document", "name": doc_name, "children": children},
        "truncated": false,
    })
}

fn expected_article(url: &str) -> Value {
    let list = serde_json::json!({
        "role": "list", "name": "", "ref": "ecc842c965dec143a",
        "header": [],
        "rows": [
            {"text": "所有権", "truncated": false, "controls": [], "controls_truncated": false},
            {"text": "借用", "truncated": false, "controls": [], "controls_truncated": false},
        ],
        "truncated_rows": 0,
        "children": [],
    });
    // 名前のない generic（html・body・p）は折り畳まれる。ref は折り畳み前と同一（TASK-23.3）。banner・contentinfo は ref を持たない（TASK-23.4）。
    envelope(
        url,
        "Rust 入門記事",
        vec![
            leaf_no_ref("banner", ""),
            leaf("heading", "はじめに", "e300ae11bc252b0fd"),
            leaf("link", "公式ドキュメント", "ed45c1d22b4e186f4"),
            leaf("heading", "要点", "ee0714a175d20c8b2"),
            list,
            leaf_no_ref("contentinfo", ""),
        ],
    )
}

fn expected_form(url: &str) -> Value {
    let mut checkbox = leaf("checkbox", "規約に同意", "eb4e606a8d0b0322c");
    checkbox["checked"] = Value::Bool(true);
    let mut radio = leaf("radio", "無料プラン", "e3ca491e4e71deece");
    radio["checked"] = Value::Bool(false);
    let mut cancel = leaf("button", "取消", "ebc8665358ad3ea30");
    cancel["disabled"] = Value::Bool(true);
    envelope(
        url,
        "登録フォーム",
        vec![
            leaf("textbox", "ユーザー名", "e60f2465f276d6230"),
            checkbox,
            radio,
            leaf("combobox", "言語", "ea907649b0395e5e0"),
            leaf("button", "送信", "e764e1c46ab9a2bd6"),
            cancel,
        ],
    )
}

/// CDP 側がスタブ（501・未実装）であることを確認する前提チェック。
async fn assert_cdp_is_stub(server: &StubServer) {
    let version = server.get("/json/version").await;
    assert_eq!(version.status, 501);
    assert_eq!(version.body, STUB_ERROR_BODY.as_bytes().to_vec());
}

/// ナビゲート前は CDP がスタブでも `no_navigation`（409）になる（TASK-19.2・`AISNAP-14`）。
#[tokio::test]
async fn aisnap7_snapshot_before_navigation_is_no_navigation_while_cdp_is_stubbed() {
    let server = StubServer::start();
    assert_cdp_is_stub(&server).await;

    let snap = server.get("/ai/snapshot").await;
    assert_eq!(snap.status, 409);
    let wire = String::from_utf8_lossy(&snap.serialize(false)).to_lowercase();
    assert!(
        wire.contains(&format!("content-type: {}", JSON_CT.to_lowercase())),
        "unexpected content-type: {wire}"
    );
    let v: Value = serde_json::from_slice(&snap.body).expect("json");
    assert_eq!(
        v,
        serde_json::json!({"code": "no_navigation", "message": "no navigation has been performed yet"})
    );
    assert!(server.app().navigation().latest().is_none());
}

/// ナビゲート済みなら CDP がスタブでも簡約表現全体（エンベロープ・全ノード）が返る。
#[tokio::test]
async fn aisnap7_snapshot_returns_full_reduced_tree_while_cdp_is_stubbed() {
    let server = StubServer::start();
    assert_cdp_is_stub(&server).await;
    server.navigate("https://example.com/article", ARTICLE);

    let snap = server.get("/ai/snapshot").await;
    assert_eq!(snap.status, 200);
    let v: Value = serde_json::from_slice(&snap.body).expect("json");
    assert_eq!(v, expected_article("https://example.com/article"));
}

/// フォームの state（checked・disabled）が出て、hidden 入力値は応答へ漏れない。
#[tokio::test]
async fn aisnap7_snapshot_reports_form_state_without_hidden_values_while_cdp_is_stubbed() {
    let server = StubServer::start();
    assert_cdp_is_stub(&server).await;
    server.navigate("https://example.com/form", FORM);

    let snap = server.get("/ai/snapshot").await;
    assert_eq!(snap.status, 200);
    let text = String::from_utf8(snap.body.clone()).expect("utf8");
    assert!(!text.contains("dummy-hidden-value"), "hidden value leaked");
    let v: Value = serde_json::from_slice(&snap.body).expect("json");
    assert_eq!(v, expected_form("https://example.com/form"));
}

// ---- TASK-20.3（Issue #475・`AISNAP-7`・MS-4）: CDP エラー応答の非波及検証 ----

/// スタブ CDP の未実装エラー応答（501）を複数回発生させ、毎回エラーであることを確認する。
/// 「成功を装うフォールバック」は作らず（`REPAIR-3`・`SEC-2`）、固定エラー応答のみで
/// CDP 側の失敗を代表させる。実 cdp との合成は cli 側（TASK-19.3・19.4）の担当。
async fn call_unimplemented_cdp(server: &StubServer) {
    for _ in 0..3 {
        assert_cdp_is_stub(server).await;
        let call = server
            .request("POST", "/cdp-stub/call", Some("127.0.0.1:9222"))
            .await;
        assert_eq!(call.status, 501);
        assert_eq!(call.body, STUB_ERROR_BODY.as_bytes().to_vec());
    }
}

/// `GET /ai/snapshot` の観測値（status・body・ワイヤ表現）。前後比較用。
async fn snapshot_observation(server: &StubServer) -> (u16, Vec<u8>, Vec<u8>) {
    let snap = server.get("/ai/snapshot").await;
    let wire = snap.serialize(false);
    (snap.status, snap.body, wire)
}

/// CDP エラー応答の前後でスナップショット応答が一致し、共有ナビゲート状態も変わらない。
#[tokio::test]
async fn aisnap7_snapshot_is_unchanged_across_unimplemented_cdp_calls() {
    let server = StubServer::start();
    let url = "https://example.com/article";
    server.navigate(url, ARTICLE);

    let before = snapshot_observation(&server).await;
    call_unimplemented_cdp(&server).await;
    let after = snapshot_observation(&server).await;

    assert_eq!(before.0, 200);
    assert_eq!(before, after);
    let v: Value = serde_json::from_slice(&after.1).expect("json");
    assert_eq!(v, expected_article(url));

    let probe = server.get("/cdp-stub/state").await;
    let p: Value = serde_json::from_slice(&probe.body).expect("json");
    assert_eq!(p["url"], url);
    let latest = server.app().navigation().latest().expect("latest");
    assert_eq!(latest.url(), url);
}

/// 未ナビゲートの `no_navigation`（409）も CDP エラー呼び出しの前後で変わらない。
#[tokio::test]
async fn aisnap7_no_navigation_error_is_unchanged_across_unimplemented_cdp_calls() {
    let server = StubServer::start();

    let before = snapshot_observation(&server).await;
    call_unimplemented_cdp(&server).await;
    let after = snapshot_observation(&server).await;

    assert_eq!(before.0, 409);
    assert_eq!(before, after);
    let v: Value = serde_json::from_slice(&after.1).expect("json");
    assert_eq!(
        v,
        serde_json::json!({"code": "no_navigation", "message": "no navigation has been performed yet"})
    );
    assert!(server.app().navigation().latest().is_none());
}

/// CDP エラーが後続ナビゲーションの結果にも影響しない（毎回の期待値と一致し、hidden 値も漏れない）。
#[tokio::test]
async fn aisnap7_cdp_errors_do_not_poison_later_navigations() {
    let server = StubServer::start();

    server.navigate("https://example.com/article", ARTICLE);
    call_unimplemented_cdp(&server).await;
    let a = server.get("/ai/snapshot").await;
    assert_eq!(a.status, 200);
    let v: Value = serde_json::from_slice(&a.body).expect("json");
    assert_eq!(v, expected_article("https://example.com/article"));

    server.navigate("https://example.com/form", FORM);
    call_unimplemented_cdp(&server).await;
    let f = server.get("/ai/snapshot").await;
    assert_eq!(f.status, 200);
    let text = String::from_utf8(f.body.clone()).expect("utf8");
    assert!(!text.contains("dummy-hidden-value"), "hidden value leaked");
    let v: Value = serde_json::from_slice(&f.body).expect("json");
    assert_eq!(v, expected_form("https://example.com/form"));
}

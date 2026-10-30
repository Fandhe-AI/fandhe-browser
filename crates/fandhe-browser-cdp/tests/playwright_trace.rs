//! Playwright `newPage()` 到達までの CDP トレース再生テスト
//! （TASK-43（43.1）・#246、ビヘイビア `CDP-2`・`CDP-6`・MS-4）。
//!
//! `harness/playwright-trace/results/newpage-trace.jsonl`（`make trace-playwright` が実 Playwright から
//! 収集・正規化したトレース。スキーマは同ディレクトリの README）を実サーバーへ再生し、
//! 「Playwright が送ったリクエストに対し、現状の実装が記録どおりの応答を返す」ことを具体値で固定する。
//! Node 不要・ネットワーク不要で常時実行できる回帰フックであり、原因調査（TASK-43.2・#247）と
//! 追加実装（TASK-43.3・#249）で応答が変わったら、トレースを再取得して本ファイルの期待値
//! （`EXPECTED_*`）を更新する。TASK-43.3a で `/json/version/` と `Browser.getVersion` を追加し、
//! TASK-43.3e（#910）で `Target.createTarget`（応答前の `Target.attachedToTarget`）と
//! `Runtime.runIfWaitingForDebugger` を追加した。以降のメソッドの no-op は追加しない（`SEC-2`・`REPAIR-3`）。
//! TASK-43.3f（#911）で `newPage()` の到達レポート（停止メソッドの一覧）を具体値で固定した（当時は未到達の記録）。
//! TASK-43.3n（#1042）で `newPage()` の到達（stage がすべて成功・失敗応答なし・クリーンアップなし）を確認し、
//! 到達として固定した。`goto()`・セレクタ取得は範囲外（TASK-44）。
//!
//! `CdpState` の構築に `Profile::open` が必要で、Windows でも `%LOCALAPPDATA%` 配下なら成功するため
//! 3 OS 共通で実行する（Issue #955。`tests/devtools_browser.rs` と同じ）。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fandhe_backend_core::server::Server;
use fandhe_browser_cdp::{BrowserId, CdpState, endpoints};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::{Value, json};

/// 収集済みトレース（リポジトリ内ファイル。`docs/spec` は参照しない）。
const TRACE: &str = include_str!("../../../harness/playwright-trace/results/newpage-trace.jsonl");

/// トレース行数・1 行サイズの上限（フィクスチャ破損時の無制限処理を防ぐ）。
const MAX_TRACE_LINES: usize = 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;

/// 現状のトレースが示す到達点（2026-10 時点・TASK-43.3o・43.3m 後）。`connectOverCDP(noDefaults:true)` は
/// `Target.setAutoAttach`（`waitForDebuggerOnStart:true`）・`Target.getTargetInfo` を経て成功し、
/// `newContext({acceptDownloads:false})` は `Target.createBrowserContext` と
/// `browserContextId` 付き `Browser.setDownloadBehavior` を経て成功する。`newPage()` は
/// `Target.createTarget` が成功して `Target.attachedToTarget`（`waitingForDebugger:true`）を応答の前に受け、
/// セッション付きの初期化で、`Page.enable`・`Page.getFrameTree`（43.3i）が成功する（`docs/design/playwright-compat.md`）。
/// `Log.enable`・`Runtime.enable`・`Network.enable`（43.3j）は有効フラグを立てて `{}` を返す（イベントは送らない。`REPAIR-3`）。
/// `Page.setLifecycleEventsEnabled`（43.3k）は有効フラグを立てて `{}` を返す。この trace は遷移を含まないため
/// `Page.lifecycleEvent` は現れない。`Page.addScriptToEvaluateOnNewDocument`（空 `source` のみ。43.3l）は
/// `{"identifier":"1"}`、セッション付き `Target.setAutoAttach`（43.3l）は状態を保持して `{}`（子 attach は 0 件）を返す。
/// `Emulation.setFocusEmulationEnabled`・`Emulation.setEmulatedMedia`（43.3m・#1041）は Playwright 既定値だけを受理して保持し
/// `{}` を返す（保持値は読まれない。`REPAIR-3`）。収集は `newContext({viewport:null})` で行うため `Browser.getWindowForTarget`・
/// `Emulation.setDeviceMetricsOverride` は送られない。`getFrameTree` の `frame.id` は targetId に一致し（43.3o・#1050）、
/// `newPage()` は成功する。直後に送られる `Page.createIsolatedWorld` は実装済み（TASK-44.6・#1217。評価器が無効な構成ではエラー）だが、`newPage()` は応答を待たず、
/// トレースには応答が記録されない（`Target.closeTarget` も送られない）。
///
/// 注意: `Browser.getVersion` の応答に `CARGO_PKG_VERSION` が入るため、workspace の版を上げたら
/// `make trace-playwright` でトレースを再取得すること。
const EXPECTED_SEND_METHODS: [&str; 18] = [
    "Browser.getVersion",
    "Target.setAutoAttach",
    "Target.getTargetInfo",
    "Target.createBrowserContext",
    "Browser.setDownloadBehavior",
    "Target.createTarget",
    "Page.enable",
    "Page.getFrameTree",
    "Log.enable",
    "Page.setLifecycleEventsEnabled",
    "Runtime.enable",
    "Page.addScriptToEvaluateOnNewDocument",
    "Network.enable",
    "Target.setAutoAttach",
    "Emulation.setFocusEmulationEnabled",
    "Emulation.setEmulatedMedia",
    "Runtime.runIfWaitingForDebugger",
    "Page.createIsolatedWorld",
];

/// トレース内の `downloadPath` の正規化済みプレースホルダ（`harness/playwright-trace/lib.mjs` の
/// `PLACEHOLDER_ARTIFACTS_DIR`）。実際の送信前に存在しない絶対パスへ戻す。
const ARTIFACTS_DIR_PLACEHOLDER: &str = "<ARTIFACTS_DIR>";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// 一時ディレクトリ（drop で再帰削除）。
///
/// `new` が非 recursive の `create_dir`（既存パスなら失敗）で排他的に新規作成できたディレクトリ
/// だけを保持するため、実行前から存在したパスをプロファイルとして開いたり削除したりしない（PROF-1）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let base = fandhe_browser_profile::test_base_dir();
        for _ in 0..64 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let candidate = base.join(format!(
                "fandhe-cdp-trace-test-{}-{nanos}-{n}",
                std::process::id()
            ));
            match std::fs::create_dir(&candidate) {
                Ok(()) => return Self(candidate),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create temp dir: {e}"),
            }
        }
        panic!("failed to create a unique temp dir");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn start(dir: &TempDir) -> (SocketAddr, Arc<CdpState>) {
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    // トレース収集サーバー（examples/trace_server.rs）と同じブラウザ ID。
    let st = Arc::new(CdpState::with_browser_id(
        app,
        BrowserId::parse("trace-1").unwrap(),
    ));
    let (router, ws_config) = endpoints(&st).expect("endpoints").into_parts();
    let bound = Server::new()
        .handler(router)
        .websocket(ws_config)
        .bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = bound.local_addr().expect("local_addr");
    tokio::spawn(bound.run());
    (addr, st)
}

/// トレースの 1 レコード（`seq` 以外は種別ごとに参照する）。
fn records() -> Vec<Value> {
    TRACE
        .lines()
        .map(|l| serde_json::from_str(l).expect("trace line is JSON"))
        .collect()
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

fn read_head(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).expect("read head"), 1, "unexpected EOF");
        buf.push(b[0]);
    }
    String::from_utf8(buf).unwrap()
}

fn status_of(head: &str) -> u16 {
    head.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// 1 回限りの HTTP GET を送り、ステータスコードを返す。
fn http_get_status(addr: SocketAddr, path: &str) -> u16 {
    let mut s = connect(addr);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes()).unwrap();
    status_of(&read_head(&mut s))
}

fn upgrade(s: &mut TcpStream, path: &str, port: u16) -> u16 {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    status_of(&read_head(s))
}

/// マスク付きテキストフレームを送る（16 bit 拡張長まで。`Target.setAutoAttach` 等の 126 バイト超に対応）。
fn send_text(s: &mut TcpStream, text: &str) {
    let payload = text.as_bytes();
    let key = [0x11u8, 0x22, 0x33, 0x44];
    let mut frame = vec![0x81];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        let len = u16::try_from(payload.len()).expect("payload fits 16 bit length");
        frame.push(0x80 | 126);
        frame.extend_from_slice(&len.to_be_bytes());
    }
    frame.extend_from_slice(&key);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
    s.write_all(&frame).unwrap();
}

fn read_text_json(s: &mut TcpStream) -> Value {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).expect("frame header");
    assert_eq!(h[0], 0x81, "expected FIN + text frame");
    assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
    let mut len = usize::from(h[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        s.read_exact(&mut ext).expect("extended length");
        len = usize::from(u16::from_be_bytes(ext));
    }
    assert!(len <= MAX_LINE_BYTES, "payload too large for test helper");
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).expect("frame payload");
    serde_json::from_slice(&payload).expect("json payload")
}

#[test]
fn cdp2_playwright_trace_is_well_formed() {
    assert!(TRACE.ends_with('\n') && !TRACE.contains('\r'), "LF only");
    assert!(TRACE.lines().count() <= MAX_TRACE_LINES);
    assert!(TRACE.lines().all(|l| l.len() <= MAX_LINE_BYTES));
    let recs = records();
    for (i, r) in recs.iter().enumerate() {
        assert_eq!(r["seq"], json!(i), "seq must be consecutive");
    }
    assert_eq!(recs[0]["kind"], json!("meta"));
    assert_eq!(recs[0]["schema"], json!(1));
    assert_eq!(recs[0]["playwright"], json!("1.63.0"));
    let sends: Vec<&str> = recs
        .iter()
        .filter(|r| r["kind"] == "cdp" && r["dir"] == "send")
        .map(|r| r["message"]["method"].as_str().expect("method is a string"))
        .collect();
    assert_eq!(sends, EXPECTED_SEND_METHODS);
    // 現状の到達点: connectOverCDP(http)・newContext・newPage がすべて成功する（43.3m・43.3o 後）。
    let stages: Vec<(&str, bool)> = recs
        .iter()
        .filter(|r| r["kind"] == "stage")
        .map(|r| (r["name"].as_str().unwrap(), r["ok"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        stages,
        [
            ("connectOverCDP(http)", true),
            ("newContext", true),
            ("newPage", true)
        ]
    );
}

#[tokio::test]
async fn cdp2_playwright_trace_http_discovery_matches_recorded_status() {
    let dir = TempDir::new();
    let (addr, _st) = start(&dir).await;
    let expected: Vec<(String, u16)> = records()
        .iter()
        .filter(|r| r["kind"] == "http")
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_owned(),
                u16::try_from(r["status"].as_u64().unwrap()).unwrap(),
            )
        })
        .collect();
    // Playwright は /json/version/（末尾スラッシュ付き）を要求する。TASK-43.3a で 200 になった。
    assert_eq!(
        expected,
        [
            ("/json/version".to_owned(), 200),
            ("/json/version/".to_owned(), 200)
        ]
    );
    tokio::task::spawn_blocking(move || {
        for (path, status) in expected {
            assert_eq!(http_get_status(addr, &path), status, "path: {path}");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp2_playwright_trace_replay_matches_recorded_responses() {
    let dir = TempDir::new();
    let (addr, st) = start(&dir).await;
    let recs = records();
    // 正規化済み `<ARTIFACTS_DIR>` は絶対パスでないため、存在しない絶対パスへ戻して送る
    // （サーバーは `downloadPath` を検証するだけでファイル操作をしない。送信後に作られていないことも確認する）。
    let artifacts = std::env::temp_dir().join("fandhe-playwright-artifacts-replay");
    let artifacts_str = artifacts.to_str().expect("utf-8 temp path").to_owned();
    let sends: Vec<Value> = recs
        .iter()
        .filter(|r| r["kind"] == "cdp" && r["dir"] == "send")
        .map(|r| {
            let mut m = r["message"].clone();
            if m["params"]["downloadPath"] == ARTIFACTS_DIR_PLACEHOLDER {
                m["params"]["downloadPath"] = json!(artifacts_str);
            }
            m
        })
        .collect();
    let recvs: Vec<Value> = recs
        .iter()
        .filter(|r| r["kind"] == "cdp" && r["dir"] == "recv")
        .map(|r| r["message"].clone())
        .collect();
    let replayed = tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        assert_eq!(
            upgrade(&mut s, "/devtools/browser/trace-1", addr.port()),
            101
        );
        let mut got = Vec::new();
        for req in &sends {
            send_text(&mut s, &req.to_string());
            // イベントを挟みうるため、対応する id の応答が来るまで読む。
            loop {
                let m = read_text_json(&mut s);
                let done = m["id"] == req["id"];
                got.push(m);
                if done {
                    break;
                }
            }
        }
        got
    })
    .await
    .unwrap();
    let product = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
    // セッション付きの未実装メソッド（`Log.enable` 等）は成功を装わず `-32601`（CDP-6・REPAIR-3）。
    let sid = "00000000000000000000000000000001";
    // `Target.createTarget` 応答の targetId（sessionId とは別カウンタ。値が同じなのは偶然）。
    let tid = "00000000000000000000000000000001";
    let version = json!({"id": 1, "result": {
        "protocolVersion": "1.3", "product": product, "userAgent": product
    }});
    assert_eq!(
        replayed,
        [
            version,
            // `waitForDebuggerOnStart:true` は受理される（43.3g。既存ターゲットは無く、イベントも付かない）。
            json!({"id": 2, "result": {}}),
            json!({"id": 3, "result": {"targetInfo": {
                "attached": true, "canAccessOpener": false,
                "targetId": "trace-1", "title": "", "type": "browser", "url": ""
            }}}),
            // `disposeOnDetach:true` の browser context を作る（43.3d。既定 context が 1、新規は 2 から）。
            json!({"id": 4, "result": {"browserContextId": "00000000000000000000000000000002"}}),
            // `browserContextId` 付きの `deny` + `eventsEnabled:true` を受理する（ダウンロード実行系は未実装）。
            json!({"id": 5, "result": {}}),
            // `Target.createTarget`（43.3e）: `Target.attachedToTarget` が応答より前に届く（CDP-5）。
            // `waitForDebuggerOnStart:true` の接続なので `waitingForDebugger:true`。
            json!({"method": "Target.attachedToTarget", "params": {
                "sessionId": sid,
                "targetInfo": {
                    "attached": true, "browserContextId": "00000000000000000000000000000002",
                    "canAccessOpener": false, "targetId": "00000000000000000000000000000001",
                    "title": "about:blank", "type": "page", "url": "about:blank"
                },
                "waitingForDebugger": true
            }}),
            json!({"id": 6, "result": {"targetId": "00000000000000000000000000000001"}}),
            // `Page.enable`・`Page.getFrameTree`（43.3i）は実装済み。止めたターゲットでも初期 about:blank の
            // メインフレーム（`loaderId` は初期確定後の世代）を返す。
            json!({"id": 7, "sessionId": sid, "result": {}}),
            // メインフレームの `frame.id` は targetId（TASK-43.3o・#1050。sessionId とは別カウンタの偶然の一致）。
            json!({"id": 8, "sessionId": sid, "result": {"frameTree": {"frame": {
                "id": tid, "loaderId": "2", "url": "about:blank", "mimeType": "text/html"
            }}}}),
            // `Log.enable`・`Runtime.enable`・`Network.enable`（43.3j）は受理して `{}`（イベントなし）。
            json!({"id": 9, "sessionId": sid, "result": {}}),
            // `Page.setLifecycleEventsEnabled`（43.3k）は有効フラグを立てて `{}`（この trace は遷移を含まず lifecycle は現れない）。
            json!({"id": 10, "sessionId": sid, "result": {}}),
            json!({"id": 11, "sessionId": sid, "result": {}}),
            // 空 `source` の `Page.addScriptToEvaluateOnNewDocument`（43.3l）は no-op で、最初の identifier は "1"。
            json!({"id": 12, "sessionId": sid, "result": {"identifier": "1"}}),
            json!({"id": 13, "sessionId": sid, "result": {}}),
            // セッション付き `Target.setAutoAttach`（43.3l）は状態を保持するだけ。子ターゲットは作らず `{}`。
            json!({"id": 14, "sessionId": sid, "result": {}}),
            // `Emulation.setFocusEmulationEnabled`・`setEmulatedMedia`（43.3m・#1041）は既定値を受理して `{}`（保持のみ）。
            json!({"id": 15, "sessionId": sid, "result": {}}),
            json!({"id": 16, "sessionId": sid, "result": {}}),
            // 止めたセッションの `Runtime.runIfWaitingForDebugger` は実際に再開して `{}`。
            json!({"id": 17, "sessionId": sid, "result": {}}),
            // `Page.createIsolatedWorld` は実装済みだが、このテストの `AppState` は評価器が無効のため
            // 使えない contextId を返さずエラー（TASK-44.6・#1217。トレースでは `newPage()` が応答を待たず未記録）。
            json!({"id": 18, "sessionId": sid, "error": {"code": -32000, "message": "execution contexts unavailable"}}),
        ]
    );
    assert!(!artifacts.exists(), "downloadPath must not be created");
    // 記録は `Page.createIsolatedWorld`（評価器が無効でエラー）の応答より前に終わるため、最後の 1 件だけ記録に無い。
    assert_eq!(
        replayed.get(..recvs.len()),
        Some(recvs.as_slice()),
        "replay must match the recorded responses"
    );
    assert_eq!(replayed.len(), recvs.len() + 1);

    // 受信メソッドログ（CDP-6）がトレースの送信メソッドと一致する。
    let snap = st.received_methods();
    let logged: Vec<(&str, u64, u64)> = snap
        .methods
        .iter()
        .map(|(k, c)| (k.as_str(), c.handled, c.unimplemented))
        .collect();
    assert_eq!(
        logged,
        [
            ("Browser.getVersion", 1, 0),
            ("Browser.setDownloadBehavior", 1, 0),
            ("Emulation.setEmulatedMedia", 1, 0),
            ("Emulation.setFocusEmulationEnabled", 1, 0),
            ("Log.enable", 1, 0),
            ("Network.enable", 1, 0),
            ("Page.addScriptToEvaluateOnNewDocument", 1, 0),
            ("Page.createIsolatedWorld", 1, 0),
            ("Page.enable", 1, 0),
            ("Page.getFrameTree", 1, 0),
            ("Page.setLifecycleEventsEnabled", 1, 0),
            ("Runtime.enable", 1, 0),
            ("Runtime.runIfWaitingForDebugger", 1, 0),
            ("Target.createBrowserContext", 1, 0),
            ("Target.createTarget", 1, 0),
            ("Target.getTargetInfo", 1, 0),
            ("Target.setAutoAttach", 2, 0)
        ],
        "snapshot: {snap:?}"
    );
}

#[tokio::test]
async fn cdp2_auto_attach_over_websocket_emits_event_after_response_and_frees_session_on_close() {
    let dir = TempDir::new();
    let (addr, st) = start(&dir).await;
    let target = st
        .registry()
        .create_target(fandhe_browser_cdp::TargetKind::Page, "about:blank")
        .unwrap();
    let st2 = Arc::clone(&st);
    let target_id = target.as_str().to_owned();
    let default_context = st.registry().default_browser_context().as_str().to_owned();
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        assert_eq!(
            upgrade(&mut s, "/devtools/browser/trace-1", addr.port()),
            101
        );
        let req = json!({"id": 1, "method": "Target.setAutoAttach",
            "params": {"autoAttach": true, "flatten": true}});
        send_text(&mut s, &req.to_string());
        // 送出順は応答 → イベント（CDP-5）。
        assert_eq!(read_text_json(&mut s), json!({"id": 1, "result": {}}));
        let ev = read_text_json(&mut s);
        assert_eq!(ev["method"], "Target.attachedToTarget");
        assert_eq!(ev["params"]["targetInfo"]["targetId"], target_id);
        assert_eq!(ev["params"]["targetInfo"]["attached"], true);
        // 既存ページは既定 browser context に属する（Playwright が assert する。CDP-2）。
        assert_eq!(
            ev["params"]["targetInfo"]["browserContextId"],
            default_context
        );
        assert_eq!(ev["params"]["waitingForDebugger"], false);
        assert_eq!(st2.registry().session_count(), 1);
        // 切断でこの接続の自動 attach セッションが解放される（CDP-7）。
        drop(s);
        let mut freed = false;
        for _ in 0..100 {
            if st2.registry().session_count() == 0 {
                freed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(freed, "session must be released on disconnect");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp2_close_target_over_websocket_emits_detached_after_response() {
    let dir = TempDir::new();
    let (addr, st) = start(&dir).await;
    let target = st
        .registry()
        .create_target(fandhe_browser_cdp::TargetKind::Page, "about:blank")
        .unwrap();
    let st2 = Arc::clone(&st);
    let target_id = target.as_str().to_owned();
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        assert_eq!(
            upgrade(&mut s, "/devtools/browser/trace-1", addr.port()),
            101
        );
        let req = json!({"id": 1, "method": "Target.setAutoAttach",
            "params": {"autoAttach": true, "flatten": true}});
        send_text(&mut s, &req.to_string());
        assert_eq!(read_text_json(&mut s), json!({"id": 1, "result": {}}));
        let attached = read_text_json(&mut s);
        let session_id = attached["params"]["sessionId"].as_str().unwrap().to_owned();
        let req = json!({"id": 2, "method": "Target.closeTarget",
            "params": {"targetId": target_id}});
        send_text(&mut s, &req.to_string());
        // 送出順は応答 → detachedFromTarget（CDP-5）。
        assert_eq!(
            read_text_json(&mut s),
            json!({"id": 2, "result": {"success": true}})
        );
        assert_eq!(
            read_text_json(&mut s),
            json!({"method": "Target.detachedFromTarget",
                "params": {"sessionId": session_id, "targetId": target_id}})
        );
        // 接続は閉じたページの後も使える。
        send_text(
            &mut s,
            &json!({"id": 3, "method": "Browser.getVersion"}).to_string(),
        );
        assert_eq!(read_text_json(&mut s)["id"], 3);
        assert!(st2.registry().target(&target).is_none());
        assert_eq!(st2.registry().session_count(), 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cdp2_create_target_over_websocket_emits_attached_before_response_and_waits_for_debugger() {
    let dir = TempDir::new();
    let (addr, st) = start(&dir).await;
    let st2 = Arc::clone(&st);
    tokio::task::spawn_blocking(move || {
        let mut s = connect(addr);
        assert_eq!(
            upgrade(&mut s, "/devtools/browser/trace-1", addr.port()),
            101
        );
        let req = json!({"id": 1, "method": "Target.setAutoAttach",
            "params": {"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}});
        send_text(&mut s, &req.to_string());
        assert_eq!(read_text_json(&mut s), json!({"id": 1, "result": {}}));
        let req = json!({"id": 2, "method": "Target.createTarget",
            "params": {"url": "about:blank"}});
        send_text(&mut s, &req.to_string());
        // 送出順は attachedToTarget → 応答（CDP-5。Playwright は応答時点でページがある前提）。
        let ev = read_text_json(&mut s);
        assert_eq!(ev["method"], "Target.attachedToTarget");
        assert_eq!(ev["params"]["waitingForDebugger"], true);
        let target_id = ev["params"]["targetInfo"]["targetId"]
            .as_str()
            .unwrap()
            .to_owned();
        let session_id = ev["params"]["sessionId"].as_str().unwrap().to_owned();
        assert_eq!(
            read_text_json(&mut s),
            json!({"id": 2, "result": {"targetId": target_id}})
        );
        // ブラウザ ID（trace-1）とは別。再開前は文書が無く、再開後に確定する。
        assert_ne!(target_id, "trace-1");
        let doc = json!({"id": 3, "method": "DOM.getDocument", "sessionId": session_id});
        send_text(&mut s, &doc.to_string());
        assert_eq!(read_text_json(&mut s)["error"]["code"], -32000);
        let run =
            json!({"id": 4, "method": "Runtime.runIfWaitingForDebugger", "sessionId": session_id});
        send_text(&mut s, &run.to_string());
        assert_eq!(
            read_text_json(&mut s),
            json!({"id": 4, "result": {}, "sessionId": session_id})
        );
        send_text(
            &mut s,
            &json!({"id": 5, "method": "DOM.getDocument", "sessionId": session_id}).to_string(),
        );
        assert!(read_text_json(&mut s)["result"]["root"].is_object());
        assert_eq!(st2.registry().target_count(), 1);
    })
    .await
    .unwrap();
}

/// 到達レポートの失敗した 1 呼び出し（エラー応答を同じ `id` の送信と突き合わせたもの）。
#[derive(Debug, PartialEq)]
struct FailedCall {
    id: u64,
    method: String,
    /// `sessionId` 付き（ページセッション宛て）か。
    session: bool,
    code: i64,
    message: String,
}

/// `newPage()` が最初に止まった地点（失敗した stage の `error` から取り出す）。
#[derive(Debug, PartialEq)]
struct FirstStop {
    stage: String,
    method: String,
    error: String,
}

/// トレースから作る到達レポート。戻り値を構造体にして将来の項目追加に備える（`REPAIR-4`）。
#[derive(Debug, PartialEq)]
struct ReachReport {
    stages: Vec<(String, bool)>,
    first_stop: Option<FirstStop>,
    failed: Vec<FailedCall>,
    /// 失敗時のクリーンアップ系メソッド（送信した `Target.closeTarget`・受信した `Target.detachedFromTarget`）と、
    /// 初期ナビゲーション系イベント（`Page.frameNavigated`・`Page.lifecycleEvent`）の観測結果。
    observed: Vec<String>,
}

/// Playwright のエラー文言 `... Protocol error (<Method>): ...` からメソッド名を取り出す。
/// 外部入力（トレース）のため `find`・`get` だけで処理し、panic しない。
fn method_from_protocol_error(error: &str) -> Option<String> {
    const MARK: &str = "Protocol error (";
    let start = error.find(MARK)? + MARK.len();
    let rest = error.get(start..)?;
    let end = rest.find(')')?;
    rest.get(..end).map(str::to_owned)
}

/// トレースを走査して到達レポートを作る。
/// 最初の停止は、失敗した stage の `error` 文字列から決める（個々の CDP エラーは停止原因とは限らないため）。
fn reach_report(recs: &[Value]) -> ReachReport {
    let mut stages = Vec::new();
    let mut first_stop = None;
    let mut sent: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut failed = Vec::new();
    let mut observed = Vec::new();
    for r in recs {
        match r.get("kind").and_then(Value::as_str) {
            Some("stage") => {
                let name = r.get("name").and_then(Value::as_str).unwrap_or("");
                let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
                stages.push((name.to_owned(), ok));
                if !ok && first_stop.is_none() {
                    let error = r.get("error").and_then(Value::as_str).unwrap_or("");
                    first_stop = Some(FirstStop {
                        stage: name.to_owned(),
                        method: method_from_protocol_error(error).unwrap_or_default(),
                        error: error.to_owned(),
                    });
                }
            }
            Some("cdp") => {
                let dir = r.get("dir").and_then(Value::as_str);
                let Some(msg) = r.get("message") else {
                    continue;
                };
                let id = msg.get("id").and_then(Value::as_u64);
                let method = msg.get("method").and_then(Value::as_str);
                let watched = matches!(
                    (dir, method),
                    (Some("send"), Some("Target.closeTarget"))
                        | (
                            Some("recv"),
                            Some(
                                "Target.detachedFromTarget"
                                    | "Page.frameNavigated"
                                    | "Page.lifecycleEvent"
                            )
                        )
                );
                if let (true, Some(m)) = (watched, method) {
                    observed.push(format!("{}:{m}", dir.unwrap_or("")));
                }
                if dir == Some("send") {
                    if let (Some(id), Some(m)) = (id, msg.get("method").and_then(Value::as_str)) {
                        sent.insert(id, m.to_owned());
                    }
                } else if let (Some(id), Some(err)) = (id, msg.get("error")) {
                    failed.push(FailedCall {
                        id,
                        method: sent.get(&id).cloned().unwrap_or_default(),
                        session: msg.get("sessionId").is_some(),
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    failed.sort_by_key(|f| f.id);
    ReachReport {
        stages,
        first_stop,
        failed,
        observed,
    }
}

/// `newPage()` 到達の確認テスト（TASK-43.3n・#1042、`CDP-2`・`CDP-6`・MS-4。前身は TASK-43.3f・#911 の未到達記録）。
///
/// 到達の基準は、stages がすべて成功・`first_stop` が `None`・`failed` が空であること。
/// `failed` が空なのは、収集オプション `newContext({viewport:null})`（TASK-43.3m）により
/// `Browser.getWindowForTarget` が送られないため（許容条件の「`-32601` のみ」より厳しい）。
/// `getWindowForTarget` は実体が無く値を作ると偽になるため、方針どおり `-32601` のまま残す（`SEC-2`・`CDP-6`）。
/// 直後に送られる `Page.createIsolatedWorld` は評価器が無効だとエラーになるが、`newPage()` は応答を待たない。
/// そのため応答は記録されない場合がある（収集のたびに入ることがある競合。記録済みトレースには無い）。
/// 失敗時のクリーンアップ（`Target.closeTarget` 送信・`Target.detachedFromTarget` 受信）と、
/// 初期 `about:blank` の `Page.frameNavigated`・`Page.lifecycleEvent` が無いまま成功することも固定する
/// （後者は段階 4 が不要という判断の実測根拠）。`goto()`・セレクタ取得・`evaluate` は範囲外（TASK-44・`CDP-2`）。
/// 期待値を緩めたり `#[ignore]` で通したりしない。
#[test]
fn cdp2_playwright_newpage_is_reached_without_failures_or_cleanup() {
    let report = reach_report(&records());
    let pretty = format!("{report:#?}");
    let stages: Vec<(&str, bool)> = report
        .stages
        .iter()
        .map(|(n, ok)| (n.as_str(), *ok))
        .collect();
    assert_eq!(
        stages,
        [
            ("connectOverCDP(http)", true),
            ("newContext", true),
            ("newPage", true)
        ],
        "{pretty}"
    );
    assert_eq!(report.first_stop, None, "{pretty}");
    let failed: Vec<(u64, &str, bool, i64, &str)> = report
        .failed
        .iter()
        .map(|f| {
            (
                f.id,
                f.method.as_str(),
                f.session,
                f.code,
                f.message.as_str(),
            )
        })
        .collect();
    assert_eq!(
        failed,
        Vec::<(u64, &str, bool, i64, &str)>::new(),
        "{pretty}"
    );
    assert_eq!(report.observed, Vec::<String>::new(), "{pretty}");
}

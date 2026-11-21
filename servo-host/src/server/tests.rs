//! `server` の単体テスト（`RENDER-3`・`RENDER-4`・`PLUG-1`・`SEC-2`）。
//!
//! 偽ハンドラと in-memory の入出力で、フレーム codec・ディスパッチ・上限・識別面の拒否・
//! ログの秘匿を具体値で検証する。ゴールデン fixture（本体側と共有する契約）も全件読む。

use std::cell::RefCell;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};

use super::*;

/// 偽ハンドラ。呼ばれたメソッドを記録し、既定の成功値か指定したエラーを返す。
struct FakeHandler {
    calls: Rc<RefCell<Vec<&'static str>>>,
    final_url: String,
    error: Option<ErrorKind>,
    /// 真なら操作モードのハンドラとして振る舞う（`dom_snapshot` を載せ、`load` に `page` を返す）。
    interactive: bool,
    /// `dom_snapshot` が返す HTML。
    html: String,
}

impl FakeHandler {
    fn new() -> (Self, Rc<RefCell<Vec<&'static str>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                calls: Rc::clone(&calls),
                final_url: "https://example.com/".to_owned(),
                error: None,
                interactive: false,
                html: "<html data-fandhe-n=\"0\"></html>".to_owned(),
            },
            calls,
        )
    }
    fn record(&self, name: &'static str) -> Result<(), HandlerError> {
        self.calls.borrow_mut().push(name);
        self.error.map_or(Ok(()), |k| Err(HandlerError(k)))
    }
}

impl Handler for FakeHandler {
    fn capabilities(&self) -> &'static [Method] {
        if self.interactive {
            &[
                Method::Load,
                Method::Screenshot,
                Method::Visibility,
                Method::BoundingBox,
                Method::DomSnapshot,
                Method::Act,
                Method::Wait,
                Method::Shutdown,
            ]
        } else {
            &[
                Method::Load,
                Method::Screenshot,
                Method::Visibility,
                Method::BoundingBox,
                Method::Shutdown,
            ]
        }
    }
    fn load(&mut self, _p: &LoadParams) -> Result<LoadResult, HandlerError> {
        self.record("load")?;
        Ok(LoadResult {
            final_url: self.final_url.clone(),
            status: Some(200),
            page: self.interactive.then(page_info),
        })
    }
    fn dom_snapshot(&mut self) -> Result<DomSnapshotResult, HandlerError> {
        self.record("dom_snapshot")?;
        Ok(DomSnapshotResult {
            page: page_info(),
            url: "https://example.com/".to_owned(),
            html: self.html.clone(),
            element_count: 1,
            snapshot: "s1".to_owned(),
            iframes: 2,
            shadow_roots: 0,
            redacted: 1,
        })
    }
    fn act(&mut self, _p: &ActParams) -> Result<ActResult, HandlerError> {
        self.record("act")?;
        Ok(ActResult {
            page: page_info(),
            navigated: false,
        })
    }
    fn wait(&mut self, _p: &WaitParams) -> Result<WaitResult, HandlerError> {
        self.record("wait")?;
        Ok(WaitResult { page: page_info() })
    }
    fn screenshot(&mut self, _p: &ScreenshotParams) -> Result<ScreenshotResult, HandlerError> {
        self.record("screenshot")?;
        Ok(ScreenshotResult {
            width: 1280,
            height: 800,
            data_base64: "iVBORw0KGgo=".to_owned(),
        })
    }
    fn visibility(&mut self, _p: &SelectorParams) -> Result<VisibilityResult, HandlerError> {
        self.record("visibility")?;
        Ok(VisibilityResult {
            visible: true,
            reason: None,
        })
    }
    fn bounding_box(&mut self, _p: &SelectorParams) -> Result<BoundingBoxResult, HandlerError> {
        self.record("bounding_box")?;
        Ok(BoundingBoxResult {
            x: 8.5,
            y: -2.0,
            width: 120.25,
            height: 18.0,
        })
    }
}

fn page_info() -> PageInfo {
    PageInfo {
        session: "0123456789abcdef0123456789abcdef".to_owned(),
        doc_seq: 3,
    }
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut v = (body.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(body);
    v
}

fn jframe(v: &Value) -> Vec<u8> {
    frame(&serde_json::to_vec(v).unwrap())
}

/// 出力バイト列をフレーム列に分解する。
fn split_frames(mut out: &[u8]) -> Vec<Value> {
    let mut v = Vec::new();
    while !out.is_empty() {
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        v.push(serde_json::from_slice(&out[4..4 + len]).unwrap());
        out = &out[4 + len..];
    }
    v
}

struct Run {
    outcome: ServeOutcome,
    frames: Vec<Value>,
    log: String,
}

fn run_with<H: Handler>(h: &mut H, input: Vec<u8>) -> Run {
    let mut out = Vec::new();
    let mut log = Vec::new();
    let outcome = serve(Cursor::new(input), &mut out, h, &mut log);
    Run {
        outcome,
        frames: split_frames(&out),
        log: String::from_utf8(log).unwrap(),
    }
}

fn run(input: Vec<u8>) -> (Run, Vec<&'static str>) {
    let (mut h, calls) = FakeHandler::new();
    let r = run_with(&mut h, input);
    let c = calls.borrow().clone();
    (r, c)
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("crates")
        .join("fandhe-browser-render")
        .join("tests")
        .join("fixtures")
        .join("protocol")
}

fn load_fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixture_dir().join(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// 本体側と共有するゴールデン fixture の要求を全件、ホストへ通す。
#[test]
fn request_fixtures_match_host_behavior() {
    let mut count = 0;
    let mut entries: Vec<_> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("request-") && n.ends_with(".json"))
        })
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let fx = load_fixture(&name);
        let body = match (fx.get("message"), fx.get("raw").and_then(Value::as_str)) {
            (Some(m), _) => serde_json::to_vec(m).unwrap(),
            (None, Some(raw)) => raw.as_bytes().to_vec(),
            _ => panic!("{name}: neither message nor raw"),
        };
        let (r, calls) = run(frame(&body));
        count += 1;
        if fx.get("expected_host_action").and_then(Value::as_str) == Some("close_connection") {
            assert!(
                matches!(r.outcome, ServeOutcome::ProtocolViolation(_)),
                "{name}: {:?}",
                r.outcome
            );
            assert_eq!(r.frames.len(), 1, "{name}: only hello expected");
            assert!(calls.is_empty(), "{name}");
        } else if let Some(expected) = fx.get("expected_response") {
            assert_eq!(r.frames.get(1), Some(expected), "{name}");
            assert!(calls.is_empty(), "{name}: handler must not be called");
        } else {
            let parsed: Value = serde_json::from_slice(&body).unwrap();
            let method = parsed["method"].as_str().unwrap().to_owned();
            let resp = r
                .frames
                .get(1)
                .unwrap_or_else(|| panic!("{name}: no response"));
            assert!(resp.get("result").is_some(), "{name}: {resp}");
            assert!(resp.get("error").is_none(), "{name}");
            if method == "shutdown" {
                assert_eq!(resp["result"], json!({}), "{name}");
                assert_eq!(r.outcome, ServeOutcome::Shutdown, "{name}");
                assert!(calls.is_empty(), "{name}");
            } else {
                assert_eq!(calls, vec![method.as_str()], "{name}");
            }
        }
    }
    assert!(count >= 30, "fixture count too small: {count}");
}

#[test]
fn hello_is_first_frame_and_matches_fixture() {
    let (r, _) = run(Vec::new());
    assert_eq!(r.outcome, ServeOutcome::InputClosed);
    assert_eq!(r.frames.len(), 1);
    let hello = &r.frames[0];
    let fx = load_fixture("hello-ok.json");
    let expected = &fx["message"];
    assert_eq!(hello["v"], 1);
    assert_eq!(hello["event"], "hello");
    assert!(hello.get("id").is_none());
    assert_eq!(hello["data"]["limits"], expected["data"]["limits"]);
    assert_eq!(hello["data"]["protocol"], json!({"min": 1, "max": 1}));
    assert_eq!(
        hello["data"]["capabilities"],
        expected["data"]["capabilities"]
    );
    assert_eq!(hello["data"]["servo_version"], "0.7.0");
    assert_eq!(hello["data"]["host_version"], env!("CARGO_PKG_VERSION"));
    let bytes = serde_json::to_vec(hello).unwrap();
    assert!(bytes.len() <= MAX_HELLO_FRAME_BYTES);
}

#[test]
fn unimplemented_handler_advertises_only_shutdown_and_refuses_everything() {
    let mut h = UnimplementedHandler;
    let input = [
        jframe(&json!({"v":1,"id":1,"method":"load","params":{"url":"https://example.com/"}})),
        jframe(&json!({"v":1,"id":2,"method":"screenshot"})),
        jframe(&json!({"v":1,"id":3,"method":"visibility","params":{"selector":"a"}})),
        jframe(&json!({"v":1,"id":4,"method":"bounding_box","params":{"selector":"a"}})),
    ]
    .concat();
    let r = run_with(&mut h, input);
    assert_eq!(r.frames[0]["data"]["capabilities"], json!(["shutdown"]));
    for (i, id) in (1..=4).enumerate() {
        assert_eq!(
            r.frames[i + 1],
            json!({"v":1,"id":id,"error":{"kind":"unsupported","message":"Method is not supported."}})
        );
    }
}

#[test]
fn codec_violations_end_without_response() {
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "zero length",
            0u32.to_le_bytes().to_vec(),
            "frame length is zero",
        ),
        (
            "over limit without body",
            65_537u32.to_le_bytes().to_vec(),
            "frame length exceeds limit",
        ),
        (
            "u32 max",
            u32::MAX.to_le_bytes().to_vec(),
            "frame length exceeds limit",
        ),
        ("partial header", vec![1, 0], "truncated frame header"),
        (
            "short body",
            [10u32.to_le_bytes().to_vec(), b"{}".to_vec()].concat(),
            "truncated frame body",
        ),
        (
            "invalid utf8",
            frame(&[0xff, 0xfe, 0x7b]),
            "request is not valid UTF-8",
        ),
        (
            "text as length",
            b"{\"v\":1}".to_vec(),
            "frame length exceeds limit",
        ),
        ("not object", frame(b"[1]"), "request is not a JSON object"),
    ];
    for (name, input, reason) in cases {
        let (r, calls) = run(input);
        assert_eq!(r.outcome, ServeOutcome::ProtocolViolation(reason), "{name}");
        assert_eq!(r.frames.len(), 1, "{name}");
        assert!(calls.is_empty(), "{name}");
    }
}

#[test]
fn request_of_exactly_max_size_is_accepted() {
    // 64 KiB ちょうどのフレーム: params を巨大にして invalid_params（長さではなく中身で拒否）になる。
    let prefix = br#"{"v":1,"id":1,"method":"shutdown","params":{"pad":""#;
    let suffix = br#""}}"#;
    let pad = MAX_REQUEST_FRAME_BYTES - prefix.len() - suffix.len();
    let mut body = prefix.to_vec();
    body.extend(std::iter::repeat_n(b'a', pad));
    body.extend_from_slice(suffix);
    assert_eq!(body.len(), MAX_REQUEST_FRAME_BYTES);
    let (r, _) = run(frame(&body));
    assert_eq!(r.frames[1]["error"]["kind"], "invalid_params");
}

fn resp_kind(input: Value) -> Option<String> {
    let (r, _) = run(jframe(&input));
    r.frames
        .get(1)?
        .get("error")
        .and_then(|e| e["kind"].as_str())
        .map(str::to_owned)
}

fn load_req(url: &str) -> Value {
    json!({"v":1,"id":1,"method":"load","params":{"url":url}})
}

#[test]
fn size_limits_are_enforced_in_bytes() {
    let host = |n: usize| format!("https://e.com/{}", "a".repeat(n - "https://e.com/".len()));
    assert_eq!(resp_kind(load_req(&host(2048))), None);
    assert_eq!(
        resp_kind(load_req(&host(2049))).as_deref(),
        Some("invalid_params")
    );
    let sel = |s: String| json!({"v":1,"id":1,"method":"visibility","params":{"selector":s}});
    assert_eq!(resp_kind(sel("a".repeat(1024))), None);
    assert_eq!(
        resp_kind(sel("a".repeat(1025))).as_deref(),
        Some("invalid_params")
    );
    // 多バイト文字はバイト数で数える（341 文字 x 3 バイト = 1023、342 文字 = 1026）。
    assert_eq!(resp_kind(sel("あ".repeat(341))), None);
    assert_eq!(
        resp_kind(sel("あ".repeat(342))).as_deref(),
        Some("invalid_params")
    );
    assert_eq!(
        resp_kind(sel(String::new())).as_deref(),
        Some("invalid_params")
    );
}

#[test]
fn timeout_and_id_boundaries() {
    let t = |raw: &str| {
        let body = format!(
            r#"{{"v":1,"id":1,"method":"load","params":{{"url":"https://e.com/","timeout_ms":{raw}}}}}"#
        );
        let (r, _) = run(frame(body.as_bytes()));
        r.frames[1]["error"]["kind"].as_str().map(str::to_owned)
    };
    assert_eq!(t("1"), None);
    assert_eq!(t("60000"), None);
    assert_eq!(t("10000.0"), None);
    assert_eq!(t("0").as_deref(), Some("invalid_params"));
    assert_eq!(t("60001").as_deref(), Some("invalid_params"));
    assert_eq!(t("10000.5").as_deref(), Some("invalid_params"));
    assert_eq!(t("-5").as_deref(), Some("invalid_params"));
    assert_eq!(t("\"10\"").as_deref(), Some("invalid_params"));

    let id = |raw: &str| {
        let body = format!(r#"{{"v":1,"id":{raw},"method":"shutdown"}}"#);
        run(frame(body.as_bytes())).0.outcome
    };
    assert_eq!(id("9007199254740991"), ServeOutcome::Shutdown);
    assert!(matches!(
        id("9007199254740992"),
        ServeOutcome::ProtocolViolation(_)
    ));
    assert!(matches!(id("1.5"), ServeOutcome::ProtocolViolation(_)));
    assert!(matches!(id("null"), ServeOutcome::ProtocolViolation(_)));
}

/// 識別面（UA・ヘッダ・Cookie・locale・TZ・proxy・画面サイズ）は params として受け付けない（SEC-2）。
#[test]
fn identity_surface_params_are_rejected_for_every_method() {
    let base = |m: &str| match m {
        "load" => json!({"url":"https://e.com/"}),
        "screenshot" => json!({}),
        _ => json!({"selector":"a"}),
    };
    for method in [
        "load",
        "screenshot",
        "visibility",
        "bounding_box",
        "shutdown",
    ] {
        for key in [
            "user_agent",
            "headers",
            "cookie",
            "locale",
            "timezone",
            "proxy",
            "width",
            "height",
        ] {
            let mut params = base(method);
            if method == "shutdown" {
                params = json!({});
            }
            params[key] = json!("x");
            let (r, calls) = run(jframe(
                &json!({"v":1,"id":3,"method":method,"params":params}),
            ));
            assert_eq!(
                r.frames[1],
                json!({"v":1,"id":3,"error":{"kind":"invalid_params","message":"Invalid parameters."}}),
                "{method}/{key}"
            );
            assert!(calls.is_empty(), "{method}/{key}");
        }
    }
}

#[test]
fn url_validation_cases() {
    for ok in [
        "https://example.com/",
        "HTTPS://EXAMPLE.COM/",
        "http://[::1]:8080/p?q=1#f",
        "https://example.com:443",
    ] {
        assert_eq!(resp_kind(load_req(ok)), None, "{ok}");
    }
    for bad in [
        "about:blank",
        "file:///etc/passwd",
        "data:text/html,x",
        "javascript:alert(1)",
        "https://u:p@example.com/",
        "https://:80/",
        "https://example.com:8a/",
        "https://exa mple.com/",
        "https://example.com/\u{7}",
        "https://example.com\\evil",
        "https://",
        "",
    ] {
        assert_eq!(
            resp_kind(load_req(bad)).as_deref(),
            Some("invalid_params"),
            "{bad:?}"
        );
    }
}

#[test]
fn requests_are_processed_one_at_a_time_in_order() {
    let input = [
        jframe(&load_req("https://example.com/a")),
        jframe(&json!({"v":1,"id":2,"method":"screenshot"})),
    ]
    .concat();
    let (r, calls) = run(input);
    assert_eq!(r.outcome, ServeOutcome::InputClosed);
    assert_eq!(r.frames[1]["id"], 1);
    assert_eq!(r.frames[2]["id"], 2);
    assert_eq!(r.frames[2]["result"]["format"], "png");
    assert_eq!(calls, vec!["load", "screenshot"]);
}

#[test]
fn shutdown_stops_without_reading_further() {
    let input = [
        jframe(&json!({"v":1,"id":1,"method":"shutdown"})),
        jframe(&load_req("https://example.com/")),
    ]
    .concat();
    let (r, calls) = run(input);
    assert_eq!(r.outcome, ServeOutcome::Shutdown);
    assert_eq!(r.frames.len(), 2);
    assert_eq!(r.frames[1], json!({"v":1,"id":1,"result":{}}));
    assert!(calls.is_empty());
}

#[test]
fn handler_errors_map_to_fixed_messages() {
    let table = [
        (ErrorKind::NotLoaded, "not_loaded", "No page is loaded."),
        (
            ErrorKind::NotFound,
            "not_found",
            "No element matches the selector.",
        ),
        (ErrorKind::Timeout, "timeout", "Operation timed out."),
        (ErrorKind::LoadFailed, "load_failed", "Navigation failed."),
        (ErrorKind::Internal, "internal", "Internal host error."),
        (
            ErrorKind::Unsupported,
            "unsupported",
            "Method is not supported.",
        ),
    ];
    for (kind, name, msg) in table {
        let (mut h, _) = FakeHandler::new();
        h.error = Some(kind);
        let r = run_with(&mut h, jframe(&load_req("https://example.com/")));
        assert_eq!(
            r.frames[1],
            json!({"v":1,"id":1,"error":{"kind":name,"message":msg}})
        );
        assert!(msg.len() <= MAX_ERROR_MESSAGE_BYTES);
    }
}

#[test]
fn error_messages_match_golden_fixtures() {
    for (file, kind) in [
        ("response-error-internal.json", ErrorKind::Internal),
        (
            "response-error-invalid-params.json",
            ErrorKind::InvalidParams,
        ),
        (
            "response-error-invalid-request.json",
            ErrorKind::InvalidRequest,
        ),
        ("response-error-load-failed.json", ErrorKind::LoadFailed),
        ("response-error-not-found.json", ErrorKind::NotFound),
        ("response-error-not-loaded.json", ErrorKind::NotLoaded),
        ("response-error-timeout.json", ErrorKind::Timeout),
        ("response-error-unsupported.json", ErrorKind::Unsupported),
    ] {
        let fx = load_fixture(file);
        let err = &fx["message"]["error"];
        assert_eq!(err["kind"], kind.as_str(), "{file}");
        assert_eq!(err["message"], kind.message(), "{file}");
    }
}

#[test]
fn oversized_response_is_replaced_by_internal_error_and_loop_continues() {
    let (mut h, _) = FakeHandler::new();
    h.final_url = "a".repeat(MAX_RESPONSE_FRAME_BYTES + 1);
    let input = [
        jframe(&load_req("https://example.com/")),
        jframe(&json!({"v":1,"id":2,"method":"visibility","params":{"selector":"a"}})),
    ]
    .concat();
    let r = run_with(&mut h, input);
    assert_eq!(
        r.frames[1],
        json!({"v":1,"id":1,"error":{"kind":"internal","message":"Internal host error."}})
    );
    assert_eq!(r.frames[2], json!({"v":1,"id":2,"result":{"visible":true}}));
    assert_eq!(r.outcome, ServeOutcome::InputClosed);
}

#[test]
fn frame_size_limits_are_checked_before_writing() {
    let mut out = Vec::new();
    assert!(write_frame(&mut out, &[b'x'; 16], 16).is_ok());
    assert!(matches!(
        write_frame(&mut out, &[b'x'; 17], 16),
        Err(FrameError::Violation(_))
    ));
    assert_eq!(MAX_SCREENSHOT_FRAME_BYTES, 24 * 1024 * 1024);
}

#[test]
fn logs_never_contain_url_or_selector() {
    let input = [
        jframe(&load_req("https://example.com/secret?token=abc123")),
        jframe(
            &json!({"v":1,"id":2,"method":"visibility","params":{"selector":"#private-selector"}}),
        ),
    ]
    .concat();
    let (r, _) = run(input);
    for needle in ["example.com", "token", "abc123", "private-selector"] {
        assert!(!r.log.contains(needle), "log leaked {needle}: {}", r.log);
    }
    assert!(r.log.contains("method=load id=1 result=ok"), "{}", r.log);
}

struct BrokenPipe;
impl Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn write_failure_ends_with_io_outcome() {
    let (mut h, _) = FakeHandler::new();
    let mut log = Vec::new();
    let outcome = serve(Cursor::new(Vec::new()), BrokenPipe, &mut h, &mut log);
    assert_eq!(outcome, ServeOutcome::Io(std::io::ErrorKind::BrokenPipe));
}

#[test]
fn servo_version_matches_manifest_pin() {
    let manifest = include_str!("../../Cargo.toml");
    assert!(
        manifest.contains(&format!("servo = {{ version = \"={SERVO_VERSION}\"")),
        "SERVO_VERSION is out of sync with Cargo.toml"
    );
}

fn interactive() -> (FakeHandler, Rc<RefCell<Vec<&'static str>>>) {
    let (mut h, calls) = FakeHandler::new();
    h.interactive = true;
    (h, calls)
}

/// RENDER-11・TASK-122: `--interactive` でないホストは dom_snapshot を unsupported で返し、ハンドラを呼ばない。
/// hello に操作モードの上限も載せない（設計 §14.1）。
#[test]
fn dom_snapshot_is_unsupported_without_interactive() {
    let (r, calls) = run(jframe(&json!({"v":1,"id":5,"method":"dom_snapshot"})));
    assert_eq!(
        r.frames[1],
        json!({"v":1,"id":5,"error":{"kind":"unsupported","message":"Method is not supported."}})
    );
    assert!(calls.is_empty());
    let caps = r.frames[0]["data"]["capabilities"].as_array().unwrap();
    assert!(!caps.contains(&json!("dom_snapshot")));
    assert!(
        r.frames[0]["data"]["limits"]
            .get("dom_snapshot_html_bytes")
            .is_none()
    );
}

/// RENDER-10・AISNAP-18・TASK-126: `--interactive` でないホストは act・wait を unsupported で返し、ハンドラを呼ばず、
/// capabilities と limits にも載せない。
#[test]
fn act_and_wait_are_unsupported_without_interactive() {
    let page = json!({"session":"s","doc_seq":1});
    let input = [
        jframe(&json!({"v":1,"id":5,"method":"act","params":{"page":page,"snapshot":"s1","node":0,"expect":{"tag":"button"},"action":{"kind":"click"}}})),
        jframe(&json!({"v":1,"id":6,"method":"wait","params":{"page":page,"condition":{"kind":"load_complete"},"timeout_ms":100}})),
    ]
    .concat();
    let (r, calls) = run(input);
    for (i, id) in [(1, 5), (2, 6)] {
        assert_eq!(
            r.frames[i],
            json!({"v":1,"id":id,"error":{"kind":"unsupported","message":"Method is not supported."}})
        );
    }
    assert!(calls.is_empty());
    let caps = r.frames[0]["data"]["capabilities"].as_array().unwrap();
    assert!(!caps.contains(&json!("act")) && !caps.contains(&json!("wait")));
    assert!(
        r.frames[0]["data"]["limits"]
            .get("act_timeout_ms")
            .is_none()
    );
}

/// RENDER-10・TASK-126: `fill` の `text`・`wait` の条件・`select` の値の上限はバイトで数える（本体と同じ）。多バイトで上限を超える
/// 要求は invalid_params で、ハンドラを呼ばない。
#[test]
fn interactive_byte_limits_count_utf8_bytes() {
    let page = json!({"session":"s","doc_seq":1});
    let (mut h, calls) = interactive();
    let input = [
        jframe(&json!({"v":1,"id":1,"method":"act","params":{"page":page,"snapshot":"s1","node":0,"expect":{"tag":"input"},"action":{"kind":"fill","text":"あ".repeat(2731)}}})),
        jframe(&json!({"v":1,"id":2,"method":"act","params":{"page":page,"snapshot":"s1","node":0,"expect":{"tag":"input"},"action":{"kind":"fill","text":"あ".repeat(2730)}}})),
        jframe(&json!({"v":1,"id":3,"method":"wait","params":{"page":page,"condition":{"kind":"text_present","text":"あ".repeat(342)},"timeout_ms":100}})),
        // select の値は 1 件ずつ 512 バイトまで（多バイトの境界をまたぐ 512 は受理、513 は拒否）。
        jframe(&json!({"v":1,"id":4,"method":"act","params":{"page":page,"snapshot":"s1","node":0,"expect":{"tag":"select"},"action":{"kind":"select","values":[format!("ab{}", "あ".repeat(170))]}}})),
        jframe(&json!({"v":1,"id":5,"method":"act","params":{"page":page,"snapshot":"s1","node":0,"expect":{"tag":"select"},"action":{"kind":"select","values":["a", format!("abc{}", "あ".repeat(170))]}}})),
    ]
    .concat();
    let r = run_with(&mut h, input);
    assert_eq!(
        r.frames[1]["error"]["kind"], "invalid_params",
        "{}",
        r.frames[1]
    );
    assert!(r.frames[2]["result"].is_object(), "{}", r.frames[2]);
    assert_eq!(
        r.frames[3]["error"]["kind"], "invalid_params",
        "{}",
        r.frames[3]
    );
    assert!(r.frames[4]["result"].is_object(), "{}", r.frames[4]);
    assert_eq!(
        r.frames[5]["error"]["kind"], "invalid_params",
        "{}",
        r.frames[5]
    );
    assert_eq!(calls.borrow().clone(), vec!["act", "act"]);
}

/// RENDER-11・TASK-122: 操作モードのホストは dom_snapshot を載せ、上限を申告し、結果をスキーマの形で返す。
#[test]
fn interactive_host_serves_dom_snapshot() {
    let (mut h, calls) = interactive();
    let input = [
        jframe(&json!({"v":1,"id":1,"method":"load","params":{"url":"https://example.com/"}})),
        jframe(&json!({"v":1,"id":2,"method":"dom_snapshot","params":{}})),
        jframe(&json!({"v":1,"id":3,"method":"dom_snapshot","params":{"page":{"session":"x","doc_seq":1}}})),
    ]
    .concat();
    let r = run_with(&mut h, input);
    let hello = &r.frames[0]["data"];
    assert!(
        hello["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("dom_snapshot"))
    );
    assert_eq!(hello["limits"]["dom_snapshot_html_bytes"], 8_388_608);
    assert_eq!(hello["limits"]["dom_snapshot_frame_bytes"], 12_582_912);
    assert_eq!(hello["limits"]["dom_snapshot_timeout_ms"], 30_000);
    assert_eq!(
        r.frames[1]["result"]["page"],
        json!({"session":"0123456789abcdef0123456789abcdef","doc_seq":3})
    );
    assert_eq!(
        r.frames[2]["result"],
        json!({
            "page": {"session":"0123456789abcdef0123456789abcdef","doc_seq":3},
            "url": "https://example.com/",
            "html": "<html data-fandhe-n=\"0\"></html>",
            "element_count": 1,
            "snapshot": "s1",
            "excluded": {"iframes": 2, "shadow_roots": 0},
            "redacted": 1
        })
    );
    // params を持たない（page を照合しない。設計 §14.2）。
    assert_eq!(r.frames[3]["error"]["kind"], "invalid_params");
    assert_eq!(calls.borrow().clone(), vec!["load", "dom_snapshot"]);
}

/// RENDER-11: 符号化した後のフレーム長が 12 MiB を超える dom_snapshot は、切り詰めずに too_large。
/// 次の要求は続けて処理する。
#[test]
fn dom_snapshot_over_frame_limit_is_too_large() {
    let (mut h, _calls) = interactive();
    // `"` は JSON で 2 バイトに増えるため、HTML の上限（8 MiB）内でもフレームは 12 MiB を超える。
    h.html = "\"".repeat(7 * 1024 * 1024);
    let input = [
        jframe(&json!({"v":1,"id":1,"method":"dom_snapshot"})),
        jframe(&json!({"v":1,"id":2,"method":"visibility","params":{"selector":"a"}})),
    ]
    .concat();
    let r = run_with(&mut h, input);
    assert_eq!(
        r.frames[1],
        json!({"v":1,"id":1,"error":{"kind":"too_large","message":"Result exceeds the size limit."}})
    );
    assert_eq!(r.frames[2]["result"]["visible"], true);
}

/// TASK-121・TASK-122・TASK-126: 操作モードの契約 fixture（`interactive/` の dom_snapshot・act・wait の要求）を
/// ホストへ通す。正常な要求は対応するハンドラを呼び、不正な要求は `expected_response`（invalid_params）を返してハンドラを呼ばない。
#[test]
fn interactive_request_fixtures_match_host_behavior() {
    let dir = fixture_dir().join("interactive");
    let mut seen = std::collections::BTreeMap::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let path = e.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let Some(method) = ["dom-snapshot", "act", "wait"]
            .into_iter()
            .find(|m| name.starts_with(&format!("request-{m}-")))
        else {
            continue;
        };
        let fx: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let (mut h, calls) = interactive();
        let r = run_with(&mut h, jframe(&fx["message"]));
        let call = method.replace('-', "_");
        match fx["expect"].as_str().unwrap() {
            "valid" => {
                assert!(
                    r.frames[1].get("result").is_some(),
                    "{name}: {}",
                    r.frames[1]
                );
                assert_eq!(calls.borrow().clone(), vec![call.as_str()], "{name}");
            }
            _ => {
                // 不正な要求の応答は fixture の `expected_response` と一致させる（kind だけでなく message・id まで）。
                let expected = fx
                    .get("expected_response")
                    .unwrap_or_else(|| panic!("{name}: invalid fixture without expected_response"));
                assert_eq!(r.frames.get(1), Some(expected), "{name}");
                assert!(calls.borrow().is_empty(), "{name}");
            }
        }
        *seen.entry(method).or_insert(0) += 1;
    }
    assert!(seen.get("dom-snapshot") >= Some(&2), "{seen:?}");
    assert!(seen.get("act") >= Some(&10), "{seen:?}");
    assert!(seen.get("wait") >= Some(&6), "{seen:?}");
}

/// TASK-126: 操作モードのホストの hello は、fixture `hello-interactive-ok` と同じ capabilities・limits を申告する。
#[test]
fn interactive_hello_matches_fixture() {
    let (mut h, _calls) = interactive();
    let r = run_with(&mut h, Vec::new());
    let fx = load_fixture("interactive/hello-interactive-ok.json");
    // capabilities は集合として比べる（並びは契約に含めない）。
    let set = |v: &Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        set(&r.frames[0]["data"]["capabilities"]),
        set(&fx["message"]["data"]["capabilities"])
    );
    assert_eq!(
        r.frames[0]["data"]["limits"],
        fx["message"]["data"]["limits"]
    );
}

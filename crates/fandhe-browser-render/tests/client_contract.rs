//! クライアント型・コーデックとゴールデン fixture の契約テスト（RENDER-3・PLUG-1・
//! SEC-2・REPAIR-4。TASK-38・#989）。
//!
//! `tests/protocol_fixtures.rs` が「スキーマと fixture の整合」を保証するのに対し、
//! 本テストは「クライアントの型付き API が fixture を期待どおり読み書きする」ことを保証する。
//! 期待結果は fixture の `expect` から機械的に導出せず、ケースごとに明示表で固定する
//! （クライアントの寛容さ・厳格さはスキーマの valid / invalid と一致しないケースがあるため）。
//! どの分類にも属さない fixture が現れたら失敗させ、新しい fixture の素通りを防ぐ。
//! 外部入力の経路ではないため `expect` / `panic!` を使う。

use std::collections::BTreeSet;
use std::path::PathBuf;

use fandhe_browser_render::codec::{
    MAX_REQUEST_FRAME_BYTES, MAX_RESPONSE_FRAME_BYTES, decode_hello, decode_response,
    effective_limit, encode_request, parse_json, read_frame, read_response, response_frame_limit,
};
use fandhe_browser_render::protocol::{
    BoundingBoxParams, HostErrorKind, ImageFormat, LoadParams, Method, MethodResult, ProtocolError,
    Request, RequestId, ScreenshotParams, VisibilityParams,
};
use serde_json::Value;

const SCHEMA_TEXT: &str = include_str!("../../../docs/design/render-host-protocol.schema.json");

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol")
}

/// fixture の `message` を JSON バイト列で返す（`raw` のものは文字列そのまま）。
fn fixture(name: &str) -> (Option<Method>, Vec<u8>, Value) {
    let text = std::fs::read_to_string(dir().join(format!("{name}.json"))).expect("fixture");
    let env: Value = serde_json::from_str(&text).expect("fixture json");
    let method = match env.get("method").and_then(Value::as_str) {
        Some("load") => Some(Method::Load),
        Some("screenshot") => Some(Method::Screenshot),
        Some("visibility") => Some(Method::Visibility),
        Some("bounding_box") => Some(Method::BoundingBox),
        Some("shutdown") => Some(Method::Shutdown),
        _ => None,
    };
    let message = env.get("message").cloned().unwrap_or(Value::Null);
    let bytes = match env.get("raw").and_then(Value::as_str) {
        Some(raw) => raw.as_bytes().to_vec(),
        None => serde_json::to_vec(&message).expect("serialize"),
    };
    (method, bytes, message)
}

fn all_fixture_names() -> BTreeSet<String> {
    std::fs::read_dir(dir())
        .expect("fixture dir")
        .filter_map(|e| {
            let p = e.expect("entry").path();
            (p.extension()? == "json").then(|| p.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect()
}

#[test]
fn render3_hello_fixtures() {
    let (_, ok, _) = fixture("hello-ok");
    let hello = decode_hello(&ok).expect("hello-ok");
    assert_eq!(hello.servo_version, "0.7.0");
    assert_eq!(hello.host_version, "0.0.0");
    assert_eq!(hello.limits.request_frame_bytes, 65536);
    assert_eq!(hello.limits.screenshot_frame_bytes, 25_165_824);
    assert_eq!(hello.limits.max_in_flight, 1);
    assert_eq!(hello.capabilities.len(), 5);

    let (_, unknown, _) = fixture("hello-unknown-field");
    assert!(decode_hello(&unknown).is_ok());

    let (_, missing, _) = fixture("hello-missing-limit");
    assert!(matches!(
        decode_hello(&missing),
        Err(ProtocolError::InvalidMessage(_))
    ));

    let v2 = br#"{"v":1,"event":"hello","data":{"host_version":"0","servo_version":"0","protocol":{"min":2,"max":2},"capabilities":[],"limits":{"request_frame_bytes":1,"response_frame_bytes":1,"hello_frame_bytes":1,"screenshot_frame_bytes":1,"url_bytes":1,"selector_bytes":1,"error_message_bytes":1}}}"#;
    assert_eq!(
        decode_hello(v2).unwrap_err(),
        ProtocolError::ProtocolMismatch { min: 2, max: 2 }
    );
}

#[test]
fn render3_response_fixtures_with_method() {
    // 成功: 具体値まで確認する。
    let (m, b, _) = fixture("response-load-ok");
    let r = decode_response(&b, m.expect("method")).expect("load-ok");
    let Ok(MethodResult::Load(l)) = r.outcome else {
        panic!("load result expected")
    };
    assert_eq!(
        (l.final_url(), l.status()),
        ("https://example.com/", Some(200))
    );

    for name in [
        "response-load-unknown-field",
        "response-envelope-unknown-field",
    ] {
        let (m, b, _) = fixture(name);
        let r = decode_response(&b, m.unwrap_or(Method::Load)).expect(name);
        assert!(matches!(r.outcome, Ok(MethodResult::Load(_))), "{name}");
    }

    let (m, b, _) = fixture("response-screenshot-ok");
    let Ok(MethodResult::Screenshot(s)) = decode_response(&b, m.expect("m")).expect("s").outcome
    else {
        panic!("screenshot result expected")
    };
    assert_eq!(
        (s.width(), s.height(), s.format()),
        (1280, 800, ImageFormat::Png)
    );
    assert_eq!(s.data_base64(), "iVBORw0KGgo=");

    for (name, visible) in [
        ("response-visibility-visible", true),
        ("response-visibility-hidden", false),
    ] {
        let (m, b, _) = fixture(name);
        let Ok(MethodResult::Visibility(v)) =
            decode_response(&b, m.expect("m")).expect(name).outcome
        else {
            panic!("visibility result expected")
        };
        assert_eq!(v.visible(), visible, "{name}");
        assert_eq!(
            v.reason(),
            if visible { None } else { Some("display_none") },
            "{name}"
        );
    }

    let (m, b, _) = fixture("response-bounding-box-ok");
    let Ok(MethodResult::BoundingBox(bb)) = decode_response(&b, m.expect("m")).expect("bb").outcome
    else {
        panic!("bounding box result expected")
    };
    assert_eq!(
        (bb.x(), bb.y(), bb.width(), bb.height()),
        (8.5, -2.0, 120.25, 18.0)
    );

    let (m, b, _) = fixture("response-shutdown-ok");
    assert!(matches!(
        decode_response(&b, m.expect("m"))
            .expect("shutdown")
            .outcome,
        Ok(MethodResult::Shutdown(_))
    ));

    // status はホストが取得できない場合に省略される（Servo 0.7.0）。None として受理する。
    let (m, b, _) = fixture("response-load-no-status");
    let r = decode_response(&b, m.expect("method")).expect("load-no-status");
    let Ok(MethodResult::Load(l)) = r.outcome else {
        panic!("load result expected")
    };
    assert_eq!((l.final_url(), l.status()), ("https://example.com/", None));

    for name in [
        "response-load-status-out-of-range",
        "response-screenshot-jpeg",
        "response-result-and-error",
    ] {
        let (m, b, _) = fixture(name);
        assert!(
            matches!(
                decode_response(&b, m.unwrap_or(Method::Load)),
                Err(ProtocolError::InvalidMessage(_))
            ),
            "{name}"
        );
    }
}

#[test]
fn render3_response_error_fixtures_any_method() {
    let kinds = [
        ("unsupported", HostErrorKind::Unsupported),
        ("invalid-request", HostErrorKind::InvalidRequest),
        ("invalid-params", HostErrorKind::InvalidParams),
        ("not-loaded", HostErrorKind::NotLoaded),
        ("not-found", HostErrorKind::NotFound),
        ("timeout", HostErrorKind::Timeout),
        ("load-failed", HostErrorKind::LoadFailed),
        ("internal", HostErrorKind::Internal),
    ];
    for (suffix, kind) in kinds {
        let name = format!("response-error-{suffix}");
        let (_, b, _) = fixture(&name);
        for method in [Method::Load, Method::Screenshot, Method::Shutdown] {
            let err = decode_response(&b, method)
                .expect(&name)
                .outcome
                .expect_err("error expected");
            assert_eq!(err.kind(), kind, "{name}");
        }
    }
    let (_, b, _) = fixture("response-error-unknown-field");
    assert!(
        decode_response(&b, Method::Load)
            .expect("e")
            .outcome
            .is_err()
    );

    // スキーマ上は invalid だが、クライアントは未知の kind を汎用エラー（Other）に写像して受理する
    // （スキーマ説明・設計 §2.2）。
    let (_, b, _) = fixture("response-error-unknown-kind");
    let err = decode_response(&b, Method::Load)
        .expect("unknown kind accepted")
        .outcome
        .expect_err("error expected");
    assert_eq!(err.kind(), HostErrorKind::Other);

    for name in [
        "response-error-message-too-long",
        "response-result-null-error",
        "response-error-and-bad-result",
    ] {
        let (_, b, _) = fixture(name);
        assert!(
            matches!(
                decode_response(&b, Method::Load),
                Err(ProtocolError::InvalidMessage(_))
            ),
            "{name}"
        );
    }
}

fn url_of(name: &str) -> String {
    let (_, _, m) = fixture(name);
    m["params"]["url"].as_str().expect("url").to_owned()
}

fn timeout_of(name: &str) -> u32 {
    let (_, _, m) = fixture(name);
    u32::try_from(m["params"]["timeout_ms"].as_u64().expect("timeout")).expect("u32")
}

/// 数値は値として比較する（`1` と `1.0` を同一視）ため、再パース後の Value 同士で比べる。
fn assert_encodes_to(name: &str, request: &Request) {
    let (_, _, expected) = fixture(name);
    let id = RequestId::new(expected["id"].as_u64().expect("id")).expect("id range");
    let bytes = encode_request(id, request).expect("encode");
    assert!(bytes.len() <= MAX_REQUEST_FRAME_BYTES);
    assert_eq!(parse_json(&bytes).expect("reparse"), expected, "{name}");
}

#[test]
fn render3_request_fixtures_encode_and_reject() {
    assert_encodes_to(
        "request-load-ok",
        &Request::Load(LoadParams::new("https://example.com/", Some(10000)).expect("p")),
    );
    assert_encodes_to(
        "request-load-no-timeout",
        &Request::Load(LoadParams::new("https://example.com/", None).expect("p")),
    );
    assert_encodes_to(
        "request-screenshot-full-page",
        &Request::Screenshot(ScreenshotParams::new(true)),
    );
    assert_encodes_to(
        "request-screenshot-viewport",
        &Request::Screenshot(ScreenshotParams::new(false)),
    );
    assert_encodes_to(
        "request-visibility-ok",
        &Request::Visibility(VisibilityParams::new("#main").expect("p")),
    );
    assert_encodes_to(
        "request-bounding-box-ok",
        &Request::BoundingBox(BoundingBoxParams::new("a.link").expect("p")),
    );
    assert_encodes_to("request-shutdown-ok", &Request::Shutdown);

    for name in [
        "request-load-file-scheme",
        "request-load-data-scheme",
        "request-load-javascript-scheme",
        "request-load-userinfo",
        "request-load-url-too-long",
    ] {
        assert!(LoadParams::new(&url_of(name), None).is_err(), "{name}");
    }
    for name in ["request-load-timeout-over", "request-load-timeout-zero"] {
        assert!(
            LoadParams::new("https://example.com/", Some(timeout_of(name))).is_err(),
            "{name}"
        );
    }
    let (_, _, m) = fixture("request-visibility-selector-too-long");
    let selector = m["params"]["selector"].as_str().expect("selector");
    assert!(VisibilityParams::new(selector).is_err());
    assert!(BoundingBoxParams::new(selector).is_err());
}

/// 型付き API では組み立てられない（型が存在しない・コンストラクタが無い）ため、ホスト側の
/// 検証範囲として本テストの対象外にする request fixture。理由は各グループのコメント参照。
const HOST_SIDE_ONLY: &[&str] = &[
    // 型で表現できない（数値表記・ID・版・封筒の崩れ）。クライアントは常に正規形を出す。
    "request-id-float-notation",
    "request-id-negative",
    "request-id-string",
    "request-version-exponent-notation",
    "request-version-float-notation",
    "request-v2",
    "request-missing-v",
    "request-method-empty",
    "request-unknown-method",
    "request-unknown-top-level",
    "request-truncated-json",
    // params の欠落・型違い・未知キー。型付きコンストラクタが必須項目を要求し、
    // 未知キー（user_agent・viewport 等 SEC-2）のフィールドは存在しない。
    "request-load-missing-url",
    "request-load-no-params",
    "request-load-timeout-float-notation",
    "request-load-timeout-fractional",
    "request-load-unknown-param",
    "request-load-user-agent",
    "request-screenshot-full-page-string",
    "request-screenshot-viewport-param",
    "request-visibility-no-params",
    "request-bounding-box-no-params",
    // クライアントは screenshot の params を常に付けるため no-params は再現できない（ホストは受理）。
    "request-screenshot-no-params",
    "request-shutdown-empty-params",
];

#[test]
fn render3_every_fixture_is_classified() {
    let tested: BTreeSet<&str> = [
        "request-load-ok",
        "request-load-no-timeout",
        "request-screenshot-full-page",
        "request-screenshot-viewport",
        "request-visibility-ok",
        "request-bounding-box-ok",
        "request-shutdown-ok",
        "request-load-file-scheme",
        "request-load-data-scheme",
        "request-load-javascript-scheme",
        "request-load-userinfo",
        "request-load-url-too-long",
        "request-load-timeout-over",
        "request-load-timeout-zero",
        "request-visibility-selector-too-long",
        "hello-ok",
        "hello-unknown-field",
        "hello-missing-limit",
        "response-load-ok",
        "response-load-unknown-field",
        "response-envelope-unknown-field",
        "response-screenshot-ok",
        "response-visibility-visible",
        "response-visibility-hidden",
        "response-bounding-box-ok",
        "response-shutdown-ok",
        "response-load-no-status",
        "response-load-status-out-of-range",
        "response-screenshot-jpeg",
        "response-result-and-error",
        "response-error-unknown-field",
        "response-error-unknown-kind",
        "response-error-message-too-long",
        "response-result-null-error",
        "response-error-and-bad-result",
    ]
    .into_iter()
    .chain(HOST_SIDE_ONLY.iter().copied())
    .collect();
    let error_prefix = "response-error-";
    for name in all_fixture_names() {
        let known = tested.contains(name.as_str())
            || [
                "unsupported",
                "invalid-request",
                "invalid-params",
                "not-loaded",
                "not-found",
                "timeout",
                "load-failed",
                "internal",
            ]
            .iter()
            .any(|k| name == format!("{error_prefix}{k}"));
        assert!(
            known,
            "unclassified fixture: {name} (add it to client_contract.rs)"
        );
    }
}

/// SEC-2: エンコード結果の params キー集合がスキーマの properties と一致し、識別面を
/// 変えるキーが現れない。
#[test]
fn sec2_encoded_params_match_schema_and_have_no_identity_keys() {
    let schema: Value = serde_json::from_str(SCHEMA_TEXT).expect("schema");
    let props = |def: &str| -> BTreeSet<String> {
        schema["$defs"][def]["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect()
    };
    let id = RequestId::new(1).expect("id");
    let cases: [(Request, &str); 4] = [
        (
            Request::Load(LoadParams::new("https://example.com/", Some(1000)).expect("p")),
            "LoadParams",
        ),
        (
            Request::Screenshot(ScreenshotParams::new(true)),
            "ScreenshotParams",
        ),
        (
            Request::Visibility(VisibilityParams::new("a").expect("p")),
            "VisibilityParams",
        ),
        (
            Request::BoundingBox(BoundingBoxParams::new("a").expect("p")),
            "BoundingBoxParams",
        ),
    ];
    let forbidden = [
        "user_agent",
        "headers",
        "cookie",
        "cookies",
        "locale",
        "timezone",
        "proxy",
        "viewport",
        "width",
        "height",
        "tls",
    ];
    for (req, def) in cases {
        let v = req.to_json(id);
        let keys: BTreeSet<String> = v["params"]
            .as_object()
            .expect("params")
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, props(def), "{def}");
        assert!(forbidden.iter().all(|f| !keys.contains(*f)), "{def}");
    }
    assert!(Request::Shutdown.to_json(id).get("params").is_none());
}

#[test]
fn render3_frame_limits_per_method() {
    let body = {
        let (_, _, _) = fixture("response-screenshot-ok");
        let pad = "A".repeat(2 * 1024 * 1024);
        format!(
            r#"{{"v":1,"id":3,"result":{{"format":"png","width":1,"height":1,"data_base64":"{pad}"}}}}"#
        )
    };
    let mut wire = (body.len() as u32).to_le_bytes().to_vec();
    wire.extend_from_slice(body.as_bytes());

    let r = read_response(
        &mut wire.as_slice(),
        Method::Screenshot,
        response_frame_limit(Method::Screenshot),
    )
    .expect("screenshot frame within 24 MiB");
    assert!(matches!(r.outcome, Ok(MethodResult::Screenshot(_))));

    assert!(matches!(
        read_frame(&mut wire.as_slice(), response_frame_limit(Method::Load)),
        Err(ProtocolError::FrameTooLarge {
            max: MAX_RESPONSE_FRAME_BYTES,
            ..
        })
    ));
    // ホスト申告値は固定上限を引き上げない。
    assert_eq!(
        effective_limit(MAX_RESPONSE_FRAME_BYTES, Some(u64::MAX)),
        MAX_RESPONSE_FRAME_BYTES
    );
}

// ---- 操作モード（TASK-123・RENDER-10・RENDER-11。fixture は `interactive/`） ----

fn interactive(name: &str) -> Value {
    let p = dir().join("interactive").join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(p).expect("fixture")).expect("fixture json")
}

fn interactive_method(env: &Value) -> Method {
    match env.get("method").and_then(Value::as_str) {
        Some("dom_snapshot") => Method::DomSnapshot,
        Some("act") => Method::Act,
        Some("wait") => Method::Wait,
        Some("load") => Method::Load,
        // error 応答の fixture は method を持たない。kind の解釈はメソッドによらない。
        _ => Method::DomSnapshot,
    }
}

/// TASK-123: 型付きの要求が、正常な要求の fixture と同じ JSON になる（JSON を連結で組まない）。
#[test]
fn render10_interactive_requests_match_fixtures() {
    use fandhe_browser_render::protocol::{
        ActExpect, ActParams, Action, PageInfo, WaitCondition, WaitParams,
    };
    let page = || PageInfo::new("3f9c2a7d1b4e4c0a9d8e7f6a5b4c3d2e", 2).unwrap();
    let act = |node, expect, action| {
        Request::Act(ActParams::new(page(), "s-7", node, expect, action).unwrap())
    };
    let cases: Vec<(&str, Request)> = vec![
        ("request-dom-snapshot-ok", Request::DomSnapshot),
        (
            "request-act-click-ok",
            act(
                12,
                ActExpect::new("button").unwrap().with_id("submit").unwrap(),
                Action::Click,
            ),
        ),
        (
            "request-act-fill-ok",
            act(
                7,
                ActExpect::new("input")
                    .unwrap()
                    .with_name("q")
                    .unwrap()
                    .with_type("text")
                    .unwrap(),
                Action::fill("example query", false).unwrap(),
            ),
        ),
        (
            "request-act-select-ok",
            act(
                9,
                ActExpect::new("select")
                    .unwrap()
                    .with_name("color")
                    .unwrap(),
                Action::select(&["blue"]).unwrap(),
            ),
        ),
        (
            "request-act-press-ok",
            act(
                7,
                ActExpect::new("input").unwrap(),
                Action::press("Enter").unwrap(),
            ),
        ),
        (
            "request-wait-selector-ok",
            Request::Wait(
                WaitParams::new(
                    page(),
                    WaitCondition::SelectorPresent("#result".into()),
                    5000,
                )
                .unwrap(),
            ),
        ),
        (
            "request-wait-text-ok",
            Request::Wait(
                WaitParams::new(page(), WaitCondition::TextPresent("Done".into()), 5000).unwrap(),
            ),
        ),
        (
            "request-wait-load-complete-ok",
            Request::Wait(WaitParams::new(page(), WaitCondition::LoadComplete, 60000).unwrap()),
        ),
    ];
    for (name, req) in cases {
        let env = interactive(name);
        let id = RequestId::new(env["message"]["id"].as_u64().unwrap()).unwrap();
        assert_eq!(req.to_json(id), env["message"], "{name}");
        assert!(encode_request(id, &req).unwrap().len() <= MAX_REQUEST_FRAME_BYTES);
    }
}

/// TASK-123: 不正な要求の fixture に当たる値は、型付きの API で組み立てられない（送信前に拒否）。
#[test]
fn render10_interactive_invalid_requests_cannot_be_built() {
    use fandhe_browser_render::protocol::{
        ActExpect, Action, MAX_FILL_TEXT_BYTES, MAX_SELECT_VALUES, MAX_WAIT_CONDITION_BYTES,
        PageInfo, WaitCondition, WaitParams,
    };
    let page = PageInfo::new("s", 1).unwrap();
    assert!(Action::fill(&"a".repeat(MAX_FILL_TEXT_BYTES + 1), false).is_err());
    assert!(Action::select(&vec!["v"; MAX_SELECT_VALUES + 1]).is_err());
    assert!(ActExpect::new("").is_err());
    assert!(WaitParams::new(page.clone(), WaitCondition::LoadComplete, 60001).is_err());
    assert!(
        WaitParams::new(
            page,
            WaitCondition::SelectorAbsent("a".repeat(MAX_WAIT_CONDITION_BYTES + 1)),
            1000
        )
        .is_err()
    );
}

/// TASK-123: `interactive/` の応答・hello の fixture をクライアントが期待どおりに読む（valid は受理、
/// invalid は拒否。新しい kind は対応する `HostErrorKind` に写る）。
#[test]
fn render11_interactive_responses_match_fixtures() {
    let d = dir().join("interactive");
    let mut seen = 0;
    for e in std::fs::read_dir(&d).expect("dir") {
        let path = e.expect("entry").path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let env: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let bytes = serde_json::to_vec(&env["message"]).unwrap();
        let valid = env["expect"] == "valid";
        match env["direction"].as_str() {
            Some("response") => {
                let r = decode_response(&bytes, interactive_method(&env));
                assert_eq!(r.is_ok(), valid, "{name}: {r:?}");
                seen += 1;
            }
            Some("hello") => {
                let h = decode_hello(&bytes).expect(&name);
                assert_eq!(
                    h.limits.dom_snapshot_frame_bytes,
                    Some(12_582_912),
                    "{name}"
                );
                assert!(h.capabilities.iter().any(|c| c == "dom_snapshot"));
                seen += 1;
            }
            _ => {}
        }
    }
    assert!(seen >= 12, "too few interactive response fixtures: {seen}");
    for (name, kind) in [
        ("response-error-invalid-state", HostErrorKind::InvalidState),
        ("response-error-stale", HostErrorKind::Stale),
        (
            "response-error-not-actionable",
            HostErrorKind::NotActionable,
        ),
        ("response-error-too-large", HostErrorKind::TooLarge),
    ] {
        let env = interactive(name);
        let r =
            decode_response(&serde_json::to_vec(&env["message"]).unwrap(), Method::Act).unwrap();
        assert_eq!(r.outcome.unwrap_err().kind(), kind, "{name}");
    }
    let env = interactive("response-load-interactive-ok");
    let r = decode_response(&serde_json::to_vec(&env["message"]).unwrap(), Method::Load).unwrap();
    match r.outcome {
        Ok(MethodResult::Load(l)) => assert_eq!(l.page().map(|p| p.doc_seq()), Some(0)),
        other => panic!("{other:?}"),
    }
    // dom_snapshot の応答フレームの上限は 12 MiB（他は 1 MiB）。
    assert_eq!(response_frame_limit(Method::DomSnapshot), 12 * 1024 * 1024);
    assert_eq!(response_frame_limit(Method::Act), MAX_RESPONSE_FRAME_BYTES);
}

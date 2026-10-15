//! レンダリングホストのプロトコル契約テスト（TASK-38・`PLUG-1`・`SEC-2`・`REPAIR-4`）。
//!
//! 本体（render crate）と Servo ホストは型を共有しないため、契約の正本は
//! `docs/design/render-host-protocol.schema.json` と `tests/fixtures/protocol/` の
//! ゴールデン fixture である。本テストはスキーマと fixture が食い違っていないことを、
//! 依存を足さずにテスト内の最小 JSON Schema 検証器で機械的に保証する。
//! プロトコル型・コーデックの実装は #989（クライアント）・#993（ホスト）の範囲で、
//! 本テストは外部入力の経路ではないため `expect` / `panic!` を使う。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

const SCHEMA_TEXT: &str = include_str!("../../../docs/design/render-host-protocol.schema.json");
const METHODS: [&str; 5] = [
    "load",
    "screenshot",
    "visibility",
    "bounding_box",
    "shutdown",
];
const ERROR_KINDS: [&str; 8] = [
    "unsupported",
    "invalid_request",
    "invalid_params",
    "not_loaded",
    "not_found",
    "timeout",
    "load_failed",
    "internal",
];

/// 検証器が解釈するキーワード（これ以外がスキーマに現れたらテストを失敗させる）。
const SUPPORTED: [&str; 15] = [
    "not",
    "type",
    "const",
    "enum",
    "required",
    "properties",
    "additionalProperties",
    "maxLength",
    "minLength",
    "minimum",
    "maximum",
    "items",
    "maxItems",
    "minItems",
    "$ref",
];
/// 注釈として素通りさせるキーワード。
const ANNOTATIONS: [&str; 7] = [
    "$schema",
    "$id",
    "title",
    "description",
    "$comment",
    "default",
    "examples",
];

fn schema() -> Value {
    serde_json::from_str(SCHEMA_TEXT).expect("schema must be valid JSON")
}

fn def<'a>(root: &'a Value, name: &str) -> &'a Value {
    root.get("$defs")
        .and_then(|d| d.get(name))
        .unwrap_or_else(|| panic!("schema has no $defs/{name}"))
}

fn pascal(snake: &str) -> String {
    snake
        .split('_')
        .map(|p| {
            let mut c = p.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn resolve<'a>(root: &'a Value, r: &str) -> &'a Value {
    let name = r
        .strip_prefix("#/$defs/")
        .unwrap_or_else(|| panic!("unsupported $ref: {r}"));
    def(root, name)
}

/// 最小検証器。違反を `path: 理由` の形で `errs` に積む。
fn validate(root: &Value, sch: &Value, v: &Value, path: &str, errs: &mut Vec<String>) {
    let Some(obj) = sch.as_object() else {
        panic!("schema node at {path} is not an object");
    };
    if let Some(r) = obj.get("$ref").and_then(Value::as_str) {
        validate(root, resolve(root, r), v, path, errs);
    }
    if let Some(c) = obj.get("const")
        && c != v
    {
        errs.push(format!("{path}: const {c} expected"));
    }
    if let Some(Value::Array(e)) = obj.get("enum")
        && !e.contains(v)
    {
        errs.push(format!("{path}: not in enum"));
    }
    if let Some(t) = obj.get("type").and_then(Value::as_str) {
        let ok = match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "boolean" => v.is_boolean(),
            // JSON Schema の integer は小数部ゼロの数値（1.0・1e0）も含む。
            "integer" => {
                v.is_i64()
                    || v.is_u64()
                    || v.as_f64()
                        .is_some_and(|x| x.is_finite() && x.fract() == 0.0)
            }
            "number" => v.is_number(),
            other => panic!("unsupported type {other}"),
        };
        if !ok {
            errs.push(format!("{path}: type {t} expected"));
            return;
        }
    }
    if let Some(s) = v.as_str() {
        let n = s.chars().count() as u64;
        if let Some(m) = obj.get("maxLength").and_then(Value::as_u64)
            && n > m
        {
            errs.push(format!("{path}: maxLength {m} exceeded ({n})"));
        }
        if let Some(m) = obj.get("minLength").and_then(Value::as_u64)
            && n < m
        {
            errs.push(format!("{path}: minLength {m} not met ({n})"));
        }
    }
    if let Some(x) = v.as_f64().filter(|_| v.is_number()) {
        if let Some(m) = obj.get("minimum").and_then(Value::as_f64)
            && x < m
        {
            errs.push(format!("{path}: minimum {m} violated ({x})"));
        }
        if let Some(m) = obj.get("maximum").and_then(Value::as_f64)
            && x > m
        {
            errs.push(format!("{path}: maximum {m} violated ({x})"));
        }
    }
    if let Some(a) = v.as_array() {
        if let Some(m) = obj.get("maxItems").and_then(Value::as_u64)
            && a.len() as u64 > m
        {
            errs.push(format!("{path}: maxItems {m} exceeded"));
        }
        if let Some(m) = obj.get("minItems").and_then(Value::as_u64)
            && (a.len() as u64) < m
        {
            errs.push(format!("{path}: minItems {m} not met"));
        }
        if let Some(items) = obj.get("items") {
            for (i, e) in a.iter().enumerate() {
                validate(root, items, e, &format!("{path}[{i}]"), errs);
            }
        }
    }
    if let Some(o) = v.as_object() {
        if let Some(Value::Array(req)) = obj.get("required") {
            for k in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(k) {
                    errs.push(format!("{path}: missing required {k}"));
                }
            }
        }
        let props = obj.get("properties").and_then(Value::as_object);
        for (k, val) in o {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => validate(root, ps, val, &format!("{path}.{k}"), errs),
                None => match obj.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        errs.push(format!("{path}: additional property {k} not allowed"));
                    }
                    Some(s @ Value::Object(_)) => {
                        validate(root, s, val, &format!("{path}.{k}"), errs);
                    }
                    _ => {}
                },
            }
        }
    }
    if let Some(n) = obj.get("not") {
        let mut e = Vec::new();
        validate(root, n, v, path, &mut e);
        if e.is_empty() {
            errs.push(format!("{path}: matches a forbidden (not) schema"));
        }
    }
    if let Some(Value::Array(alts)) = obj.get("oneOf") {
        let matched = alts
            .iter()
            .filter(|a| {
                let mut e = Vec::new();
                validate(root, a, v, path, &mut e);
                e.is_empty()
            })
            .count();
        if matched != 1 {
            errs.push(format!(
                "{path}: oneOf matched {matched} (exactly 1 expected)"
            ));
        }
    }
}

fn check(root: &Value, def_name: &str, v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    validate(root, def(root, def_name), v, "$", &mut errs);
    errs
}

struct Fixture {
    file: String,
    v: Value,
}

impl Fixture {
    fn s(&self, k: &str) -> Option<&str> {
        self.v.get(k).and_then(Value::as_str)
    }
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("protocol")
}

fn load_fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(fixtures_dir()).expect("fixtures dir must exist") {
        let p = e.expect("dir entry").path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&p).expect("fixture readable");
        let file = p.file_name().expect("name").to_string_lossy().into_owned();
        let v: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{file}: fixture is not valid JSON: {e}"));
        out.push(Fixture { file, v });
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
    out
}

/// fixture のメッセージ（`message` または `raw` をパースした結果）。raw が不正 JSON なら None。
fn message(f: &Fixture) -> Option<Value> {
    match (f.v.get("message"), f.s("raw")) {
        (Some(m), None) => Some(m.clone()),
        (None, Some(r)) => serde_json::from_str(r).ok(),
        _ => panic!("{}: exactly one of message / raw is required", f.file),
    }
}

/// fixture 1 件をスキーマで検証して違反一覧を返す。
fn violations(root: &Value, f: &Fixture) -> Vec<String> {
    let Some(m) = message(f) else {
        return vec!["$: not parseable as JSON".to_string()];
    };
    let dir = f
        .s("direction")
        .unwrap_or_else(|| panic!("{}: direction", f.file));
    let method = f.s("method");
    match dir {
        "hello" => check(root, "HelloEvent", &m),
        "request" => {
            let mut errs = check(root, "Request", &m);
            let mm = m.get("method").and_then(Value::as_str);
            if let Some(name) = method {
                assert_eq!(mm, Some(name), "{}: method mismatch", f.file);
            }
            if let Some(name) = mm.filter(|n| METHODS.contains(n)) {
                // params 省略は空オブジェクトとして照合する（必須項目は省略でも強制する）。
                let empty = Value::Object(serde_json::Map::new());
                let p = m.get("params").unwrap_or(&empty);
                let d = format!("{}Params", pascal(name));
                errs.extend(check(root, &d, p));
            }
            errs
        }
        "response" => {
            let mut errs = check(root, "Response", &m);
            if let (Some(name), Some(r)) = (method, m.get("result")) {
                errs.extend(check(root, &format!("{}Result", pascal(name)), r));
            }
            errs
        }
        other => panic!("{}: unknown direction {other}", f.file),
    }
}

#[test]
fn plug1_schema_uses_only_supported_keywords() {
    fn walk(root: &Value, node: &Value, path: &str) {
        let obj = node
            .as_object()
            .unwrap_or_else(|| panic!("{path}: schema node must be an object"));
        for (k, v) in obj {
            let p = format!("{path}/{k}");
            match k.as_str() {
                "$defs" | "properties" => {
                    for (n, s) in v.as_object().unwrap_or_else(|| panic!("{p}: map")) {
                        walk(root, s, &format!("{p}/{n}"));
                    }
                }
                "items" => walk(root, v, &p),
                "additionalProperties" => {
                    if v.is_object() {
                        walk(root, v, &p);
                    }
                }
                "not" => walk(root, v, &p),
                "oneOf" => {
                    for (i, s) in v.as_array().expect("oneOf array").iter().enumerate() {
                        walk(root, s, &format!("{p}/{i}"));
                    }
                }
                "$ref" => {
                    let r = v.as_str().expect("$ref string");
                    resolve(root, r);
                }
                k if SUPPORTED.contains(&k) || ANNOTATIONS.contains(&k) => {}
                other => panic!("{p}: unsupported keyword {other} (validator must be extended)"),
            }
        }
    }
    let root = schema();
    walk(&root, &root, "#");
}

#[test]
fn plug1_fixtures_match_schema() {
    let root = schema();
    let fixtures = load_fixtures();
    assert!(!fixtures.is_empty(), "no fixtures found");
    for f in &fixtures {
        let errs = violations(&root, f);
        match f
            .s("expect")
            .unwrap_or_else(|| panic!("{}: expect", f.file))
        {
            "valid" => assert!(
                errs.is_empty(),
                "{}: expected valid but got {errs:?}",
                f.file
            ),
            "invalid" => assert!(!errs.is_empty(), "{}: expected invalid but valid", f.file),
            other => panic!("{}: bad expect {other}", f.file),
        }
        if f.s("expected_host_action").is_some() {
            assert_eq!(
                f.s("expected_host_action"),
                Some("close_connection"),
                "{}: unknown expected_host_action",
                f.file
            );
            assert!(
                f.v.get("expected_response").is_none(),
                "{}: expected_host_action excludes expected_response",
                f.file
            );
        }
        if let Some(er) = f.v.get("expected_response") {
            let e = check(&root, "Response", er);
            assert!(e.is_empty(), "{}: expected_response invalid: {e:?}", f.file);
            assert!(
                er.get("error").is_some(),
                "{}: expected_response must be an error",
                f.file
            );
        }
    }
    eprintln!("checked {} fixtures", fixtures.len());
}

#[test]
fn plug1_fixture_coverage() {
    let fixtures = load_fixtures();
    assert!(fixtures.len() >= 40, "too few fixtures: {}", fixtures.len());
    let find = |dir: &str, method: &str, expect: &str| {
        fixtures.iter().any(|f| {
            f.s("direction") == Some(dir)
                && f.s("expect") == Some(expect)
                && f.s("method") == Some(method)
        })
    };
    for m in METHODS {
        assert!(find("request", m, "valid"), "no valid request for {m}");
        assert!(find("response", m, "valid"), "no valid result for {m}");
    }
    let kinds: BTreeSet<&str> = fixtures
        .iter()
        .filter_map(|f| f.v.get("message")?.get("error")?.get("kind")?.as_str())
        .collect();
    for k in ERROR_KINDS {
        assert!(kinds.contains(k), "error kind {k} is not covered");
    }
    let by = |name: &str| {
        fixtures
            .iter()
            .find(|f| f.file == name)
            .unwrap_or_else(|| panic!("missing fixture {name}"))
    };
    let unk = by("request-unknown-method.json");
    assert_eq!(unk.v["expected_response"]["error"]["kind"], "unsupported");
    assert_eq!(
        by("response-load-unknown-field.json").s("expect"),
        Some("valid")
    );
    assert_eq!(
        by("request-load-unknown-param.json").s("expect"),
        Some("invalid")
    );
    // 有効な id を導出できない要求には、無関係な id の応答を返さず接続を閉じる。
    for n in [
        "request-id-negative.json",
        "request-id-string.json",
        "request-truncated-json.json",
    ] {
        assert_eq!(
            by(n).s("expected_host_action"),
            Some("close_connection"),
            "{n}"
        );
    }
    // id が有効な不正 load には同じ id で invalid_params を返す。
    let frac = by("request-load-timeout-fractional.json");
    assert_eq!(frac.v["expected_response"]["id"], 1);
    assert_eq!(
        frac.v["expected_response"]["error"]["kind"],
        "invalid_params"
    );
    for n in [
        "request-load-no-params.json",
        "request-visibility-no-params.json",
        "request-bounding-box-no-params.json",
    ] {
        assert_eq!(by(n).s("expect"), Some("invalid"), "{n}");
    }
    assert_eq!(by("request-shutdown-ok.json").s("expect"), Some("valid"));
    assert_eq!(
        by("request-screenshot-no-params.json").s("expect"),
        Some("valid")
    );
    assert_eq!(
        by("response-result-null-error.json").s("expect"),
        Some("invalid")
    );
    assert_eq!(
        by("response-error-and-bad-result.json").s("expect"),
        Some("invalid")
    );
    // 数値表記の揺れ: 小数部ゼロは整数として有効、小数部ありは無効。
    for (n, e) in [
        ("request-id-float-notation.json", "valid"),
        ("request-load-timeout-float-notation.json", "valid"),
        ("request-load-timeout-fractional.json", "invalid"),
    ] {
        assert_eq!(by(n).s("expect"), Some(e), "{n}");
    }
    assert!(
        fixtures.iter().any(|f| f.s("raw").is_some()),
        "no raw fixture"
    );
}

#[test]
fn sec2_params_have_no_identity_surface() {
    const FORBIDDEN: [&str; 16] = [
        "user_agent",
        "useragent",
        "ua",
        "header",
        "cookie",
        "locale",
        "lang",
        "timezone",
        "tz",
        "tls",
        "proxy",
        "viewport",
        "screen",
        "width",
        "height",
        "device_scale",
    ];
    let root = schema();
    let defs = root["$defs"].as_object().expect("$defs");
    let mut seen = 0;
    for (name, d) in defs.iter().filter(|(n, _)| n.ends_with("Params")) {
        seen += 1;
        assert_eq!(
            d["additionalProperties"],
            Value::Bool(false),
            "{name} must be closed"
        );
        if let Some(props) = d.get("properties").and_then(Value::as_object) {
            for p in props.keys() {
                let l = p.to_lowercase();
                for bad in FORBIDDEN {
                    assert!(
                        !l.contains(bad),
                        "{name}.{p} looks like identity surface ({bad})"
                    );
                }
            }
        }
    }
    assert_eq!(seen, METHODS.len(), "unexpected number of *Params defs");
}

#[test]
fn repair4_result_defs_allow_unknown_fields() {
    let root = schema();
    let mut names: Vec<String> = METHODS
        .iter()
        .map(|m| format!("{}Result", pascal(m)))
        .collect();
    names.extend(
        [
            "Error",
            "Hello",
            "HelloLimits",
            "HelloEvent",
            "ResultResponse",
            "ErrorResponse",
        ]
        .map(String::from),
    );
    for n in names {
        let ap = def(&root, &n).get("additionalProperties");
        assert!(
            matches!(ap, None | Some(Value::Bool(true))),
            "{n} must allow unknown fields, got {ap:?}"
        );
    }
}

#[test]
fn plug1_limits_defaults_are_consistent() {
    let root = schema();
    let lim = &def(&root, "HelloLimits")["properties"];
    let expect: [(&str, u64); 14] = [
        ("request_frame_bytes", 65536),
        ("response_frame_bytes", 1_048_576),
        ("hello_frame_bytes", 16384),
        ("screenshot_frame_bytes", 25_165_824),
        ("url_bytes", 2048),
        ("selector_bytes", 1024),
        ("error_message_bytes", 512),
        ("max_in_flight", 1),
        ("handshake_timeout_ms", 10000),
        ("load_timeout_default_ms", 30000),
        ("load_timeout_max_ms", 60000),
        ("screenshot_timeout_ms", 20000),
        ("query_timeout_ms", 5000),
        ("shutdown_grace_ms", 3000),
    ];
    for (k, v) in expect {
        assert_eq!(lim[k]["default"].as_u64(), Some(v), "limit {k}");
    }
    let d = |name: &str, p: &str, k: &str| def(&root, name)["properties"][p][k].as_u64();
    assert_eq!(d("LoadParams", "url", "maxLength"), Some(2048));
    assert_eq!(d("VisibilityParams", "selector", "maxLength"), Some(1024));
    assert_eq!(d("BoundingBoxParams", "selector", "maxLength"), Some(1024));
    assert_eq!(d("Error", "message", "maxLength"), Some(512));
    assert_eq!(d("LoadParams", "timeout_ms", "maximum"), Some(60000));
    assert_eq!(d("LoadParams", "timeout_ms", "default"), Some(30000));
    assert_eq!(
        d("ScreenshotResult", "data_base64", "maxLength"),
        Some(25_165_824)
    );
}

#[test]
fn plug1_fixture_messages_fit_frame_limits() {
    for f in load_fixtures() {
        let Some(m) = message(&f) else { continue };
        let limit = match f.s("direction") {
            Some("request") => 65536,
            Some("response") => 1_048_576,
            Some("hello") => 16384,
            other => panic!("{}: direction {other:?}", f.file),
        };
        let len = serde_json::to_string(&m).expect("serialize").len();
        assert!(len <= limit, "{}: {len} bytes exceeds {limit}", f.file);
    }
}

#[test]
fn plug1_method_defs_are_paired() {
    let root = schema();
    let defs = root["$defs"].as_object().expect("$defs");
    for suffix in ["Params", "Result"] {
        let got: BTreeSet<String> = defs
            .keys()
            .filter(|k| k.ends_with(suffix) && k.as_str() != "ResultResponse")
            .cloned()
            .collect();
        let want: BTreeSet<String> = METHODS
            .iter()
            .map(|m| format!("{}{suffix}", pascal(m)))
            .collect();
        assert_eq!(got, want, "*{suffix} defs must match the method list");
    }
}

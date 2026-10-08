//! `profiles/chrome.json`・`profiles/safari.json` のデータ配置の回帰テスト。
//!
//! TASK-100.1（Issue #265）・`PLUG-8`。後続の読み込みモジュール（TASK-100.2）が
//! `include_str!` で埋め込む相対パスが解決できることと、再マッピング結果
//! （TASK-100.8・Issue #738: `width` 等の基本プロパティが本体 feature へ割り当てられ、
//! サブ機能 feature・対応開始版の誤適用で除去されない）が維持されることを固定する。
//! JSON 構文・必須フィールド・`cssProperties` の参照先 feature の存在も、依存を増やさず
//! テスト内の最小パーサーで検証する（読み込みモジュール本体は TASK-100.2 の責務）。

const CHROME: &str = include_str!("../../../profiles/chrome.json");
const SAFARI: &str = include_str!("../../../profiles/safari.json");

#[test]
fn plug8_profile_data_declares_browser() {
    assert!(CHROME.contains("\"browser\": \"chrome\""));
    assert!(SAFARI.contains("\"browser\": \"safari\""));
}

#[test]
fn plug8_profile_data_uses_lf_only() {
    assert!(!CHROME.contains('\r'));
    assert!(!SAFARI.contains('\r'));
}

#[test]
fn plug8_profile_data_records_sources_and_attribution() {
    for data in [CHROME, SAFARI] {
        assert!(data.contains("\"web-features\""));
        assert!(data.contains("\"caniuse-lite\""));
        assert!(data.contains("\"Apache-2.0\""));
        assert!(data.contains("\"CC-BY-4.0\""));
    }
}

#[test]
fn plug8_profile_data_keeps_gating_targets() {
    for data in [CHROME, SAFARI] {
        assert!(data.contains("\"user-select\": \"user-select\""));
        assert!(data.contains("\"position-area\": \"anchor-positioning\""));
    }
}

#[test]
fn plug8_profile_data_maps_basic_properties_to_body_features() {
    for data in [CHROME, SAFARI] {
        assert!(data.contains("\"width\": \"width-height\""));
        assert!(data.contains("\"margin\": \"margin\""));
        assert!(data.contains("\"-webkit-user-select\": \"user-select\""));
        assert!(!data.contains("\"width\": \"anchor-positioning\""));
        assert!(!data.contains("\"margin\": \"anchor-positioning\""));
    }
}

// ---- 構造・参照関係の検証 ----
//
// 依存を増やさないため（dependency-policy）、テスト内に最小の JSON パーサーを持つ。
// プロファイルデータは自前生成の固定書式で、`\u` エスケープ等は扱わない。

use std::collections::BTreeMap;

// 一部のペイロードは構文検証のために保持するだけで読み出さない。
#[allow(dead_code)]
#[derive(Debug)]
enum Json {
    Null,
    Bool(bool),
    Num,
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\n' | b'\t' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let start = self.i;
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b'"' => {
                    let s = std::str::from_utf8(&self.b[start..self.i])
                        .map_err(|e| e.to_string())?
                        .to_string();
                    self.i += 1;
                    return Ok(s);
                }
                b'\\' => {
                    // JSON の許可エスケープのみ受理する（`\u` は 4 桁 16 進を要求）。
                    match self.b.get(self.i + 1) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.i += 2;
                        }
                        Some(b'u') => {
                            let hex = self.b.get(self.i + 2..self.i + 6);
                            if !matches!(hex, Some(h) if h.iter().all(u8::is_ascii_hexdigit)) {
                                return Err(format!("bad \\u escape at byte {}", self.i));
                            }
                            self.i += 6;
                        }
                        _ => return Err(format!("invalid escape at byte {}", self.i)),
                    }
                }
                0x00..=0x1f => return Err(format!("control char in string at byte {}", self.i)),
                _ => self.i += 1,
            }
        }
        Err("unterminated string".into())
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.i))
        }
    }

    fn digits(&mut self) -> Result<(), String> {
        let start = self.i;
        while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            return Err(format!("expected digit at byte {}", self.i));
        }
        Ok(())
    }

    /// JSON の number 文法（`-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`）。
    fn number(&mut self) -> Result<Json, String> {
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        if self.b.get(self.i) == Some(&b'0') {
            self.i += 1;
        } else {
            self.digits()?;
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            self.digits()?;
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            self.digits()?;
        }
        Ok(Json::Num)
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut m = BTreeMap::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.eat(b':')?;
                    let v = self.value()?;
                    if m.insert(k.clone(), v).is_some() {
                        return Err(format!("duplicate key {k}"));
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(m));
                        }
                        _ => return Err(format!("bad object at byte {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return Err(format!("bad array at byte {}", self.i)),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(c) if c.is_ascii_digit() || *c == b'-' => self.number(),
            _ => Err(format!("unexpected token at byte {}", self.i)),
        }
    }
}

fn parse(data: &str) -> Json {
    let mut p = Parser {
        b: data.as_bytes(),
        i: 0,
    };
    let v = p.value().expect("profile data must be valid JSON");
    p.ws();
    assert_eq!(p.i, data.len(), "trailing data after JSON value");
    v
}

fn obj<'a>(v: &'a Json, what: &str) -> &'a BTreeMap<String, Json> {
    match v {
        Json::Obj(m) => m,
        other => panic!("{what} must be an object, got {other:?}"),
    }
}

#[test]
fn plug8_profile_data_is_valid_json_with_required_fields() {
    for (data, browser) in [(CHROME, "chrome"), (SAFARI, "safari")] {
        let root = parse(data);
        let root = obj(&root, "root");
        assert!(matches!(root.get("schemaVersion"), Some(Json::Num)));
        assert!(matches!(root.get("browser"), Some(Json::Str(s)) if s == browser));
        assert!(matches!(root.get("snapshotDate"), Some(Json::Str(s)) if !s.is_empty()));
        assert!(matches!(root.get("sources"), Some(Json::Arr(a)) if !a.is_empty()));
        let features = obj(root.get("features").expect("features"), "features");
        assert!(!features.is_empty());
        for (id, f) in features {
            let f = obj(f, id);
            assert!(
                matches!(f.get("supported"), Some(Json::Bool(_))),
                "{browser}: feature {id} lacks boolean `supported`"
            );
            assert!(
                matches!(f.get("sinceVersion"), Some(Json::Str(_) | Json::Null)),
                "{browser}: feature {id} lacks `sinceVersion`"
            );
        }
    }
}

#[test]
fn plug8_profile_data_css_properties_reference_existing_features() {
    for (data, browser) in [(CHROME, "chrome"), (SAFARI, "safari")] {
        let root = parse(data);
        let root = obj(&root, "root");
        let features = obj(root.get("features").expect("features"), "features");
        let props = obj(
            root.get("cssProperties").expect("cssProperties"),
            "cssProperties",
        );
        assert!(!props.is_empty());
        for (prop, id) in props {
            let Json::Str(id) = id else {
                panic!("{browser}: cssProperties.{prop} must be a string");
            };
            assert!(
                features.contains_key(id),
                "{browser}: cssProperties.{prop} references unknown feature {id}"
            );
        }
    }
}

#[test]
fn plug8_test_parser_rejects_invalid_json_grammar() {
    // 最小パーサー自体の文法検査（無効なエスケープ・number）を固定する。
    for bad in [r#""a\qb""#, r#""\u12G4""#, "01", "1e", "-", "1."] {
        let mut p = Parser {
            b: bad.as_bytes(),
            i: 0,
        };
        let r = p.value();
        assert!(r.is_err() || p.i != bad.len(), "must reject {bad}");
    }
    for ok in [r#""a\/bé""#, "0", "-1.5e+3", "10"] {
        let mut p = Parser {
            b: ok.as_bytes(),
            i: 0,
        };
        assert!(p.value().is_ok() && p.i == ok.len(), "must accept {ok}");
    }
}

#[test]
fn plug8_profile_data_maps_remapped_properties_to_body_features() {
    // TASK-100.8: 除外していた 59 件のうち 57 件が本体 feature へ割り当てられている。
    let expected = [
        ("content", "content"),
        ("align-content", "flexbox"),
        ("text-transform", "text-transform"),
        ("transform-origin", "transforms2d"),
        ("transition", "transitions"),
        ("-webkit-transition", "transitions"),
        ("-moz-transition", "transitions"),
        ("overflow", "overflow-shorthand"),
        ("overflow-x", "overflow-shorthand"),
        ("overflow-y", "overflow-shorthand"),
        ("outline", "outline"),
        ("gap", "grid"),
        ("height", "width-height"),
        ("top", "physical-properties"),
        ("min-width", "min-max-width-height"),
        ("inset", "logical-properties"),
        ("container-type", "container-queries"),
        ("break-inside", "page-breaks"),
        ("text-overflow", "text-overflow"),
        ("counter-reset", "counters"),
        ("-khtml-user-select", "user-select"),
    ];
    for (data, browser) in [(CHROME, "chrome"), (SAFARI, "safari")] {
        let root = parse(data);
        let root = obj(&root, "root");
        let props = obj(
            root.get("cssProperties").expect("cssProperties"),
            "cssProperties",
        );
        for (p, f) in expected {
            assert!(
                matches!(props.get(p), Some(Json::Str(id)) if id == f),
                "{browser}: {p} must map to {f}"
            );
        }
        // サブ機能 feature へは割り当てない。
        for (p, id) in props {
            if let Json::Str(id) = id {
                if p != "position-area" {
                    assert_ne!(id, "anchor-positioning", "{browser}: {p}");
                }
                assert!(
                    !matches!(
                        id.as_str(),
                        "overflow-clip"
                            | "flexbox-gap"
                            | "transition-behavior"
                            | "alt-text-generated-content"
                            | "mathml"
                            | "custom-ellipses"
                            | "counter-reset-reversed"
                            | "column-breaks"
                            | "container-anchor-position-queries"
                    ) || matches!(p.as_str(), "column-gap" | "row-gap" | "-moz-column-gap"),
                    "{browser}: {p} -> {id}"
                );
            }
        }
    }
}

#[test]
fn plug8_profile_data_keeps_text_size_adjust_prefixed_unlisted() {
    // 剥がした先の `text-size-adjust` が Safari 非対応のため、接頭辞付き 2 件は未掲載（素通し）に保つ。
    for (data, browser) in [(CHROME, "chrome"), (SAFARI, "safari")] {
        let root = parse(data);
        let root = obj(&root, "root");
        let props = obj(
            root.get("cssProperties").expect("cssProperties"),
            "cssProperties",
        );
        for p in ["-webkit-text-size-adjust", "-ms-text-size-adjust"] {
            assert!(!props.contains_key(p), "{browser}: {p} must stay unlisted");
        }
    }
}

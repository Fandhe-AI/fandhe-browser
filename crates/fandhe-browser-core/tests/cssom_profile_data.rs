//! `profiles/chrome.json`・`profiles/safari.json` のデータ配置の回帰テスト。
//!
//! TASK-100.1（Issue #265）・`PLUG-8`。後続の読み込みモジュール（TASK-100.2）が
//! `include_str!` で埋め込む相対パスが解決できることと、キュレーション結果
//! （`width` 等の基本プロパティを gating 対象に含めない）が維持されることを固定する。
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
fn plug8_profile_data_excludes_miscurated_properties() {
    for data in [CHROME, SAFARI] {
        assert!(!data.contains("\"width\":"));
        assert!(!data.contains("\"margin\":"));
        assert!(!data.contains("\"-webkit-user-select\":"));
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
                b'\\' => self.i += 2,
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
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit() || b"+-.eE".contains(c))
                {
                    self.i += 1;
                }
                Ok(Json::Num)
            }
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
fn plug8_profile_data_excludes_subfeature_mappings() {
    // サブ機能の対応開始版をプロパティ全体へ適用しない（基本プロパティを誤って gating しない）。
    for (data, browser) in [(CHROME, "chrome"), (SAFARI, "safari")] {
        let root = parse(data);
        let root = obj(&root, "root");
        let props = obj(
            root.get("cssProperties").expect("cssProperties"),
            "cssProperties",
        );
        for p in [
            "content",
            "align-content",
            "text-transform",
            "transform-origin",
            "transition",
        ] {
            assert!(
                !props.contains_key(p),
                "{browser}: {p} must not be mapped to a sub-feature"
            );
        }
    }
}

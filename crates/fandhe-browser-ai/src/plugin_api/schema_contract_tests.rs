//! `docs/design/host-api.schema.json` と [`super`]（マニフェスト型）の契約テスト
//! （`PLUG-2`・TASK-92.5・Issue #357。`runtime` / `language` は `PLUG-6`・TASK-98.1・Issue #389）。
//!
//! スキーマの制約値を Rust 側の定数・列挙型から導いた期待値と突き合わせ、どちらかだけ
//! 変更されて乖離したら落ちるようにする。crate 内に置くのは、`#[non_exhaustive]` な列挙型への
//! 網羅 `match` が crate 内でしか書けず、variant 追加時にコンパイルエラーでスキーマ更新漏れを
//! 検知できるため。エンドポイントの結合テストは TASK-92.6 の範囲でここでは扱わない。
//! `docs/spec` は参照せず、public な `docs/design/` のみをコンパイル時に埋め込む。

use std::collections::BTreeSet;

use serde_json::Value;

use super::{
    DEFAULT_LANGUAGE, DEFAULT_PROTOCOL_VERSION, DEFAULT_RUNTIME, MAX_ID_CHARS, MAX_LANGUAGE_CHARS,
    MAX_PERMISSIONS, MAX_PLUGINS, MAX_RUNTIME_CHARS, MAX_STRING_CHARS, MAX_TOOLS, ManifestError,
    ManifestField, PluginPermission, PluginTransport, RegistryError, is_valid_id,
    is_valid_runtime_token, is_valid_version,
};

const SCHEMA_TEXT: &str = include_str!("../../../../docs/design/host-api.schema.json");

fn schema() -> Value {
    serde_json::from_str(SCHEMA_TEXT).expect("schema must be valid JSON")
}

fn def(name: &str) -> Value {
    schema()["$defs"][name].clone()
}

fn manifest_props() -> Value {
    def("PluginManifest")["properties"].clone()
}

fn str_set(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .expect("array expected")
        .iter()
        .map(|s| s.as_str().expect("string expected").to_owned())
        .collect()
}

/// 網羅 `match` で全 variant を列挙する（variant 追加時にコンパイルエラーにする）。
fn all_fields() -> Vec<ManifestField> {
    let mut v = Vec::new();
    for f in [
        ManifestField::Id,
        ManifestField::Version,
        ManifestField::Transport,
        ManifestField::Tools,
        ManifestField::Permissions,
        ManifestField::ProtocolVersion,
        ManifestField::Runtime,
        ManifestField::Language,
    ] {
        match f {
            ManifestField::Id
            | ManifestField::Version
            | ManifestField::Transport
            | ManifestField::Tools
            | ManifestField::Permissions
            | ManifestField::ProtocolVersion
            | ManifestField::Runtime
            | ManifestField::Language => v.push(f),
        }
    }
    v
}

fn all_transports() -> Vec<PluginTransport> {
    let mut v = Vec::new();
    for t in [
        PluginTransport::Stdio,
        PluginTransport::UnixSocket,
        PluginTransport::Tcp,
    ] {
        match t {
            PluginTransport::Stdio | PluginTransport::UnixSocket | PluginTransport::Tcp => {
                v.push(t)
            }
        }
    }
    v
}

fn all_permissions() -> Vec<PluginPermission> {
    let mut v = Vec::new();
    for p in [
        PluginPermission::NetworkFetch,
        PluginPermission::DomRead,
        PluginPermission::DomWrite,
        PluginPermission::FsRead,
        PluginPermission::FsWrite,
    ] {
        match p {
            PluginPermission::NetworkFetch
            | PluginPermission::DomRead
            | PluginPermission::DomWrite
            | PluginPermission::FsRead
            | PluginPermission::FsWrite => v.push(p),
        }
    }
    v
}

fn all_manifest_codes() -> Vec<&'static str> {
    [
        ManifestError::TooLarge,
        ManifestError::InvalidJson,
        ManifestError::NotAnObject,
        ManifestError::MissingField(ManifestField::Id),
        ManifestError::InvalidType(ManifestField::Id),
        ManifestError::UnknownField,
        ManifestError::InvalidId,
        ManifestError::InvalidVersion,
        ManifestError::UnknownTransport,
        ManifestError::UnknownPermission,
        ManifestError::TooLong(ManifestField::Id),
        ManifestError::TooManyItems(ManifestField::Tools),
        ManifestError::EmptyTools,
        ManifestError::EmptyToolName,
        ManifestError::InvalidRuntime,
        ManifestError::InvalidLanguage,
    ]
    .iter()
    .map(|e| {
        // 網羅 match: variant 追加時にここでコンパイルエラーになる。
        match e {
            ManifestError::TooLarge
            | ManifestError::InvalidJson
            | ManifestError::NotAnObject
            | ManifestError::MissingField(_)
            | ManifestError::InvalidType(_)
            | ManifestError::UnknownField
            | ManifestError::InvalidId
            | ManifestError::InvalidVersion
            | ManifestError::UnknownTransport
            | ManifestError::UnknownPermission
            | ManifestError::TooLong(_)
            | ManifestError::TooManyItems(_)
            | ManifestError::EmptyTools
            | ManifestError::EmptyToolName
            | ManifestError::InvalidRuntime
            | ManifestError::InvalidLanguage => {}
        }
        e.code()
    })
    .collect()
}

#[test]
fn plug2_schema_is_draft_2020_12() {
    assert_eq!(
        schema()["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
}

#[test]
fn plug2_manifest_required_and_property_keys_match_rust() {
    let m = def("PluginManifest");
    assert_eq!(
        str_set(&m["required"]),
        BTreeSet::from(["id", "version", "transport", "tools"].map(String::from))
    );
    assert_eq!(m["additionalProperties"], Value::Bool(false));
    let keys: BTreeSet<String> = m["properties"]
        .as_object()
        .expect("properties object")
        .keys()
        .cloned()
        .collect();
    let expected: BTreeSet<String> = all_fields().iter().map(|f| f.as_str().to_owned()).collect();
    assert_eq!(keys, expected);
}

#[test]
fn plug2_manifest_limits_match_rust_constants() {
    let p = manifest_props();
    assert_eq!(p["id"]["maxLength"], MAX_ID_CHARS);
    assert_eq!(p["version"]["maxLength"], MAX_STRING_CHARS);
    assert_eq!(p["protocolVersion"]["maxLength"], MAX_STRING_CHARS);
    assert_eq!(p["tools"]["minItems"], 1);
    assert_eq!(p["tools"]["maxItems"], MAX_TOOLS);
    assert_eq!(p["tools"]["items"]["minLength"], 1);
    assert_eq!(p["tools"]["items"]["maxLength"], MAX_STRING_CHARS);
    assert_eq!(p["permissions"]["maxItems"], MAX_PERMISSIONS);
    assert_eq!(p["protocolVersion"]["default"], DEFAULT_PROTOCOL_VERSION);
    assert_eq!(p["permissions"]["default"], serde_json::json!([]));
    assert_eq!(p["runtime"]["maxLength"], MAX_RUNTIME_CHARS);
    assert_eq!(p["language"]["maxLength"], MAX_LANGUAGE_CHARS);
    assert_eq!(p["runtime"]["default"], DEFAULT_RUNTIME);
    assert_eq!(p["language"]["default"], DEFAULT_LANGUAGE);
}

#[test]
fn plug2_manifest_patterns_match_rust_regex_docs() {
    let p = manifest_props();
    assert_eq!(p["id"]["pattern"], r"^[a-z0-9][a-z0-9-]*(?![\s\S])");
    assert_eq!(
        p["version"]["pattern"],
        r"^\d+\.\d+\.\d+[^\n\r\u2028\u2029]*(?![\s\S])"
    );
    for key in ["runtime", "language"] {
        assert_eq!(p[key]["pattern"], r"^[a-z0-9][a-z0-9.+#_-]*(?![\s\S])");
    }
}

/// 末尾改行を含む具体値を Rust の検証関数が拒否し、スキーマ側も末尾アンカーが厳密であること
/// （`$` が末尾改行の手前に一致する検証器でも通さない）を確認する。
#[test]
fn plug2_trailing_newline_values_are_rejected_like_schema() {
    for id in ["a\n", "a\r", "a\n\n", "\na"] {
        assert!(!is_valid_id(id), "id {id:?} must be rejected");
    }
    for v in [
        "1.2.3\n",
        "1.2.3\r",
        "1.2.3\u{2028}",
        "1.2.3\u{2029}",
        "1.2.3-a\nb",
    ] {
        assert!(!is_valid_version(v), "version {v:?} must be rejected");
    }
    for id in ["a", "a-1", "0x"] {
        assert!(is_valid_id(id), "id {id:?} must be accepted");
    }
    for v in ["1.2.3", "1.2.3-beta.1", "10.20.30+meta"] {
        assert!(is_valid_version(v), "version {v:?} must be accepted");
    }
    let p = manifest_props();
    for t in ["rust\n", "rust\r", "\nrust", "node\n20", "Rust", ""] {
        assert!(!is_valid_runtime_token(t), "token {t:?} must be rejected");
    }
    for t in ["rust", "c++", "c#", "python3.12", "node-20"] {
        assert!(is_valid_runtime_token(t), "token {t:?} must be accepted");
    }
    for key in ["id", "version", "runtime", "language"] {
        let pat = p[key]["pattern"].as_str().expect("pattern string");
        assert!(
            pat.ends_with(r"(?![\s\S])"),
            "{key} pattern needs strict end anchor"
        );
        assert!(!pat.ends_with('$'), "{key} pattern must not end with $");
    }
}

#[test]
fn plug2_transport_and_permission_enums_match_rust() {
    let p = manifest_props();
    let transports: BTreeSet<String> = all_transports()
        .iter()
        .map(|t| t.as_str().to_owned())
        .collect();
    assert_eq!(str_set(&p["transport"]["enum"]), transports);
    let perms: BTreeSet<String> = all_permissions()
        .iter()
        .map(|x| x.as_str().to_owned())
        .collect();
    assert_eq!(str_set(&p["permissions"]["items"]["enum"]), perms);
}

#[test]
fn plug2_permissions_allow_duplicates() {
    // Rust 側は重複を拒否しないため、uniqueItems を付けてはならない。
    let u = &manifest_props()["permissions"]["uniqueItems"];
    assert!(u.is_null() || *u == Value::Bool(false));
}

#[test]
fn plug2_registered_manifest_requires_all_keys() {
    let r = def("RegisteredPluginManifest");
    let all = r["allOf"].as_array().expect("allOf array");
    assert_eq!(all[0]["$ref"], "#/$defs/PluginManifest");
    let expected: BTreeSet<String> = all_fields().iter().map(|f| f.as_str().to_owned()).collect();
    assert_eq!(str_set(&all[1]["required"]), expected);
}

#[test]
fn plug2_list_response_limit_matches_registry_capacity() {
    let l = def("PluginListResponse");
    assert_eq!(l["required"], serde_json::json!(["plugins"]));
    assert_eq!(l["properties"]["plugins"]["maxItems"], MAX_PLUGINS);
}

#[test]
fn plug2_register_error_codes_are_documented_in_schema() {
    let s = schema();
    let post = &s["paths"]["POST /ai/plugins/register"]["responses"];
    let documented = str_set(&post["400"]["manifestCodes"]);
    let expected: BTreeSet<String> = all_manifest_codes()
        .iter()
        .map(|c| (*c).to_owned())
        .collect();
    assert_eq!(documented, expected);

    for e in [RegistryError::DuplicateId, RegistryError::Full] {
        let (status, code) = match e {
            RegistryError::DuplicateId => ("409", e.code()),
            RegistryError::Full => ("429", e.code()),
        };
        let desc = post[status]["description"].as_str().expect("description");
        assert!(desc.contains(code), "{status} must document {code}");
    }
}

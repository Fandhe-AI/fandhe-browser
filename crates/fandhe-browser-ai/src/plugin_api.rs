//! プラグインマニフェスト型（`PLUG-2`・TASK-92.1・Issue #353・`MS-9`）。
//!
//! プラグインが自己申告する識別子・バージョン・トランスポート・提供ツール一覧・要求権限を
//! 表す [`PluginManifest`] と、その JSON からの構築・JSON への出力を提供する。
//! 外部入力（プラグイン由来で untrusted）を扱うため、スキーマ制約（PoC-15 の
//! `host-api.schema.json` の `PluginManifest`）を構築時に全て検証し、不正な状態の値を作れない
//! ようにしている。未知キー・未知の transport / permission は拒否する（fail-closed）。
//!
//! 呼び出し文脈: TASK-92.3 の登録ハンドラ（`POST /ai/plugins/register`）が [`PluginManifest::from_slice`]
//! を、TASK-92.4 の一覧（`GET /ai/plugins`）が [`PluginManifest::to_value`] を使う想定。
//!
//! # スタブ・暫定仕様について（REPAIR-3）
//!
//! - レジストリ状態・ルート・HTTP ステータス写像・登録件数上限・id 重複判定は未実装
//!   （TASK-92.2〜92.4）。本モジュールは型と検証のみ
//! - `permissions` は申告値の保持のみで、権限の付与・強制は行わない
//! - `tcp` / `unix-socket` は列挙値として受理するだけで、接続処理は持たない
//! - 新規依存を避けるため serde の derive は使わず、`serde_json::Value` から手動で抽出する

use std::fmt;

use serde_json::{Map, Value, json};

/// `id` の最大文字数（Unicode コードポイント数）。
pub const MAX_ID_CHARS: usize = 64;
/// `version`・`protocolVersion`・ツール名の最大文字数（コードポイント数）。
pub const MAX_STRING_CHARS: usize = 128;
/// `tools` の最大件数。
pub const MAX_TOOLS: usize = 64;
/// `permissions` の最大件数。
pub const MAX_PERMISSIONS: usize = 5;
/// マニフェスト JSON の最大バイト数。パース前に検証し無制限確保を避ける。
/// 正当な最大入力（ツール 64 件 × 128 文字を全て `\uXXXX` 表記）でも約 98 KiB に収まる。
pub const MAX_MANIFEST_BYTES: usize = 128 * 1024;
/// `protocolVersion` 省略時の既定値。
pub const DEFAULT_PROTOCOL_VERSION: &str = "unspecified";

/// マニフェストのフィールド。エラーの対象特定に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestField {
    Id,
    Version,
    Transport,
    Tools,
    Permissions,
    ProtocolVersion,
}

impl ManifestField {
    /// JSON 上のキー名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Version => "version",
            Self::Transport => "transport",
            Self::Tools => "tools",
            Self::Permissions => "permissions",
            Self::ProtocolVersion => "protocolVersion",
        }
    }
}

/// プラグインとホストの通信トランスポート（申告値のみ。接続処理は未実装）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginTransport {
    Stdio,
    UnixSocket,
    Tcp,
}

impl PluginTransport {
    /// JSON 上の文字列表現。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::UnixSocket => "unix-socket",
            Self::Tcp => "tcp",
        }
    }

    /// 文字列から変換する。未知の値は `None`。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "stdio" => Some(Self::Stdio),
            "unix-socket" => Some(Self::UnixSocket),
            "tcp" => Some(Self::Tcp),
            _ => None,
        }
    }
}

/// プラグインが要求する権限（申告値のみ。強制は行わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginPermission {
    NetworkFetch,
    DomRead,
    DomWrite,
    FsRead,
    FsWrite,
}

impl PluginPermission {
    /// JSON 上の文字列表現（ドット区切り）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NetworkFetch => "network.fetch",
            Self::DomRead => "dom.read",
            Self::DomWrite => "dom.write",
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
        }
    }

    /// 文字列から変換する。未知の値は `None`。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "network.fetch" => Some(Self::NetworkFetch),
            "dom.read" => Some(Self::DomRead),
            "dom.write" => Some(Self::DomWrite),
            "fs.read" => Some(Self::FsRead),
            "fs.write" => Some(Self::FsWrite),
            _ => None,
        }
    }
}

/// マニフェスト検証の失敗。`Display` は固定の英語文言で、入力値（未知キー名・不正値）を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestError {
    /// 入力が [`MAX_MANIFEST_BYTES`] を超えた。
    TooLarge,
    /// JSON として不正。
    InvalidJson,
    /// トップレベルがオブジェクトでない。
    NotAnObject,
    /// 必須フィールドがない。
    MissingField(ManifestField),
    /// フィールドの型が不正。
    InvalidType(ManifestField),
    /// 未知のキーがある。
    UnknownField,
    /// `id` の形式違反。
    InvalidId,
    /// `version` の形式違反。
    InvalidVersion,
    /// 未知の transport。
    UnknownTransport,
    /// 未知の permission。
    UnknownPermission,
    /// 文字列が最大文字数を超えた。
    TooLong(ManifestField),
    /// 配列が最大件数を超えた。
    TooManyItems(ManifestField),
    /// `tools` が空配列。
    EmptyTools,
    /// ツール名が空文字列。
    EmptyToolName,
}

impl ManifestError {
    /// 機械可読なエラーコード（TASK-92.3 が応答本文に使える）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLarge => "manifest_too_large",
            Self::InvalidJson => "invalid_json",
            Self::NotAnObject => "not_an_object",
            Self::MissingField(_) => "missing_field",
            Self::InvalidType(_) => "invalid_type",
            Self::UnknownField => "unknown_field",
            Self::InvalidId => "invalid_id",
            Self::InvalidVersion => "invalid_version",
            Self::UnknownTransport => "unknown_transport",
            Self::UnknownPermission => "unknown_permission",
            Self::TooLong(_) => "too_long",
            Self::TooManyItems(_) => "too_many_items",
            Self::EmptyTools => "empty_tools",
            Self::EmptyToolName => "empty_tool_name",
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "manifest is too large",
            Self::InvalidJson => "manifest is not valid JSON",
            Self::NotAnObject => "manifest must be a JSON object",
            Self::MissingField(_) => "manifest is missing a required field",
            Self::InvalidType(_) => "manifest field has an invalid type",
            Self::UnknownField => "manifest has an unknown field",
            Self::InvalidId => "manifest id is invalid",
            Self::InvalidVersion => "manifest version is invalid",
            Self::UnknownTransport => "manifest transport is not supported",
            Self::UnknownPermission => "manifest permission is not supported",
            Self::TooLong(_) => "manifest field is too long",
            Self::TooManyItems(_) => "manifest field has too many items",
            Self::EmptyTools => "manifest tools must not be empty",
            Self::EmptyToolName => "manifest tool name must not be empty",
        })
    }
}

impl std::error::Error for ManifestError {}

/// 検証済みのプラグインマニフェスト。フィールドは非公開で、構築経路は
/// [`from_slice`](Self::from_slice) / [`from_value`](Self::from_value) のみ。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginManifest {
    id: String,
    version: String,
    transport: PluginTransport,
    tools: Vec<String>,
    permissions: Vec<PluginPermission>,
    protocol_version: String,
}

impl PluginManifest {
    /// JSON バイト列から構築する。サイズ上限をパース前に検証する。
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge);
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| ManifestError::InvalidJson)?;
        Self::from_value(&value)
    }

    /// パース済み JSON から構築し、スキーマ制約を全て検証する。
    pub fn from_value(value: &Value) -> Result<Self, ManifestError> {
        let obj = value.as_object().ok_or(ManifestError::NotAnObject)?;
        const KNOWN: [&str; 6] = [
            "id",
            "version",
            "transport",
            "tools",
            "permissions",
            "protocolVersion",
        ];
        if obj.keys().any(|k| !KNOWN.contains(&k.as_str())) {
            return Err(ManifestError::UnknownField);
        }

        let id = required_str(obj, ManifestField::Id)?;
        let version = required_str(obj, ManifestField::Version)?;
        let transport = required_str(obj, ManifestField::Transport)?;
        let tools = required_array(obj, ManifestField::Tools)?;

        if char_len_exceeds(id, MAX_ID_CHARS) {
            return Err(ManifestError::TooLong(ManifestField::Id));
        }
        if !is_valid_id(id) {
            return Err(ManifestError::InvalidId);
        }
        if char_len_exceeds(version, MAX_STRING_CHARS) {
            return Err(ManifestError::TooLong(ManifestField::Version));
        }
        if !is_valid_version(version) {
            return Err(ManifestError::InvalidVersion);
        }
        let transport = PluginTransport::parse(transport).ok_or(ManifestError::UnknownTransport)?;

        if tools.is_empty() {
            return Err(ManifestError::EmptyTools);
        }
        if tools.len() > MAX_TOOLS {
            return Err(ManifestError::TooManyItems(ManifestField::Tools));
        }
        let mut tool_names = Vec::with_capacity(tools.len());
        for t in tools {
            let name = t
                .as_str()
                .ok_or(ManifestError::InvalidType(ManifestField::Tools))?;
            if name.is_empty() {
                return Err(ManifestError::EmptyToolName);
            }
            if char_len_exceeds(name, MAX_STRING_CHARS) {
                return Err(ManifestError::TooLong(ManifestField::Tools));
            }
            tool_names.push(name.to_owned());
        }

        let mut permissions = Vec::new();
        if let Some(p) = obj.get(ManifestField::Permissions.as_str()) {
            let items = p
                .as_array()
                .ok_or(ManifestError::InvalidType(ManifestField::Permissions))?;
            if items.len() > MAX_PERMISSIONS {
                return Err(ManifestError::TooManyItems(ManifestField::Permissions));
            }
            for item in items {
                let s = item
                    .as_str()
                    .ok_or(ManifestError::InvalidType(ManifestField::Permissions))?;
                permissions
                    .push(PluginPermission::parse(s).ok_or(ManifestError::UnknownPermission)?);
            }
        }

        let protocol_version = match obj.get(ManifestField::ProtocolVersion.as_str()) {
            None => DEFAULT_PROTOCOL_VERSION.to_owned(),
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or(ManifestError::InvalidType(ManifestField::ProtocolVersion))?;
                if char_len_exceeds(s, MAX_STRING_CHARS) {
                    return Err(ManifestError::TooLong(ManifestField::ProtocolVersion));
                }
                s.to_owned()
            }
        };

        Ok(Self {
            id: id.to_owned(),
            version: version.to_owned(),
            transport,
            tools: tool_names,
            permissions,
            protocol_version,
        })
    }

    /// プラグイン識別子。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// プラグインのバージョン。
    pub fn version(&self) -> &str {
        &self.version
    }

    /// 申告されたトランスポート。
    pub fn transport(&self) -> PluginTransport {
        self.transport
    }

    /// 提供ツール名の一覧。
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// 申告された要求権限（強制はしない）。
    pub fn permissions(&self) -> &[PluginPermission] {
        &self.permissions
    }

    /// プロトコルバージョン。未指定時は [`DEFAULT_PROTOCOL_VERSION`]。
    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    /// 6 キー全てを出力する JSON 表現（既定値のキーも省略しない）。
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "version": self.version,
            "transport": self.transport.as_str(),
            "tools": self.tools,
            "permissions": self.permissions.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "protocolVersion": self.protocol_version,
        })
    }
}

fn required_str(obj: &Map<String, Value>, field: ManifestField) -> Result<&str, ManifestError> {
    obj.get(field.as_str())
        .ok_or(ManifestError::MissingField(field))?
        .as_str()
        .ok_or(ManifestError::InvalidType(field))
}

fn required_array(
    obj: &Map<String, Value>,
    field: ManifestField,
) -> Result<&Vec<Value>, ManifestError> {
    obj.get(field.as_str())
        .ok_or(ManifestError::MissingField(field))?
        .as_array()
        .ok_or(ManifestError::InvalidType(field))
}

/// コードポイント数が `max` を超えるか。全走査せず `max + 1` 文字目で打ち切る。
fn char_len_exceeds(s: &str, max: usize) -> bool {
    s.chars().nth(max).is_some()
}

/// `^[a-z0-9][a-z0-9-]*$`。
fn is_valid_id(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `^\d+\.\d+\.\d+.*$`（`.` は改行類に一致しないため残りに改行類を許さない）。
fn is_valid_version(s: &str) -> bool {
    let mut rest = s;
    for i in 0..3 {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return false;
        }
        // ASCII 数字は 1 バイトなので文字数 = バイト数
        rest = rest.get(digits..).unwrap_or("");
        if i < 2 {
            match rest.strip_prefix('.') {
                Some(r) => rest = r,
                None => return false,
            }
        }
    }
    !rest
        .chars()
        .any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Value {
        json!({
            "id": "ref-plugin-1",
            "version": "1.2.3-beta",
            "transport": "unix-socket",
            "tools": ["a.read", "b.write"],
            "permissions": ["network.fetch", "dom.read", "dom.write", "fs.read", "fs.write"],
            "protocolVersion": "2025-06-18"
        })
    }

    fn minimal() -> Value {
        json!({"id": "p", "version": "0.1.0", "transport": "stdio", "tools": ["t"]})
    }

    fn err(v: Value) -> ManifestError {
        PluginManifest::from_value(&v).unwrap_err()
    }

    #[test]
    fn plug2_full_manifest_accessors_and_json() {
        let m = PluginManifest::from_value(&full()).unwrap();
        assert_eq!(m.id(), "ref-plugin-1");
        assert_eq!(m.version(), "1.2.3-beta");
        assert_eq!(m.transport(), PluginTransport::UnixSocket);
        assert_eq!(m.tools(), ["a.read".to_owned(), "b.write".to_owned()]);
        assert_eq!(m.permissions().len(), 5);
        assert_eq!(m.protocol_version(), "2025-06-18");
        assert_eq!(m.to_value(), full());
        assert_eq!(PluginManifest::from_value(&m.to_value()).unwrap(), m);
    }

    #[test]
    fn plug2_defaults_for_optional_fields() {
        let m = PluginManifest::from_value(&minimal()).unwrap();
        assert!(m.permissions().is_empty());
        assert_eq!(m.protocol_version(), "unspecified");
        assert_eq!(
            m.to_value(),
            json!({"id":"p","version":"0.1.0","transport":"stdio","tools":["t"],
                   "permissions":[],"protocolVersion":"unspecified"})
        );
    }

    #[test]
    fn plug2_transports_and_permissions_parse() {
        assert_eq!(
            PluginTransport::parse("stdio"),
            Some(PluginTransport::Stdio)
        );
        assert_eq!(PluginTransport::parse("tcp"), Some(PluginTransport::Tcp));
        assert_eq!(PluginTransport::parse("Stdio"), None);
        assert_eq!(
            PluginPermission::parse("fs.write"),
            Some(PluginPermission::FsWrite)
        );
        assert_eq!(PluginPermission::parse("fs.exec"), None);
    }

    #[test]
    fn plug2_rejects_missing_required_fields() {
        for (key, field) in [
            ("id", ManifestField::Id),
            ("version", ManifestField::Version),
            ("transport", ManifestField::Transport),
            ("tools", ManifestField::Tools),
        ] {
            let mut v = minimal();
            v.as_object_mut().unwrap().remove(key);
            assert_eq!(err(v), ManifestError::MissingField(field));
        }
    }

    #[test]
    fn plug2_rejects_bad_types_and_shapes() {
        let mut v = minimal();
        v["id"] = json!(1);
        assert_eq!(err(v), ManifestError::InvalidType(ManifestField::Id));
        let mut v = minimal();
        v["tools"] = json!([1]);
        assert_eq!(err(v), ManifestError::InvalidType(ManifestField::Tools));
        let mut v = minimal();
        v["permissions"] = json!("dom.read");
        assert_eq!(
            err(v),
            ManifestError::InvalidType(ManifestField::Permissions)
        );
        let mut v = minimal();
        v["protocolVersion"] = json!(3);
        assert_eq!(
            err(v),
            ManifestError::InvalidType(ManifestField::ProtocolVersion)
        );
        assert_eq!(err(json!([])), ManifestError::NotAnObject);
        let mut v = minimal();
        v["extra"] = json!(1);
        assert_eq!(err(v), ManifestError::UnknownField);
        assert_eq!(
            PluginManifest::from_slice(b"{").unwrap_err(),
            ManifestError::InvalidJson
        );
    }

    #[test]
    fn plug2_rejects_invalid_id_and_version() {
        for id in ["", "Abc", "-a", "a_b", "あ"] {
            let mut v = minimal();
            v["id"] = json!(id);
            assert_eq!(err(v), ManifestError::InvalidId, "id={id:?}");
        }
        for ver in [
            "1.2",
            "a.b.c",
            "1.2.",
            "1.2.3\nx",
            "1.2.3\u{2028}",
            "１.2.3",
            "",
        ] {
            let mut v = minimal();
            v["version"] = json!(ver);
            assert_eq!(err(v), ManifestError::InvalidVersion, "ver={ver:?}");
        }
        let mut v = minimal();
        v["version"] = json!("10.20.30.40-rc.1+build");
        assert!(PluginManifest::from_value(&v).is_ok());
    }

    #[test]
    fn plug2_rejects_unknown_transport_and_permission() {
        let mut v = minimal();
        v["transport"] = json!("http");
        assert_eq!(err(v), ManifestError::UnknownTransport);
        let mut v = minimal();
        v["permissions"] = json!(["root"]);
        assert_eq!(err(v), ManifestError::UnknownPermission);
    }

    #[test]
    fn plug2_tools_and_permissions_limits() {
        let mut v = minimal();
        v["tools"] = json!([]);
        assert_eq!(err(v), ManifestError::EmptyTools);
        let mut v = minimal();
        v["tools"] = json!([""]);
        assert_eq!(err(v), ManifestError::EmptyToolName);
        let mut v = minimal();
        v["tools"] = json!((0..64).map(|i| format!("t{i}")).collect::<Vec<_>>());
        assert!(PluginManifest::from_value(&v).is_ok());
        v["tools"] = json!((0..65).map(|i| format!("t{i}")).collect::<Vec<_>>());
        assert_eq!(err(v), ManifestError::TooManyItems(ManifestField::Tools));
        let mut v = minimal();
        v["permissions"] = json!(vec!["dom.read"; 6]);
        assert_eq!(
            err(v),
            ManifestError::TooManyItems(ManifestField::Permissions)
        );
    }

    #[test]
    fn plug2_length_limits_count_code_points() {
        let mut v = minimal();
        v["id"] = json!("a".repeat(64));
        assert!(PluginManifest::from_value(&v).is_ok());
        v["id"] = json!("a".repeat(65));
        assert_eq!(err(v), ManifestError::TooLong(ManifestField::Id));
        let mut v = minimal();
        v["tools"] = json!(["あ".repeat(128)]);
        assert!(PluginManifest::from_value(&v).is_ok());
        v["tools"] = json!(["あ".repeat(129)]);
        assert_eq!(err(v), ManifestError::TooLong(ManifestField::Tools));
        let mut v = minimal();
        v["protocolVersion"] = json!("x".repeat(129));
        assert_eq!(
            err(v),
            ManifestError::TooLong(ManifestField::ProtocolVersion)
        );
    }

    #[test]
    fn plug2_rejects_oversized_input_before_parsing() {
        let big = vec![b' '; MAX_MANIFEST_BYTES + 1];
        assert_eq!(
            PluginManifest::from_slice(&big).unwrap_err(),
            ManifestError::TooLarge
        );
    }

    #[test]
    fn plug2_error_code_and_display_are_fixed() {
        let e = ManifestError::MissingField(ManifestField::Id);
        assert_eq!(e.code(), "missing_field");
        assert_eq!(e.to_string(), "manifest is missing a required field");
        assert_eq!(ManifestField::ProtocolVersion.as_str(), "protocolVersion");
    }
}

//! プラグインマニフェスト型とレジストリ状態（`PLUG-2`・TASK-92.1・Issue #353・TASK-92.2・Issue #354・`MS-9`）。
//! `runtime` / `language` の申告フィールド（`PLUG-6`・TASK-98.1・Issue #389）も本モジュールが持つ。
//!
//! プラグインが自己申告する識別子・バージョン・トランスポート・提供ツール一覧・要求権限を
//! 表す [`PluginManifest`] と、その JSON からの構築・JSON への出力を提供する。
//! 外部入力（プラグイン由来で untrusted）を扱うため、スキーマ制約（
//! `docs/design/host-api.schema.json` の `PluginManifest`。TASK-92.5）を構築時に全て検証し、不正な状態の値を作れない
//! ようにしている。未知キー・未知の transport / permission は拒否する（fail-closed）。
//!
//! 検証済みマニフェストを保持する [`PluginRegistry`] と、それを `Arc<AppState>` と共に持つ ai 固有の
//! 状態型 [`AiState`] も本モジュールに置く（core の `AppState` は変更しない。spec
//! `self-repair-design.md` の「プロトコル固有の状態は各 crate に閉じる」決定）。
//!
//! 呼び出し文脈: TASK-92.3 の登録ハンドラ（`POST /ai/plugins/register`）が [`PluginManifest::from_slice`]
//! を、TASK-92.4 の一覧（`GET /ai/plugins`）が [`PluginManifest::to_value`] を使う想定。
//!
//! # スタブ・暫定仕様について（REPAIR-3）
//!
//! - 登録ルート `POST /ai/plugins/register` と HTTP ステータス写像は [`crate::api`] に実装済み
//!   （TASK-92.3・Issue #355）。一覧 `GET /ai/plugins` も実装済み（TASK-92.4・Issue #356。
//!   [`crate::api::router_with_state`]）
//! - レジストリはインメモリでプロセス寿命のみ保持し、永続化しない。削除・上書き API は持たない
//! - `permissions` は申告値の保持のみで、権限の付与・強制は行わない
//! - `tcp` / `unix-socket` は列挙値として受理するだけで、接続処理は持たない
//! - `runtime` / `language` は申告値の保持と出力のみ。`PLUG-3` 目標の適用判定は TASK-98.2（Issue #390）で未実装。
//!   省略時の `unspecified` は「公式サポート（Rust ネイティブ）」扱いにしない（未申告は公式扱いにしない。fail-closed）。
//!   値は自己申告で untrusted であり、プロセス起動やインタプリタ選択には使わない
//! - 新規依存を避けるため serde の derive は使わず、`serde_json::Value` から手動で抽出する

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fandhe_browser_core::AppState;

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
/// `runtime` の最大文字数（コードポイント数。`PLUG-6`）。
pub const MAX_RUNTIME_CHARS: usize = 64;
/// `language` の最大文字数（コードポイント数。`PLUG-6`）。
pub const MAX_LANGUAGE_CHARS: usize = 64;
/// `runtime` 省略時の既定値。公式サポート扱いにしない（`PLUG-6`・TASK-98.2 が判定する）。
pub const DEFAULT_RUNTIME: &str = "unspecified";
/// `language` 省略時の既定値。公式サポート扱いにしない（`PLUG-6`・TASK-98.2 が判定する）。
pub const DEFAULT_LANGUAGE: &str = "unspecified";

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
    Runtime,
    Language,
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
            Self::Runtime => "runtime",
            Self::Language => "language",
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
    /// `runtime` の形式違反（`PLUG-6`）。
    InvalidRuntime,
    /// `language` の形式違反（`PLUG-6`）。
    InvalidLanguage,
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
            Self::InvalidRuntime => "invalid_runtime",
            Self::InvalidLanguage => "invalid_language",
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
            Self::InvalidRuntime => "manifest runtime is invalid",
            Self::InvalidLanguage => "manifest language is invalid",
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
    runtime: String,
    language: String,
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
        const KNOWN: [&str; 8] = [
            "id",
            "version",
            "transport",
            "tools",
            "permissions",
            "protocolVersion",
            "runtime",
            "language",
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

        let runtime = optional_token(
            obj,
            ManifestField::Runtime,
            MAX_RUNTIME_CHARS,
            DEFAULT_RUNTIME,
            ManifestError::InvalidRuntime,
        )?;
        let language = optional_token(
            obj,
            ManifestField::Language,
            MAX_LANGUAGE_CHARS,
            DEFAULT_LANGUAGE,
            ManifestError::InvalidLanguage,
        )?;

        Ok(Self {
            id: id.to_owned(),
            version: version.to_owned(),
            transport,
            tools: tool_names,
            permissions,
            protocol_version,
            runtime,
            language,
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

    /// 申告された実行ランタイム（`PLUG-6`）。未指定時は [`DEFAULT_RUNTIME`]。
    ///
    /// 自己申告の保持のみで、公式サポートの判定は行わない（TASK-98.2・Issue #390 で未実装）。
    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    /// 申告された実装言語（`PLUG-6`）。未指定時は [`DEFAULT_LANGUAGE`]。
    ///
    /// 自己申告の保持のみで、公式サポートの判定は行わない（TASK-98.2・Issue #390 で未実装）。
    pub fn language(&self) -> &str {
        &self.language
    }

    /// 8 キー全てを出力する JSON 表現（既定値のキーも省略しない）。
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "version": self.version,
            "transport": self.transport.as_str(),
            "tools": self.tools,
            "permissions": self.permissions.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            "protocolVersion": self.protocol_version,
            "runtime": self.runtime,
            "language": self.language,
        })
    }
}

/// レジストリへ登録できるプラグインの最大件数（PoC-15 の `MAX_PLUGINS`）。無制限確保を防ぐ。
pub const MAX_PLUGINS: usize = 32;

/// レジストリ登録の拒否理由（`PLUG-2`・TASK-92.2・Issue #354）。
///
/// HTTP ステータスへの写像（重複 409・満杯 429）は `api` の登録ハンドラ（TASK-92.3）が持ち、ここでは持たない。
/// 文言は固定英語で、untrusted な id を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistryError {
    /// 同じ id が登録済み。
    DuplicateId,
    /// [`MAX_PLUGINS`] に達している。
    Full,
}

impl RegistryError {
    /// 機械可読なエラーコード。
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateId => "duplicate_plugin_id",
            Self::Full => "plugin_registry_full",
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId => f.write_str("plugin id is already registered"),
            Self::Full => f.write_str("plugin registry is full"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// 登録済みプラグインマニフェストのインメモリレジストリ（`PLUG-2`・TASK-92.2・Issue #354）。
///
/// TASK-92.3 の登録ハンドラが [`register`](Self::register) を、TASK-92.4 の一覧が
/// [`list`](Self::list) を呼ぶ想定。重複判定・上限判定・追加は単一ロック内で行い、並行登録でも
/// 上限超過・重複が起きない。永続化せず、削除・上書きはできない（既存プラグインの差し替え防止）。
pub struct PluginRegistry {
    plugins: Mutex<Vec<PluginManifest>>,
}

impl PluginRegistry {
    /// 空のレジストリを作る。
    pub fn new() -> Self {
        Self {
            plugins: Mutex::new(Vec::new()),
        }
    }

    /// poison は回復する。書き込みは push のみで途中状態が残らないため安全。
    fn guard(&self) -> MutexGuard<'_, Vec<PluginManifest>> {
        self.plugins.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// マニフェストを登録する。判定順は「id 重複 → 件数上限」（PoC-15 に合わせる）。
    pub fn register(&self, manifest: PluginManifest) -> Result<(), RegistryError> {
        let mut plugins = self.guard();
        if plugins.iter().any(|m| m.id() == manifest.id()) {
            return Err(RegistryError::DuplicateId);
        }
        if plugins.len() >= MAX_PLUGINS {
            return Err(RegistryError::Full);
        }
        plugins.push(manifest);
        Ok(())
    }

    /// 登録順の複製を返す。
    pub fn list(&self) -> Vec<PluginManifest> {
        self.guard().clone()
    }

    /// 登録件数。
    pub fn len(&self) -> usize {
        self.guard().len()
    }

    /// 登録が 0 件か。
    pub fn is_empty(&self) -> bool {
        self.guard().is_empty()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PluginRegistry {
    /// マニフェスト内容は出さず件数のみ。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("len", &self.len())
            .finish()
    }
}

/// ai crate 固有の状態（core の共通状態 `Arc<AppState>` とプラグインレジストリ。`PLUG-2`・TASK-92.2・Issue #354）。
///
/// `api::router` が構築した `Arc<AiState>` を `api::router_with_state` の各ハンドラ
/// （TASK-92.3 の登録・TASK-92.4 の一覧）が使う。1 つの `AiState` は 1 つの `AppState`（= 1 プロファイル）に対応し、グローバル
/// 共有を持たないためプロファイル間でレジストリは共有されない（`PROF-1`）。`Clone` は実装せず
/// `Arc` で共有する。
pub struct AiState {
    app: Arc<AppState>,
    plugins: PluginRegistry,
}

impl AiState {
    /// 空のレジストリで構築する。
    pub fn new(app: Arc<AppState>) -> Self {
        Self {
            app,
            plugins: PluginRegistry::new(),
        }
    }

    /// 内包する core の共通状態。
    pub fn app(&self) -> &Arc<AppState> {
        &self.app
    }

    /// プラグインレジストリ。
    pub fn plugins(&self) -> &PluginRegistry {
        &self.plugins
    }
}

impl fmt::Debug for AiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiState")
            .field("app", &self.app)
            .field("plugins", &self.plugins)
            .finish()
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

/// 任意トークン（`runtime` / `language`）を抽出する。省略時は `default`。
/// 検査順は 型（`InvalidType`）→ 長さ（`TooLong`）→ 形式（`invalid`）。
fn optional_token(
    obj: &Map<String, Value>,
    field: ManifestField,
    max_chars: usize,
    default: &str,
    invalid: ManifestError,
) -> Result<String, ManifestError> {
    match obj.get(field.as_str()) {
        None => Ok(default.to_owned()),
        Some(v) => {
            let s = v.as_str().ok_or(ManifestError::InvalidType(field))?;
            if char_len_exceeds(s, max_chars) {
                return Err(ManifestError::TooLong(field));
            }
            if !is_valid_runtime_token(s) {
                return Err(invalid);
            }
            Ok(s.to_owned())
        }
    }
}

/// `^[a-z0-9][a-z0-9.+#_-]*$`（末尾は厳密アンカー。改行を許さない。`PLUG-6`）。
/// 小文字 ASCII トークンに限り、TASK-98.2 が完全一致で判定できるようにする。
fn is_valid_runtime_token(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '+' | '#' | '_' | '-')
    })
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
mod schema_contract_tests;

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
            "protocolVersion": "2025-06-18",
            "runtime": "node",
            "language": "javascript"
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
        assert_eq!(m.runtime(), "node");
        assert_eq!(m.language(), "javascript");
        assert_eq!(m.to_value(), full());
        assert_eq!(PluginManifest::from_value(&m.to_value()).unwrap(), m);
    }

    #[test]
    fn plug2_defaults_for_optional_fields() {
        let m = PluginManifest::from_value(&minimal()).unwrap();
        assert!(m.permissions().is_empty());
        assert_eq!(m.protocol_version(), "unspecified");
        assert_eq!(m.runtime(), "unspecified");
        assert_eq!(m.language(), "unspecified");
        assert_eq!(
            m.to_value(),
            json!({"id":"p","version":"0.1.0","transport":"stdio","tools":["t"],
                   "permissions":[],"protocolVersion":"unspecified",
                   "runtime":"unspecified","language":"unspecified"})
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
        assert_eq!(ManifestField::Runtime.as_str(), "runtime");
        assert_eq!(ManifestField::Language.as_str(), "language");
    }

    fn manifest(id: &str, version: &str) -> PluginManifest {
        PluginManifest::from_value(
            &json!({"id": id, "version": version, "transport": "stdio", "tools": ["t"]}),
        )
        .unwrap()
    }

    #[test]
    fn plug2_registry_starts_empty() {
        let r = PluginRegistry::new();
        assert_eq!(r.len(), 0);
        assert!(r.is_empty());
        assert_eq!(r.list(), Vec::<PluginManifest>::new());
    }

    #[test]
    fn plug2_registry_register_then_list_keeps_order() {
        let r = PluginRegistry::new();
        let (a, b) = (manifest("beta", "1.0.0"), manifest("alpha", "2.0.0"));
        r.register(a.clone()).unwrap();
        r.register(b.clone()).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r.list(), vec![a, b]);
        let ids: Vec<String> = r.list().iter().map(|m| m.id().to_owned()).collect();
        assert_eq!(ids, ["beta", "alpha"]);
    }

    #[test]
    fn plug2_registry_rejects_duplicate_id() {
        let r = PluginRegistry::new();
        r.register(manifest("dup", "1.0.0")).unwrap();
        assert_eq!(
            r.register(manifest("dup", "2.0.0")),
            Err(RegistryError::DuplicateId)
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r.list()[0].version(), "1.0.0");
    }

    #[test]
    fn plug2_registry_rejects_when_full() {
        let r = PluginRegistry::new();
        for i in 0..MAX_PLUGINS {
            r.register(manifest(&format!("p{i}"), "1.0.0")).unwrap();
        }
        assert_eq!(
            r.register(manifest("extra", "1.0.0")),
            Err(RegistryError::Full)
        );
        assert_eq!(r.len(), 32);
        assert_eq!(
            r.register(manifest("p0", "1.0.0")),
            Err(RegistryError::DuplicateId)
        );
    }

    #[test]
    fn plug2_registry_error_code_and_display_are_fixed() {
        assert_eq!(RegistryError::DuplicateId.code(), "duplicate_plugin_id");
        assert_eq!(RegistryError::Full.code(), "plugin_registry_full");
        assert_eq!(
            RegistryError::DuplicateId.to_string(),
            "plugin id is already registered"
        );
        assert_eq!(RegistryError::Full.to_string(), "plugin registry is full");
    }

    #[test]
    fn plug2_registry_and_state_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PluginRegistry>();
        assert_send_sync::<AiState>();
    }

    #[test]
    fn plug2_registry_debug_hides_manifests() {
        let r = PluginRegistry::new();
        r.register(manifest("secret-id", "1.0.0")).unwrap();
        let d = format!("{r:?}");
        assert!(!d.contains("secret-id"));
        assert_eq!(d, "PluginRegistry { len: 1 }");
    }

    #[test]
    fn plug6_runtime_and_language_round_trip() {
        let mut v = minimal();
        v["runtime"] = json!("native");
        v["language"] = json!("rust");
        let m = PluginManifest::from_value(&v).unwrap();
        assert_eq!(m.runtime(), "native");
        assert_eq!(m.language(), "rust");
        let out = m.to_value();
        assert_eq!(out["runtime"], "native");
        assert_eq!(out["language"], "rust");
        assert_eq!(PluginManifest::from_value(&out).unwrap(), m);
    }

    #[test]
    fn plug6_rejects_invalid_runtime_and_language() {
        for (key, field, invalid) in [
            (
                "runtime",
                ManifestField::Runtime,
                ManifestError::InvalidRuntime,
            ),
            (
                "language",
                ManifestField::Language,
                ManifestError::InvalidLanguage,
            ),
        ] {
            let with = |x: Value| {
                let mut v = minimal();
                v[key] = x;
                v
            };
            assert_eq!(err(with(json!(1))), ManifestError::InvalidType(field));
            assert_eq!(
                err(with(json!("a".repeat(65)))),
                ManifestError::TooLong(field)
            );
            assert!(PluginManifest::from_value(&with(json!("a".repeat(64)))).is_ok());
            for bad in ["", "Rust", "-x", "node 20", "rust\n", "ｒｕｓｔ"] {
                assert_eq!(err(with(json!(bad))), invalid, "value {bad:?}");
            }
            for ok in ["c++", "c#", "python3.12", "node-20"] {
                assert!(PluginManifest::from_value(&with(json!(ok))).is_ok());
            }
        }
    }

    #[test]
    fn plug6_error_code_and_display_are_fixed() {
        assert_eq!(ManifestError::InvalidRuntime.code(), "invalid_runtime");
        assert_eq!(ManifestError::InvalidLanguage.code(), "invalid_language");
        assert_eq!(
            ManifestError::InvalidRuntime.to_string(),
            "manifest runtime is invalid"
        );
        assert_eq!(
            ManifestError::InvalidLanguage.to_string(),
            "manifest language is invalid"
        );
    }
}

//! ブラウザプロファイル（Chrome / Safari）の CSS 対応可否データの埋め込み読み込み。
//!
//! TASK-100.2（Issue #266）・`PLUG-8`・MS-8。`profiles/chrome.json`・`profiles/safari.json`
//! （TASK-100.1・#265）をバイナリへ埋め込み、CSS プロパティ単位の対応可否を照会する。
//! TASK-100.3（Issue #267）で公開入口 [`profile_gate`]・[`profile_gate_from_name`] と
//! ハンドル [`ProfileGate`] を追加した。TASK-100.4（Issue #268）で gating 本体
//! [`ProfileGate::filter_declarations`]・[`ProfileGate::apply`] を追加し、computed style の
//! 宣言列（`cssom::ComputedStyle`）から非対応プロパティの宣言を除去する
//! （カスケード後に絞る方式。プロパティ単位のため事前に絞る方式と結果は等価）。
//!
//! # 線引き・注意
//!
//! - 本モジュールは CSS 機能の有無を返すだけで、`navigator.userAgent` 等の識別面を
//!   読みも書きもしない（`SEC-1`・`SEC-2`）。
//! - `fandhe-browser-profile` crate のユーザープロファイル（Cookie・ストレージ）とは
//!   無関係で、その保管場所へアクセスしない。
//! - 埋め込みデータの読み込みに失敗しても panic せず [`ProfileLoadError`] を返す
//!   （fail-closed。全プロパティ素通しへ黙って落とさない）。
//! - 未実装（REPAIR-3）: `fandhe-browser-cli` の `--profile` 実配線は cli 側タスクの担当で
//!   未実装（本モジュールはライブラリ関数として gating を提供するのみ）。
//!
//! JSON の解析は依存を増やさないため非公開の最小パーサーで行う（core は `serde_json`
//! を依存に持たない。dependency-policy）。

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::sync::OnceLock;

use crate::cssom::{ComputedDeclaration, ComputedStyle};

const CHROME_JSON: &str = include_str!("../../../profiles/chrome.json");
const SAFARI_JSON: &str = include_str!("../../../profiles/safari.json");

/// エラーに載せる入力値の上限バイト数（無制限なエコーバックを避ける）。
const MAX_PROFILE_NAME_BYTES: usize = 64;
/// JSON のネスト深さ上限。
const MAX_DEPTH: usize = 16;
/// JSON 全体の要素数上限。
const MAX_ENTRIES: usize = 4096;
/// サポートするデータスキーマのバージョン。
const SCHEMA_VERSION: u64 = 1;

/// 対応可否を照会する対象のブラウザプロファイル種別。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserProfile {
    /// Chrome 相当。
    Chrome,
    /// Safari 相当。
    Safari,
}

impl BrowserProfile {
    /// 全プロファイル種別。
    pub const ALL: [BrowserProfile; 2] = [BrowserProfile::Chrome, BrowserProfile::Safari];

    /// 設定値・JSON の `browser` と同じ小文字名を返す。
    pub fn as_str(&self) -> &'static str {
        match self {
            BrowserProfile::Chrome => "chrome",
            BrowserProfile::Safari => "safari",
        }
    }
}

impl fmt::Display for BrowserProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`BrowserProfile`] の文字列解釈エラー。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserProfileParseError {
    /// 未知のプロファイル名。`value` は上限バイト数で切り詰め済み。
    Unknown {
        /// 与えられた文字列（切り詰め済み）。
        value: String,
    },
}

impl fmt::Display for BrowserProfileParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrowserProfileParseError::Unknown { value } => write!(
                f,
                "unknown browser profile '{value}' (expected chrome or safari)"
            ),
        }
    }
}

impl std::error::Error for BrowserProfileParseError {}

impl FromStr for BrowserProfile {
    type Err = BrowserProfileParseError;

    /// 完全一致の `chrome` / `safari` のみ受理する。大文字・前後空白は拒否し、
    /// 黙って既定へ落とさない（`harness/wpt_subset_runner` の `WptProfile::parse` と同方針）。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "chrome" => Ok(BrowserProfile::Chrome),
            "safari" => Ok(BrowserProfile::Safari),
            other => {
                let mut end = other.len().min(MAX_PROFILE_NAME_BYTES);
                while !other.is_char_boundary(end) {
                    end -= 1;
                }
                Err(BrowserProfileParseError::Unknown {
                    value: other.get(..end).unwrap_or("").to_string(),
                })
            }
        }
    }
}

/// CSS プロパティ単位の対応可否。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertySupport {
    /// テーブルに載っており、対象ブラウザが対応している。
    Supported,
    /// テーブルに載っており、対象ブラウザが非対応。
    Unsupported,
    /// テーブルに載っていない（素通し扱い。`--` カスタムプロパティ・対応表に無いプロパティ。
    /// 基本プロパティは TASK-100.8・Issue #738 で本体 feature へ載せ済み）。
    Unlisted,
}

impl PropertySupport {
    /// 宣言を残してよいか。`Unsupported` のみ `false`。
    pub fn is_allowed(&self) -> bool {
        !matches!(self, PropertySupport::Unsupported)
    }
}

/// 解決済みのプロファイル対応可否テーブル。
#[derive(Debug)]
pub struct ProfileTable {
    profile: BrowserProfile,
    properties: BTreeMap<String, bool>,
    feature_count: usize,
}

impl ProfileTable {
    /// このテーブルのプロファイル種別。
    pub fn profile(&self) -> BrowserProfile {
        self.profile
    }

    /// プロパティの対応可否を返す。`property` は ASCII 小文字に正規化済みであること
    /// （`cssom::Declaration::property` は正規化済みなのでそのまま渡せる）。
    pub fn property_support(&self, property: &str) -> PropertySupport {
        match self.properties.get(property) {
            Some(true) => PropertySupport::Supported,
            Some(false) => PropertySupport::Unsupported,
            None => PropertySupport::Unlisted,
        }
    }

    /// [`ProfileTable::property_support`] を真偽値へ畳む（未掲載は `true`）。
    pub fn is_supported(&self, property: &str) -> bool {
        self.property_support(property).is_allowed()
    }

    /// テーブルに載っているプロパティ数。
    pub fn property_count(&self) -> usize {
        self.properties.len()
    }

    /// データに載っている feature 数。
    pub fn feature_count(&self) -> usize {
        self.feature_count
    }
}

/// 埋め込みデータの読み込みエラー。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileLoadError {
    /// JSON 構文エラー。
    Syntax {
        /// 入力先頭からのバイトオフセット。
        offset: usize,
        /// 原因。
        message: &'static str,
    },
    /// 未対応の `schemaVersion`。
    UnsupportedSchemaVersion,
    /// `browser` が要求したプロファイルと一致しない。
    BrowserMismatch,
    /// 必須フィールドの欠落。
    MissingField {
        /// フィールド名。
        field: &'static str,
    },
    /// フィールドの型・値が不正。
    InvalidField {
        /// フィールド名。
        field: &'static str,
    },
    /// `cssProperties` が `features` に無い ID を指している。
    UnknownFeatureReference,
    /// 要素数が上限を超えた。
    TooManyEntries,
}

impl fmt::Display for ProfileLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileLoadError::Syntax { offset, message } => {
                write!(f, "profile data syntax error at byte {offset}: {message}")
            }
            ProfileLoadError::UnsupportedSchemaVersion => {
                f.write_str("unsupported profile data schemaVersion")
            }
            ProfileLoadError::BrowserMismatch => {
                f.write_str("profile data browser does not match requested profile")
            }
            ProfileLoadError::MissingField { field } => {
                write!(f, "profile data is missing field '{field}'")
            }
            ProfileLoadError::InvalidField { field } => {
                write!(f, "profile data has invalid field '{field}'")
            }
            ProfileLoadError::UnknownFeatureReference => {
                f.write_str("profile data cssProperties references an unknown feature")
            }
            ProfileLoadError::TooManyEntries => f.write_str("profile data has too many entries"),
        }
    }
}

impl std::error::Error for ProfileLoadError {}

/// 埋め込みプロファイルを（初回のみ解析して）返す。呼び出し元は [`profile_gate`]
/// （#267）・#268 の gating。失敗時の扱い（素通しにするか）は呼び出し側が決める。
pub fn load_profile(profile: BrowserProfile) -> Result<&'static ProfileTable, ProfileLoadError> {
    static CHROME: OnceLock<Result<ProfileTable, ProfileLoadError>> = OnceLock::new();
    static SAFARI: OnceLock<Result<ProfileTable, ProfileLoadError>> = OnceLock::new();
    let cell = match profile {
        BrowserProfile::Chrome => CHROME.get_or_init(|| build_table(profile, CHROME_JSON)),
        BrowserProfile::Safari => SAFARI.get_or_init(|| build_table(profile, SAFARI_JSON)),
    };
    cell.as_ref().map_err(Clone::clone)
}

/// feature gating の起動結果（無効、または指定プロファイルで有効）。
///
/// [`profile_gate`] / [`profile_gate_from_name`] が返す `Copy` のハンドルで、
/// `fandhe-browser-cli` が起動時に 1 回得て保持し、computed style 取得経路へ渡す
/// （cli 側の実配線は未実装で cli 側タスクの担当。REPAIR-3）。
/// 宣言の除去は [`ProfileGate::filter_declarations`]・[`ProfileGate::apply`]
/// （TASK-100.4・#268）が担う。
/// Cookie・ストレージのユーザープロファイルとは無関係。TASK-100.3・`PLUG-8`・MS-8。
#[derive(Debug, Clone, Copy)]
pub struct ProfileGate {
    table: Option<&'static ProfileTable>,
}

impl ProfileGate {
    /// gating 無効（`--profile` 指定なし）。全プロパティを素通しとして扱う。
    pub const fn disabled() -> Self {
        Self { table: None }
    }

    /// 有効時のプロファイル種別。無効なら `None`。
    pub fn profile(&self) -> Option<BrowserProfile> {
        self.table.map(ProfileTable::profile)
    }

    /// gating が有効（プロファイル指定あり）か。
    pub fn is_enabled(&self) -> bool {
        self.table.is_some()
    }

    /// プロパティの対応可否を返す。無効時は [`PropertySupport::Unlisted`]。
    /// `property` は ASCII 小文字に正規化済みであること
    /// （`cssom::Declaration::property` と同じ契約）。
    pub fn property_support(&self, property: &str) -> PropertySupport {
        match self.table {
            Some(table) => table.property_support(property),
            None => PropertySupport::Unlisted,
        }
    }

    /// 宣言を残してよいか。無効時と未掲載は `true`。
    pub fn allows_property(&self, property: &str) -> bool {
        self.property_support(property).is_allowed()
    }

    /// 宣言列から非対応（[`PropertySupport::Unsupported`]）プロパティの宣言だけを除く。
    ///
    /// 対応・未掲載（`--` カスタムプロパティ等）の宣言は値・重要度・由来・順序を保って残す。
    /// 呼び出し元は computed style 取得経路（cli 配線は未実装）。確保量は入力長以下。
    /// 無効な gate は全宣言を残す。TASK-100.4・Issue #268・`PLUG-8`・MS-8。
    pub fn filter_declarations(&self, declarations: &[ComputedDeclaration]) -> GatedStyle {
        let mut kept = Vec::new();
        let mut removed = Vec::new();
        for decl in declarations {
            if self.allows_property(decl.property()) {
                kept.push(decl.clone());
            } else {
                removed.push(decl.clone());
            }
        }
        GatedStyle {
            profile: self.profile(),
            declarations: kept,
            removed,
            inline_skipped: false,
        }
    }

    /// [`ComputedStyle`] 全体へ gating を適用する（`inline_skipped` を引き継ぐ）。
    /// TASK-100.4・Issue #268・`PLUG-8`。
    pub fn apply(&self, style: &ComputedStyle) -> GatedStyle {
        let mut gated = self.filter_declarations(style.declarations());
        gated.inline_skipped = style.inline_skipped();
        gated
    }
}

/// gating 適用後の宣言一覧。
///
/// [`ProfileGate`] が返す構造化結果で、残した宣言に加え除去した宣言も保持する
/// （後続の検証・AI 自己補修での診断用。`REPAIR-4`）。UA 文字列等の識別面には
/// 関与しない（`SEC-1`・`SEC-2`）。TASK-100.4・Issue #268・`PLUG-8`・MS-8。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatedStyle {
    profile: Option<BrowserProfile>,
    declarations: Vec<ComputedDeclaration>,
    removed: Vec<ComputedDeclaration>,
    inline_skipped: bool,
}

impl GatedStyle {
    /// 適用したプロファイル。gating 無効なら `None`。
    pub fn profile(&self) -> Option<BrowserProfile> {
        self.profile
    }

    /// 残した宣言（入力順を保持）。
    pub fn declarations(&self) -> &[ComputedDeclaration] {
        &self.declarations
    }

    /// 非対応として除去した宣言（入力順）。
    pub fn removed(&self) -> &[ComputedDeclaration] {
        &self.removed
    }

    /// 残した宣言から property 名（小文字）で探す。除去済みは `None`。
    pub fn get(&self, property: &str) -> Option<&ComputedDeclaration> {
        self.declarations.iter().find(|d| d.property() == property)
    }

    /// 元の `ComputedStyle` が inline 源を skip していたか。
    pub fn inline_skipped(&self) -> bool {
        self.inline_skipped
    }

    /// 残した宣言数。
    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    /// 残した宣言が無いか。
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }
}

/// プロファイル指定から feature gating を起動する公開入口。
///
/// 呼び出し元: `fandhe-browser-cli` が起動時に `--profile chrome|safari` を解釈し、
/// 値があれば `Some`、無ければ `None` を渡して 1 回呼ぶ（cli の実配線は未実装・cli 側
/// タスクの担当）。`None` は [`ProfileGate::disabled`] を返す。
/// 呼び出し先: [`load_profile`]（初回のみ解析し以後は `&'static` を共有。スレッド安全）。
/// 埋め込みデータの読み込み失敗は [`crate::Error::BrowserProfileLoad`] を返し、
/// 黙って無効へ落とさない（fail-closed）。
///
/// 宣言の除去は [`ProfileGate::apply`]（#268）。CSS 機能の有無のみを扱い、
/// `navigator.userAgent` 等の識別面は読み書きしない（`SEC-1`・`SEC-2`）。
/// TASK-100.3・Issue #267・`PLUG-8`・MS-8。
pub fn profile_gate(profile: Option<BrowserProfile>) -> crate::Result<ProfileGate> {
    match profile {
        None => Ok(ProfileGate::disabled()),
        Some(p) => Ok(ProfileGate {
            table: Some(load_profile(p)?),
        }),
    }
}

/// CLI の `--profile` 値（文字列）から直接 gating を起動する糖衣。
///
/// 呼び出し元: `fandhe-browser-cli` が引数文字列をそのまま渡す。完全一致の
/// `chrome` / `safari` のみ受理し（trim・小文字化はしない）、未知の名前は
/// [`crate::Error::BrowserProfileName`]（入力値は 64 バイトへ切り詰め済み）を返す。
/// 他の契約は [`profile_gate`] と同じ。TASK-100.3・Issue #267・`PLUG-8`。
pub fn profile_gate_from_name(name: Option<&str>) -> crate::Result<ProfileGate> {
    let profile = name.map(str::parse::<BrowserProfile>).transpose()?;
    profile_gate(profile)
}

/// 簡易照会。未掲載プロパティは `Ok(true)`。
pub fn is_property_supported(
    profile: BrowserProfile,
    property: &str,
) -> Result<bool, ProfileLoadError> {
    Ok(load_profile(profile)?.is_supported(property))
}

// ---- 最小 JSON パーサー（非公開） ----

enum Json {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr,
    Obj(Vec<(String, Json)>),
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    entries: usize,
}

impl Parser<'_> {
    fn err<T>(&self, message: &'static str) -> Result<T, ProfileLoadError> {
        Err(ProfileLoadError::Syntax {
            offset: self.i,
            message,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\t' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), ProfileLoadError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err("unexpected character")
        }
    }

    fn count(&mut self) -> Result<(), ProfileLoadError> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err(ProfileLoadError::TooManyEntries);
        }
        Ok(())
    }

    /// エスケープを含まない文字列のみ受理する（プロファイルデータは固定書式）。
    fn string(&mut self) -> Result<String, ProfileLoadError> {
        self.eat(b'"')?;
        let start = self.i;
        loop {
            match self.peek() {
                None => return self.err("unterminated string"),
                Some(b'"') => break,
                Some(b'\\') => return self.err("escape sequences are not supported"),
                Some(c) if c < 0x20 => return self.err("control character in string"),
                Some(_) => self.i += 1,
            }
        }
        let s = self
            .b
            .get(start..self.i)
            .and_then(|s| std::str::from_utf8(s).ok())
            .map(str::to_string);
        self.i += 1;
        match s {
            Some(s) => Ok(s),
            None => self.err("invalid utf-8"),
        }
    }

    fn lit(&mut self, word: &[u8], v: Json) -> Result<Json, ProfileLoadError> {
        if self.b.get(self.i..self.i.saturating_add(word.len())) == Some(word) {
            self.i += word.len();
            Ok(v)
        } else {
            self.err("invalid literal")
        }
    }

    /// 非負整数のみ受理する（小数・指数・負数は不要）。
    fn number(&mut self) -> Result<Json, ProfileLoadError> {
        let start = self.i;
        let mut n: u64 = 0;
        while let Some(c @ b'0'..=b'9') = self.peek() {
            n = match n
                .checked_mul(10)
                .and_then(|n| n.checked_add(u64::from(c - b'0')))
            {
                Some(n) => n,
                None => return self.err("number too large"),
            };
            self.i += 1;
        }
        if self.i == start {
            return self.err("invalid number");
        }
        Ok(Json::Num(n))
    }

    fn value(&mut self, depth: usize) -> Result<Json, ProfileLoadError> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        self.count()?;
        self.ws();
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                let mut members = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(members));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    self.eat(b':')?;
                    let v = self.value(depth + 1)?;
                    members.push((k, v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(members));
                        }
                        _ => return self.err("expected ',' or '}'"),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(Json::Arr);
                }
                loop {
                    self.value(depth + 1)?;
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr);
                        }
                        _ => return self.err("expected ',' or ']'"),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit(b"true", Json::Bool(true)),
            Some(b'f') => self.lit(b"false", Json::Bool(false)),
            Some(b'n') => self.lit(b"null", Json::Null),
            Some(b'0'..=b'9') => self.number(),
            _ => self.err("unexpected token"),
        }
    }
}

fn parse_json(data: &str) -> Result<Json, ProfileLoadError> {
    let mut p = Parser {
        b: data.as_bytes(),
        i: 0,
        entries: 0,
    };
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return p.err("trailing characters");
    }
    Ok(v)
}

fn member<'a>(
    members: &'a [(String, Json)],
    field: &'static str,
) -> Result<&'a Json, ProfileLoadError> {
    members
        .iter()
        .find(|(k, _)| k == field)
        .map(|(_, v)| v)
        .ok_or(ProfileLoadError::MissingField { field })
}

fn object<'a>(v: &'a Json, field: &'static str) -> Result<&'a [(String, Json)], ProfileLoadError> {
    match v {
        Json::Obj(m) => Ok(m),
        _ => Err(ProfileLoadError::InvalidField { field }),
    }
}

/// JSON 文字列から [`ProfileTable`] を構築・検証する（`load_profile` の本体。
/// ユニットテストが不正入力を渡せるよう分離している）。
fn build_table(profile: BrowserProfile, json: &str) -> Result<ProfileTable, ProfileLoadError> {
    let root = parse_json(json)?;
    let root = object(&root, "root")?;

    match member(root, "schemaVersion")? {
        Json::Num(n) if *n == SCHEMA_VERSION => {}
        Json::Num(_) => return Err(ProfileLoadError::UnsupportedSchemaVersion),
        _ => {
            return Err(ProfileLoadError::InvalidField {
                field: "schemaVersion",
            });
        }
    }
    match member(root, "browser")? {
        Json::Str(s) if s == profile.as_str() => {}
        Json::Str(_) => return Err(ProfileLoadError::BrowserMismatch),
        _ => return Err(ProfileLoadError::InvalidField { field: "browser" }),
    }

    let mut features: BTreeMap<&str, bool> = BTreeMap::new();
    for (id, v) in object(member(root, "features")?, "features")? {
        let entry = object(v, "features")?;
        match member(entry, "supported")? {
            Json::Bool(b) => features.insert(id.as_str(), *b),
            _ => return Err(ProfileLoadError::InvalidField { field: "supported" }),
        };
    }

    let mut properties = BTreeMap::new();
    for (name, v) in object(member(root, "cssProperties")?, "cssProperties")? {
        let Json::Str(id) = v else {
            return Err(ProfileLoadError::InvalidField {
                field: "cssProperties",
            });
        };
        let supported = features
            .get(id.as_str())
            .ok_or(ProfileLoadError::UnknownFeatureReference)?;
        properties.insert(name.clone(), *supported);
    }

    Ok(ProfileTable {
        profile,
        properties,
        feature_count: features.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"{"schemaVersion":1,"browser":"chrome","features":{"a":{"supported":true,"sinceVersion":"1"},"b":{"supported":false,"sinceVersion":null}},"cssProperties":{"p":"a","q":"b"}}"#;

    fn build(json: &str) -> Result<ProfileTable, ProfileLoadError> {
        build_table(BrowserProfile::Chrome, json)
    }

    #[test]
    fn plug8_build_table_resolves_support() {
        let t = build(OK).expect("valid");
        assert_eq!(t.property_support("p"), PropertySupport::Supported);
        assert_eq!(t.property_support("q"), PropertySupport::Unsupported);
        assert_eq!(t.property_support("zzz"), PropertySupport::Unlisted);
        assert_eq!((t.property_count(), t.feature_count()), (2, 2));
    }

    #[test]
    fn plug8_gate_apply_matches_filter_declarations() {
        let gate = profile_gate(Some(BrowserProfile::Chrome)).expect("ok");
        let style = ComputedStyle::default();
        let a = gate.apply(&style);
        let b = gate.filter_declarations(style.declarations());
        assert_eq!(a, b);
        assert_eq!(a.profile(), Some(BrowserProfile::Chrome));
        assert!(a.removed().is_empty());
    }

    #[test]
    fn plug8_build_table_rejects_bad_syntax() {
        assert!(matches!(
            build("{\"schemaVersion\":"),
            Err(ProfileLoadError::Syntax { .. })
        ));
        assert!(matches!(
            build("{} x"),
            Err(ProfileLoadError::Syntax { .. })
        ));
    }

    #[test]
    fn plug8_build_table_rejects_schema_and_browser() {
        let v2 = OK.replace("\"schemaVersion\":1", "\"schemaVersion\":2");
        assert_eq!(
            build(&v2).unwrap_err(),
            ProfileLoadError::UnsupportedSchemaVersion
        );
        let other = OK.replace("\"chrome\"", "\"safari\"");
        assert_eq!(
            build(&other).unwrap_err(),
            ProfileLoadError::BrowserMismatch
        );
    }

    #[test]
    fn plug8_build_table_rejects_dangling_feature_and_missing_key() {
        let dangling = OK.replace("\"q\":\"b\"", "\"q\":\"zz\"");
        assert_eq!(
            build(&dangling).unwrap_err(),
            ProfileLoadError::UnknownFeatureReference
        );
        let missing = r#"{"schemaVersion":1,"browser":"chrome","features":{}}"#;
        assert_eq!(
            build(missing).unwrap_err(),
            ProfileLoadError::MissingField {
                field: "cssProperties"
            }
        );
    }

    #[test]
    fn plug8_build_table_rejects_deep_nesting_and_escapes() {
        let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
        assert!(matches!(
            build(&deep),
            Err(ProfileLoadError::Syntax {
                message: "nesting too deep",
                ..
            })
        ));
        let esc = OK.replace("\"p\"", "\"p\\n\"");
        assert!(matches!(build(&esc), Err(ProfileLoadError::Syntax { .. })));
        let num = OK.replace("\"schemaVersion\":1", "\"schemaVersion\":1.5");
        assert!(build(&num).is_err());
    }

    #[test]
    fn plug8_build_table_rejects_too_many_entries() {
        let big = format!("[{}1]", "1,".repeat(5000));
        assert_eq!(build(&big).unwrap_err(), ProfileLoadError::TooManyEntries);
    }
}

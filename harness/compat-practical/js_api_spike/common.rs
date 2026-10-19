//! js_api_spike と page_script_probe が共有する純ロジック（引数検証・URL ポリシー・
//! 取得失敗の分類・例外分類・JSON 組み立て）。
//!
//! 呼び出し元: 両バイナリの `main.rs` が `#[path]` で取り込む。`spike.rs`（js_api_spike
//! 専用ロジック）とは分離し、probe が spike.rs 全体を取り込んで `dead_code` を
//! 抑止しなくて済むようにしてある（Issue #964・`MEAS-4`・`JS-5`）。
//! 両バイナリが使う項目だけをここに置く（片方専用の項目を足すと他方で dead_code になる）。
//!
//! 不変条件は `spike.rs` 冒頭と同じ（出力に URL・本文・生の例外メッセージを載せない）。

use fandhe_browser_core::Error;
use reqwest::Url;

/// 使用方法エラー（終了コード 2）。メッセージは英語。
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

pub fn usage<T>(msg: impl Into<String>) -> Result<T, UsageError> {
    Err(UsageError(msg.into()))
}

/// `--engine` の選択。固定の TOML 定数へ写すだけで、引数文字列は TOML に連結しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineChoice {
    V8,
    Boa,
}

impl EngineChoice {
    pub fn toml(self) -> &'static str {
        match self {
            EngineChoice::V8 => "[js]\nengine = \"v8\"\n",
            EngineChoice::Boa => "[js]\nengine = \"boa\"\n",
        }
    }
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`（tasks.json の `id` 規則）。
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    id.len() <= 64
        && first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// https のみ許可する URL 検証（空白・制御文字不可）。
pub fn parse_https_url(s: &str) -> Result<Url, UsageError> {
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return usage("URL must not contain whitespace or control characters");
    }
    let url = Url::parse(s).map_err(|_| UsageError("URL is not parseable".to_string()))?;
    if url.scheme() != "https" {
        return usage("only https URLs are allowed");
    }
    Ok(url)
}

/// `Fetcher` の失敗を写した取得失敗の分類（`fetch_error` フィールドの語彙の一部）。
/// HTTP ステータス・総時間上限は spike 専用で `spike::FetchClass` が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorClass {
    DisallowedScheme,
    DisallowedAddress,
    TooLarge,
    Timeout,
    Redirects,
    Network,
    Other,
}

impl FetchErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            FetchErrorClass::DisallowedScheme => "disallowed_scheme",
            FetchErrorClass::DisallowedAddress => "disallowed_address",
            FetchErrorClass::TooLarge => "too_large",
            FetchErrorClass::Timeout => "timeout",
            FetchErrorClass::Redirects => "redirects",
            FetchErrorClass::Network => "network",
            FetchErrorClass::Other => "other",
        }
    }
}

pub fn map_fetch_error(e: &Error) -> FetchErrorClass {
    match e {
        Error::DisallowedScheme { .. } => FetchErrorClass::DisallowedScheme,
        Error::DisallowedAddress { .. } => FetchErrorClass::DisallowedAddress,
        Error::ResponseTooLarge { .. } => FetchErrorClass::TooLarge,
        Error::Timeout { .. } => FetchErrorClass::Timeout,
        Error::TooManyRedirects { .. } => FetchErrorClass::Redirects,
        Error::Network { .. } => FetchErrorClass::Network,
        _ => FetchErrorClass::Other,
    }
}

/// scheme・host・port が同じか（external script の `same_origin` 用）。
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Classification {
    pub error_kind: Option<String>,
    pub message_class: &'static str,
    pub missing_api: Option<String>,
    /// 検証済みのプロパティ名（読み取り・設定・`in` 検索の対象。不足 API 名ではない。#1140）。
    pub property: Option<String>,
    /// 失敗ではない結果（スカラーに表現できない評価値）。
    pub non_scalar_result: bool,
}

/// `^[A-Za-z_$][A-Za-z0-9_$]{0,63}$` をドットでつないだパス（最大 128 文字・8 段）。
pub fn is_valid_api_path(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    let mut segs = 0;
    for seg in s.split('.') {
        segs += 1;
        let mut cs = seg.chars();
        let Some(first) = cs.next() else {
            return false;
        };
        if seg.len() > 64
            || !(first.is_ascii_alphabetic() || first == '_' || first == '$')
            || !cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        {
            return false;
        }
    }
    segs <= 8
}

const KINDS: [&str; 5] = [
    "ReferenceError",
    "TypeError",
    "SyntaxError",
    "RangeError",
    "Error",
];

fn api(s: &str) -> Option<String> {
    let s = s.trim();
    is_valid_api_path(s).then(|| s.to_string())
}

/// V8（`Uncaught ReferenceError: x is not defined (line N)`）と boa（接頭辞なしの
/// `x is not defined`）の両形式を扱う純関数。
pub fn classify(msg: &str) -> Classification {
    let mut body = msg.trim();
    let mut kind: Option<&str> = None;
    let stripped = body.strip_prefix("Uncaught ").unwrap_or(body);
    for k in KINDS {
        if let Some(rest) = stripped.strip_prefix(k).and_then(|r| r.strip_prefix(':')) {
            kind = Some(k);
            body = rest.trim();
            break;
        }
    }
    // V8 の末尾 ` (line N)` を除去する。
    if let Some(idx) = body.rfind(" (line ")
        && body.ends_with(')')
    {
        body = body.get(..idx).unwrap_or(body);
    }
    let mk = |k: Option<&str>, class: &'static str, missing: Option<String>| Classification {
        error_kind: k.map(str::to_string),
        message_class: class,
        missing_api: missing,
        property: None,
        non_scalar_result: false,
    };
    let with_prop = |mut c: Classification, tail: &str| {
        c.property = tail.split_once('\'').and_then(|(n, _)| api(n));
        c
    };
    if body.starts_with("evaluation result of type")
        && body.ends_with("is not representable as JsValue")
    {
        return Classification {
            error_kind: None,
            message_class: "non_scalar_result",
            missing_api: None,
            property: None,
            non_scalar_result: true,
        };
    }
    if let Some(name) = body.strip_suffix(" is not defined") {
        return mk(kind.or(Some("ReferenceError")), "not_defined", api(name));
    }
    if let Some(name) = body.strip_suffix(" is not a function") {
        return mk(kind, "not_a_function", api(name));
    }
    if let Some(name) = body.strip_suffix(" is not a constructor") {
        return mk(kind, "not_a_constructor", api(name));
    }
    if body.starts_with("Cannot read properties of") {
        let c = mk(kind, "read_property", None);
        return match body.split_once("(reading '") {
            Some((_, tail)) => with_prop(c, tail),
            None => c,
        };
    }
    if let Some(tail) = body.strip_prefix("Cannot use 'in' operator to search for '") {
        return with_prop(mk(kind, "cannot_use_in_operator", None), tail);
    }
    if body.starts_with("right-hand side of 'in' should be an object") {
        // boa は種類の接頭辞を付けないため TypeError と推定する（core と同じ規則）。
        return mk(kind.or(Some("TypeError")), "cannot_use_in_operator", None);
    }
    if body.starts_with("Cannot set properties of") {
        let c = mk(kind, "cannot_set_properties", None);
        return match body.split_once("(setting '") {
            Some((_, tail)) => with_prop(c, tail),
            None => c,
        };
    }
    mk(kind, "other", None)
}

/// stderr 専用（`--debug-messages`）。300 文字で切り詰め、制御文字を空白にする。
pub fn sanitize_debug(msg: &str) -> String {
    msg.chars()
        .take(300)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------- JSON

pub fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                o.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// 平らな JSON オブジェクトの組み立て（挿入順を保つ）。
#[derive(Default)]
pub struct Obj(Vec<(&'static str, String)>);

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn s(mut self, k: &'static str, v: &str) -> Self {
        self.0.push((k, json_escape(v)));
        self
    }
    pub fn n(mut self, k: &'static str, v: u64) -> Self {
        self.0.push((k, v.to_string()));
        self
    }
    pub fn opt_s(mut self, k: &'static str, v: Option<&str>) -> Self {
        self.0.push((k, v.map_or("null".to_string(), json_escape)));
        self
    }
    pub fn opt_n(mut self, k: &'static str, v: Option<u64>) -> Self {
        self.0
            .push((k, v.map_or("null".to_string(), |n| n.to_string())));
        self
    }
    pub fn raw(mut self, k: &'static str, v: String) -> Self {
        self.0.push((k, v));
        self
    }
    pub fn strs(self, k: &'static str, v: &[String]) -> Self {
        let items: Vec<String> = v.iter().map(|s| json_escape(s)).collect();
        self.raw(k, format!("[{}]", items.join(",")))
    }
    pub fn line(self) -> String {
        let fields: Vec<String> = self
            .0
            .into_iter()
            .map(|(k, v)| format!("{}:{}", json_escape(k), v))
            .collect();
        format!("{{{}}}", fields.join(","))
    }
}

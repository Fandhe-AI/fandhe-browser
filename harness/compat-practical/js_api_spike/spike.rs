//! js_api_spike の純ロジック（引数検証・URL ポリシー・script 抽出・例外分類・
//! JSON 書き出し・集計）。
//!
//! 呼び出し元: 同ディレクトリの `main.rs`（ネットワーク取得・JS 評価・出力の
//! 配線を担う）。本ファイルはネットワークにも JS エンジンにも触れない関数と、
//! `Fetcher` 経由の薄い取得関数 [`fetch_script`] だけを持ち、自己テストが
//! オフラインで決定的に走るようにしている。
//!
//! 役割: TASK-106・MS-6・`JS-5`（最小 DOM バインディングの対象 API 範囲を、
//! b5・d2・d3 の bundle が実際に要求する API の実測で確定する）。素の JS エンジンで
//! 文書順に評価し、script ごとの最初の未定義 API 名と例外分類を記録する。
//!
//! 守る不変条件:
//! - 取得は `fandhe_browser_core::Fetcher` 経由のみ。独自 HTTP クライアントは作らない
//!   （`reqwest::Url` は URL の解析・解決だけに使う。自己テストがソースを検査する）
//! - 出力（JSONL）に script の URL・本文・ページテキスト・ヘッダ・生の例外メッセージを
//!   載せない。URL はクエリにトークンを含み得るため同一オリジンかの真偽値だけ残す
//! - 外部入力（HTML・例外メッセージ・引数）の経路で `unwrap`/`expect`/添字を使わない
//!
//! 制約（REPAIR-3: 実測用の簡易実装であることを明示する）:
//! - 評価 1 回あたり 2 秒の上限は変更できない（`JsRuntime::execute` が評価オプションを
//!   取らない。#775・#776 が未完了）。setTimeout・イベントループは無い
//! - 文書の文字コードは `body_text_lossy`（UTF-8 lossy）固定
//! - 文書順の script 収集は #774（TASK-109）が core 側で正式化する予定で、本実装は
//!   harness 内に閉じた最小実装

use std::path::PathBuf;

use fandhe_browser_core::dom::Document;
use fandhe_browser_core::{Error, FetchResponse, Fetcher, JsEngineError};
use reqwest::Url;

/// js crate の 1 script あたり評価上限（`boa_engine.rs`/`v8_engine.rs` の
/// `MAX_SCRIPT_SOURCE_BYTES` = 1 MiB の写し。js 側は `pub(crate)` のため値を写している）。
pub const MAX_EVAL_BYTES: usize = 1_048_576;
/// 取得本文の上限。js の評価上限より大きくし、超過 script もバイト数だけ記録できるようにする。
pub const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// 引数の上限・既定値。
pub const DEFAULT_MAX_SCRIPTS: usize = 64;
pub const DEFAULT_FETCH_TIMEOUT_SEC: u64 = 15;
pub const DEFAULT_TOTAL_TIMEOUT_SEC: u64 = 600;

// ---------------------------------------------------------------- 引数

/// 使用方法エラー（終了コード 2）。メッセージは英語。
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

fn usage<T>(msg: impl Into<String>) -> Result<T, UsageError> {
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

#[derive(Debug)]
pub struct Args {
    pub targets: Vec<(String, Url)>,
    pub engine: Option<EngineChoice>,
    pub out: Option<PathBuf>,
    pub max_scripts: usize,
    pub fetch_timeout_sec: u64,
    pub total_timeout_sec: u64,
    pub debug_messages: bool,
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

fn parse_ranged<T: std::str::FromStr + PartialOrd + Copy>(
    flag: &str,
    value: Option<String>,
    lo: T,
    hi: T,
) -> Result<T, UsageError> {
    let Some(v) = value else {
        return usage(format!("{flag} requires a value"));
    };
    match v.parse::<T>() {
        Ok(n) if n >= lo && n <= hi => Ok(n),
        _ => usage(format!("{flag} is out of range or not a number")),
    }
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, UsageError> {
    let mut it = args.into_iter();
    let mut out = Args {
        targets: Vec::new(),
        engine: None,
        out: None,
        max_scripts: DEFAULT_MAX_SCRIPTS,
        fetch_timeout_sec: DEFAULT_FETCH_TIMEOUT_SEC,
        total_timeout_sec: DEFAULT_TOTAL_TIMEOUT_SEC,
        debug_messages: false,
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => {
                let Some(v) = it.next() else {
                    return usage("--target requires <id>=<url>");
                };
                let Some((id, url)) = v.split_once('=') else {
                    return usage("--target must look like <id>=<url>");
                };
                if !is_valid_id(id) {
                    return usage("target id must match ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$");
                }
                if out.targets.iter().any(|(existing, _)| existing == id) {
                    return usage("duplicate target id");
                }
                out.targets.push((id.to_string(), parse_https_url(url)?));
            }
            "--engine" => match it.next().as_deref() {
                Some("v8") => out.engine = Some(EngineChoice::V8),
                Some("boa") => out.engine = Some(EngineChoice::Boa),
                _ => return usage("--engine must be v8 or boa"),
            },
            "--out" => match it.next() {
                Some(p) if !p.is_empty() => out.out = Some(PathBuf::from(p)),
                _ => return usage("--out requires a path"),
            },
            "--max-scripts" => out.max_scripts = parse_ranged(&flag, it.next(), 1, 256)?,
            "--fetch-timeout" => out.fetch_timeout_sec = parse_ranged(&flag, it.next(), 1, 60)?,
            "--total-timeout" => out.total_timeout_sec = parse_ranged(&flag, it.next(), 1, 3600)?,
            "--debug-messages" => out.debug_messages = true,
            _ => return usage("unknown argument"),
        }
    }
    if out.targets.is_empty() {
        return usage("at least one --target <id>=<url> is required");
    }
    Ok(out)
}

// ------------------------------------------------------- 取得ポリシー

/// 取得失敗の分類（`fetch_error` フィールドの語彙）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorClass {
    DisallowedScheme,
    DisallowedAddress,
    HttpStatus,
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
            FetchErrorClass::HttpStatus => "http_status",
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

/// harness 側の事前検査（1 層目）。`src` を `base`（ページの最終 URL）基準で解決し、
/// https 以外（`file:`・`data:`・`javascript:`・`http:` 等）は取得前に拒否する。
/// 内部アドレスの拒否は `Fetcher`（2 層目）が担う。
pub fn resolve_script_url(base: &Url, src: &str) -> Result<Url, FetchErrorClass> {
    let url = base.join(src.trim()).map_err(|_| FetchErrorClass::Other)?;
    if url.scheme() != "https" {
        return Err(FetchErrorClass::DisallowedScheme);
    }
    Ok(url)
}

/// `Fetcher` 経由の取得（独自 HTTP クライアントは作らない）。
/// 4xx/5xx は `Fetcher` が `Ok` で返すため、ここで `HttpStatus` に落とす。
pub async fn fetch_script(fetcher: &Fetcher, url: &str) -> Result<FetchResponse, FetchErrorClass> {
    let resp = fetcher.get(url).await.map_err(|e| map_fetch_error(&e))?;
    if !(200..300).contains(&resp.status()) {
        return Err(FetchErrorClass::HttpStatus);
    }
    Ok(resp)
}

/// scheme・host・port が同じか（external script の `same_origin` 用）。
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

// ------------------------------------------------------- script 抽出

#[derive(Debug, PartialEq, Eq)]
pub enum ScriptSource {
    Inline(String),
    /// `src` 属性の値（出力には載せない）。
    External(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ScriptEntry {
    pub order: usize,
    pub source: ScriptSource,
    /// 評価しない理由（`module`/`nomodule`/`data_block`）。`None` なら classic。
    pub skip: Option<&'static str>,
}

fn is_js_mime(t: &str) -> bool {
    matches!(
        t.trim().to_ascii_lowercase().as_str(),
        "" | "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "application/ecmascript"
            | "text/jscript"
            | "text/livescript"
    )
}

fn skip_reason(type_attr: Option<&str>, nomodule: bool) -> Option<&'static str> {
    if let Some(t) = type_attr
        && t.trim().eq_ignore_ascii_case("module")
    {
        return Some("module");
    }
    if nomodule {
        return Some("nomodule");
    }
    match type_attr {
        Some(t) if !is_js_mime(t) => Some("data_block"),
        _ => None,
    }
}

/// 文書順に HTML 名前空間の `<script>` を拾う。`descendants` は template contents を
/// 含まないので `<template>` 内は除外される。戻り値の 2 つ目は `max` 超過で捨てた件数。
pub fn extract_scripts(doc: &Document, max: usize) -> (Vec<ScriptEntry>, usize) {
    let mut entries = Vec::new();
    let mut over = 0usize;
    for id in doc.descendants(doc.root()) {
        if doc.local_name(id) != Some("script")
            || doc.namespace_url(id) != Some("http://www.w3.org/1999/xhtml")
        {
            continue;
        }
        if entries.len() >= max {
            over = over.saturating_add(1);
            continue;
        }
        let skip = skip_reason(
            doc.attribute(id, "type"),
            doc.attribute(id, "nomodule").is_some(),
        );
        let source = match doc.attribute(id, "src") {
            Some(src) => ScriptSource::External(src.to_string()),
            None => ScriptSource::Inline(doc.text_content(id).unwrap_or_default()),
        };
        entries.push(ScriptEntry {
            order: entries.len(),
            source,
            skip,
        });
    }
    (entries, over)
}

// ------------------------------------------------------------ 例外分類

#[derive(Debug, PartialEq, Eq)]
pub struct Classification {
    pub error_kind: Option<String>,
    pub message_class: &'static str,
    pub missing_api: Option<String>,
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
        non_scalar_result: false,
    };
    if body.starts_with("evaluation result of type")
        && body.ends_with("is not representable as JsValue")
    {
        return Classification {
            error_kind: None,
            message_class: "non_scalar_result",
            missing_api: None,
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
        return mk(kind, "read_property", None);
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

// --------------------------------------------------------------- 記録

/// script 1 件の記録。URL・本文・生メッセージは持たない。
#[derive(Default)]
pub struct ScriptRecord {
    pub id: String,
    pub order: usize,
    pub external: bool,
    pub same_origin: Option<bool>,
    pub bytes: u64,
    pub elapsed_ms: u64,
    pub outcome: &'static str,
    pub skip_reason: Option<&'static str>,
    pub error_kind: Option<String>,
    pub message_class: Option<&'static str>,
    pub missing_api: Option<String>,
    pub fetch_error: Option<&'static str>,
    pub http_status: Option<u16>,
}

impl ScriptRecord {
    pub fn to_json_line(&self) -> String {
        Obj::new()
            .s("type", "script")
            .s("id", &self.id)
            .n("order", self.order as u64)
            .s("source", if self.external { "external" } else { "inline" })
            .raw(
                "same_origin",
                self.same_origin
                    .map_or("null".to_string(), |b| b.to_string()),
            )
            .n("bytes", self.bytes)
            .n("elapsed_ms", self.elapsed_ms)
            .s("outcome", self.outcome)
            .opt_s("skip_reason", self.skip_reason)
            .opt_s("error_kind", self.error_kind.as_deref())
            .opt_s("message_class", self.message_class)
            .opt_s("missing_api", self.missing_api.as_deref())
            .opt_s("fetch_error", self.fetch_error)
            .opt_n("http_status", self.http_status.map(u64::from))
            .line()
    }
}

/// サイト単位の集計。
#[derive(Default)]
pub struct SiteRecord {
    pub id: String,
    pub page_status: Option<u16>,
    pub page_error: Option<&'static str>,
    pub scripts_total: u64,
    pub scripts_evaluated: u64,
    pub scripts_failed: u64,
    pub scripts_skipped: u64,
    pub total_script_bytes: u64,
    pub max_script_bytes: u64,
    pub missing_apis: Vec<String>,
    pub aborted: Option<&'static str>,
    pub elapsed_ms: u64,
}

impl SiteRecord {
    /// script 記録を集計へ反映する（`missing_apis` は初出順で重複除去）。
    pub fn absorb(&mut self, r: &ScriptRecord) {
        self.scripts_total = self.scripts_total.saturating_add(1);
        match r.outcome {
            "skipped" => self.scripts_skipped = self.scripts_skipped.saturating_add(1),
            "ok" | "ok_non_scalar_result" => {
                self.scripts_evaluated = self.scripts_evaluated.saturating_add(1)
            }
            _ => self.scripts_failed = self.scripts_failed.saturating_add(1),
        }
        self.total_script_bytes = self.total_script_bytes.saturating_add(r.bytes);
        self.max_script_bytes = self.max_script_bytes.max(r.bytes);
        if let Some(a) = &r.missing_api
            && !self.missing_apis.contains(a)
        {
            self.missing_apis.push(a.clone());
        }
    }

    pub fn to_json_line(&self) -> String {
        Obj::new()
            .s("type", "site")
            .s("id", &self.id)
            .opt_n("page_status", self.page_status.map(u64::from))
            .opt_s("page_error", self.page_error)
            .n("scripts_total", self.scripts_total)
            .n("scripts_evaluated", self.scripts_evaluated)
            .n("scripts_failed", self.scripts_failed)
            .n("scripts_skipped", self.scripts_skipped)
            .n("total_script_bytes", self.total_script_bytes)
            .n("max_script_bytes", self.max_script_bytes)
            .opt_s(
                "first_missing_api",
                self.missing_apis.first().map(String::as_str),
            )
            .strs("missing_apis", &self.missing_apis)
            .opt_s("aborted", self.aborted)
            .n("elapsed_ms", self.elapsed_ms)
            .line()
    }
}

pub fn meta_line(args: &Args, engine: &str, measured_at_unix: u64) -> String {
    let limits = Obj::new()
        .n("max_scripts", args.max_scripts as u64)
        .n("max_body_bytes", MAX_BODY_BYTES)
        .n("max_eval_bytes", MAX_EVAL_BYTES as u64)
        .n("fetch_timeout_sec", args.fetch_timeout_sec)
        .n("total_timeout_sec", args.total_timeout_sec)
        .line();
    Obj::new()
        .s("type", "meta")
        .n("schema_version", 1)
        .n("measured_at_unix", measured_at_unix)
        .s("engine", engine)
        .s("os", std::env::consts::OS)
        .raw("limits", limits)
        .s("dom_bindings", "none")
        .line()
}

/// 評価エラーから (outcome, 分類, context 破棄で打ち切るか) を決める。
pub fn outcome_of_error(e: &Error) -> (&'static str, Option<Classification>, bool) {
    match e {
        Error::JsEvaluation(je) => match je {
            JsEngineError::Timeout(m) => ("timeout", None, m.contains("context was discarded")),
            JsEngineError::ResourceLimitExceeded(m) => {
                ("resource_limit", None, m.contains("context was discarded"))
            }
            JsEngineError::EngineUnavailable(m) => (
                "engine_unavailable",
                None,
                m.contains("context was discarded"),
            ),
            JsEngineError::EvaluationFailed(m) => {
                let c = classify(m);
                if c.non_scalar_result {
                    ("ok_non_scalar_result", Some(c), false)
                } else {
                    ("exception", Some(c), false)
                }
            }
            other => {
                let c = classify(&other.to_string());
                ("exception", Some(c), false)
            }
        },
        _ => ("engine_unavailable", None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fandhe_browser_core::{FetchOptions, ParseOptions, parse_document};

    fn fetcher() -> Fetcher {
        Fetcher::new(FetchOptions::new()).expect("fetcher")
    }

    fn base() -> Url {
        Url::parse("https://example.test/app/").expect("url")
    }

    #[test]
    fn url_policy_rejects_non_https_before_fetch() {
        for src in [
            "file:///etc/passwd",
            "data:text/javascript,1",
            "javascript:1",
            "http://example.com/a.js",
            "file:///x.js",
        ] {
            assert_eq!(
                resolve_script_url(&base(), src).unwrap_err(),
                FetchErrorClass::DisallowedScheme,
                "{src}"
            );
        }
        let ok = resolve_script_url(&base(), "//cdn.example.test/a.js").expect("ok");
        assert_eq!(ok.as_str(), "https://cdn.example.test/a.js");
        let rel = resolve_script_url(&base(), "x/y.js").expect("ok");
        assert_eq!(rel.as_str(), "https://example.test/app/x/y.js");
    }

    #[tokio::test]
    async fn fetcher_rejects_scheme_and_internal_addresses_offline() {
        let f = fetcher();
        assert!(matches!(
            f.get("file:///etc/passwd").await,
            Err(Error::DisallowedScheme { .. })
        ));
        for u in [
            "https://127.0.0.1/a.js",
            "https://10.0.0.1/a.js",
            "https://169.254.169.254/latest",
            "https://[::1]/a.js",
        ] {
            assert!(
                matches!(f.validate_url(u), Err(Error::DisallowedAddress { .. })),
                "{u}"
            );
            assert!(
                matches!(f.get(u).await, Err(Error::DisallowedAddress { .. })),
                "{u}"
            );
            assert_eq!(
                fetch_script(&f, u).await.unwrap_err(),
                FetchErrorClass::DisallowedAddress,
                "{u}"
            );
        }
        let resolved = resolve_script_url(&base(), "//192.168.0.1/x.js").expect("https");
        assert!(matches!(
            f.validate_url(resolved.as_str()),
            Err(Error::DisallowedAddress { .. })
        ));
        assert_eq!(
            fetch_script(&f, "file:///etc/passwd").await.unwrap_err(),
            FetchErrorClass::DisallowedScheme
        );
    }

    #[test]
    fn sources_do_not_build_their_own_http_client() {
        // 検査対象の語は分割して書き、このテスト自身に一致しないようにする。
        let needles = [
            ["reqwest", "::Client"].concat(),
            ["reqwest", "::get"].concat(),
            ["Client", "::new"].concat(),
            ["Client", "Builder"].concat(),
        ];
        for (name, src) in [
            ("spike.rs", include_str!("spike.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            for n in &needles {
                // 本検査の定義行（needles の連結部）は分割されているため一致しない。
                assert!(!src.contains(n.as_str()), "{name} contains {n}");
            }
        }
    }

    fn doc(html: &str) -> Document {
        parse_document(html, &ParseOptions::default().with_scripting_enabled(true))
            .expect("parse")
            .document
    }

    #[test]
    fn extract_orders_and_classifies_scripts() {
        let html = r#"<html><head>
<script>var a=1; /* MARKER_BODY */</script>
<script src="/a.js"></script>
<script type="module">1</script>
<script nomodule>1</script>
<script type="application/json">{}</script>
<script type="importmap">{}</script>
<script type="TEXT/JavaScript">2</script>
<template><script>3</script></template>
</head><body></body></html>"#;
        let (e, over) = extract_scripts(&doc(html), 100);
        assert_eq!(over, 0);
        assert_eq!(e.len(), 7);
        assert!(matches!(&e[0].source, ScriptSource::Inline(s) if s.contains("MARKER_BODY")));
        assert_eq!(e[0].skip, None);
        assert_eq!(e[1].source, ScriptSource::External("/a.js".to_string()));
        assert_eq!(e[2].skip, Some("module"));
        assert_eq!(e[3].skip, Some("nomodule"));
        assert_eq!(e[4].skip, Some("data_block"));
        assert_eq!(e[5].skip, Some("data_block"));
        assert_eq!(e[6].skip, None);
        let orders: Vec<usize> = e.iter().map(|x| x.order).collect();
        assert_eq!(orders, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn extract_respects_max_scripts() {
        let html = "<script>1</script><script>2</script><script>3</script>";
        let (e, over) = extract_scripts(&doc(html), 2);
        assert_eq!((e.len(), over), (2, 1));
    }

    #[test]
    fn classify_handles_v8_and_boa_forms() {
        let want = |c: Classification, k: Option<&str>, m: &str, a: Option<&str>| {
            assert_eq!(c.error_kind.as_deref(), k);
            assert_eq!(c.message_class, m);
            assert_eq!(c.missing_api.as_deref(), a);
        };
        want(
            classify("Uncaught ReferenceError: document is not defined (line 1)"),
            Some("ReferenceError"),
            "not_defined",
            Some("document"),
        );
        want(
            classify("document is not defined"),
            Some("ReferenceError"),
            "not_defined",
            Some("document"),
        );
        want(
            classify("Uncaught TypeError: window.foo is not a function (line 3)"),
            Some("TypeError"),
            "not_a_function",
            Some("window.foo"),
        );
        want(
            classify("TypeError: Cannot read properties of undefined (reading 'x')"),
            Some("TypeError"),
            "read_property",
            None,
        );
        let ns = classify("evaluation result of type 'object' is not representable as JsValue");
        assert!(ns.non_scalar_result);
        for bad in [
            "\"};{ is not defined".to_string(),
            "a\u{1}b is not defined".to_string(),
            format!("{} is not defined", "a".repeat(129)),
            "a..b is not defined".to_string(),
        ] {
            assert_eq!(classify(&bad).missing_api, None, "{bad:?}");
        }
    }

    #[test]
    fn json_escape_covers_specials() {
        assert_eq!(json_escape("a\"b\\c\n\u{1}é"), "\"a\\\"b\\\\c\\n\\u0001é\"");
    }

    #[test]
    fn output_lines_never_contain_script_text() {
        let mut site = SiteRecord {
            id: "t".into(),
            ..Default::default()
        };
        let rec = ScriptRecord {
            id: "t".into(),
            outcome: "exception",
            missing_api: Some("document".into()),
            ..Default::default()
        };
        site.absorb(&rec);
        let all = format!("{}\n{}", rec.to_json_line(), site.to_json_line());
        assert!(!all.contains("MARKER_BODY"));
        assert!(all.contains("\"missing_api\":\"document\""));
        assert!(all.contains("\"first_missing_api\":\"document\""));
    }

    #[test]
    fn args_validation() {
        let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert!(a(&[]).is_err());
        assert!(a(&["--target", "noequals"]).is_err());
        assert!(a(&["--target", "-bad=https://x.test/"]).is_err());
        assert!(a(&["--target", "x=http://x.test/"]).is_err());
        assert!(a(&["--target", "x=file:///etc/passwd"]).is_err());
        assert!(
            a(&[
                "--target",
                "x=https://x.test/",
                "--target",
                "x=https://y.test/"
            ])
            .is_err()
        );
        assert!(a(&["--target", "x=https://x.test/", "--max-scripts", "0"]).is_err());
        assert!(a(&["--target", "x=https://x.test/", "--fetch-timeout", "61"]).is_err());
        assert!(a(&["--target", "x=https://x.test/", "--engine", "node"]).is_err());
        let ok = a(&[
            "--target",
            "x=https://x.test/",
            "--engine",
            "boa",
            "--max-scripts",
            "256",
        ])
        .expect("ok");
        assert_eq!(ok.max_scripts, 256);
        assert_eq!(ok.engine, Some(EngineChoice::Boa));
    }

    #[test]
    fn site_aggregation_dedups_in_first_seen_order() {
        let mut s = SiteRecord::default();
        for api in ["b", "a", "b", "c"] {
            s.absorb(&ScriptRecord {
                outcome: "exception",
                missing_api: Some(api.into()),
                ..Default::default()
            });
        }
        s.absorb(&ScriptRecord {
            outcome: "ok",
            bytes: 5,
            ..Default::default()
        });
        assert_eq!(s.missing_apis, vec!["b", "a", "c"]);
        assert_eq!(
            (s.scripts_total, s.scripts_failed, s.scripts_evaluated),
            (5, 4, 1)
        );
    }
}

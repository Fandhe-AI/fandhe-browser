//! 要求フレームの検証（封筒・メソッド別 params・URL・セレクタ）。
//!
//! `server` のループから呼ばれ、JSON のバイト列を「応答を返せる要求」か「応答なしで接続を閉じる
//! 違反」かに分類する。契約の正本は `docs/design/render-host-protocol.schema.json` と
//! `crates/fandhe-browser-render/tests/fixtures/protocol/`（#986）で、本体側とは型を共有せず、
//! `serde` の derive も使わずに `serde_json::Value` を手で検査する（依存最小。`PLUG-1`・`SEC-2`・`REPAIR-4`）。
//! 外部入力の経路のため `unwrap`・`expect`・添字アクセスを使わない。

use std::time::Duration;

use serde_json::{Map, Value};

use super::{
    ErrorKind, LOAD_TIMEOUT_DEFAULT_MS, LOAD_TIMEOUT_MAX_MS, MAX_METHOD_BYTES, MAX_REQUEST_ID,
    MAX_SELECTOR_BYTES, MAX_URL_BYTES, Method,
};

/// `load` のパラメータ。UA・ヘッダ等の識別面を持たない（`SEC-2`）。
pub(crate) struct LoadParams {
    /// 検証済みの http / https URL（`ServoUrl` での再解析は #231 のハンドラ側）。
    // Handler 実装（#231）が読むまで未使用。
    #[allow(dead_code)]
    pub url: String,
    /// 読み込みの待機上限（省略時は既定 30 秒を適用済み）。
    // Handler 実装（#231）が読むまで未使用。
    #[allow(dead_code)]
    pub timeout: Duration,
}

/// `screenshot` のパラメータ。ビューポートは固定で指定できない（`SEC-2`）。
pub(crate) struct ScreenshotParams {
    // Handler 実装（#232）が読むまで未使用。
    #[allow(dead_code)]
    pub full_page: bool,
}

/// `visibility`・`bounding_box` のパラメータ。セレクタは長さだけ検証した値で、JS 評価文字列へ
/// 埋め込む場合は #233・#234 側で JSON エスケープを必須とする。
pub(crate) struct SelectorParams {
    // Handler 実装（#233・#234）が読むまで未使用。
    #[allow(dead_code)]
    pub selector: String,
}

/// 検証済みの要求。
pub(crate) enum Request {
    Load(LoadParams),
    Screenshot(ScreenshotParams),
    Visibility(SelectorParams),
    BoundingBox(SelectorParams),
    Shutdown,
}

/// 分類結果。`id` を導出できた要求だけが得られ、検証失敗は `request` 側の `ErrorKind` で返す。
pub(crate) struct Parsed {
    pub id: u64,
    /// ログ用のメソッド名。未知メソッドは攻撃者制御の文字列を出さず `"unknown"` にする。
    pub label: &'static str,
    pub request: Result<Request, ErrorKind>,
}

/// 要求フレームを分類する。`Err` はプロトコル違反（応答なしで終了。固定の英語理由）。
pub(crate) fn parse_request(bytes: &[u8]) -> Result<Parsed, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "request is not valid UTF-8")?;
    let value: Value = serde_json::from_str(text).map_err(|_| "request is not valid JSON")?;
    let obj = value.as_object().ok_or("request is not a JSON object")?;
    let id = obj
        .get("id")
        .and_then(|v| as_uint(v, MAX_REQUEST_ID))
        .ok_or("request id is missing or invalid")?;
    let (label, request) = classify(obj);
    Ok(Parsed { id, label, request })
}

fn classify(obj: &Map<String, Value>) -> (&'static str, Result<Request, ErrorKind>) {
    let bad = |k| ("unknown", Err(k));
    if !has_only_keys(obj, &["v", "id", "method", "params"]) {
        return bad(ErrorKind::InvalidRequest);
    }
    if obj.get("v").and_then(|v| as_uint(v, 1)) != Some(1) {
        return bad(ErrorKind::InvalidRequest);
    }
    let Some(name) = obj.get("method").and_then(Value::as_str) else {
        return bad(ErrorKind::InvalidRequest);
    };
    if name.is_empty() || name.len() > MAX_METHOD_BYTES {
        return bad(ErrorKind::InvalidRequest);
    }
    let empty = Map::new();
    let params = match obj.get("params") {
        None => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return bad(ErrorKind::InvalidRequest),
    };
    // 未知メソッドは params を見ずに unsupported（成功を装わない。SEC-2）。
    let Some(method) = Method::from_name(name) else {
        return bad(ErrorKind::Unsupported);
    };
    (method.as_str(), parse_params(method, params))
}

fn parse_params(method: Method, p: &Map<String, Value>) -> Result<Request, ErrorKind> {
    let invalid = ErrorKind::InvalidParams;
    match method {
        Method::Load => {
            if !has_only_keys(p, &["url", "timeout_ms"]) {
                return Err(invalid);
            }
            let url = p.get("url").and_then(Value::as_str).ok_or(invalid)?;
            if !is_valid_url(url) {
                return Err(invalid);
            }
            let ms = match p.get("timeout_ms") {
                None => LOAD_TIMEOUT_DEFAULT_MS,
                Some(v) => as_uint(v, LOAD_TIMEOUT_MAX_MS)
                    .filter(|ms| *ms >= 1)
                    .ok_or(invalid)?,
            };
            Ok(Request::Load(LoadParams {
                url: url.to_owned(),
                timeout: Duration::from_millis(ms),
            }))
        }
        Method::Screenshot => {
            if !has_only_keys(p, &["full_page"]) {
                return Err(invalid);
            }
            let full_page = match p.get("full_page") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(invalid),
            };
            Ok(Request::Screenshot(ScreenshotParams { full_page }))
        }
        Method::Visibility | Method::BoundingBox => {
            if !has_only_keys(p, &["selector"]) {
                return Err(invalid);
            }
            let selector = p.get("selector").and_then(Value::as_str).ok_or(invalid)?;
            if selector.is_empty() || selector.len() > MAX_SELECTOR_BYTES {
                return Err(invalid);
            }
            let params = SelectorParams {
                selector: selector.to_owned(),
            };
            Ok(if method == Method::Visibility {
                Request::Visibility(params)
            } else {
                Request::BoundingBox(params)
            })
        }
        Method::Shutdown => {
            if p.is_empty() {
                Ok(Request::Shutdown)
            } else {
                Err(invalid)
            }
        }
    }
}

/// すべてのキーが許可リストに含まれるか（未知キーは識別面を含めて拒否する。fail-closed）。
fn has_only_keys(obj: &Map<String, Value>, allowed: &[&str]) -> bool {
    obj.keys().all(|k| allowed.contains(&k.as_str()))
}

/// JSON Schema の整数の意味論（`1e0`・`1.0` も整数）で 0 以上 `max` 以下の整数として取り出す。
fn as_uint(v: &Value, max: u64) -> Option<u64> {
    let n = if let Some(u) = v.as_u64() {
        u
    } else {
        let f = v.as_f64()?;
        // 2^53 - 1 以下は f64 で厳密に表せる。負数・小数部あり・非有限は除く。
        if !f.is_finite() || f < 0.0 || f.fract() != 0.0 || f > MAX_REQUEST_ID as f64 {
            return None;
        }
        f as u64
    };
    (n <= max).then_some(n)
}

/// 本体側 `validate_url` と同じ規則（http / https のみ・userinfo 拒否・host 非空・port は数字）。
///
/// 残存リスク: プライベート IP の扱いは本体側（#235）、Servo 内部のリダイレクトと
/// サブリソースは #1003（判断待ち）で、ここでは検証しない（SSRF）。
fn is_valid_url(url: &str) -> bool {
    if url.is_empty() || url.len() > MAX_URL_BYTES {
        return false;
    }
    if url
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\\')
    {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let Some((host, tail)) = inner.split_once(']') else {
            return false;
        };
        match tail {
            "" => (host, None),
            t => match t.strip_prefix(':') {
                Some(p) => (host, Some(p)),
                None => return false,
            },
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    !host.is_empty() && port.is_none_or(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

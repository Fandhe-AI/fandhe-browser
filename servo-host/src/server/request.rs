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
    ErrorKind, LOAD_TIMEOUT_DEFAULT_MS, LOAD_TIMEOUT_MAX_MS, MAX_EXPECT_VALUE_BYTES,
    MAX_FILL_TEXT_BYTES, MAX_KEY_BYTES, MAX_METHOD_BYTES, MAX_NODE, MAX_REQUEST_ID,
    MAX_SELECT_VALUE_BYTES, MAX_SELECT_VALUES, MAX_SELECTOR_BYTES, MAX_SNAPSHOT_ID_BYTES,
    MAX_URL_BYTES, MAX_WAIT_CONDITION_BYTES, Method, PageInfo, WAIT_TIMEOUT_MAX_MS,
};

/// `load` のパラメータ。UA・ヘッダ等の識別面を持たない（`SEC-2`）。
pub(crate) struct LoadParams {
    /// 検証済みの http / https URL（`page` のハンドラが `ServoUrl` で再解析・再検証する）。
    pub url: String,
    /// 読み込みの待機上限（省略時は既定 30 秒を適用済み）。
    pub timeout: Duration,
}

/// `screenshot` のパラメータ。ビューポートは固定で指定できない（`SEC-2`）。
pub(crate) struct ScreenshotParams {
    pub full_page: bool,
}

/// `visibility`・`bounding_box` のパラメータ。セレクタは長さだけ検証した値で、JS 評価文字列へ
/// 埋め込む場合は `layout` が検証・JSON エスケープする。
pub(crate) struct SelectorParams {
    pub selector: String,
}

/// `act` の照合値（設計 §14.4）。`tag` は必須、他は送られたものだけを照合する。値は自己申告として扱う。
pub(crate) struct ActExpect {
    pub tag: String,
    pub id: Option<String>,
    pub name: Option<String>,
    pub r#type: Option<String>,
}

/// `act` の操作。入力文字・キーはログ・エラー・応答に出さない（設計 §14.4）。
pub(crate) enum Action {
    Click,
    /// `sensitive` はホストでは使わない（第 1 層の伏せる対象は `dom_snapshot` が決める。本体が世代の間覚える）。
    Fill {
        text: String,
    },
    Select {
        values: Vec<String>,
    },
    Press {
        key: String,
    },
}

/// `act` のパラメータ（設計 §14.4）。`node` は `dom_snapshot` の番号。
pub(crate) struct ActParams {
    pub page: PageInfo,
    pub snapshot: String,
    pub node: usize,
    pub expect: ActExpect,
    pub action: Action,
}

/// `wait` の条件（設計 §14.5）。文字列は長さだけ検証した値で、JSON の文字列リテラルにしてからスクリプトへ渡す。
pub(crate) enum WaitCondition {
    SelectorPresent(String),
    SelectorAbsent(String),
    TextPresent(String),
    LoadComplete,
}

/// `wait` のパラメータ（設計 §14.5）。
pub(crate) struct WaitParams {
    pub page: PageInfo,
    pub condition: WaitCondition,
    pub timeout: Duration,
}

/// 検証済みの要求。
pub(crate) enum Request {
    Load(LoadParams),
    Screenshot(ScreenshotParams),
    Visibility(SelectorParams),
    BoundingBox(SelectorParams),
    /// params を持たない（`page` を照合しない。設計 §14.2・§14.3）。
    DomSnapshot,
    Act(ActParams),
    Wait(WaitParams),
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
        Method::Act => parse_act(p).map(Request::Act).ok_or(invalid),
        Method::Wait => parse_wait(p).map(Request::Wait).ok_or(invalid),
        Method::DomSnapshot | Method::Shutdown => {
            if !p.is_empty() {
                return Err(invalid);
            }
            Ok(if method == Method::DomSnapshot {
                Request::DomSnapshot
            } else {
                Request::Shutdown
            })
        }
    }
}

/// 文字数（JSON Schema の `maxLength` と同じく Unicode のスカラー値で数える）が `min..=max` の文字列。
fn str_in(v: Option<&Value>, min: usize, max: usize) -> Option<&str> {
    let s = v?.as_str()?;
    let n = s.chars().count();
    (min..=max).contains(&n).then_some(s)
}

/// `PageRef`（`session` は 1〜64 文字、`doc_seq` は整数。未知キーは拒否）。
fn parse_page(v: Option<&Value>) -> Option<PageInfo> {
    let m = v?.as_object()?;
    if !has_only_keys(m, &["session", "doc_seq"]) {
        return None;
    }
    Some(PageInfo {
        session: str_in(m.get("session"), 1, 64)?.to_owned(),
        doc_seq: as_uint(m.get("doc_seq")?, MAX_REQUEST_ID)?,
    })
}

fn parse_act(p: &Map<String, Value>) -> Option<ActParams> {
    if !has_only_keys(p, &["page", "snapshot", "node", "expect", "action"]) {
        return None;
    }
    let page = parse_page(p.get("page"))?;
    let snapshot = str_in(p.get("snapshot"), 1, MAX_SNAPSHOT_ID_BYTES)?.to_owned();
    let node = usize::try_from(as_uint(p.get("node")?, MAX_NODE)?).ok()?;
    let e = p.get("expect")?.as_object()?;
    if !has_only_keys(e, &["tag", "id", "name", "type"]) {
        return None;
    }
    let opt = |k: &str| -> Option<Option<String>> {
        match e.get(k) {
            None => Some(None),
            v => str_in(v, 0, MAX_EXPECT_VALUE_BYTES).map(|s| Some(s.to_owned())),
        }
    };
    let expect = ActExpect {
        tag: str_in(e.get("tag"), 1, 64)?.to_owned(),
        id: opt("id")?,
        name: opt("name")?,
        r#type: opt("type")?,
    };
    let a = p.get("action")?.as_object()?;
    let action = match a.get("kind")?.as_str()? {
        "click" if has_only_keys(a, &["kind"]) => Action::Click,
        "fill" if has_only_keys(a, &["kind", "text", "sensitive"]) => {
            if !matches!(a.get("sensitive"), None | Some(Value::Bool(_))) {
                return None;
            }
            Action::Fill {
                text: str_in(a.get("text"), 0, MAX_FILL_TEXT_BYTES)?.to_owned(),
            }
        }
        "select" if has_only_keys(a, &["kind", "values"]) => {
            let vs = a.get("values")?.as_array()?;
            if vs.is_empty() || vs.len() > MAX_SELECT_VALUES {
                return None;
            }
            let values = vs
                .iter()
                .map(|v| str_in(Some(v), 0, MAX_SELECT_VALUE_BYTES).map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            Action::Select { values }
        }
        "press" if has_only_keys(a, &["kind", "key"]) => Action::Press {
            key: str_in(a.get("key"), 1, MAX_KEY_BYTES)?.to_owned(),
        },
        _ => return None,
    };
    Some(ActParams {
        page,
        snapshot,
        node,
        expect,
        action,
    })
}

fn parse_wait(p: &Map<String, Value>) -> Option<WaitParams> {
    if !has_only_keys(p, &["page", "condition", "timeout_ms"]) {
        return None;
    }
    let page = parse_page(p.get("page"))?;
    let ms = as_uint(p.get("timeout_ms")?, WAIT_TIMEOUT_MAX_MS).filter(|ms| *ms >= 1)?;
    let c = p.get("condition")?.as_object()?;
    let text = |k: &str| str_in(c.get(k), 1, MAX_WAIT_CONDITION_BYTES).map(str::to_owned);
    let condition = match c.get("kind")?.as_str()? {
        "selector_present" if has_only_keys(c, &["kind", "selector"]) => {
            WaitCondition::SelectorPresent(text("selector")?)
        }
        "selector_absent" if has_only_keys(c, &["kind", "selector"]) => {
            WaitCondition::SelectorAbsent(text("selector")?)
        }
        "text_present" if has_only_keys(c, &["kind", "text"]) => {
            WaitCondition::TextPresent(text("text")?)
        }
        "load_complete" if has_only_keys(c, &["kind"]) => WaitCondition::LoadComplete,
        _ => return None,
    };
    Some(WaitParams {
        page,
        condition,
        timeout: Duration::from_millis(ms),
    })
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
pub(crate) fn is_valid_url(url: &str) -> bool {
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

//! レンダリングホスト（Servo ホスト）との stdio プロトコルのクライアント側型。
//!
//! 契約の正本は `docs/design/render-host-protocol.schema.json` とゴールデン fixture
//! （`tests/fixtures/protocol/`）で、本モジュールはその Rust 表現である。ホスト側は別
//! workspace で型を共有しないため、両者の一致は契約テストで保証する
//! （RENDER-3・PLUG-1・REPAIR-4・SEC-2。TASK-38・#989）。
//!
//! - 要求（[`Request`]）は検証付きコンストラクタ経由でのみ組み立て、[`Request::to_json`] で
//!   `serde_json::Value` にする。JSON を文字列連結で組まない（インジェクション対策）
//! - 応答（[`Response`]・[`Hello`]）はホスト由来の untrusted 入力として、スキーマ相当の検証を
//!   fail-closed で行う。未知フィールドは無視する（REPAIR-4）
//! - UA・ヘッダ・Cookie・locale・タイムゾーン・TLS・proxy・ビューポート等の識別面を
//!   変えるパラメータの型は置かない（SEC-2。`docs/design/anti-bot-review.md`）
//!
//! serde（derive）はルート Cargo.toml の配置方針で core 限定のため使わず、`Value` から
//! 手作業で解釈する。フレームの読み書きは [`crate::codec`] が担当する。

use std::fmt;

use serde_json::{Map, Value, json};

/// 本クライアントが話すプロトコル版（`v`）。
pub const PROTOCOL_VERSION: u32 = 1;
/// URL の上限（UTF-8 バイト数）。
pub const MAX_URL_BYTES: usize = 2048;
/// セレクタの上限（UTF-8 バイト数）。
pub const MAX_SELECTOR_BYTES: usize = 1024;
/// エラー message の上限（UTF-8 バイト数）。
pub const MAX_ERROR_MESSAGE_BYTES: usize = 512;
/// `load` の `timeout_ms` の上限。
pub const MAX_LOAD_TIMEOUT_MS: u32 = 60_000;
/// 要求 ID の最大値（2^53-1。JSON 数値として安全に往復できる整数の上限）。
pub const MAX_REQUEST_ID: u64 = (1 << 53) - 1;

const MAX_VERSION_BYTES: usize = 64;
const MAX_CAPABILITIES: usize = 32;
const MAX_CAPABILITY_BYTES: usize = 64;
const MAX_REASON_BYTES: usize = 64;
const MAX_IMAGE_DIM: u64 = 16384;
const MAX_SCREENSHOT_BASE64_BYTES: usize = 25_165_824;

/// プロトコル層のエラー。`Display` は固定の英語文言で、フレーム内容・URL・ホストの
/// message を含めない（機密の露出対策）。codec とメッセージ解釈の双方が返す。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// 入出力エラー（種別のみ保持する）。
    Io(std::io::ErrorKind),
    /// フレームの先頭で EOF（ホストが正常に閉じた）。
    Closed,
    /// フレームの途中で EOF。
    Truncated,
    /// 長さ接頭辞が 0。
    InvalidLength,
    /// 長さ接頭辞が上限超過（確保前に判定）。
    FrameTooLarge {
        /// 申告された長さ。
        len: u64,
        /// 適用された上限。
        max: usize,
    },
    /// 本体が UTF-8 でない。
    InvalidUtf8,
    /// 本体が JSON として不正（再帰深さ超過を含む）。
    InvalidJson,
    /// JSON だがプロトコル上不正なメッセージ。
    InvalidMessage(&'static str),
    /// 送信しようとした要求パラメータが不正。
    InvalidParams(&'static str),
    /// ホストの `protocol` 範囲に本クライアントの版が含まれない。
    ProtocolMismatch {
        /// ホストが申告した最小版。
        min: u64,
        /// ホストが申告した最大版。
        max: u64,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("render host i/o error"),
            Self::Closed => f.write_str("render host closed the connection"),
            Self::Truncated => f.write_str("truncated frame"),
            Self::InvalidLength => f.write_str("invalid frame length"),
            Self::FrameTooLarge { len, max } => {
                write!(f, "frame too large: {len} bytes (max {max})")
            }
            Self::InvalidUtf8 => f.write_str("frame is not valid utf-8"),
            Self::InvalidJson => f.write_str("frame is not valid json"),
            Self::InvalidMessage(why) => write!(f, "invalid message: {why}"),
            Self::InvalidParams(why) => write!(f, "invalid params: {why}"),
            Self::ProtocolMismatch { min, max } => {
                write!(f, "protocol mismatch: host supports {min}..={max}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

/// 要求 ID（0..=2^53-1）。応答との突き合わせに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(u64);

impl RequestId {
    /// 範囲（0..=2^53-1）を検証して生成する。
    pub fn new(id: u64) -> Result<Self, ProtocolError> {
        if id > MAX_REQUEST_ID {
            return Err(ProtocolError::InvalidParams("request id out of range"));
        }
        Ok(Self(id))
    }

    /// 内部の数値。
    pub fn get(self) -> u64 {
        self.0
    }
}

/// プロトコルのメソッド（RENDER-3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// ページ読み込み。
    Load,
    /// スクリーンショット。
    Screenshot,
    /// 要素の可視性。
    Visibility,
    /// 要素の bounding box。
    BoundingBox,
    /// ホストの終了。
    Shutdown,
}

impl Method {
    /// ワイヤ上のメソッド名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Screenshot => "screenshot",
            Self::Visibility => "visibility",
            Self::BoundingBox => "bounding_box",
            Self::Shutdown => "shutdown",
        }
    }
}

/// `load` のパラメータ。送信前に URL を保守的に検証する（設計 §2.5。ホスト側でも再検証）。
///
/// UA・ヘッダ・Cookie 等を指定するフィールドは意図的に持たない（SEC-2）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoadParams {
    url: String,
    timeout_ms: Option<u32>,
}

impl LoadParams {
    /// URL（`http` / `https` のみ・userinfo 禁止・上限 2048 バイト）と任意のタイムアウト
    /// （1..=60000 ms）を検証して生成する。
    pub fn new(url: &str, timeout_ms: Option<u32>) -> Result<Self, ProtocolError> {
        validate_url(url)?;
        if let Some(t) = timeout_ms
            && !(1..=MAX_LOAD_TIMEOUT_MS).contains(&t)
        {
            return Err(ProtocolError::InvalidParams("timeout_ms out of range"));
        }
        Ok(Self {
            url: url.to_owned(),
            timeout_ms,
        })
    }

    /// 検証済み URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 指定されたタイムアウト（未指定ならホストの既定）。
    pub fn timeout_ms(&self) -> Option<u32> {
        self.timeout_ms
    }
}

/// 送信前の URL 検証。プライベート IP 等のポリシーは core の設定が要るため
/// HostRenderer 側（#235）で扱う。
fn validate_url(url: &str) -> Result<(), ProtocolError> {
    if url.is_empty() || url.len() > MAX_URL_BYTES {
        return Err(ProtocolError::InvalidParams("url length out of range"));
    }
    if url
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\\')
    {
        return Err(ProtocolError::InvalidParams(
            "url contains forbidden character",
        ));
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .ok_or(ProtocolError::InvalidParams(
            "url scheme must be http or https",
        ))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(ProtocolError::InvalidParams(
            "url must not contain userinfo",
        ));
    }
    // userinfo は上で拒否済み。authority を host と port に分け、host 部の空を拒否する
    // （`https://:80/` のようなポートだけの URL を弾く。IPv6 リテラルは括弧内を host とする）。
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, tail) = inner
            .split_once(']')
            .ok_or(ProtocolError::InvalidParams("url host is malformed"))?;
        match tail {
            "" => (host, None),
            t => (
                host,
                Some(
                    t.strip_prefix(':')
                        .ok_or(ProtocolError::InvalidParams("url host is malformed"))?,
                ),
            ),
        }
    } else {
        match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(ProtocolError::InvalidParams("url host is empty"));
    }
    if port.is_some_and(|p| !p.bytes().all(|b| b.is_ascii_digit())) {
        return Err(ProtocolError::InvalidParams("url port is malformed"));
    }
    Ok(())
}

/// `screenshot` のパラメータ。ビューポートは固定で指定できない（SEC-2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScreenshotParams {
    full_page: bool,
}

impl ScreenshotParams {
    /// 全ページ撮影かどうかを指定して生成する。
    pub fn new(full_page: bool) -> Self {
        Self { full_page }
    }

    /// 全ページ撮影か。
    pub fn full_page(&self) -> bool {
        self.full_page
    }
}

fn validate_selector(selector: &str) -> Result<(), ProtocolError> {
    if selector.is_empty() || selector.len() > MAX_SELECTOR_BYTES {
        return Err(ProtocolError::InvalidParams("selector length out of range"));
    }
    Ok(())
}

/// `visibility` のパラメータ。セレクタは JS 評価文字列へ連結されず、長さのみ検証する。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VisibilityParams {
    selector: String,
}

impl VisibilityParams {
    /// セレクタ（1..=1024 バイト）を検証して生成する。
    pub fn new(selector: &str) -> Result<Self, ProtocolError> {
        validate_selector(selector)?;
        Ok(Self {
            selector: selector.to_owned(),
        })
    }

    /// セレクタ。
    pub fn selector(&self) -> &str {
        &self.selector
    }
}

/// `bounding_box` のパラメータ。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BoundingBoxParams {
    selector: String,
}

impl BoundingBoxParams {
    /// セレクタ（1..=1024 バイト）を検証して生成する。
    pub fn new(selector: &str) -> Result<Self, ProtocolError> {
        validate_selector(selector)?;
        Ok(Self {
            selector: selector.to_owned(),
        })
    }

    /// セレクタ。
    pub fn selector(&self) -> &str {
        &self.selector
    }
}

/// クライアントからホストへの要求（RENDER-3）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// ページ読み込み。
    Load(LoadParams),
    /// スクリーンショット。
    Screenshot(ScreenshotParams),
    /// 可視性判定。
    Visibility(VisibilityParams),
    /// bounding box 取得。
    BoundingBox(BoundingBoxParams),
    /// ホスト終了（params なし）。
    Shutdown,
}

impl Request {
    /// 対応するメソッド。応答の解釈（[`crate::codec::decode_response`]）に渡す。
    pub fn method(&self) -> Method {
        match self {
            Self::Load(_) => Method::Load,
            Self::Screenshot(_) => Method::Screenshot,
            Self::Visibility(_) => Method::Visibility,
            Self::BoundingBox(_) => Method::BoundingBox,
            Self::Shutdown => Method::Shutdown,
        }
    }

    /// 要求封筒の JSON 値にする。`timeout_ms` 未指定ならキーを出さず、shutdown は
    /// `params` を省略する。
    pub fn to_json(&self, id: RequestId) -> Value {
        let mut obj = Map::new();
        obj.insert("v".into(), json!(PROTOCOL_VERSION));
        obj.insert("id".into(), json!(id.get()));
        obj.insert("method".into(), json!(self.method().as_str()));
        match self {
            Self::Load(p) => {
                let mut params = Map::new();
                params.insert("url".into(), json!(p.url));
                if let Some(t) = p.timeout_ms {
                    params.insert("timeout_ms".into(), json!(t));
                }
                obj.insert("params".into(), Value::Object(params));
            }
            Self::Screenshot(p) => {
                obj.insert("params".into(), json!({ "full_page": p.full_page }));
            }
            Self::Visibility(p) => {
                obj.insert("params".into(), json!({ "selector": p.selector }));
            }
            Self::BoundingBox(p) => {
                obj.insert("params".into(), json!({ "selector": p.selector }));
            }
            Self::Shutdown => {}
        }
        Value::Object(obj)
    }
}

/// `load` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LoadResult {
    final_url: String,
    status: Option<u16>,
}

impl LoadResult {
    /// リダイレクト後の最終 URL。
    pub fn final_url(&self) -> &str {
        &self.final_url
    }

    /// HTTP ステータス（100..=599）。ホストが取得できない場合は `None`（応答に `status` キーが無い）。
    pub fn status(&self) -> Option<u16> {
        self.status
    }
}

/// 画像形式（v1 は PNG のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageFormat {
    /// PNG。
    Png,
}

/// `screenshot` の結果。base64 のデコードは行わない（HostRenderer #235 の範囲）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScreenshotResult {
    format: ImageFormat,
    width: u32,
    height: u32,
    data_base64: String,
}

impl ScreenshotResult {
    /// 画像形式。
    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// 幅（1..=16384）。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 高さ（1..=16384）。
    pub fn height(&self) -> u32 {
        self.height
    }

    /// base64 文字列（未デコード）。
    pub fn data_base64(&self) -> &str {
        &self.data_base64
    }
}

/// `visibility` の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VisibilityResult {
    visible: bool,
    reason: Option<String>,
}

impl VisibilityResult {
    /// 可視か。
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// 不可視の理由（`display_none` 等。未知の値もそのまま保持する）。
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

/// `bounding_box` の結果。
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct BoundingBoxResult {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl BoundingBoxResult {
    /// 左上 x。
    pub fn x(&self) -> f64 {
        self.x
    }

    /// 左上 y。
    pub fn y(&self) -> f64 {
        self.y
    }

    /// 幅（0 以上）。
    pub fn width(&self) -> f64 {
        self.width
    }

    /// 高さ（0 以上）。
    pub fn height(&self) -> f64 {
        self.height
    }
}

/// `shutdown` の結果（現状フィールドなし。将来拡張用。REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShutdownResult {}

/// メソッド別の結果。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MethodResult {
    /// `load` の結果。
    Load(LoadResult),
    /// `screenshot` の結果。
    Screenshot(ScreenshotResult),
    /// `visibility` の結果。
    Visibility(VisibilityResult),
    /// `bounding_box` の結果。
    BoundingBox(BoundingBoxResult),
    /// `shutdown` の結果。
    Shutdown(ShutdownResult),
}

/// ホストが返すエラーの種別。未知の値は [`HostErrorKind::Other`] に写像する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostErrorKind {
    /// 未対応のメソッド。
    Unsupported,
    /// 要求の形式不正。
    InvalidRequest,
    /// パラメータ不正。
    InvalidParams,
    /// ページ未読み込み。
    NotLoaded,
    /// 要素が見つからない。
    NotFound,
    /// タイムアウト。
    Timeout,
    /// 読み込み失敗。
    LoadFailed,
    /// ホスト内部エラー。
    Internal,
    /// v1 の集合に無い kind（汎用エラー扱い）。
    Other,
}

/// ホストからのエラー応答。message は表示・ログに使う前に呼び出し側で扱いを決める。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostError {
    kind: HostErrorKind,
    message: String,
}

impl HostError {
    /// エラー種別。
    pub fn kind(&self) -> HostErrorKind {
        self.kind
    }

    /// ホストの message（512 バイト以内に検証済み。untrusted）。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// ホストからの応答。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Response {
    /// 対応する要求 ID。
    pub id: RequestId,
    /// 成功結果またはホストのエラー。
    pub outcome: Result<MethodResult, HostError>,
}

/// `protocol` 範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProtocolRange {
    /// 最小版。
    pub min: u64,
    /// 最大版。
    pub max: u64,
}

/// ホストが申告する上限。クライアントは固定上限との `min` だけを使う
/// （[`crate::codec::effective_limit`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct HelloLimits {
    /// 要求フレーム上限。
    pub request_frame_bytes: u64,
    /// 通常応答フレーム上限。
    pub response_frame_bytes: u64,
    /// hello フレーム上限。
    pub hello_frame_bytes: u64,
    /// スクリーンショット応答フレーム上限。
    pub screenshot_frame_bytes: u64,
    /// URL 上限。
    pub url_bytes: u64,
    /// セレクタ上限。
    pub selector_bytes: u64,
    /// エラー message 上限。
    pub error_message_bytes: u64,
    /// 同時処理数（既定 1）。
    pub max_in_flight: u64,
}

/// 起動直後にホストが送る `hello`。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Hello {
    /// ホストのバージョン。
    pub host_version: String,
    /// Servo のバージョン。
    pub servo_version: String,
    /// 対応プロトコル範囲。
    pub protocol: ProtocolRange,
    /// 実装済みメソッド名。
    pub capabilities: Vec<String>,
    /// 申告上限。
    pub limits: HelloLimits,
}

const fn bad(why: &'static str) -> ProtocolError {
    ProtocolError::InvalidMessage(why)
}

/// 2^53 以下の非負整数として読む（`1.0` のような小数部ゼロ表記も受理する）。
fn as_uint(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    let f = v.as_f64()?;
    if f.is_finite() && f.fract() == 0.0 && (0.0..=9_007_199_254_740_992.0).contains(&f) {
        // 範囲と小数部ゼロを確認済みのため、切り捨て・飽和は起きない。
        Some(f as u64)
    } else {
        None
    }
}

fn uint_field(
    obj: &Map<String, Value>,
    key: &str,
    why: &'static str,
) -> Result<u64, ProtocolError> {
    obj.get(key).and_then(as_uint).ok_or(bad(why))
}

fn str_field<'a>(
    obj: &'a Map<String, Value>,
    key: &str,
    max: usize,
    why: &'static str,
) -> Result<&'a str, ProtocolError> {
    match obj.get(key).and_then(Value::as_str) {
        Some(s) if s.len() <= max => Ok(s),
        _ => Err(bad(why)),
    }
}

fn check_version(obj: &Map<String, Value>) -> Result<(), ProtocolError> {
    match obj.get("v").and_then(as_uint) {
        Some(v) if v == u64::from(PROTOCOL_VERSION) => Ok(()),
        _ => Err(bad("unsupported envelope version")),
    }
}

impl Response {
    /// 応答封筒を検証して解釈する。`result` と `error` はちょうど一方のみ許す。
    /// `result` は `method` に対応する型として検証する。
    pub fn from_json(value: &Value, method: Method) -> Result<Self, ProtocolError> {
        let obj = value.as_object().ok_or(bad("response is not an object"))?;
        check_version(obj)?;
        let id = RequestId::new(uint_field(obj, "id", "invalid response id")?)
            .map_err(|_| bad("invalid response id"))?;
        let outcome = match (obj.get("result"), obj.get("error")) {
            (Some(r), None) => Ok(parse_result(r, method)?),
            (None, Some(e)) => Err(parse_error(e)?),
            (Some(_), Some(_)) => return Err(bad("response has both result and error")),
            (None, None) => return Err(bad("response has neither result nor error")),
        };
        Ok(Self { id, outcome })
    }
}

fn parse_error(v: &Value) -> Result<HostError, ProtocolError> {
    let obj = v.as_object().ok_or(bad("error is not an object"))?;
    let kind = match obj.get("kind").and_then(Value::as_str) {
        Some("unsupported") => HostErrorKind::Unsupported,
        Some("invalid_request") => HostErrorKind::InvalidRequest,
        Some("invalid_params") => HostErrorKind::InvalidParams,
        Some("not_loaded") => HostErrorKind::NotLoaded,
        Some("not_found") => HostErrorKind::NotFound,
        Some("timeout") => HostErrorKind::Timeout,
        Some("load_failed") => HostErrorKind::LoadFailed,
        Some("internal") => HostErrorKind::Internal,
        Some(_) => HostErrorKind::Other,
        None => return Err(bad("error kind missing")),
    };
    let message = str_field(
        obj,
        "message",
        MAX_ERROR_MESSAGE_BYTES,
        "invalid error message",
    )?;
    Ok(HostError {
        kind,
        message: message.to_owned(),
    })
}

fn parse_result(v: &Value, method: Method) -> Result<MethodResult, ProtocolError> {
    let obj = v.as_object().ok_or(bad("result is not an object"))?;
    match method {
        Method::Load => {
            let final_url = str_field(obj, "final_url", MAX_URL_BYTES, "invalid final_url")?;
            let status = match obj.get("status") {
                None => None,
                Some(_) => {
                    let raw = uint_field(obj, "status", "invalid status")?;
                    Some(
                        u16::try_from(raw)
                            .ok()
                            .filter(|s| (100..=599).contains(s))
                            .ok_or(bad("status out of range"))?,
                    )
                }
            };
            Ok(MethodResult::Load(LoadResult {
                final_url: final_url.to_owned(),
                status,
            }))
        }
        Method::Screenshot => {
            if obj.get("format").and_then(Value::as_str) != Some("png") {
                return Err(bad("unsupported image format"));
            }
            let dim = |key: &str| -> Result<u32, ProtocolError> {
                uint_field(obj, key, "invalid image size")
                    .ok()
                    .filter(|n| (1..=MAX_IMAGE_DIM).contains(n))
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or(bad("image size out of range"))
            };
            let data = str_field(
                obj,
                "data_base64",
                MAX_SCREENSHOT_BASE64_BYTES,
                "invalid image data",
            )?;
            Ok(MethodResult::Screenshot(ScreenshotResult {
                format: ImageFormat::Png,
                width: dim("width")?,
                height: dim("height")?,
                data_base64: data.to_owned(),
            }))
        }
        Method::Visibility => {
            let visible = obj
                .get("visible")
                .and_then(Value::as_bool)
                .ok_or(bad("invalid visible"))?;
            let reason = if obj.contains_key("reason") {
                Some(str_field(obj, "reason", MAX_REASON_BYTES, "invalid reason")?.to_owned())
            } else {
                None
            };
            Ok(MethodResult::Visibility(VisibilityResult {
                visible,
                reason,
            }))
        }
        Method::BoundingBox => {
            let num = |key: &str| -> Result<f64, ProtocolError> {
                obj.get(key)
                    .and_then(Value::as_f64)
                    .filter(|f| f.is_finite())
                    .ok_or(bad("invalid bounding box value"))
            };
            let (x, y, width, height) = (num("x")?, num("y")?, num("width")?, num("height")?);
            if width < 0.0 || height < 0.0 {
                return Err(bad("negative bounding box size"));
            }
            Ok(MethodResult::BoundingBox(BoundingBoxResult {
                x,
                y,
                width,
                height,
            }))
        }
        Method::Shutdown => Ok(MethodResult::Shutdown(ShutdownResult {})),
    }
}

impl Hello {
    /// `hello` イベントを検証して解釈する。`protocol` 範囲に本クライアントの版が
    /// 含まれなければ [`ProtocolError::ProtocolMismatch`]、`min > max` は不正メッセージ。
    pub fn from_json(value: &Value) -> Result<Self, ProtocolError> {
        let obj = value.as_object().ok_or(bad("hello is not an object"))?;
        check_version(obj)?;
        if obj.get("event").and_then(Value::as_str) != Some("hello") {
            return Err(bad("not a hello event"));
        }
        let data = obj
            .get("data")
            .and_then(Value::as_object)
            .ok_or(bad("hello data missing"))?;
        let host_version = str_field(
            data,
            "host_version",
            MAX_VERSION_BYTES,
            "invalid host_version",
        )?;
        let servo_version = str_field(
            data,
            "servo_version",
            MAX_VERSION_BYTES,
            "invalid servo_version",
        )?;
        let proto = data
            .get("protocol")
            .and_then(Value::as_object)
            .ok_or(bad("hello protocol missing"))?;
        let min = uint_field(proto, "min", "invalid protocol min")?;
        let max = uint_field(proto, "max", "invalid protocol max")?;
        if min < 1 || max < 1 {
            return Err(bad("protocol min and max must be at least 1"));
        }
        if min > max {
            return Err(bad("protocol min exceeds max"));
        }
        let ours = u64::from(PROTOCOL_VERSION);
        if min > ours || max < ours {
            return Err(ProtocolError::ProtocolMismatch { min, max });
        }
        let caps = data
            .get("capabilities")
            .and_then(Value::as_array)
            .ok_or(bad("hello capabilities missing"))?;
        if caps.len() > MAX_CAPABILITIES {
            return Err(bad("too many capabilities"));
        }
        let mut capabilities = Vec::with_capacity(caps.len());
        for c in caps {
            match c.as_str() {
                Some(s) if s.len() <= MAX_CAPABILITY_BYTES => capabilities.push(s.to_owned()),
                _ => return Err(bad("invalid capability")),
            }
        }
        let lim = data
            .get("limits")
            .and_then(Value::as_object)
            .ok_or(bad("hello limits missing"))?;
        let req = |key: &str| -> Result<u64, ProtocolError> {
            match uint_field(lim, key, "invalid limit")? {
                0 => Err(bad("invalid limit")),
                n => Ok(n),
            }
        };
        let limits = HelloLimits {
            request_frame_bytes: req("request_frame_bytes")?,
            response_frame_bytes: req("response_frame_bytes")?,
            hello_frame_bytes: req("hello_frame_bytes")?,
            screenshot_frame_bytes: req("screenshot_frame_bytes")?,
            url_bytes: req("url_bytes")?,
            selector_bytes: req("selector_bytes")?,
            error_message_bytes: req("error_message_bytes")?,
            max_in_flight: if lim.contains_key("max_in_flight") {
                req("max_in_flight")?
            } else {
                1
            },
        };
        Ok(Self {
            host_version: host_version.to_owned(),
            servo_version: servo_version.to_owned(),
            protocol: ProtocolRange { min, max },
            capabilities,
            limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render3_load_url_boundaries() {
        let ok = format!("https://e.com/{}", "a".repeat(MAX_URL_BYTES - 14));
        assert_eq!(ok.len(), 2048);
        assert!(LoadParams::new(&ok, None).is_ok());
        assert!(LoadParams::new(&format!("{ok}a"), None).is_err());
        assert!(LoadParams::new("HTTPS://example.com/", None).is_ok());
        assert!(LoadParams::new("https://user@example.com/", None).is_err());
        assert!(LoadParams::new("https://example.com/path@x", None).is_ok());
        assert!(LoadParams::new("https://example.com:8443/", None).is_ok());
        assert!(LoadParams::new("http://[::1]:80/", None).is_ok());
        assert!(LoadParams::new("http://[::1]/", None).is_ok());
        for u in [
            "file:///etc/passwd",
            "data:text/html,x",
            "javascript:1",
            "about:blank",
            "",
            "https://",
            "https:///x",
            "https://a b/",
            "https://:80/",
            "https://:/",
            "https://@:80/",
            "http://[]:80/",
            "http://[::1/",
            "https://e.com:x/",
        ] {
            assert!(LoadParams::new(u, None).is_err(), "{u}");
        }
    }

    #[test]
    fn render3_load_timeout_boundaries() {
        for (t, ok) in [(0, false), (1, true), (60_000, true), (60_001, false)] {
            assert_eq!(
                LoadParams::new("https://e.com/", Some(t)).is_ok(),
                ok,
                "{t}"
            );
        }
    }

    #[test]
    fn render3_selector_boundaries() {
        for (n, ok) in [(0, false), (1, true), (1024, true), (1025, false)] {
            let s = "a".repeat(n);
            assert_eq!(VisibilityParams::new(&s).is_ok(), ok, "{n}");
            assert_eq!(BoundingBoxParams::new(&s).is_ok(), ok, "{n}");
        }
    }

    #[test]
    fn render3_request_id_range() {
        assert!(RequestId::new(MAX_REQUEST_ID).is_ok());
        assert!(RequestId::new(MAX_REQUEST_ID + 1).is_err());
    }

    #[test]
    fn render3_response_float_notation_id() {
        let v: Value = serde_json::from_str(r#"{"v":1.0,"id":3.0,"result":{}}"#).unwrap();
        let r = Response::from_json(&v, Method::Shutdown).unwrap();
        assert_eq!(r.id.get(), 3);
        let v: Value = serde_json::from_str(r#"{"v":1,"id":3.5,"result":{}}"#).unwrap();
        assert!(Response::from_json(&v, Method::Shutdown).is_err());
    }
}

//! Servo ホストの stdio サーバーループ（フレーム I/O・`hello`・ディスパッチ・上限検証・`shutdown`）。
//!
//! 呼び出し元は `main` の `--stdio`（本体 `fandhe-browser-render` の `host_process` が固定引数
//! `HOST_ARGS` で起動する）。実処理（load・screenshot・visibility・bounding_box）は [`Handler`] の
//! 向こうに置き、本モジュールは Servo に依存しない（`servo::*` を import しない）。後続の
//! #231〜#234 が Servo を使うハンドラを実装し、`main` の `UnimplementedHandler` と差し替える。
//!
//! 契約の正本は `docs/design/render-host-protocol.schema.json` と
//! `crates/fandhe-browser-render/tests/fixtures/protocol/`（本体側とは型を共有しない）。
//! フレームは u32 LE の長さ接頭辞 + UTF-8 JSON。同時 in-flight は 1 で、要求を 1 件読んで処理し、
//! 応答を返してから次を読む同期ループ。対応ビヘイビア: `RENDER-3`・`RENDER-4`・`PLUG-1`・`SEC-2`
//! （関連 `REPAIR-3`・`REPAIR-4`。設計 `docs/design/servo-host-design.md` §2.1〜§2.5）。
//!
//! ログは stderr のみ（stdout はフレーム専用）。URL・セレクタ・params の中身は出さない。
//! エラーの `message` は kind ごとの固定英語文字列で、入力値や機密を含めない。

mod request;

use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::time::Instant;

use serde_json::{Value, json};

use request::{LoadParams, Request, ScreenshotParams, SelectorParams, parse_request};

/// 要求フレームの上限（バイト）。長さ接頭辞の段階で確保前に検証する。
pub(crate) const MAX_REQUEST_FRAME_BYTES: usize = 65_536;
/// 通常応答フレームの上限。
pub(crate) const MAX_RESPONSE_FRAME_BYTES: usize = 1_048_576;
/// `hello` フレームの上限。
pub(crate) const MAX_HELLO_FRAME_BYTES: usize = 16_384;
/// `screenshot` 応答フレームの上限。
pub(crate) const MAX_SCREENSHOT_FRAME_BYTES: usize = 25_165_824;
pub(crate) const MAX_URL_BYTES: usize = 2048;
pub(crate) const MAX_SELECTOR_BYTES: usize = 1024;
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 512;
pub(crate) const MAX_METHOD_BYTES: usize = 64;
/// 要求 `id` の最大値（2^53 - 1。JSON 数値として厳密に扱える範囲）。
pub(crate) const MAX_REQUEST_ID: u64 = (1 << 53) - 1;
pub(crate) const LOAD_TIMEOUT_DEFAULT_MS: u64 = 30_000;
pub(crate) const LOAD_TIMEOUT_MAX_MS: u64 = 60_000;

/// 同梱する Servo の版。`servo` crate は版の定数を公開しないため手で持ち、
/// `Cargo.toml` の pin との一致をテストで検証する。
pub(crate) const SERVO_VERSION: &str = "0.6.0";

/// 要求メソッド。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Load,
    Screenshot,
    Visibility,
    BoundingBox,
    Shutdown,
}

impl Method {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Screenshot => "screenshot",
            Self::Visibility => "visibility",
            Self::BoundingBox => "bounding_box",
            Self::Shutdown => "shutdown",
        }
    }

    /// 大文字小文字を区別して照合する。未知は `None`。
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        [
            Self::Load,
            Self::Screenshot,
            Self::Visibility,
            Self::BoundingBox,
            Self::Shutdown,
        ]
        .into_iter()
        .find(|m| m.as_str() == name)
    }
}

/// 応答 `error.kind`。メッセージは kind ごとの固定文字列（[`ErrorKind::message`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    InvalidRequest,
    InvalidParams,
    Unsupported,
    // 以下 4 種は Handler 実装（#231〜#234）が返すまで本体コードから構築されない。
    #[allow(dead_code)]
    NotLoaded,
    #[allow(dead_code)]
    NotFound,
    #[allow(dead_code)]
    Timeout,
    #[allow(dead_code)]
    LoadFailed,
    Internal,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidParams => "invalid_params",
            Self::Unsupported => "unsupported",
            Self::NotLoaded => "not_loaded",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::LoadFailed => "load_failed",
            Self::Internal => "internal",
        }
    }

    /// 入力値を含まない固定の英語メッセージ（512 バイト以下を構造で保証する）。
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid request.",
            Self::InvalidParams => "Invalid parameters.",
            Self::Unsupported => "Method is not supported.",
            Self::NotLoaded => "No page is loaded.",
            Self::NotFound => "No element matches the selector.",
            Self::Timeout => "Operation timed out.",
            Self::LoadFailed => "Navigation failed.",
            Self::Internal => "Internal host error.",
        }
    }
}

/// ハンドラの失敗。自由文を持たせず kind だけを返す（メッセージに入力値や機密を混ぜない）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HandlerError(pub ErrorKind);

/// `load` の結果。
pub(crate) struct LoadResult {
    pub final_url: String,
    pub status: u16,
}

/// `screenshot` の結果（`format` は常に `png` でサーバーが付ける。PNG と base64 の生成は #232）。
pub(crate) struct ScreenshotResult {
    pub width: u32,
    pub height: u32,
    pub data_base64: String,
}

/// `visibility` の結果。`reason` は不可視のときの既知値（`display_none` 等）。
pub(crate) struct VisibilityResult {
    pub visible: bool,
    pub reason: Option<&'static str>,
}

/// `bounding_box` の結果。
pub(crate) struct BoundingBoxResult {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 実処理の差し替え点。サーバーは検証済みの params だけを渡す。
pub(crate) trait Handler {
    /// 実装済みのメソッド（`hello.capabilities` の元。`shutdown` はサーバーが常に足す）。
    fn capabilities(&self) -> &'static [Method];
    fn load(&mut self, p: &LoadParams) -> Result<LoadResult, HandlerError>;
    fn screenshot(&mut self, p: &ScreenshotParams) -> Result<ScreenshotResult, HandlerError>;
    fn visibility(&mut self, p: &SelectorParams) -> Result<VisibilityResult, HandlerError>;
    fn bounding_box(&mut self, p: &SelectorParams) -> Result<BoundingBoxResult, HandlerError>;
}

/// 全メソッドを `unsupported` で拒否するスタブ。
///
/// 将来仕様: #231（load）・#232（screenshot）・#233（visibility）・#234（bounding_box）で
/// Servo を使うハンドラに置き換える（`RENDER-3`・`RENDER-4`）。それまで成功を装わず、
/// `hello.capabilities` にも載せない（`REPAIR-3`・`SEC-2`）。
pub(crate) struct UnimplementedHandler;

impl Handler for UnimplementedHandler {
    fn capabilities(&self) -> &'static [Method] {
        &[]
    }
    fn load(&mut self, _p: &LoadParams) -> Result<LoadResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
    fn screenshot(&mut self, _p: &ScreenshotParams) -> Result<ScreenshotResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
    fn visibility(&mut self, _p: &SelectorParams) -> Result<VisibilityResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
    fn bounding_box(&mut self, _p: &SelectorParams) -> Result<BoundingBoxResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
}

/// `serve` の終了理由。`main` が終了コードへ対応づける。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ServeOutcome {
    /// `shutdown` 要求を処理して終了した。
    Shutdown,
    /// フレーム境界で入力が閉じられた。
    InputClosed,
    /// プロトコル違反（応答なしで終了。理由は固定の英語文字列）。
    ProtocolViolation(&'static str),
    /// 読み書きの失敗（EPIPE 等）。
    Io(IoErrorKind),
}

enum FrameError {
    Violation(&'static str),
    Io(IoErrorKind),
}

/// 1 フレームを読む。境界での EOF は `Ok(None)`。長さは本体を読む前に検証する（確保前検証）。
fn read_frame<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; 4];
    let mut got = 0usize;
    while got < header.len() {
        let Some(buf) = header.get_mut(got..) else {
            break;
        };
        match r.read(buf) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Violation("truncated frame header")),
            Ok(n) => got += n,
            Err(e) if e.kind() == IoErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e.kind())),
        }
    }
    let len = usize::try_from(u32::from_le_bytes(header))
        .map_err(|_| FrameError::Violation("frame length exceeds limit"))?;
    if len == 0 {
        return Err(FrameError::Violation("frame length is zero"));
    }
    if len > MAX_REQUEST_FRAME_BYTES {
        return Err(FrameError::Violation("frame length exceeds limit"));
    }
    let mut body = Vec::with_capacity(len);
    r.take(len as u64)
        .read_to_end(&mut body)
        .map_err(|e| FrameError::Io(e.kind()))?;
    if body.len() != len {
        return Err(FrameError::Violation("truncated frame body"));
    }
    Ok(Some(body))
}

/// 1 フレームを書いて flush する。上限超過は書く前に `Violation` で返す。
fn write_frame<W: Write>(w: &mut W, body: &[u8], limit: usize) -> Result<(), FrameError> {
    if body.len() > limit {
        return Err(FrameError::Violation("frame exceeds size limit"));
    }
    let len =
        u32::try_from(body.len()).map_err(|_| FrameError::Violation("frame exceeds size limit"))?;
    w.write_all(&len.to_le_bytes())
        .and_then(|()| w.write_all(body))
        .and_then(|()| w.flush())
        .map_err(|e| FrameError::Io(e.kind()))
}

fn io_outcome(e: FrameError) -> ServeOutcome {
    match e {
        FrameError::Violation(m) => ServeOutcome::ProtocolViolation(m),
        FrameError::Io(k) => ServeOutcome::Io(k),
    }
}

fn hello_message(caps: &[Method]) -> Value {
    let mut capabilities: Vec<&str> = caps
        .iter()
        .filter(|m| **m != Method::Shutdown)
        .map(|m| m.as_str())
        .collect();
    capabilities.push(Method::Shutdown.as_str());
    json!({
        "v": 1,
        "event": "hello",
        "data": {
            "host_version": env!("CARGO_PKG_VERSION"),
            "servo_version": SERVO_VERSION,
            "protocol": { "min": 1, "max": 1 },
            "capabilities": capabilities,
            "limits": {
                "request_frame_bytes": MAX_REQUEST_FRAME_BYTES,
                "response_frame_bytes": MAX_RESPONSE_FRAME_BYTES,
                "hello_frame_bytes": MAX_HELLO_FRAME_BYTES,
                "screenshot_frame_bytes": MAX_SCREENSHOT_FRAME_BYTES,
                "url_bytes": MAX_URL_BYTES,
                "selector_bytes": MAX_SELECTOR_BYTES,
                "error_message_bytes": MAX_ERROR_MESSAGE_BYTES,
                "max_in_flight": 1,
                "handshake_timeout_ms": 10_000,
                "load_timeout_default_ms": LOAD_TIMEOUT_DEFAULT_MS,
                "load_timeout_max_ms": LOAD_TIMEOUT_MAX_MS,
                "screenshot_timeout_ms": 20_000,
                "query_timeout_ms": 5_000,
                "shutdown_grace_ms": 3_000
            }
        }
    })
}

fn ok_response(id: u64, result: Value) -> Value {
    json!({ "v": 1, "id": id, "result": result })
}

fn error_response(id: u64, kind: ErrorKind) -> Value {
    json!({ "v": 1, "id": id, "error": { "kind": kind.as_str(), "message": kind.message() } })
}

/// 検証済みの要求をハンドラへ渡し、応答値とログ用の結果 kind（成功は `ok`）を返す。
fn dispatch<H: Handler>(h: &mut H, id: u64, req: &Request) -> (Value, &'static str) {
    let result = match req {
        Request::Load(p) => h
            .load(p)
            .map(|r| json!({ "final_url": r.final_url, "status": r.status })),
        Request::Screenshot(p) => h.screenshot(p).map(|r| {
            json!({ "format": "png", "width": r.width, "height": r.height, "data_base64": r.data_base64 })
        }),
        Request::Visibility(p) => h.visibility(p).map(|r| match r.reason {
            Some(reason) if !r.visible => json!({ "visible": false, "reason": reason }),
            _ => json!({ "visible": r.visible }),
        }),
        Request::BoundingBox(p) => h
            .bounding_box(p)
            .map(|r| json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height })),
        // shutdown は serve が処理する。ここへ来た場合は成功を装わず内部エラーにする。
        Request::Shutdown => Err(HandlerError(ErrorKind::Internal)),
    };
    match result {
        Ok(v) => (ok_response(id, v), "ok"),
        Err(HandlerError(kind)) => (error_response(id, kind), kind.as_str()),
    }
}

/// ログ 1 行を stderr 側のライタへ書く（失敗は無視。ログのために処理を止めない）。
fn log_line(log: &mut impl Write, line: std::fmt::Arguments<'_>) {
    let _ = writeln!(log, "{line}");
    let _ = log.flush();
}

/// stdio サーバーループ。`hello` を最初の 1 フレームとして送り、要求を 1 件ずつ処理する。
///
/// 戻り値が終了理由。違反時は応答を返さない。`log` へは要求ごとのメソッド名・`id`・結果の
/// kind・所要時間と終了理由だけを書く（URL・セレクタ・params は出さない）。
pub(crate) fn serve<R: Read, W: Write, H: Handler>(
    mut r: R,
    mut w: W,
    h: &mut H,
    log: &mut impl Write,
) -> ServeOutcome {
    let outcome = serve_inner(&mut r, &mut w, h, log);
    match &outcome {
        ServeOutcome::Shutdown => log_line(log, format_args!("host stopping: shutdown requested")),
        ServeOutcome::InputClosed => log_line(log, format_args!("host stopping: input closed")),
        ServeOutcome::ProtocolViolation(reason) => {
            log_line(log, format_args!("protocol violation: {reason}"));
        }
        ServeOutcome::Io(kind) => log_line(log, format_args!("host stopping: i/o error {kind:?}")),
    }
    outcome
}

fn serve_inner<R: Read, W: Write, H: Handler>(
    r: &mut R,
    w: &mut W,
    h: &mut H,
    log: &mut impl Write,
) -> ServeOutcome {
    log_line(log, format_args!("host started"));
    let hello = match serde_json::to_vec(&hello_message(h.capabilities())) {
        Ok(b) => b,
        Err(_) => return ServeOutcome::ProtocolViolation("hello encoding failed"),
    };
    if let Err(e) = write_frame(w, &hello, MAX_HELLO_FRAME_BYTES) {
        return io_outcome(e);
    }
    loop {
        let frame = match read_frame(r) {
            Ok(Some(f)) => f,
            Ok(None) => return ServeOutcome::InputClosed,
            Err(e) => return io_outcome(e),
        };
        let started = Instant::now();
        let parsed = match parse_request(&frame) {
            Ok(p) => p,
            Err(reason) => return ServeOutcome::ProtocolViolation(reason),
        };
        let (response, limit, kind) = match &parsed.request {
            Err(kind) => (
                error_response(parsed.id, *kind),
                MAX_RESPONSE_FRAME_BYTES,
                kind.as_str(),
            ),
            Ok(Request::Shutdown) => {
                let body = match serde_json::to_vec(&ok_response(parsed.id, json!({}))) {
                    Ok(b) => b,
                    Err(_) => return ServeOutcome::ProtocolViolation("response encoding failed"),
                };
                return match write_frame(w, &body, MAX_RESPONSE_FRAME_BYTES) {
                    Ok(()) => ServeOutcome::Shutdown,
                    Err(e) => io_outcome(e),
                };
            }
            Ok(req) => {
                let limit = if matches!(req, Request::Screenshot(_)) {
                    MAX_SCREENSHOT_FRAME_BYTES
                } else {
                    MAX_RESPONSE_FRAME_BYTES
                };
                let (resp, kind) = dispatch(h, parsed.id, req);
                (resp, limit, kind)
            }
        };
        let mut body = match serde_json::to_vec(&response) {
            Ok(b) => b,
            Err(_) => return ServeOutcome::ProtocolViolation("response encoding failed"),
        };
        let mut kind = kind;
        if body.len() > limit {
            // 上限超過の応答は書かず、同じ id の internal に差し替えてループを続ける。
            log_line(log, format_args!("response exceeded size limit"));
            kind = ErrorKind::Internal.as_str();
            body = match serde_json::to_vec(&error_response(parsed.id, ErrorKind::Internal)) {
                Ok(b) => b,
                Err(_) => return ServeOutcome::ProtocolViolation("response encoding failed"),
            };
        }
        if let Err(e) = write_frame(w, &body, limit) {
            return io_outcome(e);
        }
        log_line(
            log,
            format_args!(
                "request method={} id={} result={} elapsed_ms={}",
                parsed.label,
                parsed.id,
                kind,
                started.elapsed().as_millis()
            ),
        );
    }
}

#[cfg(test)]
mod tests;

//! レンダリングホストとのフレームコーデック（`u32` LE の長さ接頭辞 + UTF-8 JSON）。
//!
//! 将来の子プロセス管理（K2）と HostRenderer（#235）が、ホストの stdin / stdout に対して
//! 本モジュールの読み書きを呼ぶ。ホスト由来のバイト列は untrusted のため、長さ接頭辞は
//! **確保前に**方向・メソッド別の上限と比べ、超過・0・途中切れ・不正 UTF-8・不正 JSON は
//! panic せず [`ProtocolError`] にする（RENDER-3・PLUG-1。TASK-38・#989）。
//!
//! 同時処理は 1 件（`max_in_flight = 1`）が前提で、応答の上限は直前に送った要求の
//! メソッドで決まる（[`response_frame_limit`]）。js crate の `worker_protocol` と同じ流儀だが、
//! render は js に依存しないためコードは共有しない。

use std::io::{Read, Write};

use serde_json::Value;

use crate::protocol::{Hello, Method, ProtocolError, Request, RequestId, Response};

/// 要求フレームの上限（64 KiB）。
pub const MAX_REQUEST_FRAME_BYTES: usize = 64 * 1024;
/// 通常応答フレームの上限（1 MiB）。
pub const MAX_RESPONSE_FRAME_BYTES: usize = 1024 * 1024;
/// `hello` フレームの上限（16 KiB）。
pub const MAX_HELLO_FRAME_BYTES: usize = 16 * 1024;
/// スクリーンショット応答フレームの上限（24 MiB）。
pub const MAX_SCREENSHOT_RESPONSE_FRAME_BYTES: usize = 24 * 1024 * 1024;

/// メソッドに応じた応答フレームの上限。`screenshot` だけ 24 MiB、他は 1 MiB。
pub fn response_frame_limit(method: Method) -> usize {
    match method {
        Method::Screenshot => MAX_SCREENSHOT_RESPONSE_FRAME_BYTES,
        _ => MAX_RESPONSE_FRAME_BYTES,
    }
}

/// 固定上限とホスト申告値の小さい方を返す。申告値で固定上限を引き上げない
/// （悪意あるホストによる巨大確保の防止）。
pub fn effective_limit(fixed: usize, declared: Option<u64>) -> usize {
    match declared {
        Some(d) => usize::try_from(d).map_or(fixed, |d| d.min(fixed)),
        None => fixed,
    }
}

fn io_err(e: &std::io::Error) -> ProtocolError {
    ProtocolError::Io(e.kind())
}

/// 1 フレームを読む。先頭で EOF なら `Closed`、途中で EOF なら `Truncated`。
/// 長さが 0 なら `InvalidLength`、`max_len` 超過は確保前に `FrameTooLarge`。
pub fn read_frame<R: Read>(r: &mut R, max_len: usize) -> Result<Vec<u8>, ProtocolError> {
    let mut header = [0u8; 4];
    let mut filled = 0usize;
    while filled < header.len() {
        let dst = header.get_mut(filled..).ok_or(ProtocolError::Truncated)?;
        match r.read(dst) {
            Ok(0) if filled == 0 => return Err(ProtocolError::Closed),
            Ok(0) => return Err(ProtocolError::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(&e)),
        }
    }
    let declared = u32::from_le_bytes(header);
    if declared == 0 {
        return Err(ProtocolError::InvalidLength);
    }
    let too_large = ProtocolError::FrameTooLarge {
        len: u64::from(declared),
        max: max_len,
    };
    let len = usize::try_from(declared).map_err(|_| too_large.clone())?;
    if len > max_len {
        return Err(too_large);
    }
    // 上限検証後にのみ確保する。
    let mut buf = Vec::with_capacity(len);
    let n = r
        .take(u64::from(declared))
        .read_to_end(&mut buf)
        .map_err(|e| io_err(&e))?;
    if n != len {
        return Err(ProtocolError::Truncated);
    }
    Ok(buf)
}

/// 1 フレームを書いて flush する。空・上限超過・`u32` 超過は書く前に拒否する。
pub fn write_frame<W: Write>(
    w: &mut W,
    payload: &[u8],
    max_len: usize,
) -> Result<(), ProtocolError> {
    if payload.is_empty() {
        return Err(ProtocolError::InvalidLength);
    }
    if payload.len() > max_len {
        return Err(ProtocolError::FrameTooLarge {
            len: payload.len() as u64,
            max: max_len,
        });
    }
    let len = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge {
        len: payload.len() as u64,
        max: max_len,
    })?;
    w.write_all(&len.to_le_bytes()).map_err(|e| io_err(&e))?;
    w.write_all(payload).map_err(|e| io_err(&e))?;
    w.flush().map_err(|e| io_err(&e))
}

/// UTF-8 検証と JSON パース。再帰深さは serde_json 既定の上限（128）で制限される。
pub fn parse_json(bytes: &[u8]) -> Result<Value, ProtocolError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
    serde_json::from_str(text).map_err(|_| ProtocolError::InvalidJson)
}

/// 要求を JSON バイト列にする。64 KiB を超える場合はエラー。
pub fn encode_request(id: RequestId, request: &Request) -> Result<Vec<u8>, ProtocolError> {
    let bytes = serde_json::to_vec(&request.to_json(id))
        .map_err(|_| ProtocolError::InvalidParams("request is not serializable"))?;
    if bytes.len() > MAX_REQUEST_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            len: bytes.len() as u64,
            max: MAX_REQUEST_FRAME_BYTES,
        });
    }
    Ok(bytes)
}

/// 要求をフレームとして書く。
pub fn write_request<W: Write>(
    w: &mut W,
    id: RequestId,
    request: &Request,
) -> Result<(), ProtocolError> {
    let bytes = encode_request(id, request)?;
    write_frame(w, &bytes, MAX_REQUEST_FRAME_BYTES)
}

/// `hello` のフレーム本体を解釈する。
pub fn decode_hello(bytes: &[u8]) -> Result<Hello, ProtocolError> {
    if bytes.len() > MAX_HELLO_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge {
            len: bytes.len() as u64,
            max: MAX_HELLO_FRAME_BYTES,
        });
    }
    Hello::from_json(&parse_json(bytes)?)
}

/// `hello` を 1 フレーム読んで解釈する（上限 16 KiB）。
pub fn read_hello<R: Read>(r: &mut R) -> Result<Hello, ProtocolError> {
    decode_hello(&read_frame(r, MAX_HELLO_FRAME_BYTES)?)
}

/// 応答のフレーム本体を `method` の結果型として解釈する。
pub fn decode_response(bytes: &[u8], method: Method) -> Result<Response, ProtocolError> {
    Response::from_json(&parse_json(bytes)?, method)
}

/// 応答を 1 フレーム読んで解釈する。`limit` は [`response_frame_limit`] と
/// [`effective_limit`] で決めた値を渡す。
pub fn read_response<R: Read>(
    r: &mut R,
    method: Method,
    limit: usize,
) -> Result<Response, ProtocolError> {
    decode_response(&read_frame(r, limit)?, method)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{LoadParams, MethodResult};

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn render3_read_frame_exact_limit_ok_and_over_rejected() {
        let ok = framed(&[b'a'; 8]);
        assert_eq!(read_frame(&mut ok.as_slice(), 8).unwrap(), vec![b'a'; 8]);
        let over = framed(&[b'a'; 9]);
        assert_eq!(
            read_frame(&mut over.as_slice(), 8),
            Err(ProtocolError::FrameTooLarge { len: 9, max: 8 })
        );
    }

    #[test]
    fn render3_read_frame_rejects_oversize_before_alloc() {
        // 本体なしで巨大長だけを与える。確保前に判定していれば Truncated ではなく FrameTooLarge。
        let header = 0xFFFF_FFFFu32.to_le_bytes();
        assert_eq!(
            read_frame(&mut header.as_slice(), MAX_RESPONSE_FRAME_BYTES),
            Err(ProtocolError::FrameTooLarge {
                len: 0xFFFF_FFFF,
                max: MAX_RESPONSE_FRAME_BYTES
            })
        );
    }

    #[test]
    fn render3_read_frame_zero_length_invalid() {
        let h = 0u32.to_le_bytes();
        assert_eq!(
            read_frame(&mut h.as_slice(), 8),
            Err(ProtocolError::InvalidLength)
        );
    }

    #[test]
    fn render3_read_frame_eof_variants() {
        assert_eq!(
            read_frame(&mut [].as_slice(), 8),
            Err(ProtocolError::Closed)
        );
        assert_eq!(
            read_frame(&mut [1u8, 0].as_slice(), 8),
            Err(ProtocolError::Truncated)
        );
        let mut cut = framed(b"abcd");
        cut.truncate(6);
        assert_eq!(
            read_frame(&mut cut.as_slice(), 8),
            Err(ProtocolError::Truncated)
        );
    }

    #[test]
    fn render3_parse_json_errors() {
        assert_eq!(parse_json(&[0xFF, 0xFE]), Err(ProtocolError::InvalidUtf8));
        assert_eq!(parse_json(b"{\"v\":1,"), Err(ProtocolError::InvalidJson));
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(parse_json(deep.as_bytes()), Err(ProtocolError::InvalidJson));
    }

    #[test]
    fn render3_write_frame_rejects_oversize_and_empty() {
        let mut out = Vec::new();
        assert!(matches!(
            write_frame(&mut out, &[1u8; 9], 8),
            Err(ProtocolError::FrameTooLarge { len: 9, max: 8 })
        ));
        assert_eq!(
            write_frame(&mut out, &[], 8),
            Err(ProtocolError::InvalidLength)
        );
        assert!(out.is_empty());
    }

    #[test]
    fn render3_request_roundtrip_through_frame() {
        let req = Request::Load(LoadParams::new("https://example.com/", Some(5000)).unwrap());
        let mut wire = Vec::new();
        write_request(&mut wire, RequestId::new(7).unwrap(), &req).unwrap();
        let body = read_frame(&mut wire.as_slice(), MAX_REQUEST_FRAME_BYTES).unwrap();
        let v = parse_json(&body).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "load");
        assert_eq!(v["params"]["url"], "https://example.com/");
    }

    #[test]
    fn render3_response_roundtrip_and_limits() {
        let body = br#"{"v":1,"id":2,"result":{"final_url":"https://example.com/","status":200}}"#;
        let r = read_response(
            &mut framed(body).as_slice(),
            Method::Load,
            MAX_RESPONSE_FRAME_BYTES,
        )
        .unwrap();
        assert!(matches!(r.outcome, Ok(MethodResult::Load(_))));
        assert_eq!(response_frame_limit(Method::Screenshot), 24 * 1024 * 1024);
        assert_eq!(response_frame_limit(Method::Load), 1024 * 1024);
    }

    #[test]
    fn render3_effective_limit_never_raises_fixed() {
        assert_eq!(effective_limit(100, Some(1_000_000)), 100);
        assert_eq!(effective_limit(100, Some(10)), 10);
        assert_eq!(effective_limit(100, None), 100);
        assert_eq!(effective_limit(100, Some(u64::MAX)), 100);
    }

    #[test]
    fn render3_hello_protocol_mismatch() {
        let hello = |min: u64, max: u64| {
            format!(
                r#"{{"v":1,"event":"hello","data":{{"host_version":"0","servo_version":"0","protocol":{{"min":{min},"max":{max}}},"capabilities":[],"limits":{{"request_frame_bytes":1,"response_frame_bytes":1,"hello_frame_bytes":1,"screenshot_frame_bytes":1,"url_bytes":1,"selector_bytes":1,"error_message_bytes":1}}}}}}"#
            )
        };
        assert_eq!(
            decode_hello(hello(2, 3).as_bytes()).unwrap_err(),
            ProtocolError::ProtocolMismatch { min: 2, max: 3 }
        );
        // スキーマは min・max とも minimum: 1 のため、0 は互換性判定の前に不正メッセージ
        for (min, max) in [(0, 0), (0, 1), (1, 0)] {
            assert!(matches!(
                decode_hello(hello(min, max).as_bytes()),
                Err(ProtocolError::InvalidMessage(_))
            ));
        }
        assert!(matches!(
            decode_hello(hello(3, 2).as_bytes()),
            Err(ProtocolError::InvalidMessage(_))
        ));
        assert!(decode_hello(hello(1, 2).as_bytes()).is_ok());
    }

    #[test]
    fn render3_error_display_hides_content() {
        let e = ProtocolError::InvalidJson;
        assert_eq!(e.to_string(), "frame is not valid json");
    }
}

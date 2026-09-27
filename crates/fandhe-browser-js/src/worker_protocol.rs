//! 親プロセスと JS 評価用の子プロセスの間で stdio 越しにやり取りする
//! バイナリプロトコルのフレーミング・コーデック（`JS-1`・`TASK-29`・
//! Issue #503「JS 評価の子プロセス分離」設計書 §3.5・§7 W2）。
//!
//! 呼び出し元（将来）: 子プロセスの入口（[`super::worker`]。W3 で追加）と
//! 親側のプロキシ（[`super::process_engine`]。W4 で追加）の双方が本
//! モジュールを経由してフレームを送受信する。**`v8` crate には依存しない**
//! （子・親どちらが読んでも、この段階では V8 固有の型・API を一切参照
//! しない設計にする。テストも `js-v8` feature なしで実行できる）。
//!
//! # フレーム形式
//!
//! `u32 LE len`（タグ 1 バイトを含むペイロード全体のバイト数）＋
//! `u8 tag`（[`tag`] モジュール参照）＋ `payload`（`len - 1` バイト）。
//!
//! # tag の一覧（設計書 §3.5）
//!
//! - [`tag::HELLO`]: 子 → 親。ハンドシェイク（[`encode_hello`]）
//! - [`tag::EVALUATE`]: 親 → 子。評価対象のスクリプト全文
//!   （[`encode_evaluate`]）
//! - [`tag::RESULT`]: 子 → 親。評価結果（[`encode_js_value`]）
//! - [`tag::ERROR`]: 子 → 親。評価失敗（[`encode_error`]）
//! - [`tag::NATIVE_CALL`]・[`tag::NATIVE_RETURN`]: 逆方向 RPC
//!   （グローバル関数注入・DOM 風バインディングの子プロセス対応。
//!   `TASK-29.4`/`29.5` 以降の別 Issue でペイロード形式を定める）。
//!   本 Issue では tag 番号の予約のみ行い、エンコード・デコード関数は
//!   実装しない（実装済みを装わない。REPAIR-3）
//! - [`tag::SHUTDOWN`]: 親 → 子。ペイロードなし
//!
//! # 検証方針（外部入力として扱う。coding-rust.md「外部入力」節）
//!
//! 親から見て子は untrusted な JS を実行しているプロセスであり、子から
//! 見ても親からのバイトは信頼できる保証がない（プロトコル自体にバグが
//! あれば壊れたフレームが来うる）。このため本モジュールは双方向とも
//! 「送信側を信頼しない」前提で実装する。
//!
//! - フレーム長は読み取る**前**に上限（[`read_frame`] の `max_payload_len`
//!   引数。呼び出し側が方向ごとの上限を渡す）と比較し、上限を超える場合は
//!   ペイロードを確保する前に `Err` を返す（無制限確保による DoS を防ぐ。
//!   security.md・OWASP A04）
//! - `unwrap`・`expect`・添字アクセス（`[]`）は使わない。`get()`・
//!   `try_into()`・`str::from_utf8` を使う
//! - 未知の tag・不正な UTF-8・想定より短いペイロードは
//!   [`ProtocolError`] として呼び出し側に返す。呼び出し側（`super::worker`・
//!   `super::process_engine`）がこれをプロトコル違反として扱い、相手側の
//!   プロセスを終了させる方針は W3・W4 のドキュメントコメントを参照

use std::io::{self, Read, Write};

use super::engine_trait::{EngineKind, JsValue};

/// フレームの `tag` バイトの値。
pub(crate) mod tag {
    /// 子 → 親。ハンドシェイク（[`super::encode_hello`]）。
    pub(crate) const HELLO: u8 = 1;
    /// 親 → 子。評価対象のスクリプト全文（[`super::encode_evaluate`]）。
    pub(crate) const EVALUATE: u8 = 2;
    /// 子 → 親。評価結果（[`super::encode_js_value`]）。
    pub(crate) const RESULT: u8 = 3;
    /// 子 → 親。評価失敗（[`super::encode_error`]）。
    pub(crate) const ERROR: u8 = 4;
    /// 予約: 子 → 親。逆方向 RPC（グローバル関数注入・DOM 風バインディング。
    /// `TASK-29.4`/`29.5` 以降の別 Issue でペイロード形式を定める）。
    #[allow(dead_code, reason = "本 Issue では tag 番号の予約のみ行う（REPAIR-3）")]
    pub(crate) const NATIVE_CALL: u8 = 5;
    /// 予約: 親 → 子。逆方向 RPC の戻り値。
    #[allow(dead_code, reason = "本 Issue では tag 番号の予約のみ行う（REPAIR-3）")]
    pub(crate) const NATIVE_RETURN: u8 = 6;
    /// 親 → 子。ペイロードなし。
    pub(crate) const SHUTDOWN: u8 = 7;
}

/// [`super::process_engine`] が親 → 子のフレーム読み取りに使う上限
/// （ペイロードのバイト数。タグ 1 バイトを含まない。設計書 §3.5）。
/// [`super::worker`] のスクリプト長上限（`MAX_SCRIPT_SOURCE_BYTES`。1 MiB）
/// に余裕（64 KiB）を足した値。
pub(crate) const MAX_FRAME_PAYLOAD_PARENT_TO_CHILD: usize = 1_048_576 + 65_536;

/// [`super::worker`] が子 → 親のフレーム読み取りに使う上限（ペイロードの
/// バイト数。設計書 §3.5）。結果文字列の上限（1M UTF-16 単位。UTF-8 では
/// 最悪 3 MiB）に余裕（64 KiB）を足した値。
pub(crate) const MAX_FRAME_PAYLOAD_CHILD_TO_PARENT: usize = 3 * 1_048_576 + 65_536;

/// エラーメッセージのペイロードに許容する最大バイト数（4 KiB。設計書
/// §3.5）。[`encode_error`] を呼ぶ側が送信前に切り詰める（本モジュールは
/// 切り詰め自体は行わない。呼び出し元が既存の
/// `MAX_ERROR_MESSAGE_CHARS`/`truncate_error_message` 相当の上限を適用
/// 済みであることを前提とする）。
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

/// フレーム・値のデコードに失敗したことを表すエラー（`JS-1`・`TASK-29`・
/// Issue #503）。
///
/// 呼び出し元（`super::worker`・`super::process_engine`）は、この
/// エラーをプロトコル違反として扱い、相手側のプロセスとの通信を打ち切る
/// （設計書 §3.5「プロトコル違反は子を kill する」）。
#[derive(Debug)]
pub(crate) enum ProtocolError {
    /// 下層の I/O エラー（読み取り・書き込み失敗）。
    Io(io::Error),
    /// フレーム長が 0（タグバイト自体が存在しない）。
    EmptyFrame,
    /// ペイロード長が呼び出し側の指定した上限を超えた。
    FrameTooLarge { len: usize, max: usize },
    /// 未知の tag バイト。
    UnknownTag(u8),
    /// 値のデコード時に、未知の [`JsValue`] タグバイトを受け取った。
    InvalidValueTag(u8),
    /// 真偽値の表現が `0`/`1` のどちらでもなかった。
    InvalidBoolByte(u8),
    /// 未知の [`ErrorKind`] タグバイト。
    InvalidErrorKind(u8),
    /// 未知のエンジン種別バイト（`Hello` フレームの `engine` フィールド）。
    InvalidEngineByte(u8),
    /// ペイロードが期待する長さに満たない（切り詰められている）。
    Truncated,
    /// 文字列として解釈すべきバイト列が不正な UTF-8 だった。
    InvalidUtf8,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "worker protocol I/O error: {err}"),
            Self::EmptyFrame => write!(f, "worker protocol frame has zero length"),
            Self::FrameTooLarge { len, max } => write!(
                f,
                "worker protocol frame payload of {len} bytes exceeds the {max}-byte limit"
            ),
            Self::UnknownTag(tag) => write!(f, "worker protocol frame has unknown tag {tag}"),
            Self::InvalidValueTag(tag) => write!(f, "worker protocol value has unknown tag {tag}"),
            Self::InvalidBoolByte(byte) => {
                write!(f, "worker protocol boolean byte {byte} is neither 0 nor 1")
            }
            Self::InvalidErrorKind(byte) => {
                write!(f, "worker protocol error kind byte {byte} is unknown")
            }
            Self::InvalidEngineByte(byte) => {
                write!(f, "worker protocol engine byte {byte} is unknown")
            }
            Self::Truncated => write!(f, "worker protocol payload is truncated"),
            Self::InvalidUtf8 => write!(f, "worker protocol payload is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// [`read_frame`] の内部処理: `buf` を完全に埋めるまで読み込む。
///
/// 通常の `Read::read_exact` と異なり、**1 バイトも読めないまま EOF に
/// 達した場合**（＝フレームの区切りとして正当な EOF）と、**一部だけ読めた
/// 状態で EOF に達した場合**（＝フレームの途中で相手が終了した異常な
/// 状態）を区別して返す。前者は `Ok(false)`、後者は
/// [`io::ErrorKind::UnexpectedEof`] の `Err` にする。
fn read_exact_or_clean_eof(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "worker protocol stream ended in the middle of a frame",
                ));
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

/// 1 フレームを書き出す（`u32 LE len` ＋ `tag` ＋ `payload`）。
///
/// 呼び出し側（`super::worker`・`super::process_engine`）がフレームごとに
/// `flush` することを想定し、本関数自体は `flush` しない（複数フレームを
/// まとめて `flush` したい呼び出し側の裁量に委ねる）。
pub(crate) fn write_frame(
    writer: &mut impl Write,
    tag: u8,
    payload: &[u8],
) -> Result<(), ProtocolError> {
    // タグ 1 バイト分を含めた長さを u32 として送る。呼び出し側が渡す
    // ペイロードは本モジュールの `MAX_FRAME_PAYLOAD_*` 定数を超えない
    // 前提（呼び出し元が送信前に検証する）だが、念のため `u32` へ収まる
    // ことを検査してから書き込む（無検証のキャストによる切り捨てを防ぐ。
    // coding-rust.md「外部入力」節）。
    let len_with_tag = payload
        .len()
        .checked_add(1)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or_else(|| {
            ProtocolError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker protocol payload is too large to encode a u32 frame length",
            ))
        })?;
    writer.write_all(&len_with_tag.to_le_bytes())?;
    writer.write_all(&[tag])?;
    writer.write_all(payload)?;
    Ok(())
}

/// 1 フレームを読み取る。
///
/// 戻り値:
/// - `Ok(Some((tag, payload)))`: フレームを 1 つ読み取れた
/// - `Ok(None)`: フレームの区切り（長さプレフィックスの先頭）で相手が
///   ストリームを閉じた（正常終了。設計書 §3.2「stdin を閉じる。子は
///   EOF を受けて正常に終了する」に対応する）
/// - `Err(_)`: フレームの途中で終了した、長さが `max_payload_len` を
///   超えた、その他の I/O エラー
///
/// `max_payload_len` は呼び出し側が方向ごとに渡す（親から子は
/// [`MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`]、子から親は
/// [`MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`]）。ペイロードを確保する**前**に
/// 検査するため、上限超過を主張するフレームに対して実際のメモリ確保は
/// 発生しない（OWASP A04「不安全な設計」対策）。
pub(crate) fn read_frame(
    reader: &mut impl Read,
    max_payload_len: usize,
) -> Result<Option<(u8, Vec<u8>)>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_clean_eof(reader, &mut len_buf)? {
        return Ok(None);
    }
    let len_with_tag = u32::from_le_bytes(len_buf) as usize;
    if len_with_tag == 0 {
        return Err(ProtocolError::EmptyFrame);
    }
    let payload_len = len_with_tag - 1;
    if payload_len > max_payload_len {
        return Err(ProtocolError::FrameTooLarge {
            len: payload_len,
            max: max_payload_len,
        });
    }

    let mut tag_buf = [0u8; 1];
    reader.read_exact(&mut tag_buf)?;

    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload)?;

    Ok(Some((tag_buf[0], payload)))
}

/// [`JsValue`] を、[`tag::RESULT`] フレームのペイロード（や将来の
/// `NativeCall` 引数列）として使えるバイト列へ追記する。
///
/// 表現: `u8 tag`（0=Undefined, 1=Null, 2=Bool, 3=Number, 4=String）＋
/// 種別ごとの値（Bool は `u8`、Number は `f64 LE`、String は
/// `u32 LE len` ＋ UTF-8 バイト列）。`String` を自己区切り（長さ明示）に
/// しているため、将来 `Vec<JsValue>` を連結して送る際（`NativeCall`）にも
/// そのまま再利用できる。
///
/// `JsValue` は `#[non_exhaustive]` だが、本 crate の内側からの `match` は
/// 既知の全 variant を網羅すれば `_` 分岐は不要（`#[non_exhaustive]` が
/// 制限するのは他 crate からの網羅性判定のみ）。
pub(crate) fn encode_js_value(value: &JsValue, out: &mut Vec<u8>) -> Result<(), ProtocolError> {
    match value {
        JsValue::Undefined => out.push(0),
        JsValue::Null => out.push(1),
        JsValue::Bool(b) => {
            out.push(2);
            out.push(u8::from(*b));
        }
        JsValue::Number(n) => {
            out.push(3);
            out.extend_from_slice(&n.to_le_bytes());
        }
        JsValue::String(s) => {
            out.push(4);
            let len = u32::try_from(s.len()).map_err(|_| {
                ProtocolError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "worker protocol string value is too large to encode a u32 length",
                ))
            })?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
    Ok(())
}

/// [`encode_js_value`] の逆変換。`buf` の先頭から 1 つの [`JsValue`] を
/// 読み取り、値と消費したバイト数を返す（将来 `Vec<JsValue>` を連結して
/// 読む際に、消費量から次の値の開始位置を計算できるようにするため）。
pub(crate) fn decode_js_value(buf: &[u8]) -> Result<(JsValue, usize), ProtocolError> {
    let tag = *buf.first().ok_or(ProtocolError::Truncated)?;
    match tag {
        0 => Ok((JsValue::Undefined, 1)),
        1 => Ok((JsValue::Null, 1)),
        2 => {
            let byte = *buf.get(1).ok_or(ProtocolError::Truncated)?;
            match byte {
                0 => Ok((JsValue::Bool(false), 2)),
                1 => Ok((JsValue::Bool(true), 2)),
                other => Err(ProtocolError::InvalidBoolByte(other)),
            }
        }
        3 => {
            let bytes: [u8; 8] = buf
                .get(1..9)
                .ok_or(ProtocolError::Truncated)?
                .try_into()
                .map_err(|_| ProtocolError::Truncated)?;
            Ok((JsValue::Number(f64::from_le_bytes(bytes)), 9))
        }
        4 => {
            let len_bytes: [u8; 4] = buf
                .get(1..5)
                .ok_or(ProtocolError::Truncated)?
                .try_into()
                .map_err(|_| ProtocolError::Truncated)?;
            let len = u32::from_le_bytes(len_bytes) as usize;
            let start: usize = 5;
            let end = start.checked_add(len).ok_or(ProtocolError::Truncated)?;
            let str_bytes = buf.get(start..end).ok_or(ProtocolError::Truncated)?;
            let s = std::str::from_utf8(str_bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok((JsValue::String(s.to_string()), end))
        }
        other => Err(ProtocolError::InvalidValueTag(other)),
    }
}

/// [`tag::HELLO`] フレームのペイロードを組み立てる。
///
/// 表現: `u16 LE protocol_version` ＋ `u8 engine`（0=V8, 1=Boa）。
pub(crate) fn encode_hello(protocol_version: u16, engine: EngineKind) -> Vec<u8> {
    let mut out = Vec::with_capacity(3);
    out.extend_from_slice(&protocol_version.to_le_bytes());
    out.push(match engine {
        EngineKind::V8 => 0,
        EngineKind::Boa => 1,
    });
    out
}

/// [`encode_hello`] の逆変換。
pub(crate) fn decode_hello(payload: &[u8]) -> Result<(u16, EngineKind), ProtocolError> {
    let version_bytes: [u8; 2] = payload
        .get(0..2)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let version = u16::from_le_bytes(version_bytes);
    let engine_byte = *payload.get(2).ok_or(ProtocolError::Truncated)?;
    let engine = match engine_byte {
        0 => EngineKind::V8,
        1 => EngineKind::Boa,
        other => return Err(ProtocolError::InvalidEngineByte(other)),
    };
    Ok((version, engine))
}

/// [`tag::EVALUATE`] フレームのペイロードを組み立てる（スクリプト全文の
/// UTF-8 バイト列そのもの。フレーム長で境界が定まるため追加の長さ
/// プレフィックスは持たない）。
pub(crate) fn encode_evaluate(script: &str) -> Vec<u8> {
    script.as_bytes().to_vec()
}

/// [`encode_evaluate`] の逆変換。
pub(crate) fn decode_evaluate(payload: &[u8]) -> Result<String, ProtocolError> {
    std::str::from_utf8(payload)
        .map(str::to_string)
        .map_err(|_| ProtocolError::InvalidUtf8)
}

/// [`tag::ERROR`] フレームが表す失敗の種別（`JS-1`・Issue #503 設計書
/// §3.3）。
///
/// 呼び出し元（`super::process_engine`）はこれを
/// [`super::engine_trait::JsEngineError`] へ変換する（W5 で対応表を実装
/// する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// スクリプト評価が失敗した（構文エラー・実行時例外）。
    Evaluation,
    /// グローバル関数・DOM 風オブジェクトの登録に失敗した（予約。
    /// `TASK-29.4`/`29.5` 以降で子プロセス側に同種の失敗経路が入った
    /// 際に使う）。
    #[allow(
        dead_code,
        reason = "本 Issue では tag 番号同様に variant のみ予約する（REPAIR-3）"
    )]
    Binding,
    /// 子の監視スレッド（watchdog）が実行時間の上限で打ち切った
    /// （設計書 §3.3 の表「子の watchdog が打ち切った」行）。
    Timeout,
}

impl ErrorKind {
    fn to_byte(self) -> u8 {
        match self {
            Self::Evaluation => 0,
            Self::Binding => 1,
            Self::Timeout => 2,
        }
    }

    fn from_byte(byte: u8) -> Result<Self, ProtocolError> {
        match byte {
            0 => Ok(Self::Evaluation),
            1 => Ok(Self::Binding),
            2 => Ok(Self::Timeout),
            other => Err(ProtocolError::InvalidErrorKind(other)),
        }
    }
}

/// [`tag::ERROR`] フレームのペイロードを組み立てる。
///
/// 表現: `u8 kind`（[`ErrorKind::to_byte`]）＋ `message` の UTF-8 バイト列
/// （フレーム長で境界が定まる）。呼び出し側が送信前に
/// [`MAX_ERROR_MESSAGE_BYTES`] へ切り詰め済みであることを前提とする
/// （本関数自体は切り詰めない）。
pub(crate) fn encode_error(kind: ErrorKind, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + message.len());
    out.push(kind.to_byte());
    out.extend_from_slice(message.as_bytes());
    out
}

/// [`encode_error`] の逆変換。
pub(crate) fn decode_error(payload: &[u8]) -> Result<(ErrorKind, String), ProtocolError> {
    let kind_byte = *payload.first().ok_or(ProtocolError::Truncated)?;
    let kind = ErrorKind::from_byte(kind_byte)?;
    let message_bytes = payload.get(1..).ok_or(ProtocolError::Truncated)?;
    let message = std::str::from_utf8(message_bytes).map_err(|_| ProtocolError::InvalidUtf8)?;
    Ok((kind, message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// JS-1・TASK-29・Issue #503: `write_frame`/`read_frame` の往復で
    /// タグ・ペイロードが一致すること。
    #[test]
    fn js_1_frame_round_trip_preserves_tag_and_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::EVALUATE, b"1 + 1").expect("write must succeed");

        let mut cursor = Cursor::new(buf);
        let (read_tag, payload) = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("read must succeed")
            .expect("frame must be present");
        assert_eq!(read_tag, tag::EVALUATE);
        assert_eq!(payload, b"1 + 1");
    }

    /// JS-1: ストリームの先頭（フレーム境界）で EOF になった場合は
    /// `Ok(None)` を返すこと（正常終了の合図として区別できる）。
    #[test]
    fn js_1_read_frame_returns_none_on_clean_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("clean eof must not be an error");
        assert!(result.is_none());
    }

    /// JS-1: フレームの途中（長さプレフィックスの一部だけ）で EOF に
    /// なった場合は `Err` を返すこと（正常終了と区別する）。
    #[test]
    fn js_1_read_frame_errors_on_eof_mid_frame() {
        let mut cursor = Cursor::new(vec![1u8, 2u8]); // 4 バイトの長さに満たない
        let result = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);
        assert!(matches!(result, Err(ProtocolError::Io(_))));
    }

    /// JS-1・codex レビュー想定の回帰確認: ペイロード長が上限を超える
    /// フレームは、ペイロードを確保する前に `Err` を返すこと（OWASP A04
    /// 対策の検証）。
    #[test]
    fn js_1_read_frame_rejects_oversized_payload_before_allocating() {
        let max = 16usize;
        let oversized_len_with_tag = u32::try_from(max + 2).expect("fits in u32");
        let mut buf = Vec::new();
        buf.extend_from_slice(&oversized_len_with_tag.to_le_bytes());
        // タグ・ペイロードは書かない（上限検査が確保より前に働くことの
        // 確認。もし実装が誤ってペイロード分を読もうとすれば、ここで
        // 別のエラー（EOF）になり、期待する `FrameTooLarge` にならない
        // ことでも検知できる）。
        let mut cursor = Cursor::new(buf);
        match read_frame(&mut cursor, max) {
            Err(ProtocolError::FrameTooLarge { len, max: max_seen }) => {
                assert_eq!(len, max + 1);
                assert_eq!(max_seen, max);
            }
            other => panic!("expected FrameTooLarge, got: {other:?}"),
        }
    }

    /// JS-1: 長さが 0（タグバイトすら無い）フレームは `Err` を返すこと。
    #[test]
    fn js_1_read_frame_rejects_zero_length_frame() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        let mut cursor = Cursor::new(buf);
        assert!(matches!(
            read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD),
            Err(ProtocolError::EmptyFrame)
        ));
    }

    /// JS-1: 未知の tag バイトは呼び出し側（`read_frame` の戻り値を見た
    /// `super::worker`/`super::process_engine`）が判別できるよう、
    /// `read_frame` 自体はタグの妥当性を検証せずそのまま返すこと
    /// （tag の意味解釈は呼び出し側の責務であることの確認）。
    #[test]
    fn js_1_read_frame_passes_through_unknown_tag_for_caller_to_reject() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 200, b"").expect("write must succeed");
        let mut cursor = Cursor::new(buf);
        let (read_tag, payload) = read_frame(&mut cursor, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("read must succeed")
            .expect("frame must be present");
        assert_eq!(read_tag, 200);
        assert!(payload.is_empty());
    }

    /// JS-1: `JsValue` の全 variant が `encode_js_value`/`decode_js_value`
    /// の往復で一致すること。
    #[test]
    fn js_1_js_value_round_trip_for_all_variants() {
        let values = [
            JsValue::Undefined,
            JsValue::Null,
            JsValue::Bool(true),
            JsValue::Bool(false),
            JsValue::Number(42.5),
            JsValue::Number(-1.0),
            JsValue::String(String::new()),
            JsValue::String("fandhe-browser".to_string()),
        ];
        for value in values {
            let mut buf = Vec::new();
            encode_js_value(&value, &mut buf).expect("encode must succeed");
            let (decoded, consumed) = decode_js_value(&buf).expect("decode must succeed");
            assert_eq!(decoded, value);
            assert_eq!(consumed, buf.len());
        }
    }

    /// JS-1: 複数の `JsValue` を連結したバッファから、消費バイト数を
    /// 使って順番に読み出せること（将来の `NativeCall` 引数列を想定した
    /// 契約の検証）。
    #[test]
    fn js_1_js_value_sequence_can_be_decoded_using_consumed_length() {
        let mut buf = Vec::new();
        encode_js_value(&JsValue::Number(1.0), &mut buf).expect("encode must succeed");
        encode_js_value(&JsValue::String("two".to_string()), &mut buf)
            .expect("encode must succeed");
        encode_js_value(&JsValue::Bool(true), &mut buf).expect("encode must succeed");

        let mut offset = 0;
        let (first, consumed) = decode_js_value(&buf[offset..]).expect("decode must succeed");
        assert_eq!(first, JsValue::Number(1.0));
        offset += consumed;
        let (second, consumed) = decode_js_value(&buf[offset..]).expect("decode must succeed");
        assert_eq!(second, JsValue::String("two".to_string()));
        offset += consumed;
        let (third, consumed) = decode_js_value(&buf[offset..]).expect("decode must succeed");
        assert_eq!(third, JsValue::Bool(true));
        offset += consumed;
        assert_eq!(offset, buf.len());
    }

    /// JS-1: 未知の値タグは `Err(InvalidValueTag)` を返すこと。
    #[test]
    fn js_1_decode_js_value_rejects_unknown_tag() {
        assert!(matches!(
            decode_js_value(&[9]),
            Err(ProtocolError::InvalidValueTag(9))
        ));
    }

    /// JS-1: 空のバッファは `Err(Truncated)` を返すこと（添字アクセスで
    /// panic しないことの確認）。
    #[test]
    fn js_1_decode_js_value_rejects_empty_buffer() {
        assert!(matches!(
            decode_js_value(&[]),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-1: 文字列値の長さプレフィックスが実際のバッファより大きい場合、
    /// 添字アクセスで panic せず `Err(Truncated)` を返すこと。
    #[test]
    fn js_1_decode_js_value_rejects_string_with_length_exceeding_buffer() {
        let mut buf = vec![4u8]; // String タグ
        buf.extend_from_slice(&100u32.to_le_bytes()); // 実際には無い長さ
        buf.extend_from_slice(b"short");
        assert!(matches!(
            decode_js_value(&buf),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-1: 不正な UTF-8 バイト列は `Err(InvalidUtf8)` を返すこと。
    #[test]
    fn js_1_decode_js_value_rejects_invalid_utf8_string() {
        let mut buf = vec![4u8];
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_js_value(&buf),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    /// JS-1: `Hello` フレームの往復（`EngineKind::V8`/`Boa` の双方）。
    #[test]
    fn js_1_hello_round_trip() {
        for engine in [EngineKind::V8, EngineKind::Boa] {
            let payload = encode_hello(1, engine);
            let (version, decoded_engine) = decode_hello(&payload).expect("decode must succeed");
            assert_eq!(version, 1);
            assert_eq!(decoded_engine, engine);
        }
    }

    /// JS-1: `Hello` フレームの未知エンジンバイトは `Err` を返すこと。
    #[test]
    fn js_1_decode_hello_rejects_unknown_engine_byte() {
        let payload = [1u8, 0u8, 99u8];
        assert!(matches!(
            decode_hello(&payload),
            Err(ProtocolError::InvalidEngineByte(99))
        ));
    }

    /// JS-1: `Evaluate` フレームの往復。
    #[test]
    fn js_1_evaluate_round_trip() {
        let script = "while (true) {}";
        let payload = encode_evaluate(script);
        let decoded = decode_evaluate(&payload).expect("decode must succeed");
        assert_eq!(decoded, script);
    }

    /// JS-1: `Error` フレームの往復（`ErrorKind` の全 variant）。
    #[test]
    fn js_1_error_round_trip_for_all_kinds() {
        for kind in [
            ErrorKind::Evaluation,
            ErrorKind::Binding,
            ErrorKind::Timeout,
        ] {
            let payload = encode_error(kind, "boom");
            let (decoded_kind, message) = decode_error(&payload).expect("decode must succeed");
            assert_eq!(decoded_kind, kind);
            assert_eq!(message, "boom");
        }
    }

    /// JS-1: `Error` フレームの未知 kind バイトは `Err` を返すこと。
    #[test]
    fn js_1_decode_error_rejects_unknown_kind() {
        let payload = [200u8, b'x'];
        assert!(matches!(
            decode_error(&payload),
            Err(ProtocolError::InvalidErrorKind(200))
        ));
    }

    /// JS-1: 各 `ProtocolError` variant の `Display` 実装が空文字列を
    /// 返さないこと。`UnknownTag` は `read_frame` 自体は使わず、呼び出し側
    /// （`super::worker`・`super::process_engine`。W3・W4）がタグの意味
    /// 解釈をした結果として構築する想定の variant であるため、ここで
    /// `Display` の存在を確認しておく。
    #[test]
    fn js_1_protocol_error_display_messages_are_non_empty() {
        let errors: Vec<ProtocolError> = vec![
            ProtocolError::Io(io::Error::other("boom")),
            ProtocolError::EmptyFrame,
            ProtocolError::FrameTooLarge { len: 10, max: 5 },
            ProtocolError::UnknownTag(9),
            ProtocolError::InvalidValueTag(9),
            ProtocolError::InvalidBoolByte(9),
            ProtocolError::InvalidErrorKind(9),
            ProtocolError::InvalidEngineByte(9),
            ProtocolError::Truncated,
            ProtocolError::InvalidUtf8,
        ];
        for err in errors {
            assert!(!err.to_string().is_empty());
        }
    }

    /// JS-1: エラーメッセージのペイロード上限が 4 KiB であること（設計書
    /// §3.5 の具体値の回帰確認）。
    #[test]
    fn js_1_max_error_message_bytes_is_4_kib() {
        assert_eq!(MAX_ERROR_MESSAGE_BYTES, 4096);
    }

    /// JS-1: 方向ごとのフレーム上限が設計書 §3.5 の具体値
    /// （親→子: 1 MiB + 64 KiB、子→親: 3 MiB + 64 KiB）と一致すること。
    /// `MAX_FRAME_PAYLOAD_CHILD_TO_PARENT` は W3・W4（子・親の実装）が
    /// `read_frame` へ渡す上限であり、本モジュール単体では他に使用箇所が
    /// ないため、ここで具体値を固定する回帰テストとして参照する。
    #[test]
    fn js_1_max_frame_payload_limits_match_the_design_doc() {
        assert_eq!(
            MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
            1_048_576 + 65_536,
            "parent-to-child limit must be 1 MiB + 64 KiB"
        );
        assert_eq!(
            MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
            3 * 1_048_576 + 65_536,
            "child-to-parent limit must be 3 MiB + 64 KiB"
        );
    }

    /// JS-1: 想定される tag 値が予約分も含め重複しないこと（プロトコル
    /// 定義の回帰確認）。
    #[test]
    fn js_1_tag_values_are_distinct() {
        let tags = [
            tag::HELLO,
            tag::EVALUATE,
            tag::RESULT,
            tag::ERROR,
            tag::NATIVE_CALL,
            tag::NATIVE_RETURN,
            tag::SHUTDOWN,
        ];
        let mut sorted = tags.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len(), "tag values must be distinct");
    }
}

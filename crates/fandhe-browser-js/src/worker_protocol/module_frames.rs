//! ES module 用フレームのコーデック・上限定数・module id 台帳
//! （`JS-4`・`JS-6`・`REPAIR-3`・Issue #1110・設計
//! `docs/design/es-module-design.md` §4）。
//!
//! 呼び出し元: 親側の [`super::super::process_engine`]（`JsEngine` のモジュール
//! 評価 API 5 メソッドを新フレームの送受信へ対応づける）と、子側の
//! [`super::super::worker`]（新 tag を検証して `ChildEngine` へ dispatch する。
//! boa・V8 は静的 import のグラフを評価し、動的 import・TLA は「未対応」の
//! `ERROR` を返す）。`v8` crate には依存しない（テストも feature
//! なしで動く）。
//!
//! # ワイヤ形式（プロトコル v6。整数はすべて LE）
//!
//! 文字列は `u16 len` ＋ UTF-8 本体。ただし「残り全部」の項目は長さを持たない。
//!
//! | tag | 方向 | ペイロード |
//! |---|---|---|
//! | `COMPILE_MODULE` | 親 → 子 | `u64 timeout_ms` ＋ `u16 url_len` ＋ url ＋ source（残り全部） |
//! | `MODULE_COMPILED` | 子 → 親 | `u32 module_id` ＋ `u16 count` ＋ count × specifier |
//! | `LINK_MODULE` | 親 → 子 | `u32 referrer` ＋ `u16 count` ＋ count × (specifier ＋ `u32 target`) |
//! | `EVALUATE_MODULE` | 親 → 子 | `EVALUATE` と同じ 16 バイトのヘッダ ＋ `u32 root` |
//! | `MODULE_RESULT` | 子 → 親 | `u8 status`（0 Fulfilled・1 Pending・2 Rejected）。Rejected のみ message（残り全部） |
//! | `TAKE_DYNAMIC_IMPORTS` | 親 → 子 | 空 |
//! | `DYNAMIC_IMPORTS` | 子 → 親 | `u16 count` ＋ count × (`u32 id` ＋ `u8 has_referrer` ＋ [`u32 referrer`] ＋ specifier) |
//! | `FINISH_DYNAMIC_IMPORT` | 親 → 子 | `EVALUATE` と同じ 16 バイトのヘッダ ＋ `u32 id` ＋ `u8 status`（0 Ok・1 Err）＋ Ok は `u32 module_id`、Err は message（残り全部） |
//!
//! link の応答は登録系と同じ `RESULT(Undefined)` の ack を流用する。finish の応答は
//! `import()` の Promise の確定結果を `MODULE_RESULT` で返す（Issue #1117）。
//!
//! # 検証方針（`JS-6`・coding-rust.md「外部入力」）
//!
//! 件数・長さは確保の**前**に上限と比較する。双方向とも送信側を信頼せず、
//! encode でも同じ上限を検査して `Result` を返す。違反は呼び出し側が
//! プロトコル違反として扱い、相手側プロセスを終了させる。specifier・URL は
//! untrusted で、エラーの `Display` には件数・長さ・種別だけを入れる。

use std::collections::HashSet;
use std::time::Duration;

use super::{
    EVALUATE_HEADER_BYTES, MAX_ERROR_MESSAGE_BYTES, MAX_EVALUATE_TIMEOUT,
    MAX_FRAME_PAYLOAD_CHILD_TO_PARENT, MAX_FRAME_PAYLOAD_PARENT_TO_CHILD, ProtocolError,
    decode_evaluate_header, encode_evaluate_header, wire_timeout,
};
use crate::engine_trait::{
    DynamicImportId, DynamicImportRequest, EvaluateOptions, ModuleEvaluation, ModuleId,
    ModuleRecord, ModuleRequest, ModuleResolution,
};

/// 1 モジュールの依存 specifier 数、および 1 回の `LINK_MODULE` の対応表の行数の
/// 上限（設計 §4 の例示値。`JS-6`・Issue #1110）。
pub(crate) const MAX_MODULE_REQUESTS: usize = 256;

/// specifier 1 件の最大バイト数（実用的な URL 長。`JS-6`・Issue #1110）。
///
/// 全項目が最大でも `MODULE_COMPILED` は 4 + 2 + 256 × (2 + 2048) = 524,806 B、
/// `LINK_MODULE` は 4 + 2 + 256 × (2 + 2048 + 4) = 525,830 B で、どちらも方向ごとの
/// フレーム上限（[`MAX_FRAME_PAYLOAD_CHILD_TO_PARENT`]・
/// [`MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`]）に収まる。
pub(crate) const MAX_MODULE_SPECIFIER_BYTES: usize = 2048;

/// モジュールの絶対 URL の最大バイト数。解決後の URL は specifier より長く
/// なりうるため別に設ける（`JS-6`・Issue #1110）。
///
/// `COMPILE_MODULE` の最悪値は 8 + 2 + 4096 + 1,048,576 = 1,052,682 B で
/// [`MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`]（1,114,112 B）に収まる。
pub(crate) const MAX_MODULE_URL_BYTES: usize = 4096;

/// 子 1 つ（1 世代）が保持できる compile 済みモジュール数の上限
/// （`MAX_REGISTERED_GLOBAL_FUNCTIONS` と同じ桁。`JS-6`・Issue #1110）。
/// ページ単位の `max_modules` は core 側の後続 issue で決める（設計 §7）。
pub(crate) const MAX_MODULES_PER_WORKER: usize = 1024;

/// 1 回の `DYNAMIC_IMPORTS` の件数、および親が保持する保留件数の上限。
/// 最悪値は 2 + 256 × (4 + 1 + 4 + 2 + 2048) = 527,106 B で子 → 親の
/// フレーム上限に収まる（`JS-6`・Issue #1110）。
pub(crate) const MAX_PENDING_DYNAMIC_IMPORTS: usize = 256;

/// `MODULE_RESULT` の status バイト。
const RESULT_FULFILLED: u8 = 0;
const RESULT_PENDING: u8 = 1;
const RESULT_REJECTED: u8 = 2;

/// `FINISH_DYNAMIC_IMPORT` の status バイト。
const FINISH_OK: u8 = 0;
const FINISH_ERR: u8 = 1;

/// ペイロードを前から読み進める境界検査つきの読み取り器。添字アクセスを
/// 使わず、不足は [`ProtocolError::Truncated`] にする。
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.pos.checked_add(n).ok_or(ProtocolError::Truncated)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(ProtocolError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(ProtocolError::Truncated)
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ProtocolError::Truncated)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// 残りのバイト列をすべて返して読み切った状態にする。
    fn rest(&mut self) -> &'a [u8] {
        let slice = self.buf.get(self.pos..).unwrap_or_default();
        self.pos = self.buf.len();
        slice
    }

    /// 末尾に余分なバイトが無いことを確認する。
    fn finish(self) -> Result<(), ProtocolError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ProtocolError::TrailingBytes {
                expected: self.pos,
                actual: self.buf.len(),
            })
        }
    }

    /// `u16 len` ＋ UTF-8 の specifier を読む。長さは本体を取り出す前に
    /// [`MAX_MODULE_SPECIFIER_BYTES`] と比較する。
    fn specifier(&mut self) -> Result<String, ProtocolError> {
        let len = usize::from(self.u16()?);
        if len > MAX_MODULE_SPECIFIER_BYTES {
            return Err(ProtocolError::ModuleSpecifierTooLong {
                len,
                max: MAX_MODULE_SPECIFIER_BYTES,
            });
        }
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| ProtocolError::InvalidUtf8)
    }

    /// `u16 count` を読み、確保の前に `max` と比較する。
    fn count(
        &mut self,
        max: usize,
        too_many: fn(usize, usize) -> ProtocolError,
    ) -> Result<usize, ProtocolError> {
        let count = usize::from(self.u16()?);
        if count > max {
            return Err(too_many(count, max));
        }
        Ok(count)
    }
}

fn too_many_requests(count: usize, max: usize) -> ProtocolError {
    ProtocolError::TooManyModuleRequests { count, max }
}

fn too_many_dynamic_imports(count: usize, max: usize) -> ProtocolError {
    ProtocolError::TooManyDynamicImports { count, max }
}

/// specifier の長さ上限を検査する（encode 側の送信前検査）。
fn check_specifier(specifier: &str) -> Result<(), ProtocolError> {
    if specifier.len() > MAX_MODULE_SPECIFIER_BYTES {
        return Err(ProtocolError::ModuleSpecifierTooLong {
            len: specifier.len(),
            max: MAX_MODULE_SPECIFIER_BYTES,
        });
    }
    Ok(())
}

/// 件数上限を検査する（encode 側）。
fn check_count(
    count: usize,
    max: usize,
    too_many: fn(usize, usize) -> ProtocolError,
) -> Result<u16, ProtocolError> {
    if count > max {
        return Err(too_many(count, max));
    }
    u16::try_from(count).map_err(|_| too_many(count, max))
}

/// specifier の重複を検出する。名前は保持せず種別だけ返す。
fn reject_duplicate_specifiers<'a>(
    specifiers: impl Iterator<Item = &'a str>,
) -> Result<(), ProtocolError> {
    let mut seen: HashSet<&str> = HashSet::new();
    for specifier in specifiers {
        if !seen.insert(specifier) {
            return Err(ProtocolError::DuplicateModuleSpecifier);
        }
    }
    Ok(())
}

fn push_specifier(out: &mut Vec<u8>, specifier: &str) -> Result<(), ProtocolError> {
    check_specifier(specifier)?;
    let len =
        u16::try_from(specifier.len()).map_err(|_| ProtocolError::ModuleSpecifierTooLong {
            len: specifier.len(),
            max: MAX_MODULE_SPECIFIER_BYTES,
        })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(specifier.as_bytes());
    Ok(())
}

// ---- COMPILE_MODULE ----

/// [`decode_compile_module`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompileModuleRequest {
    /// 子の watchdog 用のタイムアウト（[`MAX_EVALUATE_TIMEOUT`] でクランプ済み）。
    pub(crate) timeout: Duration,
    /// モジュールの絶対 URL（空でなく [`MAX_MODULE_URL_BYTES`] 以下）。
    pub(crate) url: String,
    /// モジュールのソース。
    pub(crate) source: String,
}

/// `COMPILE_MODULE` のペイロードを組み立てる（親 → 子）。
///
/// 呼び出し元: `process_engine` の `compile_module`。URL の空・長さ超過と、
/// 合計が [`MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`] を超えることを送信前に拒否する。
/// タイムアウトは [`wire_timeout`]（EVALUATE と同じ丸め規則）で変換する。
pub(crate) fn encode_compile_module(
    url: &str,
    source: &str,
    options: &EvaluateOptions,
) -> Result<Vec<u8>, ProtocolError> {
    if url.is_empty() {
        return Err(ProtocolError::EmptyModuleUrl);
    }
    if url.len() > MAX_MODULE_URL_BYTES {
        return Err(ProtocolError::ModuleUrlTooLong {
            len: url.len(),
            max: MAX_MODULE_URL_BYTES,
        });
    }
    let total = 8usize
        .saturating_add(2)
        .saturating_add(url.len())
        .saturating_add(source.len());
    if total > MAX_FRAME_PAYLOAD_PARENT_TO_CHILD {
        return Err(ProtocolError::FrameTooLarge {
            len: total,
            max: MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
        });
    }
    let url_len = u16::try_from(url.len()).map_err(|_| ProtocolError::ModuleUrlTooLong {
        len: url.len(),
        max: MAX_MODULE_URL_BYTES,
    })?;
    let timeout_ms = u64::try_from(wire_timeout(options).as_millis()).unwrap_or(u64::MAX);
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&timeout_ms.to_le_bytes());
    out.extend_from_slice(&url_len.to_le_bytes());
    out.extend_from_slice(url.as_bytes());
    out.extend_from_slice(source.as_bytes());
    Ok(out)
}

/// [`encode_compile_module`] の逆変換（子側。親を信頼しない）。
pub(crate) fn decode_compile_module(payload: &[u8]) -> Result<CompileModuleRequest, ProtocolError> {
    let mut r = Reader::new(payload);
    let timeout = Duration::from_millis(r.u64()?).min(MAX_EVALUATE_TIMEOUT);
    let url_len = usize::from(r.u16()?);
    if url_len > MAX_MODULE_URL_BYTES {
        return Err(ProtocolError::ModuleUrlTooLong {
            len: url_len,
            max: MAX_MODULE_URL_BYTES,
        });
    }
    if url_len == 0 {
        return Err(ProtocolError::EmptyModuleUrl);
    }
    let url = std::str::from_utf8(r.take(url_len)?)
        .map(str::to_string)
        .map_err(|_| ProtocolError::InvalidUtf8)?;
    let source = std::str::from_utf8(r.rest())
        .map(str::to_string)
        .map_err(|_| ProtocolError::InvalidUtf8)?;
    Ok(CompileModuleRequest {
        timeout,
        url,
        source,
    })
}

// ---- MODULE_COMPILED ----

/// `MODULE_COMPILED` のペイロードを組み立てる（子 → 親）。
pub(crate) fn encode_module_compiled(
    id: ModuleId,
    requests: &[ModuleRequest],
) -> Result<Vec<u8>, ProtocolError> {
    let count = check_count(requests.len(), MAX_MODULE_REQUESTS, too_many_requests)?;
    reject_duplicate_specifiers(requests.iter().map(ModuleRequest::specifier))?;
    let mut out = Vec::new();
    out.extend_from_slice(&id.raw().to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for request in requests {
        push_specifier(&mut out, request.specifier())?;
    }
    Ok(out)
}

/// [`encode_module_compiled`] の逆変換（親側。子は untrusted）。id の台帳
/// 照合は [`ModuleIdRegistry::register_compiled`] が行う。
pub(crate) fn decode_module_compiled(payload: &[u8]) -> Result<ModuleRecord, ProtocolError> {
    let mut r = Reader::new(payload);
    let id = ModuleId::from_raw(r.u32()?);
    let count = r.count(MAX_MODULE_REQUESTS, too_many_requests)?;
    let mut requests = Vec::with_capacity(count);
    for _ in 0..count {
        requests.push(ModuleRequest::new(r.specifier()?));
    }
    r.finish()?;
    reject_duplicate_specifiers(requests.iter().map(ModuleRequest::specifier))?;
    Ok(ModuleRecord::new(id, requests))
}

// ---- LINK_MODULE ----

/// `LINK_MODULE` のペイロードを組み立てる（親 → 子）。
pub(crate) fn encode_link_module(
    referrer: ModuleId,
    resolutions: &[ModuleResolution],
) -> Result<Vec<u8>, ProtocolError> {
    let count = check_count(resolutions.len(), MAX_MODULE_REQUESTS, too_many_requests)?;
    reject_duplicate_specifiers(resolutions.iter().map(ModuleResolution::specifier))?;
    let mut out = Vec::new();
    out.extend_from_slice(&referrer.raw().to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for resolution in resolutions {
        push_specifier(&mut out, resolution.specifier())?;
        out.extend_from_slice(&resolution.target().raw().to_le_bytes());
    }
    Ok(out)
}

/// [`encode_link_module`] の逆変換（子側）。
pub(crate) fn decode_link_module(
    payload: &[u8],
) -> Result<(ModuleId, Vec<ModuleResolution>), ProtocolError> {
    let mut r = Reader::new(payload);
    let referrer = ModuleId::from_raw(r.u32()?);
    let count = r.count(MAX_MODULE_REQUESTS, too_many_requests)?;
    let mut resolutions = Vec::with_capacity(count);
    for _ in 0..count {
        let specifier = r.specifier()?;
        let target = ModuleId::from_raw(r.u32()?);
        resolutions.push(ModuleResolution::new(specifier, target));
    }
    r.finish()?;
    reject_duplicate_specifiers(resolutions.iter().map(ModuleResolution::specifier))?;
    Ok((referrer, resolutions))
}

// ---- EVALUATE_MODULE ----

/// `EVALUATE_MODULE` のペイロードを組み立てる（親 → 子）。ヘッダは
/// `EVALUATE` と共通（`wire_timeout` を共用）。
pub(crate) fn encode_evaluate_module(root: ModuleId, options: &EvaluateOptions) -> Vec<u8> {
    let mut out = Vec::with_capacity(EVALUATE_HEADER_BYTES + 4);
    out.extend_from_slice(&encode_evaluate_header(options));
    out.extend_from_slice(&root.raw().to_le_bytes());
    out
}

/// [`encode_evaluate_module`] の逆変換（子側）。
pub(crate) fn decode_evaluate_module(
    payload: &[u8],
) -> Result<(ModuleId, EvaluateOptions), ProtocolError> {
    let (options, body) = decode_evaluate_header(payload)?;
    let mut r = Reader::new(body);
    let root = ModuleId::from_raw(r.u32()?);
    r.finish()?;
    Ok((root, options))
}

// ---- MODULE_RESULT ----

/// `MODULE_RESULT` のペイロードを組み立てる（子 → 親）。
pub(crate) fn encode_module_result(
    evaluation: &ModuleEvaluation,
) -> Result<Vec<u8>, ProtocolError> {
    match evaluation {
        ModuleEvaluation::Fulfilled => Ok(vec![RESULT_FULFILLED]),
        ModuleEvaluation::Pending => Ok(vec![RESULT_PENDING]),
        ModuleEvaluation::Rejected { message } => {
            let total = message.len().saturating_add(1);
            if total > MAX_FRAME_PAYLOAD_CHILD_TO_PARENT {
                return Err(ProtocolError::FrameTooLarge {
                    len: total,
                    max: MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
                });
            }
            let mut out = Vec::with_capacity(total);
            out.push(RESULT_REJECTED);
            out.extend_from_slice(message.as_bytes());
            Ok(out)
        }
    }
}

/// [`encode_module_result`] の逆変換（親側。子は untrusted）。Rejected の
/// message 長の上限（`options.max_result_bytes()`）は呼び出し側が検査する。
pub(crate) fn decode_module_result(payload: &[u8]) -> Result<ModuleEvaluation, ProtocolError> {
    let mut r = Reader::new(payload);
    match r.u8()? {
        RESULT_FULFILLED => {
            r.finish()?;
            Ok(ModuleEvaluation::Fulfilled)
        }
        RESULT_PENDING => {
            r.finish()?;
            Ok(ModuleEvaluation::Pending)
        }
        RESULT_REJECTED => {
            let message = std::str::from_utf8(r.rest())
                .map(str::to_string)
                .map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok(ModuleEvaluation::rejected(message))
        }
        other => Err(ProtocolError::InvalidModuleResultStatus(other)),
    }
}

// ---- TAKE_DYNAMIC_IMPORTS ----

/// `TAKE_DYNAMIC_IMPORTS` のペイロード（空）。
pub(crate) fn encode_take_dynamic_imports() -> Vec<u8> {
    Vec::new()
}

/// `TAKE_DYNAMIC_IMPORTS` は空でなければならない（子側）。
pub(crate) fn decode_take_dynamic_imports(payload: &[u8]) -> Result<(), ProtocolError> {
    Reader::new(payload).finish()
}

// ---- DYNAMIC_IMPORTS ----

/// `DYNAMIC_IMPORTS` のペイロードを組み立てる（子 → 親）。
pub(crate) fn encode_dynamic_imports(
    requests: &[DynamicImportRequest],
) -> Result<Vec<u8>, ProtocolError> {
    let count = check_count(
        requests.len(),
        MAX_PENDING_DYNAMIC_IMPORTS,
        too_many_dynamic_imports,
    )?;
    reject_duplicate_ids(requests.iter().map(|r| r.id().raw()))?;
    let mut out = Vec::new();
    out.extend_from_slice(&count.to_le_bytes());
    for request in requests {
        out.extend_from_slice(&request.id().raw().to_le_bytes());
        match request.referrer() {
            Some(referrer) => {
                out.push(1);
                out.extend_from_slice(&referrer.raw().to_le_bytes());
            }
            None => out.push(0),
        }
        push_specifier(&mut out, request.specifier())?;
    }
    Ok(out)
}

/// [`encode_dynamic_imports`] の逆変換（親側。子は untrusted）。id の重複は
/// ここで、保留中 id との重複・未知の referrer は
/// [`ModuleIdRegistry::accept_dynamic_imports`] が検査する。
pub(crate) fn decode_dynamic_imports(
    payload: &[u8],
) -> Result<Vec<DynamicImportRequest>, ProtocolError> {
    let mut r = Reader::new(payload);
    let count = r.count(MAX_PENDING_DYNAMIC_IMPORTS, too_many_dynamic_imports)?;
    let mut requests = Vec::with_capacity(count);
    for _ in 0..count {
        let id = DynamicImportId::from_raw(r.u32()?);
        let referrer = match r.u8()? {
            0 => None,
            1 => Some(ModuleId::from_raw(r.u32()?)),
            other => return Err(ProtocolError::InvalidReferrerFlag(other)),
        };
        let specifier = r.specifier()?;
        requests.push(DynamicImportRequest::new(id, referrer, specifier));
    }
    r.finish()?;
    reject_duplicate_ids(requests.iter().map(|req| req.id().raw()))?;
    Ok(requests)
}

fn reject_duplicate_ids(ids: impl Iterator<Item = u32>) -> Result<(), ProtocolError> {
    let mut seen: HashSet<u32> = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(ProtocolError::DuplicateDynamicImportId);
        }
    }
    Ok(())
}

// ---- FINISH_DYNAMIC_IMPORT ----

/// `FINISH_DYNAMIC_IMPORT` のペイロードを組み立てる（親 → 子）。Err の message は
/// 呼び出し側が [`super::truncate_for_wire`] で切り詰め済みであること
/// （[`MAX_ERROR_MESSAGE_BYTES`] 超は拒否する）。
pub(crate) fn encode_finish_dynamic_import(
    id: DynamicImportId,
    result: &Result<ModuleId, String>,
    options: &EvaluateOptions,
) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    out.extend_from_slice(&encode_evaluate_header(options));
    out.extend_from_slice(&id.raw().to_le_bytes());
    match result {
        Ok(module) => {
            out.push(FINISH_OK);
            out.extend_from_slice(&module.raw().to_le_bytes());
        }
        Err(message) => {
            if message.len() > MAX_ERROR_MESSAGE_BYTES {
                return Err(ProtocolError::ErrorMessageTooLong {
                    len: message.len(),
                    max: MAX_ERROR_MESSAGE_BYTES,
                });
            }
            out.push(FINISH_ERR);
            out.extend_from_slice(message.as_bytes());
        }
    }
    Ok(out)
}

/// [`encode_finish_dynamic_import`] の逆変換（子側）。
pub(crate) fn decode_finish_dynamic_import(
    payload: &[u8],
) -> Result<(DynamicImportId, Result<ModuleId, String>, EvaluateOptions), ProtocolError> {
    let (options, body) = decode_evaluate_header(payload)?;
    let mut r = Reader::new(body);
    let id = DynamicImportId::from_raw(r.u32()?);
    match r.u8()? {
        FINISH_OK => {
            let module = ModuleId::from_raw(r.u32()?);
            r.finish()?;
            Ok((id, Ok(module), options))
        }
        FINISH_ERR => {
            let bytes = r.rest();
            if bytes.len() > MAX_ERROR_MESSAGE_BYTES {
                return Err(ProtocolError::ErrorMessageTooLong {
                    len: bytes.len(),
                    max: MAX_ERROR_MESSAGE_BYTES,
                });
            }
            let message = std::str::from_utf8(bytes)
                .map(str::to_string)
                .map_err(|_| ProtocolError::InvalidUtf8)?;
            Ok((id, Err(message), options))
        }
        other => Err(ProtocolError::InvalidFinishStatus(other)),
    }
}

// ---- 親側の id 台帳 ----

/// 親が子から通知された module id・動的 import id を照合する台帳
/// （`JS-4`・`JS-6`・Issue #1110・設計 §3）。
///
/// 子から届いた id は untrusted。`WorkerHandle` が 1 つ保持し、子の破棄・再起動で
/// 台帳も消える（古い id が新しい子に対して「既知」として通らない）。違反は
/// `Err` を返し、呼び出し側が子を破棄する。呼び出し側が渡す id（link の
/// referrer・target、evaluate の root、finish の id）は照合せず子へ送る
/// （未知の id は子が `ERROR{ModuleResolution}` で返す契約）。
#[derive(Debug, Default)]
pub(crate) struct ModuleIdRegistry {
    known: HashSet<u32>,
    pending: HashSet<u32>,
}

impl ModuleIdRegistry {
    /// 空の台帳を作る。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// これ以上モジュールを compile できないか。
    pub(crate) fn is_full(&self) -> bool {
        self.known.len() >= MAX_MODULES_PER_WORKER
    }

    /// `MODULE_COMPILED` で通知された id を登録する。既知の id は
    /// [`ProtocolError::DuplicateModuleId`]、上限超過は
    /// [`ProtocolError::TooManyModules`]。失敗時は何も登録しない。
    pub(crate) fn register_compiled(&mut self, id: ModuleId) -> Result<(), ProtocolError> {
        if self.known.contains(&id.raw()) {
            return Err(ProtocolError::DuplicateModuleId);
        }
        if self.is_full() {
            return Err(ProtocolError::TooManyModules {
                count: self.known.len().saturating_add(1),
                max: MAX_MODULES_PER_WORKER,
            });
        }
        self.known.insert(id.raw());
        Ok(())
    }

    /// `DYNAMIC_IMPORTS` の一覧を検査して保留集合へ登録する。保留中の id と
    /// の重複・未知の referrer・保留件数の超過は `Err`。検査はすべて登録の
    /// 前に行い、1 件でも失敗したらこの回の分は何も登録しない。
    pub(crate) fn accept_dynamic_imports(
        &mut self,
        requests: &[DynamicImportRequest],
    ) -> Result<(), ProtocolError> {
        let total = self.pending.len().saturating_add(requests.len());
        if total > MAX_PENDING_DYNAMIC_IMPORTS {
            return Err(ProtocolError::TooManyDynamicImports {
                count: total,
                max: MAX_PENDING_DYNAMIC_IMPORTS,
            });
        }
        let mut batch: HashSet<u32> = HashSet::new();
        for request in requests {
            let id = request.id().raw();
            if self.pending.contains(&id) || !batch.insert(id) {
                return Err(ProtocolError::DuplicateDynamicImportId);
            }
            if let Some(referrer) = request.referrer()
                && !self.known.contains(&referrer.raw())
            {
                return Err(ProtocolError::UnknownModuleId);
            }
        }
        self.pending.extend(batch);
        Ok(())
    }

    /// 動的 import の完了を反映して保留集合から外す。保留中だったかを返す。
    pub(crate) fn complete_dynamic_import(&mut self, id: DynamicImportId) -> bool {
        self.pending.remove(&id.raw())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::super::{read_frame, tag, write_frame};
    use super::*;

    fn opts() -> EvaluateOptions {
        EvaluateOptions::default()
    }

    fn spec(n: usize, fill: char) -> String {
        std::iter::repeat_n(fill, n).collect()
    }

    fn requests(n: usize) -> Vec<ModuleRequest> {
        (0..n)
            .map(|i| ModuleRequest::new(format!("./m{i}.js")))
            .collect()
    }

    /// JS-4・JS-6: COMPILE_MODULE の往復（url・source・timeout）。
    #[test]
    fn js_4_compile_module_round_trips_with_concrete_values() {
        let options = opts().with_timeout(Duration::from_millis(1500));
        let payload =
            encode_compile_module("https://example.test/a.js", "export const a = 1;", &options)
                .expect("encode");
        let decoded = decode_compile_module(&payload).expect("decode");
        assert_eq!(
            decoded,
            CompileModuleRequest {
                timeout: Duration::from_millis(1500),
                url: "https://example.test/a.js".to_string(),
                source: "export const a = 1;".to_string(),
            }
        );
    }

    /// JS-4: MODULE_COMPILED の往復（id と specifier 列）。
    #[test]
    fn js_4_module_compiled_round_trips() {
        let reqs = vec![
            ModuleRequest::new("./b.js".to_string()),
            ModuleRequest::new("https://cdn.test/c.js".to_string()),
        ];
        let payload = encode_module_compiled(ModuleId::from_raw(7), &reqs).expect("encode");
        let record = decode_module_compiled(&payload).expect("decode");
        assert_eq!(record, ModuleRecord::new(ModuleId::from_raw(7), reqs));
        let empty = encode_module_compiled(ModuleId::from_raw(0), &[]).expect("encode");
        assert_eq!(
            decode_module_compiled(&empty).expect("decode"),
            ModuleRecord::new(ModuleId::from_raw(0), vec![])
        );
    }

    /// JS-4: LINK_MODULE の往復（referrer と対応表）。
    #[test]
    fn js_4_link_module_round_trips() {
        let rows = vec![
            ModuleResolution::new("./b.js".to_string(), ModuleId::from_raw(2)),
            ModuleResolution::new("./c.js".to_string(), ModuleId::from_raw(u32::MAX)),
        ];
        let payload = encode_link_module(ModuleId::from_raw(1), &rows).expect("encode");
        let (referrer, decoded) = decode_link_module(&payload).expect("decode");
        assert_eq!(referrer, ModuleId::from_raw(1));
        assert_eq!(decoded, rows);
    }

    /// JS-4・JS-6: EVALUATE_MODULE の往復（options と root）。
    #[test]
    fn js_4_evaluate_module_round_trips() {
        let options = opts()
            .with_timeout(Duration::from_millis(750))
            .with_max_result_bytes(4096);
        let payload = encode_evaluate_module(ModuleId::from_raw(9), &options);
        assert_eq!(payload.len(), 20);
        let (root, decoded) = decode_evaluate_module(&payload).expect("decode");
        assert_eq!(root, ModuleId::from_raw(9));
        assert_eq!(decoded.timeout(), Duration::from_millis(750));
        assert_eq!(decoded.max_result_bytes(), 4096);
        assert!(matches!(
            decode_evaluate_module(&payload[..19]),
            Err(ProtocolError::Truncated)
        ));
        let mut extra = payload.clone();
        extra.push(0);
        assert!(matches!(
            decode_evaluate_module(&extra),
            Err(ProtocolError::TrailingBytes { .. })
        ));
    }

    /// JS-4: MODULE_RESULT の 3 状態の往復。
    #[test]
    fn js_4_module_result_round_trips_all_statuses() {
        for evaluation in [
            ModuleEvaluation::Fulfilled,
            ModuleEvaluation::Pending,
            ModuleEvaluation::rejected("TypeError: boom".to_string()),
        ] {
            let payload = encode_module_result(&evaluation).expect("encode");
            assert_eq!(decode_module_result(&payload).expect("decode"), evaluation);
        }
        assert_eq!(
            encode_module_result(&ModuleEvaluation::Pending).expect("encode"),
            vec![1]
        );
    }

    /// JS-4: 未知の status・末尾バイト・不正 UTF-8 は拒否する。
    #[test]
    fn js_4_module_result_rejects_invalid_input() {
        assert!(matches!(
            decode_module_result(&[3]),
            Err(ProtocolError::InvalidModuleResultStatus(3))
        ));
        assert!(matches!(
            decode_module_result(&[]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            decode_module_result(&[0, 0]),
            Err(ProtocolError::TrailingBytes { .. })
        ));
        assert!(matches!(
            decode_module_result(&[1, 9]),
            Err(ProtocolError::TrailingBytes { .. })
        ));
        assert!(matches!(
            decode_module_result(&[2, 0xff, 0xfe]),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    /// JS-4: TAKE_DYNAMIC_IMPORTS は空ペイロードのみ受理する。
    #[test]
    fn js_4_take_dynamic_imports_is_empty() {
        assert!(encode_take_dynamic_imports().is_empty());
        assert!(decode_take_dynamic_imports(&[]).is_ok());
        assert!(matches!(
            decode_take_dynamic_imports(&[0]),
            Err(ProtocolError::TrailingBytes {
                expected: 0,
                actual: 1
            })
        ));
    }

    /// JS-4: DYNAMIC_IMPORTS の往復（referrer あり・なしの混在）。
    #[test]
    fn js_4_dynamic_imports_round_trips_with_and_without_referrer() {
        let reqs = vec![
            DynamicImportRequest::new(DynamicImportId::from_raw(1), None, "./a.js".to_string()),
            DynamicImportRequest::new(
                DynamicImportId::from_raw(2),
                Some(ModuleId::from_raw(5)),
                "./b.js".to_string(),
            ),
        ];
        let payload = encode_dynamic_imports(&reqs).expect("encode");
        assert_eq!(decode_dynamic_imports(&payload).expect("decode"), reqs);
        assert!(matches!(
            decode_dynamic_imports(&[0, 0]).expect("empty").len(),
            0
        ));
    }

    /// JS-4: DYNAMIC_IMPORTS の不正な flag・id の重複は拒否する。
    #[test]
    fn js_4_dynamic_imports_rejects_bad_flag_and_duplicate_ids() {
        let mut bad_flag = 1u16.to_le_bytes().to_vec();
        bad_flag.extend_from_slice(&1u32.to_le_bytes());
        bad_flag.push(2);
        assert!(matches!(
            decode_dynamic_imports(&bad_flag),
            Err(ProtocolError::InvalidReferrerFlag(2))
        ));
        let dup = vec![
            DynamicImportRequest::new(DynamicImportId::from_raw(1), None, "a".to_string()),
            DynamicImportRequest::new(DynamicImportId::from_raw(1), None, "b".to_string()),
        ];
        assert!(matches!(
            encode_dynamic_imports(&dup),
            Err(ProtocolError::DuplicateDynamicImportId)
        ));
        // 手組みのフレーム（encode を経由しない）でも decode が拒否する。
        let mut raw = 2u16.to_le_bytes().to_vec();
        for specifier in ["a", "b"] {
            raw.extend_from_slice(&1u32.to_le_bytes());
            raw.push(0);
            raw.extend_from_slice(&1u16.to_le_bytes());
            raw.extend_from_slice(specifier.as_bytes());
        }
        assert!(matches!(
            decode_dynamic_imports(&raw),
            Err(ProtocolError::DuplicateDynamicImportId)
        ));
    }

    /// JS-4: FINISH_DYNAMIC_IMPORT の Ok・Err の往復（残り時間のヘッダを含む）。
    #[test]
    fn js_4_finish_dynamic_import_round_trips_ok_and_err() {
        let id = DynamicImportId::from_raw(4);
        let options = EvaluateOptions::default().with_timeout(Duration::from_millis(1234));
        let ok =
            encode_finish_dynamic_import(id, &Ok(ModuleId::from_raw(8)), &options).expect("encode");
        let (got_id, got_result, got_options) = decode_finish_dynamic_import(&ok).expect("decode");
        assert_eq!((got_id, got_result), (id, Ok(ModuleId::from_raw(8))));
        assert_eq!(got_options.timeout(), Duration::from_millis(1234));
        let err = encode_finish_dynamic_import(id, &Err("fetch failed".to_string()), &options)
            .expect("encode");
        let (got_id, got_result, got_options) = decode_finish_dynamic_import(&err).expect("decode");
        assert_eq!((got_id, got_result), (id, Err("fetch failed".to_string())));
        assert_eq!(got_options.timeout(), Duration::from_millis(1234));
    }

    /// 先頭に `EVALUATE` 共通ヘッダ（既定値）を付けたペイロードを作る。
    fn with_header(body: &[u8]) -> Vec<u8> {
        let mut out = encode_evaluate_header(&EvaluateOptions::default()).to_vec();
        out.extend_from_slice(body);
        out
    }

    /// JS-4: FINISH_DYNAMIC_IMPORT の不正入力。
    #[test]
    fn js_4_finish_dynamic_import_rejects_invalid_input() {
        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.push(2);
        assert!(matches!(
            decode_finish_dynamic_import(&with_header(&payload)),
            Err(ProtocolError::InvalidFinishStatus(2))
        ));
        let mut trailing = 1u32.to_le_bytes().to_vec();
        trailing.push(0);
        trailing.extend_from_slice(&3u32.to_le_bytes());
        trailing.push(0);
        assert!(matches!(
            decode_finish_dynamic_import(&with_header(&trailing)),
            Err(ProtocolError::TrailingBytes { .. })
        ));
        let mut utf8 = 1u32.to_le_bytes().to_vec();
        utf8.push(1);
        utf8.push(0xff);
        assert!(matches!(
            decode_finish_dynamic_import(&with_header(&utf8)),
            Err(ProtocolError::InvalidUtf8)
        ));
        let mut long = 1u32.to_le_bytes().to_vec();
        long.push(1);
        long.extend_from_slice(&vec![b'x'; MAX_ERROR_MESSAGE_BYTES + 1]);
        assert!(matches!(
            decode_finish_dynamic_import(&with_header(&long)),
            Err(ProtocolError::ErrorMessageTooLong { .. })
        ));
        assert!(matches!(
            encode_finish_dynamic_import(
                DynamicImportId::from_raw(1),
                &Err("x".repeat(MAX_ERROR_MESSAGE_BYTES + 1)),
                &EvaluateOptions::default(),
            ),
            Err(ProtocolError::ErrorMessageTooLong { .. })
        ));
        assert!(matches!(
            decode_finish_dynamic_import(&with_header(&[1, 0, 0])),
            Err(ProtocolError::Truncated)
        ));
        // ヘッダ自体が足りない。
        assert!(matches!(
            decode_finish_dynamic_import(&[1, 0, 0]),
            Err(ProtocolError::Truncated)
        ));
    }

    /// JS-6: request 数の境界（256 は受理、257 は拒否）。
    #[test]
    fn js_6_module_request_count_boundary() {
        let at_limit = requests(MAX_MODULE_REQUESTS);
        let payload = encode_module_compiled(ModuleId::from_raw(1), &at_limit).expect("at limit");
        assert_eq!(
            decode_module_compiled(&payload).expect("decode").requests(),
            at_limit.as_slice()
        );
        assert!(matches!(
            encode_module_compiled(ModuleId::from_raw(1), &requests(MAX_MODULE_REQUESTS + 1)),
            Err(ProtocolError::TooManyModuleRequests {
                count: 257,
                max: 256
            })
        ));
        let rows: Vec<ModuleResolution> = (0..=MAX_MODULE_REQUESTS)
            .map(|i| ModuleResolution::new(format!("./m{i}.js"), ModuleId::from_raw(1)))
            .collect();
        assert!(matches!(
            encode_link_module(ModuleId::from_raw(1), &rows),
            Err(ProtocolError::TooManyModuleRequests { .. })
        ));
        assert!(encode_link_module(ModuleId::from_raw(1), &rows[..MAX_MODULE_REQUESTS]).is_ok());
    }

    /// JS-6: 件数フィールドに巨大値を主張しても確保する前に拒否する。
    #[test]
    fn js_6_huge_claimed_counts_are_rejected_before_allocation() {
        let mut compiled = 1u32.to_le_bytes().to_vec();
        compiled.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            decode_module_compiled(&compiled),
            Err(ProtocolError::TooManyModuleRequests { count: 65535, .. })
        ));
        let mut link = 1u32.to_le_bytes().to_vec();
        link.extend_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(
            decode_link_module(&link),
            Err(ProtocolError::TooManyModuleRequests { .. })
        ));
        assert!(matches!(
            decode_dynamic_imports(&u16::MAX.to_le_bytes()),
            Err(ProtocolError::TooManyDynamicImports { count: 65535, .. })
        ));
    }

    /// JS-6: specifier 長の境界（2048 は受理、2049 は拒否）。
    #[test]
    fn js_6_specifier_length_boundary() {
        let ok = vec![ModuleRequest::new(spec(MAX_MODULE_SPECIFIER_BYTES, 'a'))];
        let payload = encode_module_compiled(ModuleId::from_raw(1), &ok).expect("2048");
        assert_eq!(
            decode_module_compiled(&payload).expect("decode").requests(),
            ok.as_slice()
        );
        let too_long = vec![ModuleRequest::new(spec(
            MAX_MODULE_SPECIFIER_BYTES + 1,
            'a',
        ))];
        assert!(matches!(
            encode_module_compiled(ModuleId::from_raw(1), &too_long),
            Err(ProtocolError::ModuleSpecifierTooLong {
                len: 2049,
                max: 2048
            })
        ));
        // 手組みで長さ 2049 を主張する（本体は無い）。長さ超過が Truncated より先に出る。
        let mut raw = 1u32.to_le_bytes().to_vec();
        raw.extend_from_slice(&1u16.to_le_bytes());
        raw.extend_from_slice(&2049u16.to_le_bytes());
        assert!(matches!(
            decode_module_compiled(&raw),
            Err(ProtocolError::ModuleSpecifierTooLong { len: 2049, .. })
        ));
    }

    /// JS-6: URL 長の境界（4096 は受理、4097 は拒否）と空 URL。
    #[test]
    fn js_6_url_length_boundary_and_empty_url() {
        let url_ok = spec(MAX_MODULE_URL_BYTES, 'u');
        let payload = encode_compile_module(&url_ok, "", &opts()).expect("4096");
        assert_eq!(decode_compile_module(&payload).expect("decode").url, url_ok);
        assert!(matches!(
            encode_compile_module(&spec(MAX_MODULE_URL_BYTES + 1, 'u'), "", &opts()),
            Err(ProtocolError::ModuleUrlTooLong {
                len: 4097,
                max: 4096
            })
        ));
        assert!(matches!(
            encode_compile_module("", "x", &opts()),
            Err(ProtocolError::EmptyModuleUrl)
        ));
        let mut raw = 0u64.to_le_bytes().to_vec();
        raw.extend_from_slice(&4097u16.to_le_bytes());
        assert!(matches!(
            decode_compile_module(&raw),
            Err(ProtocolError::ModuleUrlTooLong { len: 4097, .. })
        ));
        let mut empty = 0u64.to_le_bytes().to_vec();
        empty.extend_from_slice(&0u16.to_le_bytes());
        assert!(matches!(
            decode_compile_module(&empty),
            Err(ProtocolError::EmptyModuleUrl)
        ));
    }

    /// JS-6: 動的 import の件数境界（256 は受理、257 は拒否）。
    #[test]
    fn js_6_dynamic_import_count_boundary() {
        let make = |n: usize| -> Vec<DynamicImportRequest> {
            (0..n)
                .map(|i| {
                    DynamicImportRequest::new(
                        DynamicImportId::from_raw(u32::try_from(i).unwrap_or(0)),
                        None,
                        "./x.js".to_string(),
                    )
                })
                .collect()
        };
        let at_limit = make(MAX_PENDING_DYNAMIC_IMPORTS);
        let payload = encode_dynamic_imports(&at_limit).expect("256");
        assert_eq!(decode_dynamic_imports(&payload).expect("decode"), at_limit);
        assert!(matches!(
            encode_dynamic_imports(&make(MAX_PENDING_DYNAMIC_IMPORTS + 1)),
            Err(ProtocolError::TooManyDynamicImports {
                count: 257,
                max: 256
            })
        ));
    }

    /// JS-6: 全項目が最大のフレームは方向ごとの上限内で write_frame / read_frame を往復する。
    #[test]
    fn js_6_worst_case_frames_fit_within_direction_limits() {
        // COMPILE: url 4096 ＋ source 1 MiB。
        let compile = encode_compile_module(
            &spec(MAX_MODULE_URL_BYTES, 'u'),
            &"s".repeat(1_048_576),
            &opts(),
        )
        .expect("encode");
        assert_eq!(compile.len(), 1_052_682);
        let mut wire = Vec::new();
        write_frame(&mut wire, tag::COMPILE_MODULE, &compile).expect("write");
        let (t, body) = read_frame(&mut Cursor::new(wire), MAX_FRAME_PAYLOAD_PARENT_TO_CHILD)
            .expect("read")
            .expect("frame");
        assert_eq!((t, body.len()), (tag::COMPILE_MODULE, 1_052_682));

        // LINK: 256 行 × specifier 2048。
        let rows: Vec<ModuleResolution> = (0..MAX_MODULE_REQUESTS)
            .map(|i| {
                let mut s = format!("{i:04}");
                s.push_str(&spec(MAX_MODULE_SPECIFIER_BYTES - 4, 'l'));
                ModuleResolution::new(s, ModuleId::from_raw(1))
            })
            .collect();
        let link = encode_link_module(ModuleId::from_raw(1), &rows).expect("encode");
        assert_eq!(link.len(), 525_830);
        assert!(link.len() <= MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);

        // MODULE_COMPILED: 256 件 × specifier 2048。
        let reqs: Vec<ModuleRequest> = rows
            .iter()
            .map(|r| ModuleRequest::new(r.specifier().to_string()))
            .collect();
        let compiled = encode_module_compiled(ModuleId::from_raw(1), &reqs).expect("encode");
        assert_eq!(compiled.len(), 524_806);
        let mut wire = Vec::new();
        write_frame(&mut wire, tag::MODULE_COMPILED, &compiled).expect("write");
        assert!(
            read_frame(&mut Cursor::new(wire), MAX_FRAME_PAYLOAD_CHILD_TO_PARENT)
                .expect("read")
                .is_some()
        );

        // DYNAMIC_IMPORTS: 256 件 × specifier 2048。
        let dyns: Vec<DynamicImportRequest> = (0..MAX_PENDING_DYNAMIC_IMPORTS)
            .map(|i| {
                DynamicImportRequest::new(
                    DynamicImportId::from_raw(u32::try_from(i).unwrap_or(0)),
                    Some(ModuleId::from_raw(1)),
                    spec(MAX_MODULE_SPECIFIER_BYTES, 'd'),
                )
            })
            .collect();
        let dyn_payload = encode_dynamic_imports(&dyns).expect("encode");
        assert_eq!(dyn_payload.len(), 527_106);
        assert!(dyn_payload.len() <= MAX_FRAME_PAYLOAD_CHILD_TO_PARENT);
    }

    /// JS-6: 上限 + 1 のフレーム長を宣言すると、ペイロードを確保せず `FrameTooLarge`
    /// になる（子 → 親・親 → 子の両方向）。
    #[test]
    fn js_6_frame_length_limit_plus_one_is_rejected_in_both_directions() {
        for max in [
            MAX_FRAME_PAYLOAD_PARENT_TO_CHILD,
            MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
        ] {
            // len = タグ 1 バイト ＋ ペイロード（max + 1）。本体は送らない。
            let declared = u32::try_from(max + 2).expect("fits");
            let header = declared.to_le_bytes().to_vec();
            assert!(matches!(
                read_frame(&mut Cursor::new(header), max),
                Err(ProtocolError::FrameTooLarge { len, max: m }) if len == max + 1 && m == max
            ));
        }
        // 上限ちょうど + 1 の COMPILE は encode が拒否する。
        let big = "s".repeat(MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);
        assert!(matches!(
            encode_compile_module("u", &big, &opts()),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    /// JS-6: 不正 UTF-8・切り詰め・末尾バイト。
    #[test]
    fn js_6_invalid_utf8_truncation_and_trailing_bytes() {
        // COMPILE の url・source。
        let mut bad_url = 0u64.to_le_bytes().to_vec();
        bad_url.extend_from_slice(&2u16.to_le_bytes());
        bad_url.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_compile_module(&bad_url),
            Err(ProtocolError::InvalidUtf8)
        ));
        let mut bad_source = 0u64.to_le_bytes().to_vec();
        bad_source.extend_from_slice(&1u16.to_le_bytes());
        bad_source.extend_from_slice(b"u");
        bad_source.push(0xff);
        assert!(matches!(
            decode_compile_module(&bad_source),
            Err(ProtocolError::InvalidUtf8)
        ));
        // specifier の不正 UTF-8。
        let mut bad_spec = 1u32.to_le_bytes().to_vec();
        bad_spec.extend_from_slice(&1u16.to_le_bytes());
        bad_spec.extend_from_slice(&1u16.to_le_bytes());
        bad_spec.push(0xff);
        assert!(matches!(
            decode_module_compiled(&bad_spec),
            Err(ProtocolError::InvalidUtf8)
        ));
        // 切り詰め。
        let good = encode_link_module(
            ModuleId::from_raw(1),
            &[ModuleResolution::new(
                "./a.js".to_string(),
                ModuleId::from_raw(2),
            )],
        )
        .expect("encode");
        for cut in 0..good.len() {
            assert!(
                matches!(
                    decode_link_module(&good[..cut]),
                    Err(ProtocolError::Truncated)
                ),
                "cut at {cut} must be Truncated"
            );
        }
        // 末尾バイト。
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(matches!(
            decode_link_module(&trailing),
            Err(ProtocolError::TrailingBytes { .. })
        ));
        let mut compiled = encode_module_compiled(ModuleId::from_raw(1), &[]).expect("encode");
        compiled.push(0);
        assert!(matches!(
            decode_module_compiled(&compiled),
            Err(ProtocolError::TrailingBytes { .. })
        ));
    }

    /// JS-6: LINK・MODULE_COMPILED 内の specifier の重複は拒否する。
    #[test]
    fn js_6_duplicate_specifiers_are_rejected() {
        let dup_reqs = vec![
            ModuleRequest::new("./a.js".to_string()),
            ModuleRequest::new("./a.js".to_string()),
        ];
        assert!(matches!(
            encode_module_compiled(ModuleId::from_raw(1), &dup_reqs),
            Err(ProtocolError::DuplicateModuleSpecifier)
        ));
        let dup_rows = vec![
            ModuleResolution::new("./a.js".to_string(), ModuleId::from_raw(1)),
            ModuleResolution::new("./a.js".to_string(), ModuleId::from_raw(2)),
        ];
        assert!(matches!(
            encode_link_module(ModuleId::from_raw(1), &dup_rows),
            Err(ProtocolError::DuplicateModuleSpecifier)
        ));
        // 手組みの MODULE_COMPILED（encode を経由しない）。
        let mut raw = 1u32.to_le_bytes().to_vec();
        raw.extend_from_slice(&2u16.to_le_bytes());
        for _ in 0..2 {
            raw.extend_from_slice(&1u16.to_le_bytes());
            raw.push(b'a');
        }
        assert!(matches!(
            decode_module_compiled(&raw),
            Err(ProtocolError::DuplicateModuleSpecifier)
        ));
        // 手組みの LINK。
        let mut link = 1u32.to_le_bytes().to_vec();
        link.extend_from_slice(&2u16.to_le_bytes());
        for target in [1u32, 2] {
            link.extend_from_slice(&1u16.to_le_bytes());
            link.push(b'a');
            link.extend_from_slice(&target.to_le_bytes());
        }
        assert!(matches!(
            decode_link_module(&link),
            Err(ProtocolError::DuplicateModuleSpecifier)
        ));
    }

    /// JS-6: 台帳は既知 id の再登録を拒否し、上限（1024 件目まで登録可・1025 件目は拒否）を守る。
    #[test]
    fn js_6_registry_rejects_duplicate_ids_and_enforces_module_limit() {
        let mut registry = ModuleIdRegistry::new();
        registry
            .register_compiled(ModuleId::from_raw(1))
            .expect("first");
        assert!(matches!(
            registry.register_compiled(ModuleId::from_raw(1)),
            Err(ProtocolError::DuplicateModuleId)
        ));
        for id in 2..=u32::try_from(MAX_MODULES_PER_WORKER).expect("fits") {
            registry
                .register_compiled(ModuleId::from_raw(id))
                .expect("within limit");
        }
        assert!(registry.is_full());
        assert!(matches!(
            registry.register_compiled(ModuleId::from_raw(100_000)),
            Err(ProtocolError::TooManyModules {
                count: 1025,
                max: 1024
            })
        ));
    }

    /// JS-6: 台帳の動的 import 検査（未知 referrer・保留 id の重複・原子性・完了後の再受理）。
    #[test]
    fn js_6_registry_validates_dynamic_imports_atomically() {
        let mut registry = ModuleIdRegistry::new();
        registry
            .register_compiled(ModuleId::from_raw(1))
            .expect("module");
        let req = |id: u32, referrer: Option<u32>| {
            DynamicImportRequest::new(
                DynamicImportId::from_raw(id),
                referrer.map(ModuleId::from_raw),
                "./x.js".to_string(),
            )
        };
        // 未知の referrer。
        assert!(matches!(
            registry.accept_dynamic_imports(&[req(1, Some(99))]),
            Err(ProtocolError::UnknownModuleId)
        ));
        // 1 件目が正当でも 2 件目が不正なら、この回の分は何も登録されない。
        assert!(matches!(
            registry.accept_dynamic_imports(&[req(1, Some(1)), req(2, Some(99))]),
            Err(ProtocolError::UnknownModuleId)
        ));
        registry
            .accept_dynamic_imports(&[req(1, Some(1))])
            .expect("id 1 was not registered by the failed batch");
        // 保留中の id と重複。
        assert!(matches!(
            registry.accept_dynamic_imports(&[req(1, None)]),
            Err(ProtocolError::DuplicateDynamicImportId)
        ));
        // 同じ回の中での重複。
        assert!(matches!(
            registry.accept_dynamic_imports(&[req(5, None), req(5, None)]),
            Err(ProtocolError::DuplicateDynamicImportId)
        ));
        // 完了後は同じ id を再び受理する。
        assert!(registry.complete_dynamic_import(DynamicImportId::from_raw(1)));
        assert!(!registry.complete_dynamic_import(DynamicImportId::from_raw(1)));
        registry
            .accept_dynamic_imports(&[req(1, None)])
            .expect("accepted again after completion");
    }

    /// JS-6: 保留中の動的 import 数の上限（256 件目まで受理・257 件目は拒否）。
    #[test]
    fn js_6_registry_enforces_pending_dynamic_import_limit() {
        let mut registry = ModuleIdRegistry::new();
        let batch: Vec<DynamicImportRequest> = (0..MAX_PENDING_DYNAMIC_IMPORTS)
            .map(|i| {
                DynamicImportRequest::new(
                    DynamicImportId::from_raw(u32::try_from(i).unwrap_or(0)),
                    None,
                    "./x.js".to_string(),
                )
            })
            .collect();
        registry.accept_dynamic_imports(&batch).expect("256");
        let one_more = [DynamicImportRequest::new(
            DynamicImportId::from_raw(10_000),
            None,
            "./x.js".to_string(),
        )];
        assert!(matches!(
            registry.accept_dynamic_imports(&one_more),
            Err(ProtocolError::TooManyDynamicImports {
                count: 257,
                max: 256
            })
        ));
    }
}

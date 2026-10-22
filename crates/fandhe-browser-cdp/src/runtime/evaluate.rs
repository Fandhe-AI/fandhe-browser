//! `Runtime.evaluate`・`Runtime.releaseObject` と RemoteObject の最小表現
//! （TASK-44.7・#1218、ビヘイビア `CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・関連 `JS-6`・`PERF-7`・MS-4）。
//!
//! 親モジュール [`super`] の contextId の表（[`super::ExecutionContextTable`]）で解決した isolated world の
//! 評価コンテキスト（core の `ScriptContextHandle`。確定文書の上で JS ランタイムを保持する。#1216）の上で、
//! クライアントの式を評価して CDP の RemoteObject に整える。`Page.createIsolatedWorld`・`Runtime.enable` が
//! 告知した contextId を Playwright が utility world として使う経路の評価側にあたる。呼び出し元は
//! ディスパッチャ（`protocol::builtin_handlers_with`）で、cli への配線と Playwright の実測は #1220。
//!
//! # 仕組み
//!
//! - **ハンドル表は JS の中に置く**。`helper.js`（`include_str!`）をコンテキストごとに 1 回だけ注入し
//!   （`__fandheCdpHelper`。書き換え不可・再定義不可・freeze 済み）、ユーザー式の評価（間接 eval）・
//!   結果の整形・ハンドル（`objectId` の `<contextId>.<n>` の `n`）の払い出し・解放をそこで行う。
//!   core の評価は「完了値が文字列のときだけ成功」なので、ヘルパーは常に JSON 文字列（封筒）を返す。
//! - **インジェクション対策**。式・設定値を JS ソースへ連結しない。ペイロードを serde_json で JSON にし、
//!   さらにそれを JSON 文字列リテラル 1 個にして渡し、ヘルパーの中で `JSON.parse` してから評価する
//!   （[`js_string_literal`]）。ソースの残りは固定の定数だけ。
//! - **多層防御**。封筒は core の上限で抑えた文字列だけを解析し、RemoteObject は許可リストの項目だけから
//!   Rust 側で組み立て直す（[`remote_object`]）。CDP エラーは固定文言で、入力・JS のメッセージ・core の
//!   `Display` を反映しない（JS 由来のメッセージは `exception.description` にだけ、切り詰めて入る）。
//! - **epoch**。ヘルパーの注入ごとに払い出す番号で、注入結果を表へ記録する。ペイロードの epoch がヘルパーの
//!   epoch と違う・ヘルパーが無い（コンテキストが作り直された）場合は「リセット」として contextId を失効
//!   させる（親モジュールの「差異・スタブ」参照。黙って再注入しない）。
//!
//! # 検証順
//!
//! `Runtime.evaluate`: `sessionId` なし → `UNSUPPORTED_PARAMS` → `params` が非オブジェクト → `INVALID_PARAMS`
//! → 未知キー（既知は `expression`・`contextId`・`returnByValue`・`awaitPromise`・`silent`）→
//! `UNSUPPORTED_PARAMS` → 型の誤り → `INVALID_PARAMS` → `awaitPromise: true` → `UNSUPPORTED_PARAMS` →
//! 式が 1 MiB 超 → `EXPRESSION_TOO_LARGE`（評価器を起動しない）→ 未知セッション → `INVALID_PARAMS` →
//! 止めたターゲット → `TARGET_WAITING_FOR_DEBUGGER` → 評価器が無効 → `EXECUTION_CONTEXTS_UNAVAILABLE` →
//! 確定文書なし・世代の食い違い → `TARGET_DOCUMENT_UNAVAILABLE` → contextId の解決失敗（未知・他セッションは
//! `INVALID_PARAMS`、世代違いは `EXECUTION_CONTEXT_STALE`）→ main world・`contextId` 省略 →
//! `MAIN_WORLD_EVALUATION_UNSUPPORTED`。
//!
//! `Runtime.releaseObject`: `sessionId` なし → `UNSUPPORTED_PARAMS` → 非オブジェクト → `INVALID_PARAMS` →
//! `objectId` 以外のキー → `UNSUPPORTED_PARAMS` → `objectId` 無し・非文字列・書式不正 → `INVALID_PARAMS` →
//! 未知セッション → `INVALID_PARAMS` → 評価器が無効 → `EXECUTION_CONTEXTS_UNAVAILABLE` → 確定文書なし →
//! `TARGET_DOCUMENT_UNAVAILABLE` → contextId の解決（未知・他セッション・main world・ヘルパー未注入は
//! `REMOTE_OBJECT_NOT_FOUND` で存在を区別せず、評価もしない。世代違いは `EXECUTION_CONTEXT_STALE`）。
//!
//! # 差異・スタブ（`REPAIR-3`）
//!
//! - `awaitPromise: true`（microtask の実行）は `UNSUPPORTED_PARAMS`（#1219）。`silent` は受け入れるが効果は無い。
//!   `callFunctionOn`・`getProperties`・`releaseObjectGroup` は未登録（`-32601`）。`objectGroup`・
//!   `generatePreview`・`includeCommandLineAPI` 等の未知キーも `UNSUPPORTED_PARAMS`（成功を装わない。`CDP-6`）。
//! - main world への評価はエラー（上記）。`exceptionDetails` の `lineNumber`・`columnNumber` は 0 固定で
//!   `text` は固定文言 `uncaught exception`（Chromium の文言は使わない）。
//! - `returnByValue` で関数・symbol は `VALUE_NOT_SERIALIZABLE`（Chromium は `{}`）。循環参照・入れ子の
//!   bigint も同じ。入れ子が 128 を超える値は serde の再帰上限で `SERVER_ERROR`。
//! - 結果の長さ上限は 1,000,000 UTF-16 単位（エンジンの天井 1,048,576 未満。超過は
//!   `EVALUATION_RESULT_TOO_LARGE`）。包んだソースが core の 1 MiB 上限を超える（`"` ばかりの式は
//!   エスケープで膨らむ）場合も `EXPRESSION_TOO_LARGE` で、実効の式長の上限は 1 MiB より小さくなり得る。
//! - core の scope あたりのコンテキスト数の上限は 4。isolated world は 8 個まで作れるが、5 つ目以降の
//!   初回評価は `EVALUATION_RESOURCE_LIMIT` になり得る。
//! - `description` は決定的な簡易表現（エラーは「名前: メッセージ」でスタックは載せない・ノードは nodeName〔無ければ
//!   localName・tagName、それも無ければ className〕の小文字・関数はソースの先頭 4,096 単位）。
//!   `preview`・`customPreview` は返さない。
//! - UA・ヘッダ・`navigator` 等の識別面には触れない（`SEC-2`）。評価器が無効な構成では
//!   `EXECUTION_CONTEXTS_UNAVAILABLE` で、偽の成功を返さない。

use std::sync::Arc;
use std::time::Duration;

use fandhe_browser_core::{
    DEFAULT_SCRIPT_EVAL_TIMEOUT, MAX_SCRIPT_CONTEXT_SOURCE_BYTES, NavigationGeneration,
    NavigationResult, NavigationScope, RestartCause, ScriptContextError, ScriptContextKey,
    ScriptContextLimit, ScriptEvalOptions, UnavailableReason,
};
use serde_json::{Map, Value, json};

use super::{ContextLookupError, ContextView, MAX_CONTEXT_ID, World};
use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, HandlerOutput};
use crate::server::CdpState;
use crate::target::{SessionId, TargetId};

/// 評価ヘルパー（コンテキストごとに 1 回注入する関数式）。
const HELPER_JS: &str = include_str!("helper.js");

/// コンテキスト内のハンドル（リモートオブジェクト）数の既定上限。
const DEFAULT_MAX_HANDLES: usize = 4096;
/// 封筒（結果の JSON 文字列）の長さの既定上限（UTF-16 単位。エンジンの天井 1,048,576 未満）。
const DEFAULT_MAX_RESULT_LEN: usize = 1_000_000;
/// 式の長さの既定上限（バイト。core のソース上限と同じ値）。
const DEFAULT_MAX_EXPRESSION_BYTES: usize = MAX_SCRIPT_CONTEXT_SOURCE_BYTES;
/// ヘルパーが切り詰める説明文の長さ（UTF-16 単位）。
const MAX_DESCRIPTION_UNITS: usize = 4096;
/// Rust 側で再検査する説明文のバイト長（UTF-16 単位 × 最大 3 バイト）。
const MAX_DESCRIPTION_BYTES: usize = MAX_DESCRIPTION_UNITS * 3;
/// Rust 側で再検査する `className` のバイト長（ヘルパーは 256 UTF-16 単位で切る）。
const MAX_CLASS_NAME_BYTES: usize = 256 * 3;
/// `unserializableValue`（bigint）の最大バイト長。
const MAX_UNSERIALIZABLE_BYTES: usize = 1024;
/// `objectId` の最大バイト長。
const MAX_OBJECT_ID_BYTES: usize = 64;
/// ハンドル番号・例外 ID の上限（JS の安全整数）。
const MAX_HANDLE_ID: u64 = (1 << 53) - 1;

/// `exceptionDetails.text` の固定文言（JS 由来の文言は使わない）。
const EXCEPTION_TEXT: &str = "uncaught exception";

/// 評価の上限（ハンドラが持つ。テストから差し替えられるよう値で保持する。`PageNavigate::new(fetch)` と同じ流儀）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvalLimits {
    /// 1 回の評価のタイムアウト（core が 60 秒で頭打ちにする）。
    pub(crate) timeout: Duration,
    /// コンテキスト内のハンドル数の上限。
    pub(crate) max_handles: usize,
    /// 結果（封筒）の長さの上限（UTF-16 単位）。
    pub(crate) max_result_len: usize,
    /// 式の長さの上限（バイト）。
    pub(crate) max_expression_bytes: usize,
}

impl Default for EvalLimits {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SCRIPT_EVAL_TIMEOUT,
            max_handles: DEFAULT_MAX_HANDLES,
            max_result_len: DEFAULT_MAX_RESULT_LEN,
            max_expression_bytes: DEFAULT_MAX_EXPRESSION_BYTES,
        }
    }
}

impl EvalLimits {
    /// タイムアウトだけを差し替えた値（結合テストの時間短縮用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

// ---------------------------------------------------------------------------
// ソースの組み立て（連結しない: 可変データは JSON 文字列リテラル 1 個だけ）
// ---------------------------------------------------------------------------

/// `payload` を JSON にし、それを JS の文字列リテラル 1 個にする。ヘルパーが `JSON.parse` で戻す。
/// U+2028・U+2029 は古いエンジンで文字列リテラルを壊すため `\u` エスケープへ置き換える
/// （JSON 文字列の外には現れないので置換は安全）。
fn js_string_literal(payload: &Value) -> String {
    Value::String(payload.to_string())
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// ヘルパーの注入ソース（`(関数式)(設定のリテラル)`）。完了値は `{"kind":"installed","epoch":E}`。
fn install_source(epoch: u64, limits: &EvalLimits) -> String {
    let cfg = json!({
        "epoch": epoch,
        "maxHandles": limits.max_handles,
        "maxResultLength": limits.max_result_len,
        "maxDescriptionLength": MAX_DESCRIPTION_UNITS,
        "maxUnserializableLength": MAX_UNSERIALIZABLE_BYTES,
    });
    format!("{}({})", HELPER_JS.trim(), js_string_literal(&cfg))
}

/// ヘルパー呼び出しの共通の包み。ヘルパーが無い（コンテキストが作り直された）ときは `reset` を返す。
fn helper_call_source(method: &str, payload: &Value) -> String {
    format!(
        "typeof __fandheCdpHelper === \"object\" ? __fandheCdpHelper.{method}({}) : \"{{\\\"kind\\\":\\\"reset\\\"}}\"",
        js_string_literal(payload)
    )
}

/// 式の評価ソース。可変のデータ（epoch・式・returnByValue）はすべてリテラルの中に入る。
fn evaluate_source(epoch: u64, expression: &str, return_by_value: bool) -> String {
    helper_call_source(
        "evaluate",
        &json!({
            "epoch": epoch,
            "expression": expression,
            "returnByValue": return_by_value,
        }),
    )
}

/// ハンドルの解放ソース。
fn release_source(epoch: u64, handle: u64) -> String {
    helper_call_source("release", &json!({ "epoch": epoch, "handle": handle }))
}

// ---------------------------------------------------------------------------
// objectId
// ---------------------------------------------------------------------------

/// `"<contextId>.<n>"` を厳密にパースする。64 バイト以下・`.` はちょうど 1 個・両側は先頭ゼロなしの
/// ASCII 10 進数字だけ（符号・空白・非 ASCII 数字は不可）・contextId は `1..=MAX_CONTEXT_ID`・
/// n は `1..=2^53−1`。
fn parse_object_id(s: &str) -> Option<(u64, u64)> {
    if s.len() > MAX_OBJECT_ID_BYTES {
        return None;
    }
    let (ctx, handle) = s.split_once('.')?;
    let ctx = parse_decimal(ctx, MAX_CONTEXT_ID)?;
    let handle = parse_decimal(handle, MAX_HANDLE_ID)?;
    Some((ctx, handle))
}

/// 先頭ゼロなしの ASCII 10 進数字を `1..=max` の値として読む。
fn parse_decimal(s: &str, max: u64) -> Option<u64> {
    let first = s.as_bytes().first()?;
    if *first == b'0' || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = s.parse().ok()?;
    (1..=max).contains(&n).then_some(n)
}

// ---------------------------------------------------------------------------
// 封筒と RemoteObject
// ---------------------------------------------------------------------------

/// ヘルパーが返す JSON 文字列（封筒）を解析した値。
#[derive(Debug, PartialEq)]
enum Envelope {
    Installed { epoch: u64 },
    Reset,
    InternalError,
    HandleLimit,
    ResultTooLarge,
    NotSerializable,
    Value { result: Value },
    Exception { id: u64, exception: Value },
    Released { found: bool },
}

/// 封筒を解析する。解析失敗・未知の `kind`・欠落は `SERVER_ERROR`（成功を装わない）。
fn parse_envelope(text: &str) -> Result<Envelope, CdpError> {
    let v: Value = serde_json::from_str(text).map_err(|_| CdpError::SERVER_ERROR)?;
    let kind = v
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(CdpError::SERVER_ERROR)?;
    let field = |name: &str| v.get(name).cloned().ok_or(CdpError::SERVER_ERROR);
    let safe_id = |name: &str| {
        v.get(name)
            .and_then(Value::as_u64)
            .filter(|n| (1..=MAX_HANDLE_ID).contains(n))
            .ok_or(CdpError::SERVER_ERROR)
    };
    Ok(match kind {
        "installed" => Envelope::Installed {
            epoch: safe_id("epoch")?,
        },
        "reset" => Envelope::Reset,
        "internalError" => Envelope::InternalError,
        "handleLimit" => Envelope::HandleLimit,
        "resultTooLarge" => Envelope::ResultTooLarge,
        "notSerializable" => Envelope::NotSerializable,
        "value" => Envelope::Value {
            result: field("result")?,
        },
        "exception" => Envelope::Exception {
            id: safe_id("exceptionId")?,
            exception: field("exception")?,
        },
        "released" => Envelope::Released {
            found: v
                .get("found")
                .and_then(Value::as_bool)
                .ok_or(CdpError::SERVER_ERROR)?,
        },
        _ => return Err(CdpError::SERVER_ERROR),
    })
}

/// `type` の許可リスト（CDP の RemoteObject.type のうち本実装が返すもの）。
const TYPES: [&str; 8] = [
    "object",
    "function",
    "undefined",
    "string",
    "number",
    "boolean",
    "symbol",
    "bigint",
];

/// `subtype` の許可リスト（`function` は CDP の列挙に無いため `type: "function"` で表す）。
const SUBTYPES: [&str; 9] = [
    "array", "null", "node", "regexp", "date", "map", "set", "error", "promise",
];

/// `unserializableValue` として許す number の値。
const UNSERIALIZABLE_NUMBERS: [&str; 4] = ["NaN", "Infinity", "-Infinity", "-0"];

/// `^-?[0-9]+n$`（bigint の `unserializableValue`）か。
fn is_bigint_literal(s: &str) -> bool {
    if s.len() > MAX_UNSERIALIZABLE_BYTES {
        return false;
    }
    let Some(body) = s.strip_suffix('n') else {
        return false;
    };
    let digits = body.strip_prefix('-').unwrap_or(body);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// ヘルパーが返した RemoteObject の素材を、許可リストの項目だけから CDP の RemoteObject に組み立て直す。
/// 型と項目の組み合わせが矛盾する・長さが上限を超える素材は `SERVER_ERROR`（ヘルパーの故障。
/// 成功を装わない）。`handle` は `objectId`（`"<context_id>.<n>"`）へ変換する。
fn remote_object(raw: &Value, context_id: u64) -> Result<Value, CdpError> {
    let obj = raw.as_object().ok_or(CdpError::SERVER_ERROR)?;
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| TYPES.contains(t))
        .ok_or(CdpError::SERVER_ERROR)?;
    let text_field = |name: &str, max: usize| -> Result<Option<&str>, CdpError> {
        match obj.get(name) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .filter(|s| s.len() <= max)
                .map(Some)
                .ok_or(CdpError::SERVER_ERROR),
        }
    };
    let subtype = match text_field("subtype", 16)? {
        Some(s) if SUBTYPES.contains(&s) => Some(s),
        Some(_) => return Err(CdpError::SERVER_ERROR),
        None => None,
    };
    let class_name = text_field("className", MAX_CLASS_NAME_BYTES)?;
    let description = text_field("description", MAX_DESCRIPTION_BYTES)?;
    let unserializable = text_field("unserializableValue", MAX_UNSERIALIZABLE_BYTES)?;
    let handle = match obj.get("handle") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|n| (1..=MAX_HANDLE_ID).contains(n))
                .ok_or(CdpError::SERVER_ERROR)?,
        ),
    };
    let value = obj.get("value");

    // 型ごとの項目の組み合わせ（矛盾は拒否する）。
    let valid = match ty {
        "undefined" => value.is_none() && unserializable.is_none() && handle.is_none(),
        "boolean" => {
            value.is_some_and(Value::is_boolean) && unserializable.is_none() && handle.is_none()
        }
        "string" => {
            value.is_some_and(Value::is_string) && unserializable.is_none() && handle.is_none()
        }
        "number" => {
            handle.is_none()
                && match (value, unserializable) {
                    (Some(v), None) => v.is_number(),
                    (None, Some(u)) => UNSERIALIZABLE_NUMBERS.contains(&u),
                    _ => false,
                }
        }
        "bigint" => {
            handle.is_none() && value.is_none() && unserializable.is_some_and(is_bigint_literal)
        }
        "symbol" | "function" => value.is_none() && unserializable.is_none() && handle.is_some(),
        // object: null（値が null）・ハンドル付き・値渡しのいずれか。
        _ => {
            unserializable.is_none()
                && match (subtype, handle, value) {
                    (Some("null"), None, Some(v)) => v.is_null(),
                    (_, Some(_), None) => subtype != Some("null"),
                    (st, None, Some(_)) => st != Some("null"),
                    _ => false,
                }
        }
    };
    if !valid {
        return Err(CdpError::SERVER_ERROR);
    }

    let mut out = Map::new();
    out.insert("type".to_owned(), json!(ty));
    if let Some(s) = subtype {
        out.insert("subtype".to_owned(), json!(s));
    }
    if let Some(s) = class_name {
        out.insert("className".to_owned(), json!(s));
    }
    if let Some(v) = value {
        out.insert("value".to_owned(), v.clone());
    }
    if let Some(s) = unserializable {
        out.insert("unserializableValue".to_owned(), json!(s));
    }
    if let Some(s) = description {
        out.insert("description".to_owned(), json!(s));
    }
    if let Some(n) = handle {
        out.insert("objectId".to_owned(), json!(format!("{context_id}.{n}")));
    }
    Ok(Value::Object(out))
}

// ---------------------------------------------------------------------------
// core のエラーの写像
// ---------------------------------------------------------------------------

/// core の評価エラーを固定文言の CDP エラーへ写す。2 つ目の値は「そのコンテキストが作り直された
/// （状態が失われた）ので contextId を失効させる」印。`Display` は使わない（長さ・時間を含むため）。
fn map_script_error(e: &ScriptContextError) -> (CdpError, bool) {
    match e {
        ScriptContextError::Unavailable {
            reason: UnavailableReason::Disabled,
        } => (CdpError::EXECUTION_CONTEXTS_UNAVAILABLE, false),
        ScriptContextError::NotFound => (CdpError::EXECUTION_CONTEXT_STALE, false),
        ScriptContextError::LimitExceeded { limit } => (
            match limit {
                ScriptContextLimit::SourceBytes { .. } => CdpError::EXPRESSION_TOO_LARGE,
                ScriptContextLimit::ResultBytes { .. } => CdpError::EVALUATION_RESULT_TOO_LARGE,
                _ => CdpError::EVALUATION_RESOURCE_LIMIT,
            },
            false,
        ),
        ScriptContextError::Timeout { .. } => (CdpError::EVALUATION_TIMED_OUT, false),
        ScriptContextError::ResourceLimit => (CdpError::EVALUATION_RESOURCE_LIMIT, false),
        ScriptContextError::EngineRestarted { cause } => (
            match cause {
                RestartCause::Timeout => CdpError::EVALUATION_TIMED_OUT,
                RestartCause::ResourceLimit => CdpError::EVALUATION_RESOURCE_LIMIT,
                _ => CdpError::EXECUTION_CONTEXT_RESET,
            },
            true,
        ),
        // `Unavailable`（Disabled 以外）・`EvaluationFailed`（ユーザーの式はヘルパーの try の内側で動くため、
        // ラッパー自体の故障）・将来の種別。
        _ => (CdpError::SERVER_ERROR, false),
    }
}

// ---------------------------------------------------------------------------
// 共通の評価経路
// ---------------------------------------------------------------------------

/// 確定文書の確認と contextId の解決の結果。
struct Located {
    scope: NavigationScope,
    generation: NavigationGeneration,
    doc: Arc<NavigationResult>,
    /// `contextId` を指定したときだけ `Some`。
    view: Option<ContextView>,
}

/// 確定文書の確認と contextId の解決の失敗。呼び出し側が自分の検証順に合わせて CDP エラーへ写す。
enum LocateError {
    /// 確定文書が無い・世代が食い違う。
    Document,
    /// contextId の解決に失敗した。
    Lookup(ContextLookupError),
}

/// 確定文書を確認し（世代は current → latest → current の順に読み、食い違えば文書なし）、`context_id` が
/// あれば現在の世代に照らして解決する。遷移の開始・確定と同じゲートの中で行い、ゲートは戻る前に手放す
/// （core の評価を await する間は持ち越さない。競合は core の `NotFound` → `EXECUTION_CONTEXT_STALE` で受ける）。
fn locate(
    state: &CdpState,
    sid: &SessionId,
    tid: &TargetId,
    context_id: Option<u64>,
) -> Result<Located, LocateError> {
    let nav = state.navigations().get(tid).ok_or(LocateError::Document)?;
    let _gate = crate::page::lock_gate(state.commit_gate());
    let before = nav.current_generation();
    let doc = nav.latest();
    let after = nav.current_generation();
    let doc = match doc {
        Some(d) if before == after => d,
        _ => return Err(LocateError::Document),
    };
    let view = match context_id {
        None => None,
        Some(id) => Some(
            state
                .registry()
                .resolve_context(sid, id, after)
                .map_err(LocateError::Lookup)?,
        ),
    };
    Ok(Located {
        scope: nav.scope(),
        generation: after,
        doc,
        view,
    })
}

/// 1 回の評価を core へ出し、結果の文字列（封筒）を返す。
async fn run_script(
    state: &CdpState,
    key: ScriptContextKey,
    doc: &Arc<NavigationResult>,
    source: &str,
    timeout: Duration,
) -> Result<String, ScriptContextError> {
    let opts = ScriptEvalOptions::default().with_timeout(timeout);
    state
        .app_state()
        .script_contexts()
        .evaluate(key, Arc::clone(doc), source, &opts)
        .await
        .map(fandhe_browser_core::ScriptEvalOutput::into_value)
}

/// コンテキストの評価ヘルパーを用意し、その epoch を返す。未注入なら epoch を払い出して注入し、結果を
/// 表へ記録する。記録済みの値と応答の epoch が違えば（並行した注入の競合）リセットとして失効させる。
async fn ensure_helper(
    state: &CdpState,
    sid: &SessionId,
    view: &ContextView,
    located: &Located,
    key: ScriptContextKey,
    limits: &EvalLimits,
) -> Result<u64, CdpError> {
    if let Some(e) = view.helper_epoch {
        return Ok(e);
    }
    let registry = state.registry();
    let epoch = registry
        .allocate_helper_epoch()
        .map_err(super::ContextError::to_cdp)?;
    let text = run_script(
        state,
        key,
        &located.doc,
        &install_source(epoch, limits),
        limits.timeout,
    )
    .await
    .map_err(|e| map_script_error(&e).0)?;
    let Envelope::Installed { epoch: got } = parse_envelope(&text)? else {
        return Err(CdpError::SERVER_ERROR);
    };
    let recorded = registry
        .record_helper_epoch(sid, view.id, located.generation, got)
        .map_err(ContextLookupError::to_cdp)?;
    if recorded != got {
        registry.invalidate_context(sid, view.id);
        return Err(CdpError::EXECUTION_CONTEXT_RESET);
    }
    Ok(recorded)
}

/// 任意の bool パラメータ（無ければ `false`。bool 以外は `INVALID_PARAMS`）。
fn bool_param(obj: &Map<String, Value>, name: &str) -> Result<bool, CdpError> {
    match obj.get(name) {
        None => Ok(false),
        Some(v) => v.as_bool().ok_or(CdpError::INVALID_PARAMS),
    }
}

// ---------------------------------------------------------------------------
// Runtime.evaluate
// ---------------------------------------------------------------------------

/// `Runtime.evaluate`（`CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・TASK-44.7・#1218）。
///
/// isolated world の contextId の上で式を評価し、`{"result": RemoteObject}`、JS の例外は成功応答の中の
/// `exceptionDetails`（`result` は例外の RemoteObject）で返す。タイムアウト・資源上限は例外を装わず
/// `-32000` の固定文言で返す。検証順・差異は本モジュールのドキュメント参照。
pub(crate) struct RuntimeEvaluate {
    limits: EvalLimits,
}

impl RuntimeEvaluate {
    /// 上限を指定して作る（組込みは [`EvalLimits::default`]）。
    pub(crate) fn new(limits: EvalLimits) -> Self {
        Self { limits }
    }
}

impl CommandHandler for RuntimeEvaluate {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if obj.keys().any(|k| {
                !matches!(
                    k.as_str(),
                    "expression" | "contextId" | "returnByValue" | "awaitPromise" | "silent"
                )
            }) {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let expression = obj
                .get("expression")
                .and_then(Value::as_str)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let context_id = match obj.get("contextId") {
                None => None,
                Some(v) => Some(
                    v.as_u64()
                        .filter(|n| (1..=MAX_CONTEXT_ID).contains(n))
                        .ok_or(CdpError::INVALID_PARAMS)?,
                ),
            };
            let return_by_value = bool_param(obj, "returnByValue")?;
            let await_promise = bool_param(obj, "awaitPromise")?;
            // `silent` は受け入れるが効果は無い（`REPAIR-3`）。型だけ検査する。
            bool_param(obj, "silent")?;
            if await_promise {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            if expression.len() > self.limits.max_expression_bytes {
                return Err(CdpError::EXPRESSION_TOO_LARGE);
            }
            let state = ctx.state;
            let registry = state.registry();
            let tid = registry
                .session_target(sid)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if registry.is_paused(&tid) {
                return Err(CdpError::TARGET_WAITING_FOR_DEBUGGER);
            }
            if !state.evaluator_enabled() {
                return Err(CdpError::EXECUTION_CONTEXTS_UNAVAILABLE);
            }
            let located = locate(state, sid, &tid, context_id).map_err(|e| match e {
                LocateError::Document => CdpError::TARGET_DOCUMENT_UNAVAILABLE,
                LocateError::Lookup(l) => l.to_cdp(),
            })?;
            let view = match &located.view {
                Some(v) if v.world != World::Main => v.clone(),
                _ => return Err(CdpError::MAIN_WORLD_EVALUATION_UNSUPPORTED),
            };
            let key = ScriptContextKey::new(located.scope, located.generation, view.id);
            let epoch = ensure_helper(state, sid, &view, &located, key, &self.limits).await?;
            let source = evaluate_source(epoch, expression, return_by_value);
            let text =
                match run_script(state, key, &located.doc, &source, self.limits.timeout).await {
                    Ok(t) => t,
                    Err(e) => {
                        let (err, invalidate) = map_script_error(&e);
                        if invalidate {
                            registry.invalidate_context(sid, view.id);
                        }
                        return Err(err);
                    }
                };
            match parse_envelope(&text)? {
                Envelope::Value { result } => Ok(HandlerOutput::result(
                    json!({ "result": remote_object(&result, view.id)? }),
                )),
                Envelope::Exception { id, exception } => {
                    let ro = remote_object(&exception, view.id)?;
                    Ok(HandlerOutput::result(json!({
                        "result": ro,
                        "exceptionDetails": {
                            "exceptionId": id,
                            "text": EXCEPTION_TEXT,
                            "lineNumber": 0,
                            "columnNumber": 0,
                            "exception": ro,
                        },
                    })))
                }
                Envelope::Reset => {
                    registry.invalidate_context(sid, view.id);
                    Err(CdpError::EXECUTION_CONTEXT_RESET)
                }
                Envelope::HandleLimit => Err(CdpError::TOO_MANY_REMOTE_OBJECTS),
                Envelope::ResultTooLarge => Err(CdpError::EVALUATION_RESULT_TOO_LARGE),
                Envelope::NotSerializable => Err(CdpError::VALUE_NOT_SERIALIZABLE),
                Envelope::InternalError
                | Envelope::Installed { .. }
                | Envelope::Released { .. } => Err(CdpError::SERVER_ERROR),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Runtime.releaseObject
// ---------------------------------------------------------------------------

/// `Runtime.releaseObject`（`CDP-2`・`CDP-6`・`CDP-7`・`SEC-2`・TASK-44.7・#1218）。
///
/// `objectId`（`"<contextId>.<n>"`）のハンドルを解放して `{}` を返す。他セッション・未知の contextId・
/// main world・ヘルパー未注入・解放済みは `REMOTE_OBJECT_NOT_FOUND`（存在を区別して漏らさない。
/// ヘルパー未注入では評価器を起動しない）。検証順は本モジュールのドキュメント参照。
pub(crate) struct RuntimeReleaseObject;

impl CommandHandler for RuntimeReleaseObject {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if obj.keys().any(|k| k != "objectId") {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let object_id = obj
                .get("objectId")
                .and_then(Value::as_str)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let (context_id, handle) =
                parse_object_id(object_id).ok_or(CdpError::INVALID_PARAMS)?;
            let state = ctx.state;
            let registry = state.registry();
            let tid = registry
                .session_target(sid)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if !state.evaluator_enabled() {
                return Err(CdpError::EXECUTION_CONTEXTS_UNAVAILABLE);
            }
            let located = locate(state, sid, &tid, Some(context_id)).map_err(|e| match e {
                LocateError::Document => CdpError::TARGET_DOCUMENT_UNAVAILABLE,
                LocateError::Lookup(ContextLookupError::UnknownContext) => {
                    CdpError::REMOTE_OBJECT_NOT_FOUND
                }
                LocateError::Lookup(l) => l.to_cdp(),
            })?;
            let (view, epoch) = match &located.view {
                Some(v) if v.world != World::Main => match v.helper_epoch {
                    Some(e) => (v.clone(), e),
                    None => return Err(CdpError::REMOTE_OBJECT_NOT_FOUND),
                },
                _ => return Err(CdpError::REMOTE_OBJECT_NOT_FOUND),
            };
            let key = ScriptContextKey::new(located.scope, located.generation, view.id);
            let timeout = DEFAULT_SCRIPT_EVAL_TIMEOUT;
            let text = match run_script(
                state,
                key,
                &located.doc,
                &release_source(epoch, handle),
                timeout,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    let (err, invalidate) = map_script_error(&e);
                    if invalidate {
                        registry.invalidate_context(sid, view.id);
                    }
                    return Err(err);
                }
            };
            match parse_envelope(&text)? {
                Envelope::Released { found: true } => Ok(HandlerOutput::result(json!({}))),
                Envelope::Released { found: false } => Err(CdpError::REMOTE_OBJECT_NOT_FOUND),
                Envelope::Reset => {
                    registry.invalidate_context(sid, view.id);
                    Err(CdpError::EXECUTION_CONTEXT_RESET)
                }
                _ => Err(CdpError::SERVER_ERROR),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp7_default_limits_are_concrete_values() {
        let l = EvalLimits::default();
        assert_eq!(l.timeout, Duration::from_secs(5));
        assert_eq!(l.max_handles, 4096);
        assert_eq!(l.max_result_len, 1_000_000);
        assert_eq!(l.max_expression_bytes, 1_048_576);
    }

    /// 連結していないこと: 組み立てたソースが固定の前置き + リテラル + 固定の後置きと一致し、リテラルを
    /// 戻すと元のペイロード・式に一致する。
    #[test]
    fn sec2_expression_is_never_concatenated_into_source() {
        let nasty = [
            "1+1",
            "\")",
            "'",
            "\\",
            "`${1}`",
            "</script>",
            "a\nb\r\nc",
            "x\u{2028}y\u{2029}z",
            "nul\u{0}byte",
            "\"); throw 1; (\"",
            "'); throw 1; ('",
            "日本語 😀",
        ];
        for expr in nasty {
            let src = evaluate_source(7, expr, true);
            let payload = json!({"epoch": 7, "expression": expr, "returnByValue": true});
            let lit = js_string_literal(&payload);
            let want = format!(
                "typeof __fandheCdpHelper === \"object\" ? __fandheCdpHelper.evaluate({lit}) : \"{{\\\"kind\\\":\\\"reset\\\"}}\""
            );
            assert_eq!(src, want, "{expr:?}");
            // リテラルは 1 個の JSON 文字列で、素の U+2028・U+2029 を含まない。
            assert!(!lit.contains('\u{2028}') && !lit.contains('\u{2029}'));
            let inner: String = serde_json::from_str(&lit).expect("literal is a JSON string");
            let back: Value = serde_json::from_str(&inner).expect("inner is JSON");
            assert_eq!(back, payload);
            assert_eq!(back["expression"], expr);
            // 式がリテラルの外（固定の前置き・後置き）に現れない。
            let prefix = "typeof __fandheCdpHelper === \"object\" ? __fandheCdpHelper.evaluate(";
            assert!(src.starts_with(prefix));
            if expr.len() > 3 {
                assert!(!prefix.contains(expr));
            }
        }
    }

    #[test]
    fn sec2_install_source_is_helper_plus_one_literal() {
        let limits = EvalLimits::default();
        let src = install_source(3, &limits);
        assert!(src.starts_with("// ") || src.starts_with("(function"));
        let cfg = json!({
            "epoch": 3,
            "maxHandles": 4096,
            "maxResultLength": 1_000_000,
            "maxDescriptionLength": 4096,
            "maxUnserializableLength": 1024,
        });
        assert!(src.ends_with(&format!("({})", js_string_literal(&cfg))));
        assert!(src.starts_with(HELPER_JS.trim()));
    }

    #[test]
    fn cdp7_object_id_parse_is_strict() {
        assert_eq!(parse_object_id("1.1"), Some((1, 1)));
        assert_eq!(parse_object_id("7.3"), Some((7, 3)));
        assert_eq!(
            parse_object_id("9007199254740991.9007199254740991"),
            Some((MAX_CONTEXT_ID, MAX_HANDLE_ID))
        );
        let long = format!("1.{}", "1".repeat(63));
        for bad in [
            "",
            "1",
            "1.",
            ".1",
            "01.1",
            "1.01",
            "+1.1",
            "-1.1",
            "1.1.1",
            " 1.1",
            "1.1 ",
            "a.1",
            "1.1x",
            "0.1",
            "1.0",
            "9007199254740992.1",
            "1.9007199254740992",
            "١.1",
            "1.١",
            long.as_str(),
        ] {
            assert_eq!(parse_object_id(bad), None, "{bad:?}");
        }
        assert_eq!(long.len(), 65);
    }

    #[test]
    fn cdp2_envelope_parse_maps_each_kind() {
        assert_eq!(
            parse_envelope(r#"{"kind":"installed","epoch":4}"#),
            Ok(Envelope::Installed { epoch: 4 })
        );
        assert_eq!(parse_envelope(r#"{"kind":"reset"}"#), Ok(Envelope::Reset));
        assert_eq!(
            parse_envelope(r#"{"kind":"handleLimit"}"#),
            Ok(Envelope::HandleLimit)
        );
        assert_eq!(
            parse_envelope(r#"{"kind":"released","found":true}"#),
            Ok(Envelope::Released { found: true })
        );
        assert_eq!(
            parse_envelope(
                r#"{"kind":"exception","exceptionId":2,"exception":{"type":"undefined"}}"#
            ),
            Ok(Envelope::Exception {
                id: 2,
                exception: json!({"type":"undefined"})
            })
        );
        for bad in [
            "",
            "not json",
            "[]",
            r#"{"kind":"unknown"}"#,
            r#"{"kind":"installed"}"#,
            r#"{"kind":"installed","epoch":0}"#,
            r#"{"kind":"value"}"#,
            r#"{"kind":"released","found":1}"#,
            r#"{"kind":"exception","exceptionId":0,"exception":{}}"#,
        ] {
            assert_eq!(parse_envelope(bad), Err(CdpError::SERVER_ERROR), "{bad:?}");
        }
    }

    #[test]
    fn cdp2_remote_object_primitives_and_unserializable_values() {
        let ro = |v: Value| remote_object(&v, 7);
        assert_eq!(
            ro(json!({"type":"number","value":2})),
            Ok(json!({"type":"number","value":2}))
        );
        assert_eq!(
            ro(json!({"type":"string","value":"a"})),
            Ok(json!({"type":"string","value":"a"}))
        );
        assert_eq!(
            ro(json!({"type":"boolean","value":true})),
            Ok(json!({"type":"boolean","value":true}))
        );
        assert_eq!(
            ro(json!({"type":"undefined"})),
            Ok(json!({"type":"undefined"}))
        );
        assert_eq!(
            ro(json!({"type":"object","subtype":"null","value":null})),
            Ok(json!({"type":"object","subtype":"null","value":null}))
        );
        for u in ["NaN", "-0", "Infinity", "-Infinity"] {
            assert_eq!(
                ro(json!({"type":"number","unserializableValue":u})),
                Ok(json!({"type":"number","unserializableValue":u}))
            );
        }
        assert_eq!(
            ro(json!({"type":"bigint","unserializableValue":"12n"})),
            Ok(json!({"type":"bigint","unserializableValue":"12n"}))
        );
        assert_eq!(
            ro(json!({"type":"bigint","unserializableValue":"-5n"})),
            Ok(json!({"type":"bigint","unserializableValue":"-5n"}))
        );
    }

    #[test]
    fn cdp2_remote_object_handle_becomes_object_id() {
        let v = remote_object(
            &json!({"type":"object","subtype":"array","className":"Array","description":"Array(2)","handle":3}),
            7,
        );
        assert_eq!(
            v,
            Ok(
                json!({"type":"object","subtype":"array","className":"Array","description":"Array(2)","objectId":"7.3"})
            )
        );
        assert_eq!(
            remote_object(
                &json!({"type":"function","className":"Function","description":"function f(){}","handle":1}),
                9
            ),
            Ok(
                json!({"type":"function","className":"Function","description":"function f(){}","objectId":"9.1"})
            )
        );
        // 値渡しのオブジェクト（objectId なし）。
        assert_eq!(
            remote_object(
                &json!({"type":"object","className":"Object","value":{"a":[1]}}),
                9
            ),
            Ok(json!({"type":"object","className":"Object","value":{"a":[1]}}))
        );
    }

    #[test]
    fn sec2_remote_object_rejects_malformed_material() {
        let long_desc = "a".repeat(MAX_DESCRIPTION_BYTES + 1);
        let long_class = "a".repeat(MAX_CLASS_NAME_BYTES + 1);
        let cases = [
            json!(1),
            json!({}),
            json!({"type":"evil"}),
            json!({"type":"object","subtype":"function","handle":1}),
            json!({"type":"object","subtype":"bogus","handle":1}),
            json!({"type":"number","unserializableValue":"1e5"}),
            json!({"type":"number","value":1,"unserializableValue":"NaN"}),
            json!({"type":"number","value":"1"}),
            json!({"type":"bigint","unserializableValue":"12"}),
            json!({"type":"bigint","unserializableValue":"n"}),
            json!({"type":"object","handle":0}),
            json!({"type":"object","handle":9007199254740992u64}),
            json!({"type":"object","handle":-1}),
            json!({"type":"object","handle":1,"value":1}),
            json!({"type":"object"}),
            json!({"type":"object","subtype":"null"}),
            json!({"type":"object","subtype":"null","value":1}),
            json!({"type":"string"}),
            json!({"type":"undefined","value":1}),
            json!({"type":"function","value":1,"handle":1}),
            json!({"type":"function"}),
            json!({"type":"object","handle":1,"description":long_desc}),
            json!({"type":"object","handle":1,"className":long_class}),
            json!({"type":"object","handle":1,"description":5}),
        ];
        for c in cases {
            assert_eq!(remote_object(&c, 1), Err(CdpError::SERVER_ERROR), "{c}");
        }
    }

    #[test]
    fn sec2_remote_object_drops_unknown_fields() {
        let v = remote_object(
            &json!({"type":"string","value":"a","evil":"x","preview":{"a":1}}),
            1,
        );
        assert_eq!(v, Ok(json!({"type":"string","value":"a"})));
    }

    #[test]
    fn cdp7_script_errors_map_to_fixed_cdp_errors() {
        use fandhe_browser_core::ScriptContextLimit as L;
        let d = Duration::from_secs(5);
        let m = |e: ScriptContextError| map_script_error(&e);
        assert_eq!(
            m(ScriptContextError::Unavailable {
                reason: UnavailableReason::Disabled
            }),
            (CdpError::EXECUTION_CONTEXTS_UNAVAILABLE, false)
        );
        for r in [
            UnavailableReason::EngineCreation,
            UnavailableReason::ContextInit,
            UnavailableReason::SpawnFailed,
            UnavailableReason::Terminated,
        ] {
            assert_eq!(
                m(ScriptContextError::Unavailable { reason: r }),
                (CdpError::SERVER_ERROR, false)
            );
        }
        assert_eq!(
            m(ScriptContextError::NotFound),
            (CdpError::EXECUTION_CONTEXT_STALE, false)
        );
        assert_eq!(
            m(ScriptContextError::LimitExceeded {
                limit: L::SourceBytes { len: 2, limit: 1 }
            }),
            (CdpError::EXPRESSION_TOO_LARGE, false)
        );
        assert_eq!(
            m(ScriptContextError::LimitExceeded {
                limit: L::ResultBytes { len: 2, limit: 1 }
            }),
            (CdpError::EVALUATION_RESULT_TOO_LARGE, false)
        );
        for l in [
            L::ContextsPerScope { limit: 4 },
            L::LiveContexts { limit: 16 },
            L::Queue { capacity: 64 },
            L::TrackedScopes { limit: 1 },
        ] {
            assert_eq!(
                m(ScriptContextError::LimitExceeded { limit: l }),
                (CdpError::EVALUATION_RESOURCE_LIMIT, false)
            );
        }
        assert_eq!(
            m(ScriptContextError::Timeout { limit: d }),
            (CdpError::EVALUATION_TIMED_OUT, false)
        );
        assert_eq!(
            m(ScriptContextError::ResourceLimit),
            (CdpError::EVALUATION_RESOURCE_LIMIT, false)
        );
        assert_eq!(
            m(ScriptContextError::EngineRestarted {
                cause: RestartCause::Timeout
            }),
            (CdpError::EVALUATION_TIMED_OUT, true)
        );
        assert_eq!(
            m(ScriptContextError::EngineRestarted {
                cause: RestartCause::ResourceLimit
            }),
            (CdpError::EVALUATION_RESOURCE_LIMIT, true)
        );
        assert_eq!(
            m(ScriptContextError::EngineRestarted {
                cause: RestartCause::Crashed
            }),
            (CdpError::EXECUTION_CONTEXT_RESET, true)
        );
        assert_eq!(
            m(ScriptContextError::EvaluationFailed {
                message: "x".into()
            }),
            (CdpError::SERVER_ERROR, false)
        );
    }

    #[test]
    fn cdp7_error_messages_are_fixed_strings() {
        assert_eq!(
            CdpError::EXPRESSION_TOO_LARGE.message,
            "expression too large"
        );
        assert_eq!(
            CdpError::MAIN_WORLD_EVALUATION_UNSUPPORTED.message,
            "evaluation in the main world is not supported"
        );
        assert_eq!(
            CdpError::EVALUATION_TIMED_OUT.message,
            "evaluation timed out"
        );
        assert_eq!(
            CdpError::EVALUATION_RESOURCE_LIMIT.message,
            "evaluation resource limit exceeded"
        );
        assert_eq!(
            CdpError::EVALUATION_RESULT_TOO_LARGE.message,
            "evaluation result too large"
        );
        assert_eq!(
            CdpError::TOO_MANY_REMOTE_OBJECTS.message,
            "too many remote objects"
        );
        assert_eq!(
            CdpError::REMOTE_OBJECT_NOT_FOUND.message,
            "remote object not found"
        );
        assert_eq!(
            CdpError::VALUE_NOT_SERIALIZABLE.message,
            "value could not be returned by value"
        );
        assert_eq!(
            CdpError::EXECUTION_CONTEXT_RESET.message,
            "execution context was reset"
        );
    }
}

/// ディスパッチャ経由の単体テスト（評価器が無効な構成。検証順・固定文言・評価器を起動しないこと）。
/// 実エンジンでの評価は `tests/runtime_evaluate_{v8,boa}.rs`。
#[cfg(test)]
mod dispatch_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use fandhe_browser_core::{AppState, ScriptContextHandle};
    use fandhe_browser_profile::Profile;

    use super::*;
    use crate::protocol::Dispatcher;
    use crate::target::TargetKind;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            Self(fandhe_browser_profile::test_base_dir().join(format!(
                "fandhe-cdp-evaluate-test-{}-{n}",
                std::process::id()
            )))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state(dir: &TempDir) -> Arc<CdpState> {
        let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
        let app = AppState::with_disabled_renderer(profile)
            .with_script_contexts(ScriptContextHandle::disabled());
        Arc::new(CdpState::new(Arc::new(app)))
    }

    fn dispatcher() -> Dispatcher {
        Dispatcher::from_handlers(vec![
            (
                "Runtime.evaluate",
                Box::new(RuntimeEvaluate::new(EvalLimits::default())),
            ),
            ("Runtime.releaseObject", Box::new(RuntimeReleaseObject)),
        ])
        .unwrap()
    }

    /// 確定済みの初期 `about:blank` を持つページとセッション。
    fn page_session(st: &Arc<CdpState>) -> (TargetId, SessionId) {
        let reg = st.registry();
        let tid = reg.create_target(TargetKind::Page, "about:blank").unwrap();
        let sid = reg.attach(&tid).unwrap();
        assert!(st.navigations().commit_initial_blank(reg, &tid));
        (tid, sid)
    }

    /// 応答 1 件（`sessionId` は任意）。
    async fn call(
        st: &Arc<CdpState>,
        sid: Option<&SessionId>,
        method: &str,
        params: &str,
    ) -> Value {
        let sid_field = sid
            .map(|s| format!(r#","sessionId":"{}""#, s.as_str()))
            .unwrap_or_default();
        let text = format!(r#"{{"id":1,"method":"{method}"{sid_field},"params":{params}}}"#);
        let frames = dispatcher().dispatch(st, &text).await.into_frames();
        assert_eq!(frames.len(), 1, "{frames:?}");
        serde_json::from_str(&frames[0]).unwrap()
    }

    fn error(v: &Value) -> (i64, &str) {
        (
            v["error"]["code"].as_i64().unwrap(),
            v["error"]["message"].as_str().unwrap(),
        )
    }

    fn worker_spawns(st: &Arc<CdpState>) -> u64 {
        st.app_state().script_contexts().stats().worker_spawns()
    }

    #[tokio::test]
    async fn cdp6_evaluate_validates_params_in_order() {
        let dir = TempDir::new();
        let st = state(&dir);
        let (_, sid) = page_session(&st);
        let m = "Runtime.evaluate";
        let unsupported = (-32602, "unsupported params");
        let invalid = (-32602, "invalid params");
        // sessionId なし。
        let v = call(&st, None, m, r#"{"expression":"1","contextId":1}"#).await;
        assert_eq!(error(&v), unsupported);
        // 未知キーは型の誤りより先。
        let v = call(
            &st,
            Some(&sid),
            m,
            r#"{"expression":1,"generatePreview":true}"#,
        )
        .await;
        assert_eq!(error(&v), unsupported);
        for p in [
            r#"{}"#,
            r#"{"expression":1}"#,
            r#"{"expression":"1","contextId":"1"}"#,
            r#"{"expression":"1","contextId":0}"#,
            r#"{"expression":"1","contextId":-1}"#,
            r#"{"expression":"1","contextId":1.5}"#,
            r#"{"expression":"1","contextId":9007199254740992}"#,
            r#"{"expression":"1","returnByValue":"yes"}"#,
            r#"{"expression":"1","awaitPromise":1}"#,
            r#"{"expression":"1","silent":null}"#,
        ] {
            let v = call(&st, Some(&sid), m, p).await;
            assert_eq!(error(&v), invalid, "{p}");
        }
        // awaitPromise: true は未対応（型検査のあと、サイズ検査の前）。
        let big = "1".repeat(1_048_577);
        let p = format!(r#"{{"expression":"{big}","contextId":1,"awaitPromise":true}}"#);
        let v = call(&st, Some(&sid), m, &p).await;
        assert_eq!(error(&v), unsupported);
        // 未知のセッション。
        let ghost = SessionId::parse("NOPE").unwrap();
        let v = call(&st, Some(&ghost), m, r#"{"expression":"1","contextId":1}"#).await;
        assert_eq!(error(&v), invalid);
        assert_eq!(worker_spawns(&st), 0);
    }

    #[tokio::test]
    async fn cdp7_oversize_expression_is_rejected_without_starting_the_evaluator() {
        let dir = TempDir::new();
        let st = state(&dir);
        let (_, sid) = page_session(&st);
        let big = "1".repeat(1_048_577);
        let p = format!(r#"{{"expression":"{big}","contextId":1}}"#);
        let v = call(&st, Some(&sid), "Runtime.evaluate", &p).await;
        assert_eq!(
            v["error"],
            json!({"code": -32000, "message": "expression too large"})
        );
        // 上限ちょうどは大きさの検査を通り、次の検査（評価器が無効）に進む。
        let ok = "1".repeat(1_048_576);
        let p = format!(r#"{{"expression":"{ok}","contextId":1}}"#);
        let v = call(&st, Some(&sid), "Runtime.evaluate", &p).await;
        assert_eq!(error(&v), (-32000, "execution contexts unavailable"));
        assert_eq!(worker_spawns(&st), 0);
    }

    #[tokio::test]
    async fn sec2_evaluate_is_unavailable_when_evaluator_disabled() {
        let dir = TempDir::new();
        let st = state(&dir);
        let (_, sid) = page_session(&st);
        let v = call(
            &st,
            Some(&sid),
            "Runtime.evaluate",
            r#"{"expression":"1","contextId":1}"#,
        )
        .await;
        assert_eq!(
            v,
            json!({"id":1,"sessionId":sid.as_str(),"error":{"code":-32000,"message":"execution contexts unavailable"}})
        );
        assert_eq!(worker_spawns(&st), 0);
    }

    #[tokio::test]
    async fn cdp6_release_object_validates_params_in_order() {
        let dir = TempDir::new();
        let st = state(&dir);
        let (_, sid) = page_session(&st);
        let m = "Runtime.releaseObject";
        let unsupported = (-32602, "unsupported params");
        let invalid = (-32602, "invalid params");
        let v = call(&st, None, m, r#"{"objectId":"1.1"}"#).await;
        assert_eq!(error(&v), unsupported);
        let v = call(
            &st,
            Some(&sid),
            m,
            r#"{"objectId":"1.1","objectGroup":"g"}"#,
        )
        .await;
        assert_eq!(error(&v), unsupported);
        for p in [
            r#"{}"#,
            r#"{"objectId":1}"#,
            r#"{"objectId":""}"#,
            r#"{"objectId":"1"}"#,
            r#"{"objectId":"01.1"}"#,
            r#"{"objectId":"1.0"}"#,
            r#"{"objectId":"-1.1"}"#,
            r#"{"objectId":"1.1.1"}"#,
        ] {
            let v = call(&st, Some(&sid), m, p).await;
            assert_eq!(error(&v), invalid, "{p}");
        }
        let ghost = SessionId::parse("NOPE").unwrap();
        let v = call(&st, Some(&ghost), m, r#"{"objectId":"1.1"}"#).await;
        assert_eq!(error(&v), invalid);
        // 書式が正しく、評価器が無効。
        let v = call(&st, Some(&sid), m, r#"{"objectId":"1.1"}"#).await;
        assert_eq!(error(&v), (-32000, "execution contexts unavailable"));
        assert_eq!(worker_spawns(&st), 0);
    }
}

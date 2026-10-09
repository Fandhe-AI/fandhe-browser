//! boa（`boa_engine`）による [`JsEngine`] 実装（MS-3・TASK-32（32.2）・
//! ビヘイビア `JS-1`・Issue #166）。
//!
//! 呼び出し元: [`crate::boa_worker::BoaChildEngine`] が **子プロセスの中で**
//! [`BoaEngine::new`] を呼ぶ。ホスト側へは [`crate::engine_trait::create_engine`]
//! が子プロセス版エンジン（`process_engine`）を返し、本型は直接ホストへ渡らない
//! （`mod` は非公開。boa の具象型 `Context`・`JsValue` 等も本モジュールの外へ
//! 出さない。coding-rust.md「V8 / boa の具象型を上位 crate へ漏らさない」）。
//!
//! 本型自体は呼び出しスレッド上で同期実行する素の boa ラッパーで、boa に
//! 中断 API・ヒープ上限 API が無いため、実時間・メモリの上限は **ホストと
//! 別プロセスで動かすこと** で強制する（AGENTS.md「リソース上限」P0。
//! 詳細は `boa_worker` のモジュール doc）。`Context` は `!Send` であり、
//! [`JsEngine`] 自体も `Send` を要求しない。
//!
//! # `unsafe` を使わない設計
//!
//! [`NativeFn`] は `Copy` でも boa の `Trace` でもないため、boa のクロージャへ
//! 直接捕獲できない（捕獲には `unsafe` な API が必要）。そこで [`NativeFn`] は
//! GC の外にある登録簿（[`NativeRegistry`]。`Context::insert_data` で保持）へ
//! 置き、boa 側の関数は `Copy` な ID だけを捕獲する
//! （`NativeFunction::from_copy_closure`。safe API）。
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! boa 0.22 は V8 と同等のリソース制御を持たない。本型の中での打ち切りは
//! 再帰深度・スタックサイズだけで、実時間・ヒープの上限は持たない。実時間と
//! メモリは本型を子プロセスの中で動かし、親が強制する（`boa_worker`・
//! `process_engine`・`resource_limits`）。
//!
//! - **ループ反復上限は設けない**（`TASK-109`・Issue #776）: 反復上限を
//!   `EvaluateOptions::timeout` と競合させると `while (true) {}` が
//!   `Timeout` ではなく `ResourceLimitExceeded` になり、V8 と種別が揃わない。
//!   実時間は親が子を期限で `kill` して強制する（Context は破棄され、親が
//!   返す `Timeout` のメッセージに "context was discarded" を含む）
//! - 再帰・スタックの上限は [`JsEngineError::ResourceLimitExceeded`] として
//!   区別して返す。コンテキストは残るため、メッセージに
//!   "context was discarded" は含めない
//! - タイムアウト 0 は評価せずに `Timeout`、評価完了後に経過時間が
//!   `options.timeout()` 以上だった場合は結果を捨てて `Timeout`（fail-closed。
//!   いずれも Context は残る）
//! - 結果文字列の UTF-8 バイト数上限（`options.max_result_bytes()`）は本型で
//!   検査する（`JS-6`）
//! - [`NativeFn`] は本型を動かすプロセスの中で同期実行する。子プロセスでは
//!   親へ転送するプロキシ関数（`boa_worker`）で、期限は親が掛ける

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use boa_engine::context::HostHooks;
use boa_engine::error::{EngineError, RuntimeLimitError};
use boa_engine::object::{FunctionObjectBuilder, ObjectInitializer};
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{
    Context, JsError, JsNativeError, JsResult, JsString, JsValue as BoaValue, NativeFunction,
    Source, js_string,
};

use crate::engine_trait::{EvaluateOptions, JsEngine, JsEngineError, JsValue, NativeFn};
use crate::worker_protocol::{
    MAX_DOM_LIKE_MEMBER_NAME_BYTES, MAX_DOM_LIKE_OBJECT_MEMBERS, MAX_GLOBAL_FUNCTION_NAME_BYTES,
    MAX_NATIVE_CALL_ARGS, MAX_REGISTERED_GLOBAL_FUNCTIONS,
};

/// 評価できるスクリプトの最大バイト数。`v8_engine::MAX_SCRIPT_SOURCE_BYTES`
/// と同値に保つ（V8 版と同じ入力上限。OWASP A04）。
const MAX_SCRIPT_SOURCE_BYTES: usize = 1_048_576; // 1 MiB

/// 文字列の評価結果・ネイティブ関数の引数／戻り値に許容する最大 UTF-16 単位数。
/// `v8_engine::MAX_RESULT_STRING_UTF16_UNITS` と同値に保つ。
const MAX_STRING_UTF16_UNITS: usize = 1_048_576;

/// [`JsEngineError`] へ入れるエラーメッセージの最大文字数。
/// `v8_engine::MAX_ERROR_MESSAGE_CHARS` と同値に保つ（巨大な throw 値で
/// エラー値が肥大化しないようにする）。
const MAX_ERROR_MESSAGE_CHARS: usize = 1024;

/// `ArrayBuffer`・`SharedArrayBuffer` 1 つあたりの最大バイト数（128 MiB）。
/// V8 版の `resource_limits::MAX_ARRAY_BUFFER_ALLOCATION_BYTES` と同値に保つ。
/// boa 既定（1.5 GiB）では単発の巨大確保が親の RSS 監視（320 MiB）より先に
/// OS の上限へ達しうるため、ホストフックで引き下げる。
///
/// 単位は**バイト**（boa 0.22 の `create_byte_data_block` /
/// `create_shared_byte_data_block` が確保サイズ（バイト）をこのフックの戻り値と
/// 直接比較する。boa 既定値 1_610_612_736 も 1.5 GiB のバイト数）。なお
/// フックは 1 回の確保の上限しか決められず、合計の確保量は制限できない。
/// 合計はプロセス単位の上限（Linux の `RLIMIT_DATA`・Windows の Job Object・
/// 親の RSS 監視）で抑える（`resource_limits::tighten_child_memory_limit_for_boa`）。
const MAX_BUFFER_BYTES: u64 = 128 * 1024 * 1024;

/// boa の `HostHooks` のうち、バッファ確保の上限だけを差し替える実装
/// （他のフックは boa 既定のまま。safe API のみ）。
struct BoundedBufferHooks;

impl HostHooks for BoundedBufferHooks {
    fn max_buffer_size(&self, _context: &mut Context) -> u64 {
        MAX_BUFFER_BYTES
    }
}

/// グローバル関数と DOM 風オブジェクトのメンバーを合わせた、登録簿の総エントリ
/// 数の上限。グローバル名の件数上限（[`MAX_REGISTERED_GLOBAL_FUNCTIONS`]）とは
/// 別軸の上限で、メンバー数の多い DOM 風オブジェクトで登録簿が肥大化するのを
/// 防ぐ。メソッド 0 個の bind は登録簿を増やさないため、こちらでは数えられない
/// （グローバル名の件数上限が防ぐ）。
const MAX_REGISTRY_ENTRIES: usize = 4096;

/// `Context` に保持する [`NativeFn`] の登録簿。
///
/// GC の外に置くことで `Trace` 不要になる。boa 側の関数は添字（ID）だけを
/// 捕獲して [`dispatch_native`] 経由でここを引く。
struct NativeRegistry {
    functions: RefCell<Vec<NativeFn>>,
}

/// boa による [`JsEngine`] 実装。[`crate::engine_trait::create_engine`] から
/// のみ生成される。
///
/// `Context` は最初の操作で遅延生成する（`new` は I/O もコンテキスト生成も
/// しない。V8 版の「起動失敗は初回呼び出しのエラーとして現れる」契約と揃える）。
pub(crate) struct BoaEngine {
    context: Option<Context>,
    /// 定義済みのグローバル名（関数・DOM 風オブジェクト）。重複登録を拒否する。
    defined_names: HashSet<String>,
    /// 登録済みのグローバル名の件数（グローバル関数と DOM 風オブジェクトを
    /// 合算する。メソッド 0 個の DOM 風オブジェクトも 1 件に数える。V8 版
    /// `process_engine`・`worker` の `registered_count` と同じ数え方）。
    /// 上限は [`MAX_REGISTERED_GLOBAL_FUNCTIONS`]。
    global_function_count: usize,
    /// ループ反復上限。本番（`new`）は `None`（無効。実時間は親の kill で強制する。
    /// モジュールドキュメント参照）。`RuntimeLimitError::LoopIteration` の
    /// 変換経路をテストするためだけに `Some` を指定できる。
    loop_iteration_limit: Option<u64>,
}

impl BoaEngine {
    /// エンジンを作る（コンテキストはまだ作らない）。
    pub(crate) fn new() -> Self {
        Self {
            context: None,
            defined_names: HashSet::new(),
            global_function_count: 0,
            loop_iteration_limit: None,
        }
    }

    /// テスト専用: ループ反復上限を有効にしたエンジンを作る（`LoopIteration` の
    /// `ResourceLimitExceeded` 変換経路の検証用）。
    #[cfg(test)]
    fn with_loop_iteration_limit_for_test(limit: u64) -> Self {
        let mut engine = Self::new();
        engine.loop_iteration_limit = Some(limit);
        engine
    }

    /// コンテキストを（未生成なら）作って返す。生成失敗は
    /// [`JsEngineError::EngineUnavailable`]。
    fn context_mut(&mut self) -> Result<&mut Context, JsEngineError> {
        if self.context.is_none() {
            // `Context::default()` は内部で `expect` するため使わない。
            let mut context = Context::builder()
                .host_hooks(Rc::new(BoundedBufferHooks))
                .build()
                .map_err(|err| {
                    JsEngineError::EngineUnavailable(truncate_message(&format!(
                        "failed to create boa context: {err}"
                    )))
                })?;
            if let Some(limit) = self.loop_iteration_limit {
                context.runtime_limits_mut().set_loop_iteration_limit(limit);
            }
            // recursion・stack_size は boa 既定（512・10240）を維持する。
            context.insert_data(NativeRegistry {
                functions: RefCell::new(Vec::new()),
            });
            self.context = Some(context);
        }
        self.context
            .as_mut()
            .ok_or_else(|| JsEngineError::EngineUnavailable("boa context is missing".into()))
    }
}

/// `Timeout` のメッセージ。V8 版（`V8Failure::Terminated`）と同じ書式。
fn timeout_message(stage: &str, timeout: Duration) -> String {
    format!(
        "script {stage} exceeded the {} second timeout and was terminated",
        timeout.as_secs_f64()
    )
}

/// 評価の経過時間が `timeout` に達したか（評価後の fail-closed 判定。純粋関数）。
fn exceeded_timeout(elapsed: Duration, timeout: Duration) -> bool {
    elapsed >= timeout
}

// TASK-109・`JS-6`: 結果サイズ上限は本型、実行中の実時間上限は親の子 kill で強制する
// （モジュールドキュメント参照）。
impl JsEngine for BoaEngine {
    fn evaluate_script(
        &mut self,
        script: &str,
        options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        if script.len() > MAX_SCRIPT_SOURCE_BYTES {
            return Err(JsEngineError::EvaluationFailed(format!(
                "script exceeds the maximum supported size of {MAX_SCRIPT_SOURCE_BYTES} bytes"
            )));
        }
        // タイムアウト 0 は V8 版と同様、スクリプトを実行せず即 `Timeout`。
        if options.timeout().is_zero() {
            return Err(JsEngineError::Timeout(timeout_message(
                "execution",
                options.timeout(),
            )));
        }
        let context = self.context_mut()?;
        let started = Instant::now();
        let outcome = context.eval(Source::from_bytes(script));
        // boa には中断手段が無く、実行中の打ち切りは親の kill に任せる。完了後でも
        // 期限を過ぎていれば結果を成功扱いにしない（V8 なら打ち切られていた評価）。
        if exceeded_timeout(started.elapsed(), options.timeout()) {
            return Err(JsEngineError::Timeout(timeout_message(
                "execution",
                options.timeout(),
            )));
        }
        match outcome {
            Ok(value) => from_boa_value_with_limit(&value, Some(options.max_result_bytes()))
                .map_err(JsEngineError::EvaluationFailed),
            Err(err) => Err(convert_eval_error(&err, context)),
        }
    }

    fn inject_global_function(&mut self, name: &str, func: NativeFn) -> Result<(), JsEngineError> {
        validate_name(name, MAX_GLOBAL_FUNCTION_NAME_BYTES, "global function")?;
        if self.global_function_count >= MAX_REGISTERED_GLOBAL_FUNCTIONS {
            return Err(JsEngineError::BindingFailed(format!(
                "too many global functions (maximum {MAX_REGISTERED_GLOBAL_FUNCTIONS})"
            )));
        }
        if self.defined_names.contains(name) {
            return Err(JsEngineError::BindingFailed(format!(
                "global name '{name}' is already defined"
            )));
        }
        let context = self.context_mut()?;
        let id = next_registry_id(context, 1)?;
        let function = FunctionObjectBuilder::new(context.realm(), native_for_id(id))
            .name(JsString::from(name))
            .length(0)
            .build();
        define_global(context, name, BoaValue::from(function))?;
        push_registry(context, vec![func])?;
        self.defined_names.insert(name.to_string());
        self.global_function_count += 1;
        Ok(())
    }

    fn bind_dom_like_object(
        &mut self,
        name: &str,
        methods: Vec<(String, NativeFn)>,
    ) -> Result<(), JsEngineError> {
        validate_name(name, MAX_GLOBAL_FUNCTION_NAME_BYTES, "dom-like object")?;
        if methods.len() > MAX_DOM_LIKE_OBJECT_MEMBERS {
            return Err(JsEngineError::BindingFailed(format!(
                "too many members (maximum {MAX_DOM_LIKE_OBJECT_MEMBERS})"
            )));
        }
        let mut seen = HashSet::new();
        for (member, _) in &methods {
            validate_name(member, MAX_DOM_LIKE_MEMBER_NAME_BYTES, "member")?;
            if !seen.insert(member.as_str()) {
                return Err(JsEngineError::BindingFailed(format!(
                    "duplicate member name '{member}'"
                )));
            }
        }
        if self.global_function_count >= MAX_REGISTERED_GLOBAL_FUNCTIONS {
            return Err(JsEngineError::BindingFailed(format!(
                "too many global names (maximum {MAX_REGISTERED_GLOBAL_FUNCTIONS})"
            )));
        }
        if self.defined_names.contains(name) {
            return Err(JsEngineError::BindingFailed(format!(
                "global name '{name}' is already defined"
            )));
        }
        let context = self.context_mut()?;
        let base = next_registry_id(context, methods.len())?;
        let mut initializer = ObjectInitializer::new(context);
        let mut functions = Vec::with_capacity(methods.len());
        for (offset, (member, func)) in methods.into_iter().enumerate() {
            initializer.function(
                native_for_id(base.saturating_add(offset)),
                JsString::from(member.as_str()),
                0,
            );
            functions.push(func);
        }
        let object = initializer.build();
        define_global(context, name, BoaValue::from(object))?;
        push_registry(context, functions)?;
        self.defined_names.insert(name.to_string());
        self.global_function_count += 1;
        Ok(())
    }
}

/// 名前（関数名・オブジェクト名・メンバー名）を検証する。空・長すぎる名前は
/// `BindingFailed`。名前は JS ソースへ連結せずプロパティキーとして定義する
/// （名前経由のコード注入経路を作らない）。
fn validate_name(name: &str, max_bytes: usize, what: &str) -> Result<(), JsEngineError> {
    if name.is_empty() {
        return Err(JsEngineError::BindingFailed(format!(
            "{what} name must not be empty"
        )));
    }
    if name.len() > max_bytes {
        return Err(JsEngineError::BindingFailed(format!(
            "{what} name exceeds the maximum of {max_bytes} bytes"
        )));
    }
    Ok(())
}

/// 次に割り当てる登録簿 ID を返し、`additional` 個の追加が総数上限を超える場合は
/// `BindingFailed` にする。
fn next_registry_id(context: &Context, additional: usize) -> Result<usize, JsEngineError> {
    let registry = context
        .get_data::<NativeRegistry>()
        .ok_or_else(|| JsEngineError::BindingFailed("native registry is missing".into()))?;
    let len = registry
        .functions
        .try_borrow()
        .map_err(|_| JsEngineError::BindingFailed("native registry is busy".into()))?
        .len();
    if len.saturating_add(additional) > MAX_REGISTRY_ENTRIES {
        return Err(JsEngineError::BindingFailed(format!(
            "too many native functions registered (maximum {MAX_REGISTRY_ENTRIES})"
        )));
    }
    Ok(len)
}

/// boa 側の定義が成功した後に、[`NativeFn`] を登録簿へ積む（定義失敗時に
/// 登録簿へ残骸を残さないため、定義の後に呼ぶ）。
fn push_registry(context: &Context, functions: Vec<NativeFn>) -> Result<(), JsEngineError> {
    let registry = context
        .get_data::<NativeRegistry>()
        .ok_or_else(|| JsEngineError::BindingFailed("native registry is missing".into()))?;
    registry
        .functions
        .try_borrow_mut()
        .map_err(|_| JsEngineError::BindingFailed("native registry is busy".into()))?
        .extend(functions);
    Ok(())
}

/// `Copy` な ID だけを捕獲する boa 関数を作る（`unsafe` 不要の橋渡し）。
fn native_for_id(id: usize) -> NativeFunction {
    NativeFunction::from_copy_closure(move |_this, args, context| {
        dispatch_native(id, args, context)
    })
}

/// グローバルオブジェクトへ writable・non-enumerable・configurable で定義する。
/// `undefined` 等の non-configurable な既存プロパティへの定義失敗は
/// `BindingFailed`（V8 版と同じ挙動）。
fn define_global(context: &mut Context, name: &str, value: BoaValue) -> Result<(), JsEngineError> {
    let descriptor = PropertyDescriptor::builder()
        .value(value)
        .writable(true)
        .enumerable(false)
        .configurable(true)
        .build();
    let global = context.global_object();
    global
        .define_property_or_throw(PropertyKey::from(JsString::from(name)), descriptor, context)
        .map(|_| ())
        .map_err(|err| {
            JsEngineError::BindingFailed(truncate_message(&format!(
                "failed to define global '{name}': {err}"
            )))
        })
}

/// boa から呼ばれるネイティブ呼び出しの入口。登録簿から [`NativeFn`] を引いて
/// 実行する。引数・登録簿 ID は untrusted として扱い、`unwrap`・添字アクセス・
/// `borrow_mut` は使わない。失敗は catch 可能な JS 例外へ変換する（成功を装わない）。
fn dispatch_native(id: usize, args: &[BoaValue], context: &mut Context) -> JsResult<BoaValue> {
    if args.len() > MAX_NATIVE_CALL_ARGS {
        return Err(native_error(&format!(
            "too many arguments (maximum {MAX_NATIVE_CALL_ARGS})"
        )));
    }
    let mut converted = Vec::with_capacity(args.len());
    for arg in args {
        converted.push(from_boa_value(arg).map_err(|msg| native_error(&msg))?);
    }
    let outcome = {
        let registry = context
            .get_data::<NativeRegistry>()
            .ok_or_else(|| native_error("native registry is missing"))?;
        let mut functions = registry
            .functions
            .try_borrow_mut()
            .map_err(|_| native_error("native function called re-entrantly"))?;
        let function = functions
            .get_mut(id)
            .ok_or_else(|| native_error("unknown native function id"))?;
        function(&converted)
    };
    match outcome {
        Ok(value) => to_boa_value(&value).map_err(|msg| native_error(&msg)),
        Err(err) => Err(native_error(engine_error_message(&err))),
    }
}

/// [`JsEngineError`] が保持する元メッセージを返す。`Display` はバリアント名の
/// 接頭辞（"script evaluation failed: " 等）を付けるため、JS の `Error.message`
/// へ渡す際に二重の接頭辞（親側でラップ済みのメッセージへ再度付与）にならない
/// よう、接頭辞なしの本文だけを取り出す。
fn engine_error_message(err: &JsEngineError) -> &str {
    match err {
        JsEngineError::EvaluationFailed(msg)
        | JsEngineError::BindingFailed(msg)
        | JsEngineError::ResourceLimitExceeded(msg)
        | JsEngineError::Timeout(msg)
        | JsEngineError::EngineUnavailable(msg) => msg,
    }
}

/// catch 可能な JS `Error` を作る。
fn native_error(message: &str) -> JsError {
    JsNativeError::error()
        .with_message(truncate_message(message))
        .into()
}

/// boa の値を [`JsValue`] へ変換する。表現できない型（Object・Symbol・BigInt）
/// と上限超過の文字列は `Err`（切り詰めず、成功を装わない）。
fn from_boa_value(value: &BoaValue) -> Result<JsValue, String> {
    from_boa_value_with_limit(value, None)
}

/// [`from_boa_value`] に評価結果用の UTF-8 バイト上限（`max_result_bytes`）を
/// 足したもの。`None` はネイティブ関数引数の経路（固定の UTF-16 上限のみ。
/// V8 版と同じ方針）。バイト数は `to_std_string_lossy` の後で数える（lone
/// surrogate は U+FFFD = 3 バイト）。巨大確保を避けるため UTF-16 上限を先に判定する。
fn from_boa_value_with_limit(
    value: &BoaValue,
    max_result_bytes: Option<usize>,
) -> Result<JsValue, String> {
    if value.is_undefined() {
        return Ok(JsValue::Undefined);
    }
    if value.is_null() {
        return Ok(JsValue::Null);
    }
    if let Some(b) = value.as_boolean() {
        return Ok(JsValue::Bool(b));
    }
    if let Some(n) = value.as_number() {
        return Ok(JsValue::Number(n));
    }
    if let Some(s) = value.as_string() {
        if s.len() > MAX_STRING_UTF16_UNITS {
            return Err(format!(
                "string result exceeds the maximum supported length of \
                 {MAX_STRING_UTF16_UNITS} UTF-16 code units"
            ));
        }
        let converted = s.to_std_string_lossy();
        if let Some(limit) = max_result_bytes
            && converted.len() > limit
        {
            return Err(format!(
                "string result of {} bytes exceeds the result size limit of {limit} bytes",
                converted.len()
            ));
        }
        return Ok(JsValue::String(converted));
    }
    Err(format!(
        "evaluation result of type '{}' is not representable as JsValue",
        value.type_of()
    ))
}

/// [`JsValue`] を boa の値へ変換する。`ObjectHandle` と将来の variant は
/// 変換せず `Err`（JS 側で catch 可能なエラーになる。[`JsValue::ObjectHandle`]
/// の契約）。
fn to_boa_value(value: &JsValue) -> Result<BoaValue, String> {
    match value {
        JsValue::Undefined => Ok(BoaValue::undefined()),
        JsValue::Null => Ok(BoaValue::null()),
        JsValue::Bool(b) => Ok(BoaValue::from(*b)),
        JsValue::Number(n) => Ok(BoaValue::from(*n)),
        JsValue::String(s) => {
            if s.encode_utf16().count() > MAX_STRING_UTF16_UNITS {
                return Err(format!(
                    "native function returned a string longer than \
                     {MAX_STRING_UTF16_UNITS} UTF-16 code units"
                ));
            }
            Ok(BoaValue::from(JsString::from(s.as_str())))
        }
        _ => Err("native function returned a value that is not representable in boa".to_string()),
    }
}

/// 評価エラーを [`JsEngineError`] へ変換する。ランタイム上限は
/// `ResourceLimitExceeded`、それ以外は `EvaluationFailed`。
fn convert_eval_error(err: &JsError, context: &mut Context) -> JsEngineError {
    if let Some(EngineError::RuntimeLimit(limit)) = err.as_engine() {
        let what = match limit {
            RuntimeLimitError::LoopIteration => "loop iteration limit",
            RuntimeLimitError::Recursion => "recursion limit",
            RuntimeLimitError::StackSize => "stack size limit",
        };
        return JsEngineError::ResourceLimitExceeded(format!("boa runtime limit reached: {what}"));
    }
    JsEngineError::EvaluationFailed(truncate_message(&error_message(err, context)))
}

/// エラーからメッセージを取り出す。getter・`toString` を走らせないよう、
/// オブジェクトは own データプロパティ `message` のみを読む。
fn error_message(err: &JsError, context: &mut Context) -> String {
    if let Ok(native) = err.try_native(context) {
        return native.message().to_string();
    }
    if let Some(value) = err.as_opaque() {
        if let Some(object) = value.as_object() {
            let key = PropertyKey::from(js_string!("message"));
            let message = object
                .borrow()
                .properties()
                .get(&key)
                .and_then(|desc| desc.value().and_then(|v| v.as_string()));
            return match message {
                Some(s) if s.len() <= MAX_ERROR_MESSAGE_CHARS * 4 => s.to_std_string_lossy(),
                Some(_) => "error message is too long".to_string(),
                None => "uncaught exception (object)".to_string(),
            };
        }
        if let Some(s) = value.as_string() {
            if s.len() > MAX_ERROR_MESSAGE_CHARS * 4 {
                return "error message is too long".to_string();
            }
            return s.to_std_string_lossy();
        }
        return value.display().to_string();
    }
    err.to_string()
}

/// メッセージを [`MAX_ERROR_MESSAGE_CHARS`] 文字へ切り詰める。
fn truncate_message(message: &str) -> String {
    message.chars().take(MAX_ERROR_MESSAGE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn eval(engine: &mut BoaEngine, script: &str) -> Result<JsValue, JsEngineError> {
        engine.evaluate_script(script, &EvaluateOptions::default())
    }

    /// JS-1: 算術・文字列・プリミティブリテラルの評価。
    #[test]
    fn js_1_boa_evaluates_primitives() {
        let mut e = BoaEngine::new();
        assert_eq!(eval(&mut e, "1 + 2 * 3").unwrap(), JsValue::Number(7.0));
        assert_eq!(eval(&mut e, "1.5 + 1").unwrap(), JsValue::Number(2.5));
        assert_eq!(
            eval(&mut e, "'a' + '-' + 'b'").unwrap(),
            JsValue::String("a-b".into())
        );
        assert_eq!(eval(&mut e, "true").unwrap(), JsValue::Bool(true));
        assert_eq!(eval(&mut e, "null").unwrap(), JsValue::Null);
        assert_eq!(eval(&mut e, "undefined").unwrap(), JsValue::Undefined);
    }

    /// JS-1: 構文エラー・例外は EvaluationFailed で、後続の評価も可能・状態は保持される。
    #[test]
    fn js_1_boa_errors_keep_engine_usable_and_state() {
        let mut e = BoaEngine::new();
        eval(&mut e, "var x = 41").unwrap();
        assert!(matches!(
            eval(&mut e, "1 +"),
            Err(JsEngineError::EvaluationFailed(_))
        ));
        match eval(&mut e, "throw new Error('boom')") {
            Err(JsEngineError::EvaluationFailed(msg)) => assert_eq!(msg, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(eval(&mut e, "x + 1").unwrap(), JsValue::Number(42.0));
    }

    /// JS-1: 表現できない結果型は EvaluationFailed（型名を含む）。
    #[test]
    fn js_1_boa_rejects_unrepresentable_result() {
        let mut e = BoaEngine::new();
        match eval(&mut e, "({})") {
            Err(JsEngineError::EvaluationFailed(msg)) => assert_eq!(
                msg,
                "evaluation result of type 'object' is not representable as JsValue"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JS-1: 巨大な throw 値のメッセージは 1024 文字へ切り詰められる。
    #[test]
    fn js_1_boa_truncates_error_message() {
        let mut e = BoaEngine::new();
        match eval(&mut e, "throw new Error('a'.repeat(5000))") {
            Err(JsEngineError::EvaluationFailed(msg)) => assert_eq!(msg.chars().count(), 1024),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JS-1: スクリプト長上限超過は評価せず EvaluationFailed。
    #[test]
    fn js_1_boa_rejects_oversized_script() {
        let mut e = BoaEngine::new();
        let script = " ".repeat(MAX_SCRIPT_SOURCE_BYTES + 1);
        assert!(matches!(
            eval(&mut e, &script),
            Err(JsEngineError::EvaluationFailed(_))
        ));
    }

    /// JS-1: 上限超過の結果文字列は切り詰めず Err（上限ちょうどは成功）。
    /// boa の組込み `repeat` もループ上限に数えられるため、半分の長さの文字列を
    /// 連結して長さを作る。
    #[test]
    fn js_1_boa_rejects_oversized_result_string() {
        let mut e = BoaEngine::new();
        let half = MAX_STRING_UTF16_UNITS / 2;
        eval(&mut e, &format!("var s = 'a'.repeat({half})")).unwrap();
        match eval(&mut e, "s + s") {
            Ok(JsValue::String(v)) => assert_eq!(v.len(), MAX_STRING_UTF16_UNITS),
            other => panic!("unexpected: {other:?}"),
        }
        match eval(&mut e, "s + s + 'a'") {
            Err(JsEngineError::EvaluationFailed(msg)) => assert_eq!(
                msg,
                "string result exceeds the maximum supported length of 1048576 UTF-16 code units"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JS-1: テスト用にループ反復上限を有効にした場合のみ、無限ループは
    /// ResourceLimitExceeded になり、その後もエンジンが使える
    /// （本番の `new()` は反復上限を持たない。実時間は親の kill で強制する）。
    #[test]
    fn js_1_boa_infinite_loop_hits_resource_limit() {
        let mut e = BoaEngine::with_loop_iteration_limit_for_test(1000);
        match eval(&mut e, "while (true) {}") {
            Err(JsEngineError::ResourceLimitExceeded(msg)) => {
                assert_eq!(msg, "boa runtime limit reached: loop iteration limit")
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(eval(&mut e, "1 + 1").unwrap(), JsValue::Number(2.0));
    }

    fn eval_with(
        engine: &mut BoaEngine,
        script: &str,
        options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
        engine.evaluate_script(script, options)
    }

    /// JS-6: タイムアウト 0 はスクリプトを実行せず、V8 と同じ文言の Timeout。
    #[test]
    fn js_6_boa_zero_timeout_rejects_without_running_script() {
        let mut e = BoaEngine::new();
        let zero = EvaluateOptions::default().with_timeout(Duration::ZERO);
        match eval_with(&mut e, "globalThis.x = 1", &zero) {
            Err(JsEngineError::Timeout(msg)) => assert_eq!(
                msg,
                "script execution exceeded the 0 second timeout and was terminated"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(
            eval(&mut e, "typeof globalThis.x").unwrap(),
            JsValue::String("undefined".to_string())
        );
    }

    /// JS-6: 評価後の経過時間判定（純粋関数）の境界。
    #[test]
    fn js_6_boa_exceeded_timeout_boundary() {
        let t = Duration::from_millis(10);
        assert!(!exceeded_timeout(Duration::from_millis(9), t));
        assert!(exceeded_timeout(t, t));
        assert!(exceeded_timeout(Duration::from_millis(11), t));
    }

    /// JS-6: 完了しても期限を過ぎていれば結果を捨てて Timeout（Context は残る）。
    #[test]
    fn js_6_boa_late_completion_is_reported_as_timeout() {
        let mut e = BoaEngine::new();
        let tiny = EvaluateOptions::default().with_timeout(Duration::from_millis(1));
        // 少なくとも 1ms はかかる有限ループ（時刻を見て確実に待つ）。
        let script = "var t = Date.now(); while (Date.now() - t < 20) {} 1";
        match eval_with(&mut e, script, &tiny) {
            Err(JsEngineError::Timeout(msg)) => assert_eq!(
                msg,
                "script execution exceeded the 0.001 second timeout and was terminated"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(eval(&mut e, "1 + 1").unwrap(), JsValue::Number(2.0));
    }

    /// JS-6: 既定 options でも固定の UTF-16 上限（1M 単位）の境界は従来どおり。
    #[test]
    fn js_6_boa_result_size_limit_boundary_with_default_options() {
        let mut e = BoaEngine::new();
        match eval(&mut e, "'a'.repeat(1048576)") {
            Ok(JsValue::String(v)) => assert_eq!(v.len(), 1_048_576),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            eval(&mut e, "'a'.repeat(1048577)"),
            Err(JsEngineError::EvaluationFailed(_))
        ));
    }

    /// JS-6: 明示した結果上限の境界（ちょうどは成功、+1 は Err）。
    #[test]
    fn js_6_boa_result_size_limit_boundary_with_small_limit() {
        let mut e = BoaEngine::new();
        let opts = EvaluateOptions::default().with_max_result_bytes(16);
        match eval_with(&mut e, "'a'.repeat(16)", &opts) {
            Ok(JsValue::String(v)) => assert_eq!(v.len(), 16),
            other => panic!("unexpected: {other:?}"),
        }
        match eval_with(&mut e, "'a'.repeat(17)", &opts) {
            Err(JsEngineError::EvaluationFailed(msg)) => assert_eq!(
                msg,
                "string result of 17 bytes exceeds the result size limit of 16 bytes"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        // 超過後もエンジンは使える。
        assert_eq!(
            eval_with(&mut e, "1 + 1", &opts).unwrap(),
            JsValue::Number(2.0)
        );
    }

    /// JS-6: 上限は UTF-8 バイト数で数える（`あ` は 3 バイト）。
    #[test]
    fn js_6_boa_result_size_limit_counts_utf8_bytes() {
        let mut e = BoaEngine::new();
        let opts = EvaluateOptions::default().with_max_result_bytes(9);
        assert!(matches!(
            eval_with(&mut e, "'あ'.repeat(3)", &opts),
            Ok(JsValue::String(_))
        ));
        assert!(matches!(
            eval_with(&mut e, "'あ'.repeat(3) + 'a'", &opts),
            Err(JsEngineError::EvaluationFailed(_))
        ));
    }

    /// JS-6: 既定上限（3 MiB）は UTF-16 上限内の非 ASCII 結果を通す
    /// （400000 文字 = 1.2 MB）。
    #[test]
    fn js_6_boa_default_options_keep_large_non_ascii_result() {
        let mut e = BoaEngine::new();
        match eval(&mut e, "'あ'.repeat(400000)") {
            Ok(JsValue::String(v)) => assert_eq!(v.len(), 1_200_000),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JS-1: 深い再帰は ResourceLimitExceeded。
    #[test]
    fn js_1_boa_deep_recursion_hits_resource_limit() {
        let mut e = BoaEngine::new();
        let result = eval(&mut e, "function f() { return f() + 1 } f()");
        assert!(
            matches!(result, Err(JsEngineError::ResourceLimitExceeded(_))),
            "unexpected: {result:?}"
        );
        assert_eq!(eval(&mut e, "2 + 2").unwrap(), JsValue::Number(4.0));
    }

    /// JS-1: ネイティブ関数の往復・Err が JS で catch 可能。
    #[test]
    fn js_1_boa_native_function_round_trip() {
        let mut e = BoaEngine::new();
        let seen: Arc<Mutex<Vec<JsValue>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        e.inject_global_function(
            "rec",
            Box::new(move |args| {
                seen2.lock().unwrap().extend_from_slice(args);
                Ok(JsValue::String("ret".into()))
            }),
        )
        .unwrap();
        e.inject_global_function(
            "fail",
            Box::new(|_| Err(JsEngineError::BindingFailed("nope".into()))),
        )
        .unwrap();
        assert_eq!(
            eval(&mut e, "rec(1, 'two', true, null)").unwrap(),
            JsValue::String("ret".into())
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                JsValue::Number(1.0),
                JsValue::String("two".into()),
                JsValue::Bool(true),
                JsValue::Null
            ]
        );
        assert_eq!(
            eval(&mut e, "try { fail() } catch (err) { err.message }").unwrap(),
            JsValue::String("nope".into())
        );
    }

    /// JS-1: 引数 65 個・オブジェクト引数は関数を呼ばずエラー。
    #[test]
    fn js_1_boa_rejects_bad_arguments_without_calling() {
        let mut e = BoaEngine::new();
        let calls = Arc::new(Mutex::new(0usize));
        let calls2 = Arc::clone(&calls);
        e.inject_global_function(
            "f",
            Box::new(move |_| {
                *calls2.lock().unwrap() += 1;
                Ok(JsValue::Undefined)
            }),
        )
        .unwrap();
        let many = vec!["0"; MAX_NATIVE_CALL_ARGS + 1].join(",");
        assert!(eval(&mut e, &format!("f({many})")).is_err());
        assert!(eval(&mut e, "f({})").is_err());
        assert_eq!(*calls.lock().unwrap(), 0);
        eval(&mut e, "f(1)").unwrap();
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    /// JS-1: ObjectHandle の戻り値は catch 可能な JS エラー。
    #[test]
    fn js_1_boa_object_handle_return_is_catchable_error() {
        let mut e = BoaEngine::new();
        e.inject_global_function(
            "h",
            Box::new(|_| Ok(JsValue::ObjectHandle(crate::ObjectHandle::from_raw(1)))),
        )
        .unwrap();
        assert_eq!(
            eval(&mut e, "try { h(); 'no' } catch (err) { 'caught' }").unwrap(),
            JsValue::String("caught".into())
        );
    }

    /// JS-1: 不正な名前・non-configurable な既存名・重複・件数上限は BindingFailed で、
    /// 失敗後もエンジンが使える。
    #[test]
    fn js_1_boa_binding_failures() {
        let mut e = BoaEngine::new();
        let noop = || -> NativeFn { Box::new(|_| Ok(JsValue::Undefined)) };
        assert!(matches!(
            e.inject_global_function("", noop()),
            Err(JsEngineError::BindingFailed(_))
        ));
        let long = "a".repeat(MAX_GLOBAL_FUNCTION_NAME_BYTES + 1);
        assert!(matches!(
            e.inject_global_function(&long, noop()),
            Err(JsEngineError::BindingFailed(_))
        ));
        assert!(matches!(
            e.inject_global_function("undefined", noop()),
            Err(JsEngineError::BindingFailed(_))
        ));
        e.inject_global_function("dup", noop()).unwrap();
        assert!(matches!(
            e.inject_global_function("dup", noop()),
            Err(JsEngineError::BindingFailed(_))
        ));
        assert_eq!(eval(&mut e, "1").unwrap(), JsValue::Number(1.0));
        for i in 0..(MAX_REGISTERED_GLOBAL_FUNCTIONS - 1) {
            e.inject_global_function(&format!("g{i}"), noop()).unwrap();
        }
        assert!(matches!(
            e.inject_global_function("overflow", noop()),
            Err(JsEngineError::BindingFailed(_))
        ));
        assert_eq!(eval(&mut e, "g0()").unwrap(), JsValue::Undefined);
    }

    /// JS-1: DOM 風オブジェクトのメソッド振り分け・重複メンバー・メンバー数超過。
    #[test]
    fn js_1_boa_dom_like_object() {
        let mut e = BoaEngine::new();
        e.bind_dom_like_object(
            "dom",
            vec![
                ("one".into(), Box::new(|_| Ok(JsValue::Number(1.0)))),
                ("two".into(), Box::new(|_| Ok(JsValue::Number(2.0)))),
            ],
        )
        .unwrap();
        assert_eq!(
            eval(&mut e, "dom.one() + dom.two()").unwrap(),
            JsValue::Number(3.0)
        );
        let noop = || -> NativeFn { Box::new(|_| Ok(JsValue::Undefined)) };
        assert!(matches!(
            e.bind_dom_like_object("d2", vec![("a".into(), noop()), ("a".into(), noop())]),
            Err(JsEngineError::BindingFailed(_))
        ));
        let too_many: Vec<(String, NativeFn)> = (0..=MAX_DOM_LIKE_OBJECT_MEMBERS)
            .map(|i| (format!("m{i}"), noop()))
            .collect();
        assert!(matches!(
            e.bind_dom_like_object("d3", too_many),
            Err(JsEngineError::BindingFailed(_))
        ));
        assert_eq!(eval(&mut e, "dom.one()").unwrap(), JsValue::Number(1.0));
    }

    /// JS-1: DOM 風オブジェクトもグローバル名の件数上限（メソッド 0 個でも 1 件）に
    /// 数えられ、関数と合算で `MAX_REGISTERED_GLOBAL_FUNCTIONS` を超えられない。
    #[test]
    fn js_1_boa_dom_like_objects_count_toward_global_name_cap() {
        let mut e = BoaEngine::new();
        let noop = || -> NativeFn { Box::new(|_| Ok(JsValue::Undefined)) };
        for i in 0..(MAX_REGISTERED_GLOBAL_FUNCTIONS - 1) {
            e.bind_dom_like_object(&format!("empty{i}"), Vec::new())
                .unwrap();
        }
        e.inject_global_function("last", noop()).unwrap();
        assert!(matches!(
            e.bind_dom_like_object("overflow", Vec::new()),
            Err(JsEngineError::BindingFailed(_))
        ));
        assert!(matches!(
            e.inject_global_function("overflow_fn", noop()),
            Err(JsEngineError::BindingFailed(_))
        ));
        assert_eq!(
            eval(&mut e, "typeof empty0").unwrap(),
            JsValue::String("object".into())
        );
    }

    /// JS-1: 生成直後に何も評価せず drop しても問題ない（遅延生成）。
    #[test]
    fn js_1_boa_lazy_context_creation() {
        let e = BoaEngine::new();
        assert!(e.context.is_none());
        drop(e);
    }
}

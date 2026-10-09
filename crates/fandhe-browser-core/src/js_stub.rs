//! js_stub: JS 実行呼び出しの境界（PoC-2 `core-proto/src/js_stub.rs` 相当。
//! TASK-24（24.9）・`CORE-1`・Issue #43 で導入し、TASK-30（30.3）・`JS-2`・
//! Issue #161 で `fandhe-browser-js` のエンジン生成関数呼び出しへ置換した）。
//!
//! `dom`（TASK-24.5・#39）・`query`（TASK-24.7・#41）や、将来の `fandhe-browser-cdp`
//! の `Runtime.evaluate` ハンドラなど、JS 実行が必要な箇所はこのモジュールの
//! [`execute_js_stub`] を境界として呼び出す。`docs/spec/04-behavior/js-engine.md`
//! の JS-2 は本関数名を名指ししているため、関数名・配置パスは変更しない。
//!
//! # 構成
//!
//! - [`JsRuntime`]: 設定（[`crate::config::JsConfig`]）が選んだ [`EngineKind`] を
//!   `fandhe_browser_js::create_engine` へ渡して得た `Box<dyn JsEngine>` を保持する。
//!   評価のたびに生成し直さない（子プロセス版 V8 は評価ごとに起動すると
//!   コンテキストも失うため）。エンジンなしビルド（`JsConfig::engine() == None`）
//!   では JS 無効として構築され、評価は必ずエラーになる（TASK-30（30.4）・
//!   Issue #162。`tests/js_stub_no_engine.rs` が crate 外から検証する）。
//! - [`execute_js_stub`]: [`JsRuntime`] へスクリプト評価を委譲する薄い境界。
//!
//! # 契約・注意
//!
//! - エンジンの選択はプロセス起動時に 1 回だけ（`js-engine.md` 決定 1）。
//!   別エンジンへの黙示的なフォールバックはしない（fail-closed）。
//! - [`JsRuntime`] は `!Send`（`JsEngine` が `Send` 境界を持たないため）。将来
//!   `AppState`・cdp へ載せる際は専用スレッドで保持する必要がある。
//! - 子プロセス版エンジンを使うホストバイナリは、`main` の先頭で
//!   core の再エクスポート `fandhe_browser_core::run_js_worker_if_requested`
//!   （実体は js crate の同名関数。#513）を呼ばなければならない。
//! - `create_engine(V8)` は子プロセスを起動しない（最初の `evaluate_script` で
//!   遅延起動。PERF-7・TASK-29.6）。その契約を守るため、本モジュールは自発的には
//!   `inject_global_function`・`bind_dom_like_object` を呼ばない（呼ぶと
//!   その時点でワーカーが起動する）。呼び出し元が
//!   [`JsRuntime::inject_global_function`] を明示的に呼んだときだけ注入する
//!   （`PLUG-10`・TASK-101.2.1・#553。WPT ランナーが結果受け取り用の関数を注入する）。
//!   DOM shim も [`JsRuntime::install_dom_shim`] を呼んだときだけ bind・注入する。
//!
//! # DOM shim の注入（`JS-5`・TASK-108・Issue #778）
//!
//! [`JsRuntime::install_dom_shim`] が呼び出し元の明示指示でのみ `__dom`（DOM ブリッジ）を
//! bind し、`js_shim` の JS ソースを評価する。ページごと・子プロセス再起動のたびに
//! 呼び直す契約は同メソッドの doc を参照。
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - ページ実行ランナーからの `install_dom_shim` の配線（ページごとの再注入・再起動検知）は
//!   TASK-109（#780・#784）の担当で未実装。
//!
//! # 可観測性（`REPAIR-9`・`TASK-10.2.2`・Issue #550）
//!
//! [`execute_js_stub_with_options`] に [`JsStubOptions::with_recorder`] で
//! recorder を渡すと、呼び出し 1 回につき [`OperationKind::JsStub`] を 1 件記録する。
//! 失敗時は [`crate::Error`] の種別のみを記録し、`script` の内容は含まれない。

use std::sync::Arc;
use std::time::Instant;

// 注入関数の型を core 経由で使えるよう再公開する（js crate へ直接依存してよいのは
// core のみという crate 間の許可依存を、harness 等の利用側に守らせるため）。
use fandhe_browser_js::{
    CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsValue, create_engine,
};
pub use fandhe_browser_js::{JsEngineError, NativeFn};

use crate::config::JsConfig;
use crate::dom_bridge::DomBridge;
use crate::js_shim::{self, JsShimInstallation};
use crate::observability::{OperationKind, OperationRecorder, RecorderHandle};

/// [`execute_js_stub_with_options`] の設定（`REPAIR-9`・`TASK-10.2.2`）。
///
/// `FetchOptions`・`ParseOptions` と同じ「options + `with_recorder`」方式で
/// recorder を注入する。既定は recorder なし（何も記録しない）。将来
/// タイムアウト等を非破壊に追加できるよう `#[non_exhaustive]` にしてある（REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct JsStubOptions {
    recorder: RecorderHandle,
}

impl JsStubOptions {
    /// 既定の options（recorder なし）を作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// 操作レコードの受け口を設定する。呼び出し 1 回につき `JsStub` を
    /// ちょうど 1 件記録する（`REPAIR-9`）。
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn OperationRecorder>) -> Self {
        self.recorder = RecorderHandle::new(recorder);
        self
    }
}

/// JS 実行結果を表す型（REPAIR-4: フラットな文字列だけにせず構造を持つ）。
///
/// 呼び出し元（cdp の `Runtime.evaluate` 等。将来）は、人間・ログ向けには
/// [`value`](Self::value)、型を区別したい場合は [`js_value`](Self::js_value) を使う。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct JsExecutionOutput {
    /// JS 実行結果の文字列表現（ECMAScript の ToString 相当。`ObjectHandle` は
    /// `"[object Object]"`。型は [`js_value`](Self::js_value) 側に残る）。
    pub value: String,
    /// エンジンが返した構造化された値。
    pub js_value: JsValue,
}

/// 評価先エンジンの状態。
enum RuntimeState {
    /// 設定が選んだエンジンを保持する。
    Enabled {
        kind: EngineKind,
        engine: Box<dyn JsEngine>,
        /// `__dom` を bind 済みのブリッジ（[`JsRuntime::install_dom_shim`]）。同じエンジンへ
        /// `__dom` を 2 回 bind すると失敗するため、再 install 時の判定に保持する。
        dom_bridge: Option<DomBridge>,
    },
    /// エンジンなしビルド、または明示的な JS 無効。評価は常に失敗する。
    Disabled,
}

/// 設定で選ばれた JS エンジンを保持し、スクリプト評価を委譲する実行環境
/// （`JS-2`・TASK-30（30.3）・Issue #161）。
///
/// 起動シーケンスが [`JsRuntime::from_config`] で 1 回だけ作り、以後
/// [`execute_js_stub`] へ渡す。`!Send`（モジュール doc 参照）。
pub struct JsRuntime {
    state: RuntimeState,
}

impl std::fmt::Debug for JsRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsRuntime")
            .field("engine_kind", &self.engine_kind())
            .finish()
    }
}

/// エンジンなし（JS 無効）で評価を要求された際の固定メッセージ。`script` は含めない。
const DISABLED_MESSAGE: &str =
    "no JS engine is compiled into this binary; JavaScript execution is disabled";

/// `create_engine` の失敗を fail-closed の `JsExecutionUnavailable` へ変換する。
fn engine_creation_error(e: CreateEngineError) -> crate::Error {
    crate::Error::JsExecutionUnavailable {
        message: e.to_string(),
    }
}

impl JsRuntime {
    /// 設定が選んだエンジン種別からランタイムを作る。
    ///
    /// - `engine() == None`: JS 無効のランタイムを返す（`Ok`）
    /// - `Some(kind)`: `create_engine(kind)` を呼ぶ。失敗（未同梱・未実装）は
    ///   別エンジンへフォールバックせず `JsExecutionUnavailable` で返す
    ///
    /// グローバル関数注入・DOM バインドは行わない（PERF-7。モジュール doc 参照）。
    pub fn from_config(js: &JsConfig) -> crate::Result<JsRuntime> {
        match js.engine() {
            None => Ok(JsRuntime::disabled()),
            Some(kind) => {
                let engine = create_engine(kind).map_err(engine_creation_error)?;
                Ok(JsRuntime {
                    state: RuntimeState::Enabled {
                        kind,
                        engine,
                        dom_bridge: None,
                    },
                })
            }
        }
    }

    /// JS 無効のランタイムを明示的に作る。評価は常に `JsExecutionUnavailable`
    /// で失敗する（成功を装わない。security.md）。
    pub fn disabled() -> JsRuntime {
        JsRuntime {
            state: RuntimeState::Disabled,
        }
    }

    /// 選択されているエンジン種別。JS 無効なら `None`。
    pub fn engine_kind(&self) -> Option<EngineKind> {
        match &self.state {
            RuntimeState::Enabled { kind, .. } => Some(*kind),
            RuntimeState::Disabled => None,
        }
    }

    /// スクリプトを `JsEngine::evaluate_script` へ渡して結果を変換する。
    ///
    /// エンジンの失敗は [`crate::Error::JsEvaluation`] として種別を保ったまま返す。
    /// エラーメッセージ（core が組み立てる分）に `script` は埋め込まない。
    pub fn execute(&mut self, script: &str) -> crate::Result<JsExecutionOutput> {
        match &mut self.state {
            RuntimeState::Disabled => Err(crate::Error::JsExecutionUnavailable {
                message: DISABLED_MESSAGE.to_string(),
            }),
            RuntimeState::Enabled { engine, .. } => {
                let js_value = engine
                    .evaluate_script(script, &EvaluateOptions::default())
                    .map_err(crate::Error::JsEvaluation)?;
                let value = js_value_to_display_string(&js_value)?;
                Ok(JsExecutionOutput { value, js_value })
            }
        }
    }

    /// JS のグローバル関数 `name` を注入する（`JS-2`・`PLUG-10`・TASK-101.2.1・#553）。
    ///
    /// 呼び出し元: WPT サブセットランナー（`harness/wpt_subset_runner`）が、結果受け取り
    /// 用のネイティブ関数を testharness.js の評価前に登録するために呼ぶ。
    ///
    /// - JS 無効のランタイムは成功を装わず `JsExecutionUnavailable` を返す
    /// - 失敗は [`crate::Error::JsEvaluation`] に包み、`BindingFailed` 等の種別を保つ
    /// - 子プロセス版エンジン（V8）では、この呼び出しの時点でワーカーが起動する
    ///   （PERF-7 の遅延起動は、本関数を呼ばない経路では維持される）
    /// - 子が再起動した場合、ホストが登録した関数は再登録されるがスクリプト上の状態
    ///   （グローバル変数・定義済み関数）は失われる（js crate の契約）
    /// - `func` に渡る引数は JS 側由来の untrusted な値。型・範囲を検証して使うこと
    pub fn inject_global_function(&mut self, name: &str, func: NativeFn) -> crate::Result<()> {
        match &mut self.state {
            RuntimeState::Disabled => Err(crate::Error::JsExecutionUnavailable {
                message: DISABLED_MESSAGE.to_string(),
            }),
            RuntimeState::Enabled { engine, .. } => engine
                .inject_global_function(name, func)
                .map_err(crate::Error::JsEvaluation),
        }
    }

    /// `document` / Node 系と `window` / `location` / `navigator` / `console` の JS shim を
    /// 注入する（`JS-5`・TASK-108・Issue #778・#779）。
    ///
    /// `location` の URL は `DomBridge::set_location` で設定する（shim は毎回読むため評価の
    /// 前後どちらでも反映される。未設定は `about:blank`）。
    ///
    /// 初回は `bridge` を `__dom` として bind してから `js_shim::install` で shim を評価する。
    /// 同じ `bridge` での 2 回目以降は bind を省き shim の再評価だけを行う（同一コンテキストでも
    /// SyntaxError にならない）。別の `bridge` を渡すと `Err`（`__dom` は 1 回しか bind できない）。
    ///
    /// 契約:
    /// - ページ（`DomBridge::attach`）のたびに呼び直す。shim は文書の ID を内部に持つ
    /// - 子プロセス版エンジンが再起動した後（`EngineUnavailable` の後）はスクリプト上の状態が
    ///   失われるため、呼び直す
    /// - 呼び出し元が明示的に呼んだときだけ bind する（PERF-7 の遅延起動と矛盾しない。V8 では
    ///   この呼び出しでワーカーが起動する）
    /// - JS 無効のランタイムは成功を装わず `JsExecutionUnavailable`
    pub fn install_dom_shim(&mut self, bridge: &DomBridge) -> crate::Result<JsShimInstallation> {
        match &mut self.state {
            RuntimeState::Disabled => Err(crate::Error::JsExecutionUnavailable {
                message: DISABLED_MESSAGE.to_string(),
            }),
            RuntimeState::Enabled {
                engine, dom_bridge, ..
            } => {
                match dom_bridge {
                    Some(bound) if !bound.same_instance(bridge) => {
                        return Err(crate::Error::Unsupported {
                            message: "a different DOM bridge is already bound to this JS runtime"
                                .to_string(),
                        });
                    }
                    Some(_) => {}
                    None => {
                        bridge
                            .register(engine.as_mut())
                            .map_err(crate::Error::JsEvaluation)?;
                        *dom_bridge = Some(bridge.clone());
                    }
                }
                js_shim::install(engine.as_mut(), &EvaluateOptions::default())
                    .map_err(crate::Error::JsEvaluation)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn from_engine_for_test(kind: EngineKind, engine: Box<dyn JsEngine>) -> Self {
        JsRuntime {
            state: RuntimeState::Enabled {
                kind,
                engine,
                dom_bridge: None,
            },
        }
    }
}

/// [`JsValue`] を ECMAScript の ToString 相当の文字列へ変換する。
///
/// 未知の将来 variant は偽の文字列で成功させず `Unsupported` を返す。
fn js_value_to_display_string(value: &JsValue) -> crate::Result<String> {
    Ok(match value {
        JsValue::Undefined => "undefined".to_string(),
        JsValue::Null => "null".to_string(),
        JsValue::Bool(b) => b.to_string(),
        JsValue::String(s) => s.clone(),
        JsValue::Number(n) => number_to_string(*n),
        JsValue::ObjectHandle(_) => "[object Object]".to_string(),
        _ => {
            return Err(crate::Error::Unsupported {
                message: "unsupported JS value type returned by engine".to_string(),
            });
        }
    })
}

/// Number の ToString 相当（NaN・±Infinity・-0・指数表記の閾値を JS に合わせる）。
fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".to_string();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if n == 0.0 {
        return "0".to_string();
    }
    if !(1e-6..1e21).contains(&n.abs()) {
        let s = format!("{n:e}");
        // JS は非負指数に `+` を付ける（1e21 -> "1e+21"）。
        return match s.split_once('e') {
            Some((m, exp)) if !exp.starts_with('-') => format!("{m}e+{exp}"),
            _ => s,
        };
    }
    format!("{n}")
}

/// JS 実行の境界（`js-engine.md` JS-2 が名指しする関数。TASK-30（30.3）・Issue #161）。
///
/// `runtime` が保持するエンジンへ `script` の評価を委譲する。エンジンなし・
/// 評価失敗はいずれも `Err`（成功を装わない）。cdp の `Runtime.evaluate` 等から
/// 呼ばれる想定。エラーメッセージには `script` の内容を埋め込まない。
pub fn execute_js_stub(runtime: &mut JsRuntime, script: &str) -> crate::Result<JsExecutionOutput> {
    execute_js_stub_with_options(runtime, script, &JsStubOptions::default())
}

/// [`execute_js_stub`] の計装付き版（`REPAIR-9`・`TASK-10.2.2`）。
///
/// `options` に recorder があれば `JsStub` を 1 件記録する（成功は `Success`、
/// 失敗は `Failure` に [`crate::Error`] の種別のみ）。
pub fn execute_js_stub_with_options(
    runtime: &mut JsRuntime,
    script: &str,
    options: &JsStubOptions,
) -> crate::Result<JsExecutionOutput> {
    let start = options.recorder.is_enabled().then(Instant::now);
    let result = runtime.execute(script);
    if let Some(start) = start {
        options
            .recorder
            .record_result(OperationKind::JsStub, &result, start.elapsed());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use fandhe_browser_js::{JsEngineError, NativeFn, ObjectHandle};
    use std::sync::Mutex;

    /// テスト用エンジン。受け取った script を記録し、決めた結果を返す。
    /// 注入・バインドの呼び出し回数も数える（PERF-7 の回帰防止）。
    struct FakeEngine {
        scripts: Arc<Mutex<Vec<String>>>,
        binds: Arc<Mutex<usize>>,
        result: Option<Result<JsValue, JsEngineError>>,
    }

    impl JsEngine for FakeEngine {
        fn evaluate_script(
            &mut self,
            script: &str,
            _options: &EvaluateOptions,
        ) -> Result<JsValue, JsEngineError> {
            self.scripts.lock().unwrap().push(script.to_string());
            self.result.take().expect("FakeEngine は 1 回だけ評価する")
        }

        fn inject_global_function(
            &mut self,
            _name: &str,
            _func: NativeFn,
        ) -> Result<(), JsEngineError> {
            *self.binds.lock().unwrap() += 1;
            Ok(())
        }

        fn bind_dom_like_object(
            &mut self,
            _name: &str,
            _methods: Vec<(String, NativeFn)>,
        ) -> Result<(), JsEngineError> {
            *self.binds.lock().unwrap() += 1;
            Ok(())
        }
    }

    type Recorded = (JsRuntime, Arc<Mutex<Vec<String>>>, Arc<Mutex<usize>>);

    fn fake_runtime(result: Result<JsValue, JsEngineError>) -> Recorded {
        let scripts = Arc::new(Mutex::new(Vec::new()));
        let binds = Arc::new(Mutex::new(0));
        let engine = FakeEngine {
            scripts: scripts.clone(),
            binds: binds.clone(),
            result: Some(result),
        };
        let rt = JsRuntime::from_engine_for_test(EngineKind::V8, Box::new(engine));
        (rt, scripts, binds)
    }

    /// JS-2（TASK-30.3・#161）: script がそのままトレイト経由でエンジンへ渡り、
    /// 結果が値・文字列表現の両方で返る。注入・バインドは呼ばれない。
    #[test]
    fn js_2_execute_js_stub_delegates_script_to_engine_trait() {
        let (mut rt, scripts, binds) = fake_runtime(Ok(JsValue::String("ab".to_string())));
        let out = execute_js_stub(&mut rt, "'a' + 'b'").expect("評価成功");
        assert_eq!(out.value, "ab");
        assert_eq!(out.js_value, JsValue::String("ab".to_string()));
        assert_eq!(*scripts.lock().unwrap(), vec!["'a' + 'b'".to_string()]);
        assert_eq!(*binds.lock().unwrap(), 0);
        assert_eq!(rt.engine_kind(), Some(EngineKind::V8));
    }

    /// JS-2（TASK-30.3・#161）: `JsEngineError` の各種別が区別されたまま
    /// `Error::JsEvaluation` になる。
    #[test]
    fn js_2_engine_errors_are_preserved_as_js_evaluation() {
        let cases = [
            JsEngineError::EvaluationFailed("e".to_string()),
            JsEngineError::BindingFailed("b".to_string()),
            JsEngineError::ResourceLimitExceeded("r".to_string()),
            JsEngineError::Timeout("t".to_string()),
            JsEngineError::EngineUnavailable("u".to_string()),
        ];
        for case in cases {
            let expected = case.to_string();
            let (mut rt, _, _) = fake_runtime(Err(case));
            let err = execute_js_stub(&mut rt, "x").expect_err("失敗する");
            assert_eq!(err.to_string(), format!("JS evaluation failed: {expected}"));
            assert!(matches!(err, Error::JsEvaluation(_)));
        }
    }

    /// JS-2（TASK-30.3・#161）: JS 無効ランタイムは固定メッセージで失敗し、
    /// 非 ASCII を含む script を反響しない。
    #[test]
    fn js_2_disabled_runtime_fails_without_echoing_script() {
        let mut rt = JsRuntime::disabled();
        assert_eq!(rt.engine_kind(), None);
        let script = "console.log('こんにちは')";
        let err = execute_js_stub(&mut rt, script).expect_err("常に失敗する");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
        assert_eq!(
            err.to_string(),
            "JS execution unavailable: no JS engine is compiled into this binary; \
             JavaScript execution is disabled"
        );
        assert!(!err.to_string().contains(script));
    }

    /// JS-2（TASK-30.3・#161）: JsValue から文字列表現への変換（ECMAScript ToString 相当）。
    #[test]
    fn js_2_value_to_display_string_table() {
        let cases: Vec<(JsValue, &str)> = vec![
            (JsValue::Undefined, "undefined"),
            (JsValue::Null, "null"),
            (JsValue::Bool(true), "true"),
            (JsValue::Bool(false), "false"),
            (JsValue::String("s".to_string()), "s"),
            (JsValue::Number(3.0), "3"),
            (JsValue::Number(-0.0), "0"),
            (JsValue::Number(f64::NAN), "NaN"),
            (JsValue::Number(f64::INFINITY), "Infinity"),
            (JsValue::Number(f64::NEG_INFINITY), "-Infinity"),
            (JsValue::Number(1e21), "1e+21"),
            (JsValue::Number(1.5e-7), "1.5e-7"),
            (JsValue::Number(1e20), "100000000000000000000"),
            (JsValue::Number(0.1 + 0.2), "0.30000000000000004"),
            (
                JsValue::ObjectHandle(ObjectHandle::from_raw(1)),
                "[object Object]",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(
                js_value_to_display_string(&value).expect("変換できる"),
                expected,
                "value = {value:?}"
            );
        }
    }

    /// JS-2（TASK-30.3・#161）: `create_engine` の失敗は fail-closed で
    /// `JsExecutionUnavailable` になる。
    #[test]
    fn js_2_create_engine_errors_map_to_js_execution_unavailable() {
        let e = engine_creation_error(CreateEngineError::NotYetImplemented {
            requested: EngineKind::Boa,
        });
        assert_eq!(
            e.to_string(),
            "JS execution unavailable: js engine \"boa\" is bundled but not yet implemented"
        );
        let e = engine_creation_error(CreateEngineError::NotBundled {
            requested: EngineKind::V8,
            bundled: &[],
        });
        assert_eq!(
            e.to_string(),
            "JS execution unavailable: js engine \"v8\" is not bundled into this binary \
             (bundled: none)"
        );
    }

    /// JS-2（TASK-30.3・#161）: エンジンなし構成では `from_config` が JS 無効になる。
    #[cfg(not(any(feature = "js-v8", feature = "js-boa")))]
    #[test]
    fn js_2_from_config_without_engine_is_disabled() {
        let rt = JsRuntime::from_config(&JsConfig::default()).expect("Ok");
        assert_eq!(rt.engine_kind(), None);
    }

    /// JS-2（TASK-30.3・#161）: `js-v8` 構成で V8 が選ばれる（評価はしない。
    /// 評価は `tests/js_stub_v8.rs`）。
    #[cfg(feature = "js-v8")]
    #[test]
    fn js_2_from_config_selects_v8() {
        let cfg = crate::Config::from_toml_str("[js]\nengine = \"v8\"\n").expect("設定");
        let rt = JsRuntime::from_config(cfg.js()).expect("Ok");
        assert_eq!(rt.engine_kind(), Some(EngineKind::V8));
    }

    /// JS-2（TASK-30.3・#161・TASK-32.2・#166）: `js-boa` 構成で boa が選ばれる
    /// （評価はしない。設定経由の評価確認は `tests/js_stub_boa.rs`。TASK-32.3・#167）。
    #[cfg(all(feature = "js-boa", not(target_os = "macos")))]
    #[test]
    fn js_2_from_config_selects_boa() {
        let cfg = crate::Config::from_toml_str("[js]\nengine = \"boa\"\n").expect("設定");
        let rt = JsRuntime::from_config(cfg.js()).expect("Ok");
        assert_eq!(rt.engine_kind(), Some(EngineKind::Boa));
    }

    /// JS-2・REPAIR-3（TASK-32.2・#166）: macOS では boa が無効（確保時に効く
    /// メモリ上限を強制できない）のため、`from_config` は別エンジンへ
    /// フォールバックせず `JsExecutionUnavailable` で失敗する。
    #[cfg(all(feature = "js-boa", target_os = "macos"))]
    #[test]
    fn js_2_from_config_boa_is_unavailable_on_macos() {
        let cfg = crate::Config::from_toml_str("[js]\nengine = \"boa\"\n").expect("設定");
        match JsRuntime::from_config(cfg.js()) {
            Err(err) => assert_eq!(
                err.to_string(),
                "JS execution unavailable: js engine \"boa\" is bundled but not yet implemented"
            ),
            Ok(_) => panic!("boa must be disabled on macOS"),
        }
    }

    /// JS-2・PLUG-10（TASK-101.2.1・#553）: 注入がエンジンへ委譲され、
    /// 評価（`execute`）は注入を呼ばない（PERF-7 の回帰防止）。
    #[test]
    fn plug_10_inject_global_function_delegates_to_engine() {
        let (mut rt, _, binds) = fake_runtime(Ok(JsValue::Undefined));
        rt.inject_global_function("hostFn", Box::new(|_| Ok(JsValue::Undefined)))
            .expect("注入成功");
        assert_eq!(*binds.lock().unwrap(), 1);
        execute_js_stub(&mut rt, "1").expect("評価成功");
        assert_eq!(*binds.lock().unwrap(), 1);
    }

    /// JS-2・PLUG-10（TASK-101.2.1・#553）: 注入失敗は種別を保ったまま
    /// `Error::JsEvaluation(BindingFailed)` になる。
    #[test]
    fn plug_10_inject_global_function_preserves_binding_failed() {
        struct FailingEngine;
        impl JsEngine for FailingEngine {
            fn evaluate_script(
                &mut self,
                _script: &str,
                _options: &EvaluateOptions,
            ) -> Result<JsValue, JsEngineError> {
                Ok(JsValue::Undefined)
            }
            fn inject_global_function(
                &mut self,
                _name: &str,
                _func: NativeFn,
            ) -> Result<(), JsEngineError> {
                Err(JsEngineError::BindingFailed("nope".to_string()))
            }
            fn bind_dom_like_object(
                &mut self,
                _name: &str,
                _methods: Vec<(String, NativeFn)>,
            ) -> Result<(), JsEngineError> {
                Ok(())
            }
        }
        let mut rt = JsRuntime::from_engine_for_test(EngineKind::V8, Box::new(FailingEngine));
        let err = rt
            .inject_global_function("f", Box::new(|_| Ok(JsValue::Undefined)))
            .expect_err("失敗する");
        assert!(matches!(
            err,
            Error::JsEvaluation(JsEngineError::BindingFailed(_))
        ));
        assert_eq!(
            err.to_string(),
            "JS evaluation failed: binding registration failed: nope"
        );
    }

    /// JS-2・PLUG-10（TASK-101.2.1・#553）: JS 無効のランタイムは注入に成功せず
    /// `JsExecutionUnavailable` を返す。
    #[test]
    fn plug_10_inject_global_function_on_disabled_runtime_fails() {
        let mut rt = JsRuntime::disabled();
        let err = rt
            .inject_global_function("f", Box::new(|_| Ok(JsValue::Undefined)))
            .expect_err("常に失敗する");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));
    }

    /// REPAIR-9: recorder 付きの失敗呼び出しは `JsStub` / `Failure` を 1 件記録する。
    #[test]
    fn repair_9_execute_js_stub_with_options_records_failure() {
        use crate::observability::{FailureKind, InMemoryRecorder, OperationOutcome};

        let rec = Arc::new(InMemoryRecorder::with_capacity(8));
        let options = JsStubOptions::new().with_recorder(rec.clone());
        let mut rt = JsRuntime::disabled();
        let err = execute_js_stub_with_options(&mut rt, "1 + 1", &options).expect_err("失敗");
        assert!(matches!(err, Error::JsExecutionUnavailable { .. }));

        let records = rec.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation(), OperationKind::JsStub);
        assert_eq!(
            records[0].outcome(),
            OperationOutcome::Failure {
                kind: FailureKind::JsExecutionUnavailable
            }
        );
    }

    /// REPAIR-9: 評価成功は `Success` として記録される。
    #[test]
    fn repair_9_execute_js_stub_with_options_records_success() {
        use crate::observability::{InMemoryRecorder, OperationOutcome};

        let rec = Arc::new(InMemoryRecorder::with_capacity(8));
        let options = JsStubOptions::new().with_recorder(rec.clone());
        let (mut rt, _, _) = fake_runtime(Ok(JsValue::Number(3.0)));
        execute_js_stub_with_options(&mut rt, "1 + 2", &options).expect("成功");
        let records = rec.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome(), OperationOutcome::Success);
    }
}

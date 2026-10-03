//! environment: testharness.js を読み込める最小のグローバル環境と、結果の受け渡し
//! アダプタ（`PLUG-10`・TASK-101.2.1・MS-8・Issue #553）。
//!
//! 後続の #554（TASK-101.2.2。WPT の取得・実行・合否集計）が、ファイルごとに作った
//! [`JsRuntime`] へ次の順で呼ぶ契約とする。
//!
//! 1. [`install_testharness_globals`]: 結果受け取り用ネイティブ関数を注入し、`self` を定義する
//! 2. testharness.js を評価する
//! 3. [`attach_result_reporter`]: testharness.js の `add_result_callback` /
//!    `add_completion_callback` へアダプタを登録する（testharnessreport.js 相当）
//! 4. テストファイルを評価する
//! 5. [`ResultCollector::take`] で結果を取り出す
//!
//! # 方針
//!
//! `document`・`window`・`location`・`setTimeout` は意図的に定義しない。testharness.js は
//! `document` が無ければ ShellTestEnvironment を選ぶため、偽の DOM やタイマーで動作を
//! 装わず、DOM を要するテストは testharness.js 自身の判定で失敗させる（security.md・REPAIR-3）。
//! 評価するスクリプトは本ファイルの固定定数だけで、外部文字列を JS 評価文字列へ連結しない。
//!
//! # 制限（簡易実装。実装済みを装わない。REPAIR-3）
//!
//! - DOM を要するテストは Shell 環境で失敗する（合否は testharness.js の判定のまま返す）
//! - `setTimeout`・イベントループが無いため `async_test`・`promise_test`・`step_timeout`
//!   系は未対応
//! - microtask の実行はエンジン依存（V8 実装は microtask checkpoint を保証せず、boa の
//!   `eval` は job を実行しない）。completion の通知時期・有無は保証せず、確実な経路は
//!   同期 `test()` の result 通知だけ
//! - 子プロセスの再起動（`EngineUnavailable`・`ResourceLimitExceeded`・context 破棄を伴う
//!   `Timeout`）では注入関数は再登録されるが、prelude・testharness.js・アダプタの状態は
//!   失われる。#554 はファイル単位の失敗として扱い、ファイルごとに新しい `JsRuntime` を作る
//! - 評価 1 回あたりの上限（スクリプト 1 MiB・実行 2 秒）は js crate の固定値に従う
//! - `JsValue` はスカラーのみのため、受け渡しは文字列・数値に限る

use fandhe_browser_core::js_stub::JsRuntime;

use crate::results::ResultCollector;

/// 結果通知用ネイティブ関数のグローバル名（サブテスト 1 件ごと）。
pub const REPORT_RESULT_FN: &str = "__fandheWptReportResult";
/// 完了通知用ネイティブ関数のグローバル名。
pub const REPORT_COMPLETION_FN: &str = "__fandheWptReportCompletion";

/// `self` だけを定義する前置きスクリプト。testharness.js は末尾が `})(self);` で、
/// `self` が無いと読み込み時に ReferenceError になる。完了値は `undefined`
/// （`JsValue` はオブジェクトを表現できず、オブジェクトが完了値になると `Err` になるため）。
const PRELUDE_SCRIPT: &str = "\
if (typeof self === 'undefined') {
    Object.defineProperty(globalThis, 'self', {
        value: globalThis,
        writable: true,
        configurable: true,
        enumerable: false
    });
}
undefined;
";

/// testharnessreport.js 相当のアダプタ。testharness.js 未読み込みなら例外にする
/// （fail-closed）。
const REPORTER_SCRIPT: &str = "\
if (typeof add_result_callback !== 'function' ||
    typeof add_completion_callback !== 'function') {
    throw new Error('testharness.js is not loaded');
}
add_result_callback(function (t) {
    __fandheWptReportResult(
        String(t.name), t.status, t.message == null ? null : String(t.message));
});
add_completion_callback(function (_tests, hs) {
    __fandheWptReportCompletion(
        hs.status, hs.message == null ? null : String(hs.message));
});
undefined;
";

/// 環境構築の失敗。どの段階で失敗したかを区別する（REPAIR-4）。
#[non_exhaustive]
#[derive(Debug)]
pub enum EnvironmentError {
    /// ネイティブ関数の注入に失敗した（JS 無効のランタイムを含む）。
    Inject(fandhe_browser_core::Error),
    /// 前置きスクリプトの評価に失敗した。
    Prelude(fandhe_browser_core::Error),
    /// アダプタの登録に失敗した（testharness.js 未読み込みを含む）。
    Reporter(fandhe_browser_core::Error),
}

impl std::fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inject(e) => write!(f, "failed to inject host functions: {e}"),
            Self::Prelude(e) => write!(f, "failed to evaluate prelude script: {e}"),
            Self::Reporter(e) => write!(f, "failed to attach result reporter: {e}"),
        }
    }
}

impl std::error::Error for EnvironmentError {}

/// 結果受け取り用ネイティブ関数を注入し、`self` を定義する。呼び出し順は module doc。
///
/// 返す [`ResultCollector`] は、テスト評価後に [`ResultCollector::take`] で結果を取り出す。
pub fn install_testharness_globals(
    runtime: &mut JsRuntime,
) -> Result<ResultCollector, EnvironmentError> {
    let collector = ResultCollector::new();
    runtime
        .inject_global_function(REPORT_RESULT_FN, collector.report_result_fn())
        .map_err(EnvironmentError::Inject)?;
    runtime
        .inject_global_function(REPORT_COMPLETION_FN, collector.report_completion_fn())
        .map_err(EnvironmentError::Inject)?;
    runtime
        .execute(PRELUDE_SCRIPT)
        .map_err(EnvironmentError::Prelude)?;
    Ok(collector)
}

/// testharness.js の評価後・テスト本体の評価前に呼び、結果通知のアダプタを登録する。
pub fn attach_result_reporter(runtime: &mut JsRuntime) -> Result<(), EnvironmentError> {
    runtime
        .execute(REPORTER_SCRIPT)
        .map_err(EnvironmentError::Reporter)?;
    Ok(())
}

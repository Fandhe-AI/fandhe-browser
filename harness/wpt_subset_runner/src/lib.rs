//! wpt_subset_runner: WPT サブセット実行ハーネスの土台（`PLUG-10`・TASK-101.2.1・
//! MS-8・Issue #553）。
//!
//! 本 crate は `fandhe-browser-core` の [`JsRuntime`](fandhe_browser_core::js_stub::JsRuntime)
//! だけを使い（js crate へは直接依存しない）、testharness.js が読み込める最小の
//! グローバル環境（[`environment`]）と、各サブテストの結果を Rust 側で受け取る経路
//! （[`results`]）を提供する。WPT の取得・リビジョン固定・実行・合否集計は後続の
//! #554（TASK-101.2.2）が担う。
//!
//! 実行できない reftest・other の理由と確度は [`report`]（TASK-101.5・#277）が記録する。
//!
//! 呼び出し順の契約は [`environment`] の module doc を参照。

pub mod environment;
pub mod report;
pub mod results;

pub use environment::{
    EnvironmentError, REPORT_COMPLETION_FN, REPORT_RESULT_FN, attach_result_reporter,
    install_testharness_globals,
};
pub use report::{Basis, ReportError, UnrunnableReason, UnrunnableReport, classify_unrunnable};
pub use results::{
    CollectError, CollectedResults, HarnessCompletion, HarnessStatus, ResultCollector,
    SubtestResult, SubtestStatus,
};

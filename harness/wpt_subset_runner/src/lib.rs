//! wpt_subset_runner: WPT サブセット実行ハーネス（`PLUG-10`・TASK-101.2.1・
//! TASK-101.2.2・MS-8・Issue #553・#554）。
//!
//! 本 crate は `fandhe-browser-core` の [`JsRuntime`](fandhe_browser_core::js_stub::JsRuntime)
//! だけを使い（js crate へは直接依存しない）、testharness.js が読み込める最小の
//! グローバル環境（[`environment`]）と、各サブテストの結果を Rust 側で受け取る経路
//! （[`results`]）を提供する。[`runner`] は固定リビジョンの WPT から `subset.tsv` を読み、
//! ファイル単位で実行して合否を分類する（取得は `fetch-wpt.sh`）。合格率の集計・
//! レポートは #276 が担う。プロファイル別の実行指定（TASK-101.3・#275）は [`runner`] が
//! 配線を提供する（挙動差は TASK-100・`PLUG-8` 完了後）。
//!
//! 実行できない reftest・other の理由と確度は [`report`]（TASK-101.5・#277）が記録する。
//!
//! 呼び出し順の契約は [`environment`] の module doc を参照。

pub mod environment;
pub mod report;
pub mod results;
pub mod runner;

pub use environment::{
    EnvironmentError, REPORT_COMPLETION_FN, REPORT_RESULT_FN, attach_result_reporter,
    install_testharness_globals,
};
pub use report::{Basis, ReportError, UnrunnableReason, UnrunnableReport, classify_unrunnable};
pub use results::{
    CollectError, CollectedResults, HarnessCompletion, HarnessStatus, ResultCollector,
    SubtestResult, SubtestStatus,
};
pub use runner::{
    FileOutcome, HarnessKind, LimitKind, ProfileParseError, ProfileRun, ProfileSelectionError,
    RunLimits, RunOptions, SubsetEntry, SubsetError, Verdict, WptProfile, parse_subset_tsv,
    run_entry, run_subset, run_subset_for_profiles,
};

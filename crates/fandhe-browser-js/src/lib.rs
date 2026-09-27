//! JS エンジン抽象を担う crate（REPAIR-1・TASK-1（1.3）・MS-1）。
//!
//! `fandhe-browser-core` の DOM 実装から呼ばれ、JS 実行系（既定 V8 /
//! `rusty_v8`、切替先 boa / `boa_engine`）をトレイト越しに抽象化する
//! ことを目指す crate。上位 crate（core・cdp・ai 等）へ V8 / boa の
//! 具象型を漏らさない契約とする（coding-rust.md「JS エンジンはトレイト
//! 抽象越しに使い、V8 / boa の具象型を上位 crate へ漏らさない」）。
//!
//! [`engine_trait`] は TASK-28（28.2・Issue #148）でエンジン種別の列挙型
//! （[`EngineKind`]）と同梱一覧関数（[`bundled_engines`]）を、TASK-28（28.3・
//! Issue #149）でエンジン抽象トレイト本体（[`JsEngine`]）と種別からトレイト
//! オブジェクトを生成する関数（[`create_engine`]）を追加した。両エンジン
//! 共通のコンフォーマンステスト（TASK-28.4・Issue #150）は本 crate の
//! `tests/conformance.rs`（結合テスト）にある。
//!
//! V8（`rusty_v8`）・boa（`boa_engine`）それぞれの実装切替（JS-3）は、
//! 対応する依存追加（TASK-29・TASK-32）を経て別途行う。
//!
//! # スタブについて
//!
//! [`create_engine`] は同梱済みの種別には `NotYetImplemented`、
//! 同梱されていない種別には `NotBundled`（詳細は [`engine_trait`] を参照）
//! を返す。以下は未実装（実装済みを装わない。REPAIR-3）。
//!
//! - V8 の具象実装（`JS-1`、`TASK-29`、`MS-3`）: Platform/Isolate 初期化
//!   （`29.2`）のみ実装済み。`JsEngine` 実装・`create_engine` への配線は
//!   `29.3`〜`29.6`
//! - boa の具象実装（`JS-1`、`TASK-32`、`MS-3`）
//! - core への統合（`js_stub` の置換。`JS-2`、`TASK-30`、`MS-3`）

pub mod engine_trait;
// V8（`rusty_v8`）の Platform/Isolate 初期化（TASK-29.2）を担う非公開
// モジュール。具象型を上位 crate へ漏らさないため `pub` を付けず、
// `pub use` もしない（AC-2・coding-rust.md「JS エンジンはトレイト抽象
// 越しに使い、V8 / boa の具象型を上位 crate へ漏らさない」）。
#[cfg(feature = "js-v8")]
mod v8_engine;
// JS 評価用の子プロセスと stdio でやり取りするバイナリプロトコルの
// フレーミング・コーデック（TASK-29・Issue #503「JS プロセス分離」
// 設計書 §3.5・§7 W2）。`v8` crate に依存しない（`js-v8` feature の
// 有無に関わらずコンパイル・テストできる）。子プロセス側の入口・親側の
// プロキシ（W3・W4 で追加）が消費するまでは lib 本体から未使用のため、
// 非テストビルドでは `dead_code` の期待を宣言する（REPAIR-3）。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "W3（worker.rs）・W4（process_engine.rs）から使われるまで lib 本体からは未使用（REPAIR-3）"
    )
)]
mod worker_protocol;

pub use engine_trait::{
    CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue, NativeFn,
    bundled_engines, create_engine,
};

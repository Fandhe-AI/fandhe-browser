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
//! オブジェクトを生成する関数（[`create_engine`]）を追加した。同モジュール
//! には今後、両エンジン共通のコンフォーマンステスト（TASK-28.4・Issue #150）
//! が追加される予定である（実装済みを装わない。REPAIR-3）。
//!
//! V8（`rusty_v8`）・boa（`boa_engine`）それぞれの実装切替（JS-3）は、
//! 対応する依存追加（TASK-29・TASK-32）を経て別途行う。

pub mod engine_trait;

pub use engine_trait::{
    CreateEngineError, EngineKind, EvaluateOptions, JsEngine, JsEngineError, JsValue, NativeFn,
    bundled_engines, create_engine,
};

//! JS エンジン抽象を担う crate（REPAIR-1・TASK-1（1.3）・MS-1）。
//!
//! `fandhe-browser-core` の DOM 実装から呼ばれ、JS 実行系（既定 V8 /
//! `rusty_v8`、切替先 boa / `boa_engine`）をトレイト越しに抽象化する
//! ことを目指す crate。上位 crate（core・cdp・ai 等）へ V8 / boa の
//! 具象型を漏らさない契約とする（coding-rust.md「JS エンジンはトレイト
//! 抽象越しに使い、V8 / boa の具象型を上位 crate へ漏らさない」）。
//!
//! 現時点は workspace 分割（TASK-1）の雛形のみであり、公開 API・
//! エンジン抽象トレイト・V8 / boa の実装はまだ存在しない
//! （実装済みを装わない。REPAIR-3）。将来的には次を提供する想定:
//!
//! - JS 実行エンジンを表すトレイト抽象（`JsEngine` 等。JS-1）
//! - V8（`rusty_v8`）・boa（`boa_engine`）それぞれの実装切替（JS-3）
//!
//! これらは TASK-28 以降で追加する。

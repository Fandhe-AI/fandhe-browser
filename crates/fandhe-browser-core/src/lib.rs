//! `fandhe-browser-core`: fandhe-browser のコア crate。
//!
//! fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・
//! config（設定）・可観測性（ログ・トレーシング）を担う crate。将来的には
//! `fandhe-browser-js`（workspace 内 crate に依存しない下位 crate）に依存する
//! 想定だが、本 PR（TASK-24（24.1）・ビヘイビア `CORE-1`）時点でも
//! `Cargo.toml` の `[dependencies]` は空であり、まだ依存を追加していない
//! （REPAIR-3: 実装済みを装わない。依存追加は該当タスクで
//! dependency-policy.md のユーザー承認制に従って行う）。`fandhe-browser-ai`・
//! `fandhe-browser-cdp` 等の上位 crate からは一方向に依存される
//! （AGENTS.md「crate 間の許可依存」・coding-rust.md「循環依存を作らない」）。
//!
//! REPAIR-1（TASK-24（24.1）・MS-1）: 本 PR で追加したのは `fetch`/`parse`/
//! `dom`/`query` の各モジュール構成と、それらが共通で使うエラー型
//! （[`error::Error`] / [`error::Result`]）のみである。各モジュール本体は
//! 空 skeleton のままで機能を一切実装していない（REPAIR-3: 実装済みを装わない）。
//! 本実装は以降の後続 Issue で行う: `fetch` は TASK-24.2（#36）、`parse` は
//! TASK-24.4（#38）、`dom` は TASK-24.5（#39）、`query` は TASK-24.7（#41）。
//! JS 実行スタブとの境界は本 PR（TASK-24（24.9）・Issue #43）で `js_stub`
//! モジュールとして追加した。関数本体（[`js_stub::execute_js_stub`]）は
//! 常にエラーを返すスタブであり、TASK-30（Issue #143・ビヘイビア `JS-2`）で
//! `fandhe-browser-js` の実装へ置換される。

pub mod dom;
pub mod error;
pub mod fetch;
pub mod js_stub;
pub mod parse;
pub mod query;

pub use error::{Error, Result};

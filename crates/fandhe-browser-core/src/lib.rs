//! `fandhe-browser-core`: fandhe-browser のコア crate。
//!
//! fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・
//! config（設定）・可観測性（ログ・トレーシング）・描画機能への境界（[`render`]）を
//! 担う crate。将来的には `fandhe-browser-js`（workspace 内 crate に依存しない
//! 下位 crate）に依存する想定だが、本ファイル時点では `Cargo.toml` の
//! `[dependencies]` は空であり、まだ依存を追加していない（REPAIR-3: 実装済みを
//! 装わない。依存追加は該当タスクで dependency-policy.md のユーザー承認制に
//! 従って行う）。`fandhe-browser-ai`・`fandhe-browser-cdp` 等の上位 crate からは
//! 一方向に依存される（AGENTS.md「crate 間の許可依存」・coding-rust.md
//! 「循環依存を作らない」）。
//!
//! REPAIR-1（TASK-1（旧サブ番号 1.2）・MS-1）: workspace 分割の一環として追加した
//! 空 skeleton から出発している。REPAIR-1（TASK-24（24.1）・MS-1）: `fetch`/
//! `parse`/`dom`/`query` の各モジュール構成と、それらが共通で使うエラー型
//! （[`error::Error`] / [`error::Result`]）を追加したが、各モジュール本体は
//! 空 skeleton のままで機能を一切実装していない（REPAIR-3: 実装済みを装わない）。
//! 本実装は以降の後続 Issue で行う: `fetch` は TASK-24.2（#36）、`parse` は
//! TASK-24.4（#38）、`dom` は TASK-24.5（#39）、`query` は TASK-24.7（#41）。
//! JS 実行スタブとの境界は TASK-24.9（#43）で追加する。[`render`] モジュールは
//! TASK-33（サブタスク 33.2・ビヘイビア `RENDER-1`）で追加した描画トレイトの
//! 定義のみを含み、既定実装や `fandhe-browser-render`（Servo）側の本実装は
//! 含まない（別 issue の担当。render モジュールの doc コメントを参照）。

pub mod dom;
pub mod error;
pub mod fetch;
pub mod parse;
pub mod query;
pub mod render;

pub use error::{Error, Result};

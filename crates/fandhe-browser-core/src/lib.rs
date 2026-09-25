//! `fandhe-browser-core`: fandhe-browser のコア crate。
//!
//! fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・
//! config（設定）・可観測性（ログ・トレーシング）を担う crate。将来的には
//! `fandhe-browser-js`（workspace 内 crate に依存しない下位 crate）に依存する
//! 想定だが、本 PR（TASK-1（旧サブ番号 1.2）の空 skeleton 段階）時点では
//! `Cargo.toml` の `[dependencies]` は空であり、まだ依存を追加していない
//! （REPAIR-3: 実装済みを装わない。依存追加は該当タスクで
//! dependency-policy.md のユーザー承認制に従って行う）。`fandhe-browser-ai`・
//! `fandhe-browser-cdp` 等の上位 crate からは一方向に依存される
//! （AGENTS.md「crate 間の許可依存」・coding-rust.md「循環依存を作らない」）。
//!
//! REPAIR-1（TASK-1（旧サブ番号 1.2）・MS-1）: 本ファイルは workspace 分割の
//! 一環として追加した空 skeleton であり、現時点では機能を一切実装していない
//! （REPAIR-3: 実装済みを装わない）。fetch・HTML パース・DOM・query の本実装は
//! 別タスク TASK-24（ビヘイビア `CORE-1`）で行う。

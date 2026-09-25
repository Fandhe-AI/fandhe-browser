//! `fandhe-browser-core`: fandhe-browser のコア crate。
//!
//! fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・
//! config（設定）・可観測性（ログ・トレーシング）を担う下位 crate として、
//! `fandhe-browser-js`・`fandhe-browser-ai`・`fandhe-browser-cdp` 等の上位 crate
//! から一方向に依存される（coding-rust.md「循環依存を作らない」）。
//!
//! REPAIR-1（TASK-1（旧サブ番号 1.2）・MS-1）: 本ファイルは workspace 分割の
//! 一環として追加した空 skeleton であり、現時点では機能を一切実装していない
//! （REPAIR-3: 実装済みを装わない）。fetch・HTML パース・DOM・query の本実装は
//! 別タスク TASK-24（ビヘイビア `CORE-1`）で行う。

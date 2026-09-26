//! `core::dom::Document` から、方式 B（役割ベースアクセシビリティツリー）の
//! 簡約表現 `Snapshot`/`Node` を構築するモジュール（`AISNAP-1`・`TASK-11`・`MS-2`）。
//!
//! 呼び出し文脈: 将来 `fandhe-browser-cli` 層の配線を経由して、
//! `fandhe-browser-cdp` の `/ai/snapshot` ルータから利用される想定
//! （`TASK-19`・`AISNAP-6`/`AISNAP-7`）。本 crate（`ai`）は `cdp` に
//! 直接依存しない（crate 間の許可依存。lib.rs のドキュメンテーションコメント
//! を参照）。
//!
//! # スタブについて
//!
//! 本ファイルは TASK-11.1（`AISNAP-1`・`MS-2`）が追加する空スケルトンであり、
//! 公開アイテムはまだ置かない（実装済みを装わない。REPAIR-3）。段階的に
//! 以下の Issue で実装する：
//!
//! - 公開型 `Snapshot`/`Node`: TASK-11.2（Issue #71）
//! - role（役割）算出: TASK-11.3（Issue #72）
//! - accessible name（アクセシブルネーム）算出: TASK-11.4（Issue #73）
//! - state（状態）算出: TASK-11.5（Issue #74）
//! - ref（role + name シグネチャによる再特定要求。`AISNAP-10`）: TASK-11.6（Issue #75）
//! - DOM から `Snapshot` へのツリー構築統合: TASK-11.7（Issue #76）
//! - ユニットテスト一式: TASK-11.8（Issue #77）

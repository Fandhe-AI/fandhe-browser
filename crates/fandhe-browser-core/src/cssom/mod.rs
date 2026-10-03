//! CSSOM 最小構築の入口（TASK-105・MS-8・ビヘイビア `CORE-5`。`PLUG-8` の前提条件）。
//!
//! TASK-100 の CSSOM プロファイル feature gating が将来呼ぶ computed style API の土台として、
//! スタイルシート・ルール・宣言・詳細度の型を core に置く。レイアウトは含まない。
//! TASK-33 の [`crate::render`] 境界とは重ならず、新規依存も足さない（#263 の決定:
//! `cssparser` / `selectors` は MPL-2.0 のため core では使わず、セレクタは
//! [`crate::selector`] を共有・拡張する）。
//!
//! 現状は型定義のみ（TASK-105.1・#255）。以下は予定している兄弟モジュールで、
//! まだ存在しない（REPAIR-3: 実装済みを装わない）。
//!
//! - `declaration`: 宣言パーサー（#256）
//! - `selector`: セレクタ拡張と詳細度の計算（#257）
//! - ルール分割（#551）・`stylesheet` 構築（#552）
//! - マッチング（#259）・`computed`: カスケードと computed style API（#260）
//! - 上限検証（#261）・結合テスト（#262）

mod types;

pub use types::{Declaration, Importance, Specificity, StyleRule, Stylesheet};

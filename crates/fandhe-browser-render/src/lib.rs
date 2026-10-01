//! レンダリング層（Servo 組込）を隔離する専用 crate。
//!
//! MPL-2.0 である Servo を本 crate 内に閉じ込め、既定ビルドの依存グラフへ
//! 混入させないためのライセンス境界を crate 分割として先に確保する
//! （[licensing](../../../.claude/rules/licensing.md)・RENDER-1）。
//! 対応: TASK-1（1.4）・REPAIR-1・MS-1。
//!
//! 現状はディレクトリ・manifest のみの雛形であり、本 crate に公開 API は
//! まだ存在せず、「実装済みを装う」スタブ関数も置かない（REPAIR-3）。
//!
//! # スタブについて
//!
//! - （済み）feature `rendering` の Cargo 定義（workspace 登録 #45・cli 側の結線
//!   TASK-33.4・#459。RENDER-1・MS-1）
//! - core の描画トレイト（`Renderer`。`fandhe-browser-core::render`）の実装と、
//!   cli による `AppState` への注入（RENDER-1・TASK-38・MS-4）
//! - Servo の組込・スクリーンショット取得等の本実装（RENDER-1・TASK-38・
//!   MS-4）

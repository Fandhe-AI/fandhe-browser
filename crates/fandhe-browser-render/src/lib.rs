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
//! - feature `rendering` の Cargo 定義・core の描画トレイト（`Renderer`。
//!   `fandhe-browser-core::render`）との結線（RENDER-1・TASK-33・MS-1）
//! - Servo の組込・スクリーンショット取得等の本実装（RENDER-1・TASK-38・
//!   MS-4）

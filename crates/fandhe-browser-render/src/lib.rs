//! レンダリング層（Servo 組込）を隔離する専用 crate。
//!
//! MPL-2.0 である Servo を本 crate 内に閉じ込め、既定ビルドの依存グラフへ
//! 混入させないためのライセンス境界を crate 分割として先に確保する
//! （[licensing](../../../.claude/rules/licensing.md)・RENDER-1）。
//! 対応: TASK-1（1.4）・REPAIR-1・MS-1。
//!
//! 現状はディレクトリ・manifest のみの雛形であり、feature gate `rendering`
//! の追加・core 側の描画トレイトとの結線・Servo 依存の導入・実装は
//! いずれも別タスク（TASK-33、ビヘイビア RENDER-1）で行う。本 crate に
//! 公開 API はまだ存在せず、「実装済みを装う」スタブ関数も置かない
//! （REPAIR-3）。

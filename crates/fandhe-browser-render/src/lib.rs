//! レンダリング層（Servo 組込）を隔離する専用 crate。
//!
//! MPL-2.0 である Servo を本 crate 内に閉じ込め、既定ビルドの依存グラフへ
//! 混入させないためのライセンス境界を crate 分割として先に確保する
//! （[licensing](../../../.claude/rules/licensing.md)・RENDER-1）。
//! 対応: TASK-1（1.4）・REPAIR-1・MS-1。
//!
//! 本 crate は Servo に依存しない IPC クライアント（設計 §3.1）で、公開 API は
//! プロトコル型・コーデックのみ。「実装済みを装う」スタブ関数は置かない（REPAIR-3）。
//!
//! 本体とレンダリングホストの stdio プロトコル契約の正本は
//! `docs/design/render-host-protocol.schema.json` と `tests/fixtures/protocol/`
//! （契約テスト `tests/protocol_fixtures.rs`。PLUG-1・SEC-2・REPAIR-4・TASK-38）。
//! クライアント側のプロトコル型は [`protocol`]、フレームコーデックは [`codec`]
//! （済み。RENDER-3・TASK-38・#989）。
//!
//! # スタブについて
//!
//! - （済み）feature `rendering` の Cargo 定義（workspace 登録 #45・cli 側の結線
//!   TASK-33.4・#459。RENDER-1・MS-1）
//! - core の描画トレイト（`Renderer`。`fandhe-browser-core::render`）の実装と、
//!   cli による `AppState` への注入（RENDER-1・TASK-38・MS-4）
//! - Servo の組込・スクリーンショット取得等の本実装（RENDER-1・TASK-38・
//!   MS-4）
//! - 子プロセス（Servo ホスト）の起動・タイムアウト・kill・再起動、`hello` 申告値の
//!   実運用への反映（RENDER-3・TASK-38・K2）
//! - `ProtocolError` の core `RenderError` への写像、`data_base64` のデコード、
//!   プライベート IP / loopback ポリシーとの統合（HostRenderer #235。RENDER-3）

pub mod codec;
pub mod protocol;

//! `fandhe-browser-core`: fandhe-browser のコア crate。
//!
//! fetch（ネットワーク取得）・HTML パース・DOM・query（DOM 探索）・CSSOM・
//! config（設定）・可観測性（ログ・トレーシング）・描画機能への境界（[`render`]）を
//! 担う crate。将来的には `fandhe-browser-js`（workspace 内 crate に依存しない
//! 下位 crate）に依存する想定。`Cargo.toml` の `[dependencies]` には
//! `html5ever = "=0.40.1"`（Issue #35 承認済み・TASK-24.4・#38）と reqwest・
//! rustls（同じく Issue #35 承認済み・TASK-24.2・#36）を持つ。他の依存追加は
//! 該当タスクで dependency-policy.md のユーザー承認制に従って行う。
//! `fandhe-browser-ai`・`fandhe-browser-cdp` 等の
//! 上位 crate からは一方向に依存される（AGENTS.md「crate 間の許可依存」・
//! coding-rust.md「循環依存を作らない」）。
//!
//! REPAIR-1（TASK-1（旧サブ番号 1.2）・MS-1）: workspace 分割の一環として追加した
//! 空 skeleton から出発している。REPAIR-1（TASK-24（24.1）・MS-1）: `fetch`/
//! `parse`/`dom`/`query` の各モジュール構成と、それらが共通で使うエラー型
//! （[`error::Error`] / [`error::Result`]）を追加した。`fetch`（TASK-24.2・#36）は
//! reqwest・rustls を使う本実装を持つ。`parse`（TASK-24.4・#38）は html5ever の
//! `TreeSink` を自作実装し、HTML 文字列・バイト列から `dom::Document`（arena）を
//! 構築する本実装を持つ。`dom`（TASK-24.5・#39）は arena の型定義
//! （[`dom::Document`]・[`dom::Node`] 等）に加え、走査 API（親子・兄弟・祖先・
//! 子孫を辿るイテレータ）・要素/属性アクセサ・`text_content` の本実装を持つ
//! （セレクタ照合はスコープ外。REPAIR-3: 実装済みを装わない）。
//! `selector`（TASK-24.7・#41）は CSS セレクタの
//! サブセットをパースする本実装を持つ。`query`（TASK-24.10・#418）は
//! `selector` の AST を `dom::Document` に照合し、`querySelector`/
//! `querySelectorAll`/`Element.matches()` 相当の API を提供する本実装を持つ。
//! JS 実行スタブとの境界は TASK-24（24.9・Issue #43）で `js_stub` モジュールとして
//! 追加した。関数本体（[`js_stub::execute_js_stub`]）は常にエラーを返すスタブであり、
//! TASK-30（Issue #143・ビヘイビア `JS-2`）で `fandhe-browser-js` の実装へ置換される。
//! [`render`] モジュールは TASK-33（サブタスク 33.2・ビヘイビア `RENDER-1`）で追加した
//! 描画トレイトの定義に加え、feature `rendering` 無効時に用いる既定実装
//! `DisabledRenderer`（TASK-33（33.3）・issue #47）を含む。`fandhe-browser-render`
//! （Servo）側の本実装・`AppState` への配線は含まない（別 issue の担当。render
//! モジュールの doc コメントを参照）。
//!
//! # スタブについて
//!
//! 未実装・簡易実装の詳細は各モジュールの `//!` を参照（`REPAIR-3`: 実装済みを
//! 装わない）。crate 直下では対象モジュールの名前のみを挙げる。
//!
//! - [`js_stub`]（`JS-2`・`TASK-30`・`MS-3`）
//! - [`render::DisabledRenderer`]（`RENDER-1`・`TASK-33`/`TASK-38`・`MS-1`/`MS-4`）

pub mod dom;
pub mod error;
pub mod fetch;
pub mod js_stub;
pub mod parse;
pub mod query;
pub mod render;
pub mod selector;

pub use dom::{
    Ancestors, Attribute, Children, Descendants, Document, Node, NodeData, NodeId, QuirksMode,
};
pub use error::{Error, ParseError, Result};
pub use fetch::{FetchOptions, FetchResponse, Fetcher};
pub use parse::{
    ParseDiagnostics, ParseErrorPolicy, ParseOptions, ParsedDocument, parse_document,
    parse_document_bytes,
};
pub use query::{
    element_matches, query_selector, query_selector_all, query_selector_all_str, query_selector_str,
};

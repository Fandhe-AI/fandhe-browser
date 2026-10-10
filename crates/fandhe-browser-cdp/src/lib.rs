//! CDP（Chrome DevTools Protocol）互換サーバー crate。
//!
//! `fandhe-browser-cli` から起動され、Playwright / Puppeteer 等の CDP クライアントに
//! 対して DOM 操作・ページ制御 API を公開することを目指す（README「実装方針（要点）」の
//! CDP 互換方針を参照）。`fandhe-browser-core` の `AppState` に依存し、cdp 固有の
//! 状態（ターゲット表・セッション表）は [`server::CdpState`] が内包する。
//!
//! # スタブについて
//!
//! 状態型（CDP セッション・ターゲット表。`CDP-1`、TASK-41.2）と `/json/*`
//! ルータ（`server::router`。`CDP-1`、TASK-41.3）と `/devtools/browser/{id}` の WS 受け口
//! （`server::browser_websocket_config`。`CDP-1`、TASK-41.4。ハンドラは TASK-42.1 でディスパッチャへ委譲）は
//! [`server::endpoints`] が一体で公開する。さらに cli での組み立て・起動
//! （`fandhe-browser-cli` の `server` モジュール。TASK-41.5、`MS-3`）も実装済み。
//! 以下は未実装で、実装済みを装う公開 API・ダミー実装は置かない
//! （`code-comment-style.md`・REPAIR-3）。
//!
//! - JSON-RPC ディスパッチャ（`protocol.rs`。TASK-42.1）は実装済みで、組込みメソッドは `Page.navigate`（TASK-42.2）・`DOM.getDocument`（TASK-42.4。`sessionId` 付きはターゲット別の確定文書を返す。#658）・`DOM.querySelector`（TASK-42.5。#243）・`DOM.requestChildNodes`（#657）・`Browser.getVersion`（TASK-43.3a。`CDP-2`）
//! - 個別 CDP メソッドのハンドラ（`CDP-1`/`CDP-5`/`CDP-7`、`TASK-42.2`〜`42.5`、`MS-4`）
//!
//! 未実装メソッドの応答方針（`-32601` を返し成功を捏造しない）と受信ログは TASK-42.6（`CDP-6`・`SEC-2`）で
//! 確定済み（`protocol.rs` のモジュールドキュメント・[`CdpState::received_methods`]）。

mod discovery;
mod dom;
mod method_log;
mod navigation;
mod page;
mod playwright_compat;
mod protocol;
pub mod server;
pub mod target;
mod ws;

pub use method_log::{MethodCounts, ReceivedMethodsSnapshot};
pub use server::{CdpEndpoints, CdpState, WsConfigError, endpoints};
pub use target::{
    BrowserId, CdpStateError, MAX_SESSIONS, MAX_TARGETS, SessionId, TargetId, TargetInfo,
    TargetKind, TargetRegistry,
};

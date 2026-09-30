//! CDP（Chrome DevTools Protocol）互換サーバー crate。
//!
//! `fandhe-browser-cli` から起動され、Playwright / Puppeteer 等の CDP クライアントに
//! 対して DOM 操作・ページ制御 API を公開することを目指す（README「実装方針（要点）」の
//! CDP 互換方針を参照）。`fandhe-browser-core` の `AppState` に依存し、cdp 固有の
//! 状態（ターゲット表・セッション表）は [`server::CdpState`] が内包する。
//!
//! # スタブについて
//!
//! 状態型（CDP セッション・ターゲット表。`CDP-1`、TASK-41.2）は実装済み。
//! 以下は未実装で、実装済みを装う公開 API・ダミー実装は置かない
//! （`code-comment-style.md`・REPAIR-3）。
//!
//! - `/json/*` ルータ（TASK-41.3）・`/devtools/browser/{id}` 受け口（TASK-41.4）
//! - CDP メソッドのハンドラ（`CDP-1`/`CDP-5`/`CDP-6`/`CDP-7`、`TASK-42`、`MS-4`）
//! - cli での組み立て・起動（TASK-41.5。`CDP-7`/`AISNAP-6`/`SEC-4`、`MS-3`）

pub mod server;
pub mod target;

pub use server::CdpState;
pub use target::{
    BrowserId, CdpStateError, MAX_SESSIONS, MAX_TARGETS, SessionId, TargetId, TargetInfo,
    TargetKind, TargetRegistry,
};

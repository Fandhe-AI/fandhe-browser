//! CDP（Chrome DevTools Protocol）互換サーバー crate。
//!
//! `fandhe-browser-cli` から起動され、Playwright / Puppeteer 等の CDP クライアントに
//! 対して DOM 操作・ページ制御 API を公開することを目指す（README「実装方針（要点）」の
//! CDP 互換方針を参照）。将来的には `fandhe-browser-core`（DOM・fetch）・
//! `fandhe-browser-profile`（プロファイル分離）に依存する設計だが、両 crate が
//! 本 crate と並行して整備中のため、現時点ではまだ依存を張っていない。
//!
//! # スタブについて
//!
//! 本ファイルは crate の雛形（TASK-1（1.6）・ビヘイビア `REPAIR-1`）であり、
//! 実装済みを装う公開 API・ダミー実装は置かない（`code-comment-style.md`・REPAIR-3）。
//!
//! - CDP メソッドのハンドラ（`CDP-1`/`CDP-5`/`CDP-6`/`CDP-7`、`TASK-42`、`MS-4`）
//! - サーバーの起動（`AppState`・CDP ルータの骨格。`CDP-1`/`CDP-7`/`AISNAP-6`/
//!   `SEC-4`、`TASK-41`、`MS-3`。担当は cli/cdp 両クレートに跨る）

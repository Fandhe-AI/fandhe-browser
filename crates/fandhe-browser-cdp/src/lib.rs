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
//! CDP メソッドのハンドラ・サーバー起動処理は未実装。本実装は TASK-42 で行う。
//! 実装済みを装う公開 API・ダミー実装は置かない（`code-comment-style.md`・REPAIR-3）。

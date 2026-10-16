//! 外部スクリプト（Node 製クライアント等）を実サーバーへ接続させて結果を構造化回収する
//! 共有基盤（TASK-45.1・#480、ビヘイビア `CDP-3`・MS-4）。
//!
//! `tests/puppeteer_connect.rs`（基盤の自己テスト）・`tests/puppeteer_contract.rs`（契約テスト）が `mod script_harness;` で取り込む
//! （実 Puppeteer の試験ターゲットは導入承認後に追加し、同様に取り込む）。Puppeteer 固有の事柄は
//! 呼び出し側へ寄せ、このモジュールは「サーバー起動・子プロセス実行・結果行の解析」だけを
//! 担う（Playwright 側の基盤 #476 からも再利用できる形に保つ）。
//!
//! # スクリプトとの契約
//! - WS エンドポイントは環境変数 [`ENDPOINT_ENV`] で渡す（シェル文字列は経由しない）。
//! - スクリプトは stdout に [`RESULT_PREFIX`] で始まる 1 行の JSON
//!   `{"ok": bool, "step": string, "error": {"name": string, "message": string} | null}` を出す。
//! - 任意フィールド `stages`（TASK-45.2・#481）は段階ごとの到達結果の配列
//!   `[{"name": string, "status": "ok" | "failed" | "not_reached", "error": {...} | null}]`。
//!   無ければ空として扱う（後方互換）。件数は [`MAX_STAGES`]、名前長は
//!   [`MAX_STAGE_NAME_BYTES`] が上限で、整合性（`failed` は error 必須・`ok` 後続の矛盾・
//!   トップレベル `ok: true` との不一致・`ok: false` なのに失敗段階が無い矛盾・`step` が最初の
//!   失敗段階名と異なる矛盾）を検証し、違反は `Malformed` にする。
//!
//! # Playwright（TASK-44.1・#476、`CDP-2`）で追加された契約
//! - エンドポイントは同じ [`ENDPOINT_ENV`] で渡すが、Playwright の `connectOverCDP` は HTTP
//!   エンドポイント（[`http_endpoint`]。`http://127.0.0.1:<port>`）を受け取る。
//! - ローカル fixture ページの URL は [`FIXTURE_URL_ENV`]（[`start_fixture_server`] が提供）。
//! - playwright-core の導入先ディレクトリは [`PLAYWRIGHT_MODULE_DIR_ENV`]
//!   （[`preflight_playwright`] が検査する）。
//!
//! # 安全性
//! 子プロセスは締め切りで kill し、stderr は末尾のみ・stdout は結果行のみを上限付きで保持し、
//! kill 後のリーダー待機にも上限を設ける
//! （無制限確保による DoS の防止）。サーバーは `127.0.0.1:0` にのみ bind する。
mod fixture;
mod preflight;
mod runner;
mod server;

#[allow(unused_imports)] // fixture を使わないテストバイナリでは未使用
pub use fixture::*;
pub use preflight::*;
pub use runner::*;
pub use server::*;

//! fandhe-browser-mcp: MCP 参照プラグインのバイナリ入口（TASK-94.2・PLUG-3・MS-9）。
//!
//! ブラウザ本体とは別プロセスで動き、HTTP・stdio・JSON Schema の契約のみで接続する
//! （PLUG-1）。workspace 内の他 crate には依存しない。
//!
//! stdio トランスポートで MCP サーバーを待ち受け、initialize ハンドシェイクに応答する。
//! stdout は JSON-RPC 専用のため自前では何も書かず、診断は stderr に英語で出す。
//! シグナル処理は持たず、stdin の EOF で終了する。stdin は `limit::LimitedReader` で包み、
//! 1 メッセージ（改行区切り 1 行）が `MAX_MESSAGE_BYTES` を超えた時点で読み取りを打ち切って
//! セッションを終了する（改行なしの巨大入力によるメモリ無制限確保の防止）。
//! 起動順は「ホストへ自己申告（TASK-94.5・`register`）→ stdio セッション」。自己申告は
//! 環境変数 `FANDHE_BROWSER_HOST_ADDR`（loopback の `ip:port`・既定 `127.0.0.1:9333`）の
//! ホストへ `POST /ai/plugins/register` を 1 回送る。失敗（接続不可・拒否・不正応答・設定不正）
//! は成功を装わず、stderr に英語で出して失敗終了し MCP セッションは開始しない。ホストに同 id が
//! 登録済み（409 `duplicate_plugin_id`）の場合のみ警告して継続する（再起動時の暫定対処）。
//! navigate（TASK-94.3）はホストの `POST /ai/navigate` を `host.rs` 経由で呼ぶ。
//! snapshot（TASK-94.4・PLUG-4）はホストの `GET /ai/snapshot` を同様に呼ぶ。

mod host;
mod limit;
mod navigate;
mod register;
mod server;
mod snapshot;

use std::process::ExitCode;

use rmcp::ServiceExt;

use limit::{LimitedReader, MAX_MESSAGE_BYTES};
use register::RegisterOutcome;
use server::FandheBrowserMcp;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match register::register_with_host() {
        Ok(RegisterOutcome::Registered) => {}
        Ok(RegisterOutcome::AlreadyRegistered) => {
            eprintln!(
                "fandhe-browser-mcp: warning: host already has this plugin registered; continuing"
            );
        }
        Err(e) => {
            eprintln!("fandhe-browser-mcp: failed to register with host: {e}");
            return ExitCode::FAILURE;
        }
    }
    let service = match FandheBrowserMcp
        .serve((
            LimitedReader::new(tokio::io::stdin(), MAX_MESSAGE_BYTES),
            tokio::io::stdout(),
        ))
        .await
    {
        Ok(service) => service,
        Err(e) => {
            eprintln!("fandhe-browser-mcp: failed to initialize MCP session: {e}");
            return ExitCode::FAILURE;
        }
    };
    match service.waiting().await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fandhe-browser-mcp: MCP session terminated abnormally: {e}");
            ExitCode::FAILURE
        }
    }
}

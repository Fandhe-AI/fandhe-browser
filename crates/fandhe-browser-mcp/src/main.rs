//! fandhe-browser-mcp: MCP 参照プラグインのバイナリ入口（TASK-94.2・PLUG-3）。
//!
//! ブラウザ本体とは別プロセスで動き、HTTP・stdio・JSON Schema の契約のみで接続する
//! （PLUG-1）。workspace 内の他 crate には依存しない。
//!
//! stdio トランスポートで MCP サーバーを待ち受け、initialize ハンドシェイクに応答する。
//! stdout は JSON-RPC 専用のため自前では何も書かず、診断は stderr に英語で出す。
//! シグナル処理は持たず、stdin の EOF で終了する。行長の上限は rmcp の公開 API から
//! 指定できず未設定（改行なしの巨大入力でメモリが伸び得る既知の制限）。
//! 未実装: navigate（TASK-94.3）・snapshot（TASK-94.4）・自己申告（TASK-94.5）。

mod server;

use std::process::ExitCode;

use rmcp::ServiceExt;
use rmcp::transport::stdio;

use server::FandheBrowserMcp;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let service = match FandheBrowserMcp.serve(stdio()).await {
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

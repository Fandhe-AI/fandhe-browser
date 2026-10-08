//! fandhe-browser-mcp: MCP 参照プラグインのバイナリ入口（雛形）。
//!
//! ブラウザ本体とは別プロセスで動き、HTTP・stdio・JSON Schema の契約のみで接続する
//! （PLUG-1）。workspace 内の他 crate には依存しない。
//!
//! 現状はスタブで、MCP サーバーは未実装（TASK-94.1・PLUG-3）。実装済みを装わないため
//! （REPAIR-3）、stderr に英語で通知して失敗終了する。stdout は将来 JSON-RPC 専用に
//! なるため何も書かない。将来仕様: TASK-94.2 で rmcp による stdio サーバー、
//! TASK-94.3 で navigate、TASK-94.4 で snapshot、TASK-94.5 で自己申告を実装する
//! （PLUG-3・PLUG-4）。

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("fandhe-browser-mcp: MCP server is not implemented yet (TASK-94.2)");
    ExitCode::FAILURE
}

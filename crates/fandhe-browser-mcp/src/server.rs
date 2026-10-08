//! MCP サーバーのハンドラ実装（TASK-94.2・PLUG-3・MS-9）。
//!
//! `main.rs` が stdio トランスポート上でこのハンドラを起動する。現状は initialize
//! ハンドシェイクに応答するだけの基盤で、ツールは未実装のため capabilities を空にし、
//! 実装済みを装わない（REPAIR-3）。将来仕様: TASK-94.3 で navigate、TASK-94.4 で
//! snapshot を追加して `enable_tools()` を広告し、TASK-94.5 でホストへ自己申告する
//! （PLUG-3・PLUG-4）。

use rmcp::ServerHandler;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig};

/// サーバーが採用する MCP プロトコル版。stdio は initialize ハンドシェイクを使うため、
/// それを持つ 2025-11-25 に固定する（`LATEST` は将来 initialize を廃した版へ進み得る）。
/// TASK-94.5 の自己申告も同じ定数を参照する。
pub(crate) const SERVER_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// fandhe-browser の MCP 参照プラグイン本体。状態を持たない（ツール群は TASK-94.3 以降で追加）。
pub(crate) struct FandheBrowserMcp;

impl ServerHandler for FandheBrowserMcp {
    /// initialize 要求への応答内容を返す。サーバー名・版は自 crate の値を使う。
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_protocol_version(SERVER_PROTOCOL_VERSION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::serde_json::{self, json};

    /// PLUG-3 / TASK-94.2: get_info は空 capabilities・自 crate の serverInfo・固定プロトコル版を返す。
    #[test]
    fn plug3_get_info_returns_empty_capabilities_and_server_info() {
        let info = serde_json::to_value(FandheBrowserMcp.get_info()).expect("serialize");
        assert_eq!(info["capabilities"], json!({}));
        assert_eq!(info["serverInfo"]["name"], "fandhe-browser-mcp");
        assert_eq!(info["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info["protocolVersion"], "2025-11-25");
    }

    /// PLUG-3 / TASK-94.2: 採用するプロトコル版は 2025-11-25 に固定されている。
    #[test]
    fn plug3_protocol_version_is_pinned() {
        assert_eq!(SERVER_PROTOCOL_VERSION.as_str(), "2025-11-25");
    }
}

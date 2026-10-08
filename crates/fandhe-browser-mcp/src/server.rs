//! MCP サーバーのハンドラ実装（TASK-94.2・TASK-94.3・PLUG-3・MS-9）。
//!
//! `main.rs` が stdio トランスポート上でこのハンドラを起動する。initialize に応答し、
//! `navigate` ツール（TASK-94.3）を公開する。navigate はホストの `POST /ai/navigate`
//! （PoC-15 契約。本リポのホストには未実装のため実ホストでは 404 となり得る。REPAIR-3）を
//! `host.rs` 経由で呼び、失敗は必ず `isError` で返す（成功を装わない）。
//! 将来仕様: TASK-94.4 で snapshot を追加し、TASK-94.5 でホストへ自己申告する（PLUG-3・PLUG-4）。

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::serde::Deserialize;
use rmcp::serde_json::json;
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};

use crate::host;
use crate::navigate::{self, NavigateOutcome};

/// サーバーが採用する MCP プロトコル版。stdio は initialize ハンドシェイクを使うため、
/// それを持つ 2025-11-25 に固定する（`LATEST` は将来 initialize を廃した版へ進み得る）。
/// TASK-94.5 の自己申告も同じ定数を参照する。
pub(crate) const SERVER_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// fandhe-browser の MCP 参照プラグイン本体。状態を持たない。
pub(crate) struct FandheBrowserMcp;

/// navigate ツールの入力。
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(crate = "rmcp::serde")]
#[schemars(crate = "rmcp::schemars")]
pub(crate) struct NavigateParams {
    /// 遷移先 URL（http / https / about:blank）。
    url: String,
}

fn error_result(message: &str, extra: rmcp::serde_json::Value) -> CallToolResult {
    let mut v = json!({ "ok": false, "error": message });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    CallToolResult::structured_error(v)
}

#[tool_router]
impl FandheBrowserMcp {
    /// ホストへページ遷移を依頼する。検証失敗・接続失敗・ホストの失敗応答は `isError`。
    #[tool(description = "Navigate the browser to a URL (http, https or about:blank).")]
    async fn navigate(&self, Parameters(params): Parameters<NavigateParams>) -> CallToolResult {
        let url = match navigate::validate_url(&params.url) {
            Ok(u) => u,
            Err(e) => return error_result(&e.to_string(), json!({})),
        };
        let addr = match host::resolve_from_env() {
            Ok(a) => a,
            Err(e) => return error_result(&e.to_string(), json!({})),
        };
        let body = navigate::request_body(url);
        // main は current_thread ランタイムのため、ブロッキング I/O は専用スレッドへ逃がす。
        let res =
            tokio::task::spawn_blocking(move || host::post_json(addr, "/ai/navigate", &body)).await;
        let resp = match res {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return error_result(&e.to_string(), json!({})),
            Err(_) => return error_result("navigate task failed", json!({})),
        };
        match navigate::interpret(resp.status, &resp.body) {
            NavigateOutcome::Success { url } => CallToolResult::structured(json!({
                "ok": true,
                "url": url.unwrap_or_else(|| params.url.clone()),
            })),
            NavigateOutcome::Failure { status, code } => error_result(
                "host rejected navigation",
                json!({ "status": status, "code": code }),
            ),
        }
    }
}

#[tool_handler]
impl ServerHandler for FandheBrowserMcp {
    /// initialize 要求への応答内容を返す。サーバー名・版は自 crate の値を使う。
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
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

    /// PLUG-3 / TASK-94.2: get_info は tools capabilities・自 crate の serverInfo・固定プロトコル版を返す。
    #[test]
    fn plug3_get_info_returns_tools_capability_and_server_info() {
        let info = serde_json::to_value(FandheBrowserMcp.get_info()).expect("serialize");
        assert_eq!(info["capabilities"], json!({"tools":{}}));
        assert_eq!(info["serverInfo"]["name"], "fandhe-browser-mcp");
        assert_eq!(info["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info["protocolVersion"], "2025-11-25");
    }

    /// PLUG-3 / TASK-94.3: ツール一覧は navigate のみで、入力スキーマは url 必須。
    #[test]
    fn plug3_tool_list_has_navigate_with_required_url() {
        let tools = FandheBrowserMcp::tool_router().list_all();
        assert_eq!(tools.len(), 1);
        let tool = serde_json::to_value(&tools[0]).expect("serialize");
        assert_eq!(tool["name"], "navigate");
        assert_eq!(tool["inputSchema"]["required"], json!(["url"]));
    }

    /// PLUG-3 / TASK-94.2: 採用するプロトコル版は 2025-11-25 に固定されている。
    #[test]
    fn plug3_protocol_version_is_pinned() {
        assert_eq!(SERVER_PROTOCOL_VERSION.as_str(), "2025-11-25");
    }
}

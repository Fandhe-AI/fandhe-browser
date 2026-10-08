//! MCP サーバーのハンドラ実装（TASK-94.2・TASK-94.3・PLUG-3・MS-9）。
//!
//! `main.rs` が stdio トランスポート上でこのハンドラを起動する。initialize に応答し、
//! `navigate` ツール（TASK-94.3）を公開する。navigate はホストの `POST /ai/navigate`
//! （PoC-15 契約。本リポのホストには未実装のため実ホストでは 404 となり得る。REPAIR-3）を
//! `host.rs` 経由で呼び、失敗は必ず `isError` で返す（成功を装わない）。
//! `snapshot` ツール（TASK-94.4・PLUG-4）はホストの `GET /ai/snapshot` の簡約スナップショットを
//! text コンテンツ 1 件で返す（`structuredContent` との二重出力でトークンを増やさない）。
//! 将来仕様: TASK-94.5 でホストへ自己申告する（PLUG-3）。

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::serde::Deserialize;
use rmcp::serde_json::json;
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};

use crate::host;
use crate::navigate::{self, NavigateOutcome};
use crate::snapshot::{self, SnapshotOutcome};

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
        let Some(permit) = host::try_acquire() else {
            return error_result("too many concurrent host requests", json!({}));
        };
        // main は current_thread ランタイムのため、ブロッキング I/O は専用スレッドへ逃がす。
        // 枠は blocking クロージャが終わるまで保持する（呼び出しが取り消されても解放しない）。
        let res = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            host::post_json(addr, "/ai/navigate", &body)
        })
        .await;
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

    /// ホストの簡約スナップショットを取得する。失敗・不完全な応答はすべて `isError`。
    #[tool(
        description = "Return the simplified accessibility snapshot of the most recently navigated page."
    )]
    async fn snapshot(&self) -> CallToolResult {
        let addr = match host::resolve_from_env() {
            Ok(a) => a,
            Err(e) => return error_result(&e.to_string(), json!({})),
        };
        let Some(permit) = host::try_acquire() else {
            return error_result("too many concurrent host requests", json!({}));
        };
        // 枠は blocking クロージャが終わるまで保持する（呼び出しが取り消されても解放しない）。
        // 最大 4 MiB の JSON 解析・再直列化も同じクロージャで行い、current_thread ランタイムを塞がない。
        let res = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let resp = host::get(
                addr,
                snapshot::SNAPSHOT_PATH,
                snapshot::MAX_SNAPSHOT_RESPONSE_BYTES,
            )?;
            Ok::<_, host::HostError>(snapshot::interpret(resp.status, &resp.body))
        })
        .await;
        let outcome = match res {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return error_result(&e.to_string(), json!({})),
            Err(_) => return error_result("snapshot task failed", json!({})),
        };
        match outcome {
            SnapshotOutcome::Success { text } => {
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            SnapshotOutcome::Failure { status, code } => error_result(
                "host rejected snapshot",
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

    /// PLUG-3 / TASK-94.3 / TASK-94.4: ツール一覧は navigate（url 必須）と snapshot（必須引数なし）。
    #[test]
    fn plug3_tool_list_has_navigate_and_snapshot() {
        let router = FandheBrowserMcp::tool_router();
        let tools = serde_json::to_value(router.list_all()).expect("serialize");
        let find = |name: &str| {
            tools
                .as_array()
                .and_then(|a| a.iter().find(|t| t["name"] == name))
                .cloned()
        };
        assert_eq!(tools.as_array().map(Vec::len), Some(2));
        let nav = find("navigate").expect("navigate");
        assert_eq!(nav["inputSchema"]["required"], json!(["url"]));
        let snap = find("snapshot").expect("snapshot");
        assert_eq!(snap["inputSchema"]["type"], "object");
        assert!(snap["inputSchema"].get("required").is_none());
    }

    /// PLUG-3 / TASK-94.2: 採用するプロトコル版は 2025-11-25 に固定されている。
    #[test]
    fn plug3_protocol_version_is_pinned() {
        assert_eq!(SERVER_PROTOCOL_VERSION.as_str(), "2025-11-25");
    }
}

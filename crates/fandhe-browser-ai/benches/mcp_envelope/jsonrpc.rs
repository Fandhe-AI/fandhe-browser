//! MCP エンベロープ測定ハーネスの JSON-RPC 純関数群（TASK-97.1・Issue #385・`PLUG-5`・`MS-9`）。
//!
//! `mcp_client.rs` が mcp バイナリ（`fandhe-browser-mcp`）の stdio へ送る要求行の生成と、
//! stdout から読んだ応答行の解析を担う。I/O を持たない純関数だけで構成し、`jsonrpc_tests.rs`
//! が 3 OS で具体値を検証する。要求は `serde_json` で組み立て、文字列連結はしない。
//!
//! 応答検証は fail-closed: `id` 不一致・`error` 応答・`isError: true`・content 欠落・不正 JSON は
//! すべて明示的なエラーにし、成功を装わない。
//!
//! 将来仕様（REPAIR-3）: ホストに `POST /ai/navigate` が実装されたら、`navigate` ツール経由の
//! 測定へ切り替える。スナップショットのスキーマ確定（`AISNAP-6`）後は比較対象を更新する。

use std::fmt;

use serde_json::{Value, json};

/// ハーネスが initialize で名乗るプロトコル版。mcp 側の固定版（2025-11-25）に合わせる。
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// 応答行の検証エラー。メッセージにはスナップショット本文を含めない。
#[derive(Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// 応答行が JSON として不正。
    InvalidJson,
    /// 応答 `id` が要求と一致しない。
    IdMismatch { expected: u64 },
    /// JSON-RPC `error` 応答（code のみ保持）。
    RpcError { code: Option<i64> },
    /// `result.isError` が true。
    ToolError,
    /// `result.content[0].text` が無い。
    MissingText,
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => f.write_str("response line is not valid JSON"),
            Self::IdMismatch { expected } => {
                write!(f, "response id does not match request id {expected}")
            }
            Self::RpcError { code } => match code {
                Some(c) => write!(f, "JSON-RPC error response (code {c})"),
                None => f.write_str("JSON-RPC error response"),
            },
            Self::ToolError => f.write_str("tool call returned isError=true"),
            Self::MissingText => f.write_str("result.content[0].text is missing"),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// 1 行の JSON-RPC 要求（改行なし）を作る。
pub fn request_line(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// 1 行の JSON-RPC 通知（`id` なし・改行なし）を作る。
pub fn notification_line(method: &str) -> String {
    json!({"jsonrpc": "2.0", "method": method}).to_string()
}

/// `initialize` の params。
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "fandhe-mcp-envelope-bench", "version": "0"},
    })
}

/// `tools/call snapshot` の params。
pub fn snapshot_call_params() -> Value {
    json!({"name": "snapshot", "arguments": {}})
}

fn parse_with_id(line: &str, expected_id: u64) -> Result<Value, EnvelopeError> {
    let v: Value = serde_json::from_str(line).map_err(|_| EnvelopeError::InvalidJson)?;
    if v.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Err(EnvelopeError::IdMismatch {
            expected: expected_id,
        });
    }
    if let Some(e) = v.get("error") {
        return Err(EnvelopeError::RpcError {
            code: e.get("code").and_then(Value::as_i64),
        });
    }
    Ok(v)
}

/// 応答行が `id` に対応する成功応答であることを検証し、`result.content[0].text` を返す。
pub fn extract_snapshot_text(line: &str, expected_id: u64) -> Result<String, EnvelopeError> {
    let v = parse_with_id(line, expected_id)?;
    let result = v.get("result").ok_or(EnvelopeError::MissingText)?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(EnvelopeError::ToolError);
    }
    result
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(EnvelopeError::MissingText)
}

/// 応答行が `id` に対応する成功応答（`error` なし・`result` あり）かだけを検証する（initialize 用）。
pub fn expect_ok_response(line: &str, expected_id: u64) -> Result<(), EnvelopeError> {
    let v = parse_with_id(line, expected_id)?;
    if v.get("result").is_none() {
        return Err(EnvelopeError::MissingText);
    }
    Ok(())
}

//! `jsonrpc.rs` のユニットテスト（TASK-97.1・Issue #385・`PLUG-5`）。

#[allow(dead_code)]
#[path = "jsonrpc.rs"]
mod jsonrpc;

use jsonrpc::*;
use serde_json::{Value, json};

#[test]
fn plug5_request_line_is_single_line_json_rpc() {
    let line = request_line(7, "tools/call", snapshot_call_params());
    assert!(!line.contains('\n'));
    let v: Value = serde_json::from_str(&line).expect("json");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 7);
    assert_eq!(v["method"], "tools/call");
    assert_eq!(v["params"], json!({"name": "snapshot", "arguments": {}}));
}

#[test]
fn plug5_notification_has_no_id() {
    let v: Value = serde_json::from_str(&notification_line("notifications/initialized")).unwrap();
    assert_eq!(v["method"], "notifications/initialized");
    assert!(v.get("id").is_none());
}

#[test]
fn plug5_initialize_params_pin_protocol_version() {
    assert_eq!(initialize_params()["protocolVersion"], "2025-11-25");
}

#[test]
fn plug5_extract_text_from_success_response() {
    let line =
        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"a\":1}"}]}}"#;
    assert_eq!(extract_snapshot_text(line, 3), Ok("{\"a\":1}".to_string()));
}

#[test]
fn plug5_extract_text_rejects_each_failure_mode() {
    let ok = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"x"}]}}"#;
    assert_eq!(
        extract_snapshot_text(ok, 4),
        Err(EnvelopeError::IdMismatch { expected: 4 })
    );
    assert_eq!(
        extract_snapshot_text("not json", 3),
        Err(EnvelopeError::InvalidJson)
    );
    let rpc_err = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"m"}}"#;
    assert_eq!(
        extract_snapshot_text(rpc_err, 3),
        Err(EnvelopeError::RpcError { code: Some(-32601) })
    );
    let tool_err = r#"{"jsonrpc":"2.0","id":3,"result":{"isError":true,"content":[{"type":"text","text":"x"}]}}"#;
    assert_eq!(
        extract_snapshot_text(tool_err, 3),
        Err(EnvelopeError::ToolError)
    );
    let no_content = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[]}}"#;
    assert_eq!(
        extract_snapshot_text(no_content, 3),
        Err(EnvelopeError::MissingText)
    );
    let no_result = r#"{"jsonrpc":"2.0","id":3}"#;
    assert_eq!(
        extract_snapshot_text(no_result, 3),
        Err(EnvelopeError::MissingText)
    );
}

#[test]
fn plug5_expect_ok_response_checks_id_and_result() {
    assert_eq!(
        expect_ok_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, 1),
        Ok(())
    );
    assert_eq!(
        expect_ok_response(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#, 1),
        Err(EnvelopeError::IdMismatch { expected: 1 })
    );
    assert_eq!(
        expect_ok_response(r#"{"jsonrpc":"2.0","id":1,"error":{"message":"m"}}"#, 1),
        Err(EnvelopeError::RpcError { code: None })
    );
}

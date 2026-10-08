//! snapshot ツールの結合テスト（TASK-94.4・PLUG-3・PLUG-4・MS-9）。
//!
//! 実バイナリを起動し、テスト内の偽ホスト（loopback の `TcpListener`）に対して
//! `GET /ai/snapshot` が送られ、応答が MCP 結果へ反映されることを検証する。
//! 実ホストとの end-to-end（ホストの `/ai/navigate` が未実装のため成功系を作れない）は対象外。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rmcp::serde_json::{self, Value, json};

mod common;

/// 偽ホスト。1 接続だけ受け、要求ヘッダ全文を返し、`reply` を応答して切断する。
fn fake_host(reply: Vec<u8>) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        common::serve_register(&listener);
        if let Ok((mut s, _)) = listener.accept() {
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                if n == 0 || raw.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
            let _ = s.write_all(&reply);
        }
    });
    (addr, rx)
}

fn http(status_line: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

const ENVELOPE: &str = r#"{"url":"https://example.com/","tree":{"role":"document","name":"Example"},"truncated":false}"#;

/// MCP セッションを張り、snapshot を呼んだ結果（id=3）と ping 応答の有無を返す。
fn call_snapshot(host_addr: &str) -> (Value, bool) {
    let input = format!(
        "{}\n{}\n{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"test","version":"0.0.0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"snapshot","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"ping"}),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
        .env("FANDHE_BROWSER_HOST_ADDR", host_addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    // 応答を読み切ってから stdin を閉じる（snapshot の完了前に EOF にしない）。
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
        stdin
    });
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let mut buf = [0u8; 65536];
        while !(text.contains("\"id\":3") && text.contains("\"id\":4") && text.ends_with('\n')) {
            let n = stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            text.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        text
    });
    let text = {
        let start = std::time::Instant::now();
        while !reader.is_finished() {
            if start.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("mcp binary did not respond within 30s");
            }
            thread::sleep(Duration::from_millis(20));
        }
        reader.join().expect("reader")
    };
    drop(writer.join().expect("writer"));
    let _ = child.wait();
    let msgs: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let result = msgs
        .iter()
        .find(|m| m["id"] == 3)
        .expect("snapshot response")["result"]
        .clone();
    let pinged = msgs.iter().any(|m| m["id"] == 4);
    (result, pinged)
}

/// PLUG-4 / TASK-94.4: tools/list に snapshot が載り、必須引数を持たない。
#[test]
fn plug4_tools_list_exposes_snapshot() {
    let input = format!(
        "{}\n{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"test","version":"0.0.0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let (host_addr, _reg) = common::register_only();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
        .env("FANDHE_BROWSER_HOST_ADDR", host_addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(input.as_bytes()).expect("write");
    drop(stdin);
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read");
    let _ = child.wait();
    let msgs: Vec<Value> = out
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let tools = msgs.iter().find(|m| m["id"] == 2).expect("list")["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(tools.len(), 2);
    let snap = tools
        .iter()
        .find(|t| t["name"] == "snapshot")
        .expect("snapshot tool");
    assert!(snap["inputSchema"].get("required").is_none());
}

/// PLUG-4 / TASK-94.4: 正常系。GET /ai/snapshot が送られ、text 1 件で返る。
#[test]
fn plug4_snapshot_gets_from_host_and_returns_text() {
    let (addr, rx) = fake_host(http("200 OK", ENVELOPE));
    let (result, _) = call_snapshot(&addr);
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("request");
    assert!(req.starts_with("GET /ai/snapshot HTTP/1.1\r\n"), "{req}");
    assert!(req.contains(&format!("Host: {addr}\r\n")), "{req}");
    assert_ne!(result["isError"], true);
    assert!(result.get("structuredContent").is_none());
    let content = result["content"].as_array().expect("content");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    let snap: Value = serde_json::from_str(content[0]["text"].as_str().expect("text")).unwrap();
    assert_eq!(snap["url"], "https://example.com/");
    assert_eq!(snap["tree"]["role"], "document");
    assert_eq!(snap["truncated"], false);
}

/// PLUG-4 / TASK-94.4: navigate の 64 KiB 上限を超える大きなスナップショットも成功する。
#[test]
fn plug4_snapshot_accepts_response_over_64kib() {
    let big = "x".repeat(200 * 1024);
    let body =
        json!({"url":"u","tree":{"role":"document","name":big},"truncated":true}).to_string();
    let (addr, _rx) = fake_host(http("200 OK", &body));
    let (result, _) = call_snapshot(&addr);
    assert_ne!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(text.len() > 200 * 1024);
}

/// PLUG-4 / TASK-94.4: 409 no_navigation は isError。ホストの自由文は漏らさない。
#[test]
fn plug4_snapshot_no_navigation_is_error_without_leaking_text() {
    let body = r#"{"code":"no_navigation","message":"INTERNAL-SECRET"}"#;
    let (addr, _rx) = fake_host(http("409 Conflict", body));
    let (result, _) = call_snapshot(&addr);
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["status"], 409);
    assert_eq!(result["structuredContent"]["code"], "no_navigation");
    assert!(!result.to_string().contains("INTERNAL-SECRET"));
}

/// PLUG-4 / TASK-94.4: Content-Length より短い本文（途中切断）は成功を装わず isError。
#[test]
fn plug4_snapshot_truncated_body_is_error() {
    let mut reply = http("200 OK", ENVELOPE);
    reply.truncate(reply.len() - 20);
    let (addr, _rx) = fake_host(reply);
    let (result, _) = call_snapshot(&addr);
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"],
        "malformed host response"
    );
}

/// PLUG-4 / TASK-94.4: 形の違う 200 応答は isError。
#[test]
fn plug4_snapshot_malformed_envelope_is_error() {
    let (addr, _rx) = fake_host(http("200 OK", r#"{"url":"u"}"#));
    let (result, _) = call_snapshot(&addr);
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["status"], 200);
}

/// PLUG-4 / TASK-94.4: 4 MiB 超の応答は isError。
#[test]
fn plug4_snapshot_oversized_response_is_error() {
    let big = "x".repeat(4 * 1024 * 1024 + 1024);
    let body = json!({"url":"u","tree":{"name":big},"truncated":false}).to_string();
    let (addr, _rx) = fake_host(http("200 OK", &body));
    let (result, _) = call_snapshot(&addr);
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"],
        "host response too large"
    );
}

/// PLUG-4 / TASK-94.4: ホスト未起動でも isError を返し、セッションは継続する。
#[test]
fn plug4_snapshot_connection_refused_keeps_session() {
    let addr = common::register_then_close();
    let (result, pinged) = call_snapshot(&addr);
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"],
        "failed to connect to host"
    );
    assert!(pinged);
}

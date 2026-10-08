//! navigate ツールの結合テスト（TASK-94.3・PLUG-3・MS-9）。
//!
//! 実バイナリを起動し、テスト内の偽ホスト（loopback の `TcpListener`）に対して
//! `POST /ai/navigate` が送られ、応答が MCP 結果へ反映されることを検証する。
//! 実ホスト（本リポのホストには `/ai/navigate` が未実装）との end-to-end は対象外。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rmcp::serde_json::{self, Value, json};

/// 偽ホスト。1 接続だけ受け、受信した要求全文を返し、`reply` を応答する。
fn fake_host(reply: &'static str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if n == 0 || body_complete(&text) {
                    let _ = tx.send(text);
                    break;
                }
            }
            let _ = s.write_all(reply.as_bytes());
        }
    });
    (addr, rx)
}

fn body_complete(text: &str) -> bool {
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let len = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    body.len() >= len
}

/// MCP セッションを張り、navigate を呼んだ結果（id=3）と ping 応答の有無を返す。
fn call_navigate(host_addr: &str, url: &str) -> (Value, bool) {
    let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"navigate","arguments":{"url":url}}});
    let input = format!(
        "{}\n{}\n{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"test","version":"0.0.0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        call,
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
    // 応答を読み切ってから stdin を閉じる（navigate の完了前に EOF にしない）。
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
        stdin
    });
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let mut buf = [0u8; 4096];
        // id=3（navigate）と id=4（ping）の両応答まで読む。要求は並行処理されるため順序は不定。
        while !(text.contains("\"id\":3") && text.contains("\"id\":4")) {
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
        .expect("navigate response")["result"]
        .clone();
    let pinged = msgs.iter().any(|m| m["id"] == 4);
    (result, pinged)
}

/// PLUG-3 / TASK-94.3: tools/list に navigate が 1 件載り、url が必須。
#[test]
fn plug3_tools_list_exposes_navigate() {
    let input = format!(
        "{}\n{}\n{}\n",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"test","version":"0.0.0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
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
    let list = &msgs.iter().find(|m| m["id"] == 2).expect("list")["result"]["tools"];
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["name"], "navigate");
    assert_eq!(list[0]["inputSchema"]["required"], json!(["url"]));
}

/// PLUG-3 / TASK-94.3: 正常系。POST /ai/navigate が送られ結果が反映される。
#[test]
fn plug3_navigate_posts_to_host_and_returns_url() {
    let (addr, rx) = fake_host(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"ok\":true,\"url\":\"https://example.com/\"}",
    );
    let (result, _) = call_navigate(&addr, "https://example.com/");
    let req = rx.recv_timeout(Duration::from_secs(5)).expect("request");
    assert!(req.starts_with("POST /ai/navigate HTTP/1.1\r\n"), "{req}");
    assert!(req.ends_with("{\"url\":\"https://example.com/\"}"), "{req}");
    assert_ne!(result["isError"], true);
    assert_eq!(result["structuredContent"]["ok"], true);
    assert_eq!(result["structuredContent"]["url"], "https://example.com/");
}

/// PLUG-3 / TASK-94.3: ホストが失敗応答でも isError。自由文は漏らさず status と code のみ。
#[test]
fn plug3_navigate_host_failure_is_error_without_leaking_text() {
    let (addr, _rx) = fake_host(
        "HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n{\"ok\":false,\"code\":\"fetch_failed\",\"error\":\"INTERNAL-SECRET\"}",
    );
    let (result, _) = call_navigate(&addr, "https://example.com/");
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["status"], 502);
    assert_eq!(result["structuredContent"]["code"], "fetch_failed");
    assert!(!result.to_string().contains("INTERNAL-SECRET"));
}

/// PLUG-3 / TASK-94.3: 404（ホスト未対応）も成功を装わず isError。
#[test]
fn plug3_navigate_host_404_is_error() {
    let (addr, _rx) = fake_host("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
    let (result, _) = call_navigate(&addr, "https://example.com/");
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["status"], 404);
}

/// PLUG-3 / TASK-94.3: ホスト未起動でも isError を返し、セッションは継続する。
#[test]
fn plug3_navigate_connection_refused_keeps_session() {
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").to_string()
    };
    let (result, pinged) = call_navigate(&addr, "https://example.com/");
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"],
        "failed to connect to host"
    );
    assert!(pinged);
}

/// PLUG-3 / TASK-94.3: 不正 URL は偽ホストへ接続せず isError。
#[test]
fn plug3_navigate_invalid_url_never_contacts_host() {
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "",
        "http://a b",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr = listener.local_addr().expect("addr").to_string();
        let (result, _) = call_navigate(&addr, url);
        assert_eq!(result["isError"], true, "{url}");
        assert!(listener.accept().is_err(), "host contacted for {url}");
    }
}

/// PLUG-3 / TASK-94.3: 非 loopback の接続先は接続せず isError。
#[test]
fn plug3_navigate_non_loopback_host_is_rejected() {
    let (result, _) = call_navigate("192.0.2.1:9333", "https://example.com/");
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"],
        "host address must be a loopback IP address"
    );
}

//! 起動時のホスト自己申告の結合テスト（TASK-94.5・PLUG-2・PLUG-3・MS-9）。実バイナリ + 偽ホスト。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rmcp::serde_json::{self, Value, json};

mod support;
use support::FakeHost;

struct Out {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0.0.0"}}}"#;

fn run(addr: &str) -> Out {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
        .env("FANDHE_BROWSER_HOST_ADDR", addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        // 登録失敗で子が先に終了した場合の BrokenPipe は想定内。
        let _ = stdin.write_all(format!("{INIT}\n").as_bytes());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("o")
        .read_to_string(&mut stdout)
        .expect("r");
    child
        .stderr
        .take()
        .expect("e")
        .read_to_string(&mut stderr)
        .expect("r");
    Out {
        stdout,
        stderr,
        code: status.code(),
    }
}

/// PLUG-2 / TASK-94.5: 起動時に登録要求を送り、その後 initialize に応答して正常終了する。
#[test]
fn plug2_startup_sends_register_request_to_host() {
    let host = FakeHost::spawn(200, r#"{"ok":true,"id":"fandhe-browser-mcp"}"#);
    let addr = host.addr();
    let out = run(&addr);
    let req = host.received().expect("request received");
    assert!(
        req.starts_with("POST /ai/plugins/register HTTP/1.1\r\n"),
        "{req}"
    );
    assert!(req.contains(&format!("Host: {addr}\r\n")));
    assert!(req.contains("Content-Type: application/json\r\n"));
    assert!(!req.to_ascii_lowercase().contains("origin"));
    let body: Value =
        serde_json::from_str(req.split_once("\r\n\r\n").expect("body").1).expect("json");
    assert_eq!(
        body,
        json!({
            "id": "fandhe-browser-mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "transport": "stdio",
            "tools": ["navigate", "snapshot"],
            "permissions": ["network.fetch", "dom.read"],
            "protocolVersion": "2025-11-25",
            "runtime": "native",
            "language": "rust",
        })
    );
    assert!(out.stdout.contains(r#""id":1"#), "{}", out.stdout);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
}

/// PLUG-2 / TASK-94.5: ホストに接続できなければ失敗終了し stdout は空。
#[test]
fn plug2_register_connection_refused_fails_startup() {
    let addr = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").to_string()
    };
    let out = run(&addr);
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("failed to register with host"),
        "{}",
        out.stderr
    );
}

/// PLUG-2 / TASK-94.5: ホストが 400 で拒否したら失敗終了し status と code を stderr に出す。
#[test]
fn plug2_register_rejected_by_host_fails_startup() {
    let host = FakeHost::spawn(400, r#"{"code":"invalid_id","message":"bad"}"#);
    let out = run(&host.addr());
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("status 400 (invalid_id)"),
        "{}",
        out.stderr
    );
}

/// PLUG-2 / TASK-94.5: 非 loopback の接続先は接続前に拒否する。
#[test]
fn plug2_register_non_loopback_addr_is_rejected_before_connect() {
    let out = run("192.0.2.1:9333");
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(out.stderr.contains("loopback"), "{}", out.stderr);
}

/// PLUG-2 / TASK-94.5: 409 duplicate_plugin_id は警告して継続し、セッションが成立する。
#[test]
fn plug2_register_duplicate_id_continues_with_warning() {
    let host = FakeHost::spawn(409, r#"{"code":"duplicate_plugin_id","message":"dup"}"#);
    let out = run(&host.addr());
    assert!(out.stderr.contains("warning"), "{}", out.stderr);
    assert!(out.stdout.contains(r#""id":1"#), "{}", out.stdout);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
}

/// PLUG-2 / TASK-94.5: 200 でも本文が登録成功の形でなければ失敗終了する。
#[test]
fn plug2_register_unexpected_200_body_fails_startup() {
    let host = FakeHost::spawn(200, "{}");
    let out = run(&host.addr());
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("unexpected host response body"),
        "{}",
        out.stderr
    );
}

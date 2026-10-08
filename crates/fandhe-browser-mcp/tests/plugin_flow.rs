//! 登録 → 一覧 → navigate → snapshot を 1 セッションで通す結合テスト（TASK-94.6・PLUG-2・PLUG-3・PLUG-4・MS-9）。
//!
//! 実バイナリを起動し、loopback の偽ホスト（`TcpListener`）とスキーマ契約
//! （`docs/design/host-api.schema.json`）で、起動時の自己申告フローとツール呼び出しの
//! 通し動作を検証する。個々のツールの異常系は `navigate_tool.rs`・`snapshot_tool.rs`、
//! 500 拒否・非 loopback 拒否は `stdio_handshake.rs` が担うため、ここでは重複させない。
//! 実ホスト（`fandhe-browser-cli`）との end-to-end は PLUG-1 の依存境界（mcp は workspace 内の
//! 他 crate・バイナリに依存しない）のため対象外。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rmcp::serde_json::{self, Value, json};

mod common;

/// 子プロセスとの I/O 全体に課す期限（ハング防止）。
const DEADLINE: Duration = Duration::from_secs(30);

/// 組み立て済み HTTP 応答（`Content-Length`・`Connection: close` 付き）。
fn http(status_line: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// 複数接続を順に受ける偽ホスト。`replies` の件数だけ accept し、要求全文を到着順に channel へ送る。
/// 全応答後も listener を保持し、余分な接続が来たら 2 つ目の channel へ通知する。
fn scripted_host(replies: Vec<Vec<u8>>) -> (String, mpsc::Receiver<String>, mpsc::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (tx, rx) = mpsc::channel();
    let (extra_tx, extra_rx) = mpsc::channel();
    thread::spawn(move || {
        for reply in replies {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap_or(0);
                raw.extend_from_slice(&buf[..n]);
                if n == 0 || common::body_complete(&String::from_utf8_lossy(&raw)) {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
            let _ = s.write_all(&reply);
        }
        if listener.accept().is_ok() {
            let _ = extra_tx.send(());
        }
    });
    (addr, rx, extra_rx)
}

/// 要求を 1 件ずつ送り、応答を待ってから次へ進める MCP セッションドライバ。
struct McpSession {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: thread::JoinHandle<String>,
}

impl McpSession {
    fn spawn(host_addr: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
            .env("FANDHE_BROWSER_HOST_ADDR", host_addr)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout");
        let mut stderr_pipe = child.stderr.take().expect("stderr");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr_pipe.read_to_string(&mut s);
            s
        });
        Self {
            child,
            stdin,
            lines,
            stderr,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{msg}").expect("write");
        stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc":"2.0","method":method}));
    }

    /// 要求を送り、同じ id の応答が届くまで待つ。期限超過は kill して panic。
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let start = Instant::now();
        loop {
            let left = DEADLINE.saturating_sub(start.elapsed());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&line)
                        && v["id"] == id
                    {
                        return v["result"].clone();
                    }
                }
                Err(_) => {
                    let _ = self.child.kill();
                    panic!("no response for id={id} within {DEADLINE:?}");
                }
            }
        }
    }

    fn initialize(&mut self) -> Value {
        let r = self.request(
            1,
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"test","version":"0.0.0"}}),
        );
        self.notify("notifications/initialized");
        r
    }

    /// stdin を閉じて終了を待ち、終了コードと stderr を返す。
    fn finish(mut self) -> (Option<i32>, String) {
        drop(self.stdin.take());
        let start = Instant::now();
        let code = loop {
            if let Some(st) = self.child.try_wait().expect("try_wait") {
                break st.code();
            }
            if start.elapsed() > DEADLINE {
                let _ = self.child.kill();
                panic!("mcp binary did not exit within {DEADLINE:?}");
            }
            thread::sleep(Duration::from_millis(20));
        };
        (code, self.stderr.join().expect("stderr"))
    }
}

/// 入力なしで起動し、期限付きで終了を待つ（登録失敗で終了するケース用）。
fn run_once(host_addr: &str, limit: Duration) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
        .env("FANDHE_BROWSER_HOST_ADDR", host_addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdin = child.stdin.take();
    let mut out = child.stdout.take().expect("stdout");
    let mut err = child.stderr.take().expect("stderr");
    let o = thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let e = thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st.code();
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            panic!("mcp binary did not exit within {limit:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(stdin);
    (code, o.join().expect("stdout"), e.join().expect("stderr"))
}

fn tool_names(list: &Value) -> Vec<String> {
    let mut v: Vec<String> = list["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    v.sort();
    v
}

const ENVELOPE: &str = r#"{"url":"https://example.com/","tree":{"role":"document","name":"Example"},"truncated":false}"#;

/// PLUG-2 / PLUG-3 / PLUG-4 / TASK-94.6: 登録 → 一覧 → navigate → snapshot を 1 セッションで通す。
#[test]
fn plug3_plug4_full_flow_register_navigate_snapshot_in_one_session() {
    let (addr, rx, extra) = scripted_host(vec![
        common::REGISTER_OK.as_bytes().to_vec(),
        http("200 OK", r#"{"ok":true,"url":"https://example.com/"}"#),
        http("200 OK", ENVELOPE),
    ]);
    let mut s = McpSession::spawn(&addr);
    s.initialize();
    // 自己申告は MCP セッション確立より先に完了している。
    let reg = rx.try_recv().expect("register request before initialize");
    assert!(
        reg.starts_with("POST /ai/plugins/register HTTP/1.1\r\n"),
        "{reg}"
    );
    let manifest: Value =
        serde_json::from_str(reg.split_once("\r\n\r\n").expect("body").1).expect("json");
    let mut declared: Vec<String> = manifest["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t.as_str().expect("name").to_string())
        .collect();
    declared.sort();

    let list = s.request(2, "tools/list", json!({}));
    assert_eq!(tool_names(&list), vec!["navigate", "snapshot"]);
    assert_eq!(declared, tool_names(&list));

    let nav = s.request(
        3,
        "tools/call",
        json!({"name":"navigate","arguments":{"url":"https://example.com/"}}),
    );
    assert_ne!(nav["isError"], true);
    assert_eq!(nav["structuredContent"]["ok"], true);
    assert_eq!(nav["structuredContent"]["url"], "https://example.com/");

    let snap = s.request(4, "tools/call", json!({"name":"snapshot","arguments":{}}));
    assert_ne!(snap["isError"], true);
    assert!(snap.get("structuredContent").is_none());
    let content = snap["content"].as_array().expect("content");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
    let tree: Value = serde_json::from_str(content[0]["text"].as_str().expect("text")).unwrap();
    assert_eq!(tree["url"], "https://example.com/");
    assert_eq!(tree["tree"]["role"], "document");
    assert_eq!(tree["truncated"], false);

    let (code, _) = s.finish();
    assert_eq!(code, Some(0));

    let nav_req = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("navigate req");
    assert!(
        nav_req.starts_with("POST /ai/navigate HTTP/1.1\r\n"),
        "{nav_req}"
    );
    assert!(
        nav_req.ends_with("{\"url\":\"https://example.com/\"}"),
        "{nav_req}"
    );
    let snap_req = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("snapshot req");
    assert!(
        snap_req.starts_with("GET /ai/snapshot HTTP/1.1\r\n"),
        "{snap_req}"
    );
    assert!(extra.recv_timeout(Duration::from_millis(300)).is_err());
}

/// PLUG-2 / PLUG-3 / TASK-94.6: 登録要求のヘッダと本文がスキーマ契約に適合する。
#[test]
fn plug2_register_request_is_well_formed_and_matches_schema() {
    let (addr, handle) = common::register_only();
    let mut s = McpSession::spawn(&addr);
    s.initialize();
    let (code, _) = s.finish();
    assert_eq!(code, Some(0));
    let raw = handle.join().expect("host thread");

    let (head, body) = raw.split_once("\r\n\r\n").expect("split");
    assert!(
        head.starts_with("POST /ai/plugins/register HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(head.contains(&format!("Host: {addr}\r\n")), "{head}");
    assert!(head.contains("Content-Type: application/json"), "{head}");
    assert!(
        head.contains(&format!("Content-Length: {}", body.len())),
        "{head}"
    );
    assert!(!head.to_ascii_lowercase().contains("\r\norigin:"), "{head}");

    let schema: Value =
        serde_json::from_str(include_str!("../../../docs/design/host-api.schema.json"))
            .expect("schema");
    let def = &schema["$defs"]["PluginManifest"];
    let props = def["properties"].as_object().expect("properties");
    let m: Value = serde_json::from_str(body).expect("manifest");
    let obj = m.as_object().expect("object");
    for k in def["required"].as_array().expect("required") {
        assert!(obj.contains_key(k.as_str().expect("key")), "missing {k}");
    }
    for k in obj.keys() {
        assert!(props.contains_key(k), "unknown key {k}");
    }
    assert!(
        props["transport"]["enum"]
            .as_array()
            .expect("enum")
            .contains(&m["transport"]),
        "{m}"
    );
    for p in m["permissions"].as_array().cloned().unwrap_or_default() {
        assert!(
            props["permissions"]["items"]["enum"]
                .as_array()
                .expect("enum")
                .contains(&p),
            "{p}"
        );
    }
    assert_eq!(m["id"], "fandhe-browser-mcp");
    assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(m["protocolVersion"], "2025-11-25");
}

/// PLUG-2 / PLUG-3 / TASK-94.6: 409 duplicate_plugin_id は警告して継続し、ツールが使える。
#[test]
fn plug2_duplicate_registration_warns_and_session_continues() {
    let (addr, _rx, _extra) = scripted_host(vec![
        http(
            "409 Conflict",
            r#"{"ok":false,"code":"duplicate_plugin_id"}"#,
        ),
        http("200 OK", r#"{"ok":true,"url":"https://example.com/"}"#),
    ]);
    let mut s = McpSession::spawn(&addr);
    s.initialize();
    let nav = s.request(
        2,
        "tools/call",
        json!({"name":"navigate","arguments":{"url":"https://example.com/"}}),
    );
    assert_eq!(nav["structuredContent"]["ok"], true);
    let (code, err) = s.finish();
    assert_eq!(code, Some(0));
    assert!(
        err.contains("host already has this plugin registered"),
        "{err}"
    );
}

/// PLUG-2 / TASK-94.6: 200 でも id が一致しなければ失敗終了する（成功を装わない）。
#[test]
fn plug2_register_id_mismatch_fails_closed() {
    let (addr, _rx, _extra) =
        scripted_host(vec![http("200 OK", r#"{"ok":true,"id":"other-plugin"}"#)]);
    let (code, out, err) = run_once(&addr, DEADLINE);
    assert_eq!(code, Some(1));
    assert_eq!(out, "");
    assert!(err.contains("failed to register with host"), "{err}");
    assert!(err.contains("unexpected host response body"), "{err}");
}

/// PLUG-2 / TASK-94.6: 不正な接続先環境変数（DNS 名）は失敗終了する。
#[test]
fn plug2_invalid_host_addr_env_fails_closed() {
    let (code, out, err) = run_once("localhost:9333", DEADLINE);
    assert_eq!(code, Some(1));
    assert_eq!(out, "");
    assert!(err.contains("invalid host address"), "{err}");
}

/// PLUG-2 / TASK-94.6: ホストが応答しない場合はタイムアウトで失敗終了する。
#[test]
fn plug2_register_timeout_fails_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            // 応答せず接続を保持する（テスト終了通知まで）。
            let _ = done_rx.recv();
        }
    });
    let (code, out, err) = run_once(&addr, Duration::from_secs(15));
    let _ = done_tx.send(());
    assert_eq!(code, Some(1));
    assert_eq!(out, "");
    assert!(err.contains("timed out waiting for host"), "{err}");
}

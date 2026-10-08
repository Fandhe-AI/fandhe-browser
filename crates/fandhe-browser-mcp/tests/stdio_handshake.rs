//! stdio MCP ハンドシェイクの結合テスト（TASK-94.2・PLUG-3）。実バイナリを起動して検証する。

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rmcp::serde_json::{self, Value};

struct Output {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// バイナリを起動し、`input`（None なら即 EOF）を送って期限付きで完了を待つ。
fn run(input: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp binary");
    if let Some(text) = input {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(text.as_bytes()).expect("write stdin");
        drop(stdin);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("mcp binary did not exit within 30s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    Output {
        stdout,
        stderr,
        code: status.code(),
    }
}

fn init_line(version: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"test","version":"0.0.0"}}}}}}"#
    ) + "\n"
}

fn parse(out: &Output) -> Vec<Value> {
    out.stdout
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("stdout line is JSON"))
        .collect()
}

fn find_id(msgs: &[Value], id: i64) -> &Value {
    msgs.iter()
        .find(|m| m["id"] == id)
        .expect("response with id")
}

/// PLUG-3 / TASK-94.2: initialize 要求に応答し ping も処理して正常終了する。
#[test]
fn plug3_stdio_initialize_handshake_succeeds() {
    let input = init_line("2025-06-18")
        + "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"
        + "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
    let out = run(Some(&input));
    let msgs = parse(&out);
    let init = &find_id(&msgs, 1)["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "fandhe-browser-mcp");
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(init["capabilities"], serde_json::json!({}));
    assert_eq!(find_id(&msgs, 2)["result"], serde_json::json!({}));
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
}

/// PLUG-3 / TASK-94.2: initialize 前に stdin が閉じたら失敗終了し stdout は空。
#[test]
fn plug3_stdio_eof_before_initialize_fails() {
    let out = run(None);
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("failed to initialize MCP session"),
        "{}",
        out.stderr
    );
}

/// PLUG-3 / TASK-94.2: 未知のプロトコル版にはサーバー側の最新版で応答する。
#[test]
fn plug3_stdio_unknown_protocol_version_negotiates_latest() {
    let out = run(Some(&init_line("1999-01-01")));
    let msgs = parse(&out);
    assert_eq!(find_id(&msgs, 1)["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
}

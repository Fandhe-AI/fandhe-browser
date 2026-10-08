//! stdio MCP ハンドシェイクの結合テスト（TASK-94.2・PLUG-3・MS-9）。実バイナリを起動して検証する。

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
/// `FANDHE_BROWSER_HOST_ADDR` には接続を試みれば失敗終了する非 loopback の値を渡す。
/// 未実装ツールの自己申告を延期している間（TASK-94.3 / 94.4 完了まで）は接続しないため、
/// 成功終了すること自体がホストへ接続していない証拠になる（REPAIR-3）。
fn run(input: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-mcp"))
        .env("FANDHE_BROWSER_HOST_ADDR", "192.0.2.1:9333")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp binary");
    // 期限は送信開始前から数える。子が stdin を読まないとパイプが満杯になり
    // write_all が止まるため、送信は別スレッドで行い下の期限監視・kill を必ず通す。
    let deadline = Instant::now() + Duration::from_secs(30);
    let writer = input.map(|text| {
        let mut stdin = child.stdin.take().expect("stdin");
        let bytes = text.as_bytes().to_vec();
        std::thread::spawn(move || {
            // 子が上限超過で先に終了した場合の BrokenPipe は想定内のため無視する。
            let _ = stdin.write_all(&bytes);
            drop(stdin);
        })
    });
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
    // 子の終了で stdin が閉じるため、送信スレッドは BrokenPipe で終了する。
    if let Some(w) = writer {
        let _ = w.join();
    }
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

/// PLUG-3 / TASK-94.2: 改行なしで 4 MiB を超える入力は打ち切られ、失敗終了し stdout は空。
#[test]
fn plug3_stdio_oversized_message_terminates_session() {
    let input = "x".repeat(4 * 1024 * 1024 + 1024);
    let out = run(Some(&input));
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr.contains("failed to initialize MCP session"),
        "{}",
        out.stderr
    );
}

/// PLUG-3 / TASK-94.2: 上限以内の長い行（約 1 MiB）でも通常どおり応答する。
#[test]
fn plug3_stdio_large_message_within_limit_is_accepted() {
    let pad = "y".repeat(1024 * 1024);
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-11-25","capabilities":{{}},"clientInfo":{{"name":"test","version":"0.0.0"}},"_meta":{{"pad":"{pad}"}}}}}}"#
    ) + "\n";
    let out = run(Some(&line));
    let msgs = parse(&out);
    assert_eq!(find_id(&msgs, 1)["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
}

/// PLUG-3 / TASK-94.5: 提供ツールが無い間は自己申告を延期し、その旨を stderr に出して継続する。
#[test]
fn plug3_startup_defers_registration_while_no_tools_implemented() {
    let out = run(Some(&init_line("2025-11-25")));
    assert!(
        out.stderr
            .contains("host registration deferred: no tools implemented"),
        "{}",
        out.stderr
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
}

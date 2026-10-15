//! `--stdio` の実バイナリを子プロセスとして起動するブラックボックス結合テスト
//! （`RENDER-3`・`RENDER-4`・`PLUG-1`・`SEC-2`・#993・#231）。
//!
//! フレーム（u32 LE 長さ接頭辞 + UTF-8 JSON）を対話的に読み書きし、hello・`load`（ループバック
//! HTTP サーバー相手。外部ネットワークへ出ない）・未実装メソッドの `unsupported`・`shutdown`・
//! プロトコル違反時の終了コード・stdout の純粋性・ログの秘匿を確かめる。
//! このテストバイナリ自体は Servo をリンクしない。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const LIMIT: Duration = Duration::from_secs(60);

struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Vec<u8>,
    stderr: std::thread::JoinHandle<String>,
}

fn spawn(args: &[&str]) -> Host {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-servo-host"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn servo host");
    let stdin = child.stdin.take();
    let mut err = child.stderr.take().expect("stderr");
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    Host {
        child,
        stdin,
        stdout: Vec::new(),
        stderr,
    }
}

impl Host {
    fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("write");
        stdin.flush().expect("flush");
    }

    fn send(&mut self, json: &str) {
        let mut v = (json.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(json.as_bytes());
        self.send_raw(&v);
    }

    /// 次のフレームを読む（制限時間付き。超過なら子を kill して失敗）。
    fn recv(&mut self) -> String {
        // 子の stdout は借用で別スレッドへ渡せないため、1 フレームずつ都度スレッドで読む。
        let mut out = self.child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut hdr = [0u8; 4];
            let r = out.read_exact(&mut hdr).and_then(|()| {
                let mut body = vec![0u8; u32::from_le_bytes(hdr) as usize];
                out.read_exact(&mut body).map(|()| body)
            });
            let _ = tx.send((out, r));
        });
        match rx.recv_timeout(LIMIT) {
            Ok((out, r)) => {
                self.child.stdout = Some(out);
                let body = r.expect("read frame");
                self.stdout.extend_from_slice(&body);
                String::from_utf8(body).expect("utf8")
            }
            Err(_) => {
                let _ = self.child.kill();
                panic!("no frame within {LIMIT:?}");
            }
        }
    }

    /// stdin を閉じて終了を待ち、(終了コード, 残りの stdout バイト数, stderr) を返す。
    fn finish(mut self) -> (Option<i32>, usize, String) {
        drop(self.stdin.take());
        let mut rest = Vec::new();
        if let Some(mut o) = self.child.stdout.take() {
            let _ = o.read_to_end(&mut rest);
        }
        let status = self.child.wait().expect("wait");
        (
            status.code(),
            rest.len(),
            self.stderr.join().unwrap_or_default(),
        )
    }
}

#[test]
fn hello_then_unsupported_then_shutdown() {
    let mut h = spawn(&["--stdio"]);
    let hello: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(hello["event"], "hello");
    assert_eq!(
        hello["data"]["capabilities"],
        serde_json::json!(["load", "shutdown"])
    );

    h.send(r#"{"v":1,"id":1,"method":"evaluate","params":{"script":"1"}}"#);
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":1,"error":{"kind":"unsupported","message":"Method is not supported."}}"#
    );
    h.send(r#"{"v":1,"id":2,"method":"load","params":{"url":"file:///etc/passwd"}}"#);
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":2,"error":{"kind":"invalid_params","message":"Invalid parameters."}}"#
    );
    // SEC-2: UA 等の識別面を受け取るパラメータは持たない。
    h.send(
        r#"{"v":1,"id":3,"method":"load","params":{"url":"http://127.0.0.1:1/","user_agent":"x"}}"#,
    );
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":3,"error":{"kind":"invalid_params","message":"Invalid parameters."}}"#
    );
    // 未実装メソッドは成功を装わない（REPAIR-3）。
    h.send(r#"{"v":1,"id":4,"method":"screenshot"}"#);
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":4,"error":{"kind":"unsupported","message":"Method is not supported."}}"#
    );
    h.send(r#"{"v":1,"id":5,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":5,"result":{}}"#);

    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
}

/// ループバックの小さな HTTP サーバー。`hang` なら接続を受けても応答しない。
/// 戻り値は (ポート, スレッド, 停止通知)。
fn serve_http(hang: bool) -> (u16, std::thread::JoinHandle<()>, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let mut held = Vec::new();
        while stop_rx.try_recv().is_err() {
            match listener.accept() {
                Ok((mut conn, _)) => {
                    let _ = conn.set_nonblocking(false);
                    if hang {
                        held.push(conn);
                        continue;
                    }
                    let mut buf = [0u8; 4096];
                    let _ = conn.read(&mut buf);
                    let body = "<html><head><title>fixture</title></head><body><h1>hello</h1></body></html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = conn.write_all(resp.as_bytes());
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    });
    (port, handle, stop_tx)
}

fn load_req(id: u64, url: &str, timeout_ms: u64) -> String {
    format!(
        r#"{{"v":1,"id":{id},"method":"load","params":{{"url":"{url}","timeout_ms":{timeout_ms}}}}}"#
    )
}

/// RENDER-3・RENDER-4: post-load フレームまでタイムアウトせずに完了する。失敗・タイムアウトの後も
/// 同一プロセスで再度 load でき、stdout はフレームのみ、ログに URL・クエリ値が出ない。
#[test]
fn load_succeeds_fails_and_recovers_in_one_process() {
    let (port, srv, stop) = serve_http(false);
    let (hang_port, hang_srv, hang_stop) = serve_http(true);
    // bind して閉じたポート（接続拒否になる）。
    let closed_port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    let ok_url = format!("http://127.0.0.1:{port}/page?token=abc123");
    h.send(&load_req(1, &ok_url, 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["id"], 1, "{r}");
    assert_eq!(r["result"]["final_url"], ok_url, "{r}");
    // Servo 0.6.0 は HTTP ステータスを返せないため、キーを出さない（REPAIR-3）。
    assert!(r["result"].get("status").is_none(), "{r}");

    // 接続拒否は load_failed（エラーページの判定経路）。
    h.send(&load_req(
        2,
        &format!("http://127.0.0.1:{closed_port}/"),
        20_000,
    ));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":2,"error":{"kind":"load_failed","message":"Navigation failed."}}"#
    );

    // 応答しないサーバーは timeout（上限 + 余裕以内に返る）。
    let started = std::time::Instant::now();
    h.send(&load_req(
        3,
        &format!("http://127.0.0.1:{hang_port}/"),
        1_000,
    ));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":3,"error":{"kind":"timeout","message":"Operation timed out."}}"#
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );

    // 失敗後も同じプロセスで正常に load できる（Engine を再利用）。
    h.send(&load_req(4, &ok_url, 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["result"]["final_url"], ok_url, "{r}");

    h.send(r#"{"v":1,"id":5,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":5,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    assert!(
        !stderr.contains("abc123") && !stderr.contains("127.0.0.1"),
        "{stderr}"
    );

    let _ = stop.send(());
    let _ = hang_stop.send(());
    let _ = srv.join();
    let _ = hang_srv.join();
}

#[test]
fn protocol_violations_exit_with_code_2() {
    for bad in [
        0u32.to_le_bytes().to_vec(),
        65_537u32.to_le_bytes().to_vec(),
        {
            let mut v = 7u32.to_le_bytes().to_vec();
            v.extend_from_slice(b"{\"v\":1,");
            v
        },
    ] {
        let mut h = spawn(&["--stdio"]);
        let _hello = h.recv();
        h.send_raw(&bad);
        let (code, rest, stderr) = h.finish();
        assert_eq!(code, Some(2), "stderr: {stderr}");
        assert_eq!(rest, 0, "no response expected");
    }
}

#[test]
fn closing_stdin_at_frame_boundary_exits_zero() {
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let (code, _, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn stdio_rejects_extra_arguments() {
    let h = spawn(&["--stdio", "extra"]);
    let (code, _, stderr) = h.finish();
    assert_eq!(code, Some(1));
    assert!(stderr.contains("supports only --version, --selftest and --stdio"));
}

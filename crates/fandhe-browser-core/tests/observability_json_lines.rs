//! `JsonLinesRecorder` / `StderrRecorder` の結合テスト（`REPAIR-9`・`TASK-10.3`・Issue #221）。
//!
//! 公開 API の recorder を `FetchOptions` / `ParseOptions` へ注入し、実操作が
//! 「1 レコード 1 行」の JSON Lines を出力することを検証する。出力先は共有バッファで、
//! 外部ネットワークには依存せずループバックだけを使う。

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fandhe_browser_core::{
    FetchOptions, Fetcher, JsonLinesRecorder, OperationRecorder, ParseOptions, StderrRecorder,
    parse_document,
};

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// writer スレッドが `n` 行書き終えるまで待ち、出力全体を返す（最大 10 秒）。
fn wait_lines(buf: &SharedBuf, n: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = String::from_utf8(buf.0.lock().unwrap_or_else(PoisonError::into_inner).clone())
            .expect("utf8");
        if text.matches('\n').count() >= n {
            return text;
        }
        assert!(Instant::now() < deadline, "timed out; got {text:?}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// `REPAIR-9`: parse の成否が 1 操作 1 行の JSON Lines として出力される。
#[test]
fn repair_9_json_lines_parse_success_and_failure() {
    let buf = SharedBuf::default();
    let rec: Arc<dyn OperationRecorder> = Arc::new(JsonLinesRecorder::new(buf.clone()));
    let ok = ParseOptions::default().with_recorder(rec.clone());
    parse_document("<p>hi</p>", &ok).expect("parse");
    let ng = ParseOptions::default()
        .with_max_nodes(0)
        .with_recorder(rec.clone());
    assert!(parse_document("<p>hi</p>", &ng).is_err());

    let out = wait_lines(&buf, 2);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected output: {out}");
    assert!(lines[0].contains("\"operation\":\"parse\""), "{}", lines[0]);
    assert!(lines[0].contains("\"outcome\":\"success\""), "{}", lines[0]);
    assert!(lines[1].contains("\"operation\":\"parse\""), "{}", lines[1]);
    assert!(lines[1].contains("\"outcome\":\"failure\""), "{}", lines[1]);
    for l in &lines {
        assert!(l.starts_with('{') && l.ends_with('}'), "{l}");
    }
}

/// `REPAIR-9`: fetch の成否が JSON Lines で出力され、URL は含まれない。
#[tokio::test]
async fn repair_9_json_lines_fetch_success_and_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let mut data = Vec::new();
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 8192 {
                        break;
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });

    let buf = SharedBuf::default();
    let rec: Arc<dyn OperationRecorder> = Arc::new(JsonLinesRecorder::new(buf.clone()));
    let fetcher = Fetcher::new(
        FetchOptions::new()
            .with_allow_private_network_access(true)
            .with_recorder(rec),
    )
    .expect("Fetcher::new");
    fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("200");
    assert!(fetcher.get("file:///etc/hosts").await.is_err());

    let out = wait_lines(&buf, 2);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "unexpected output: {out}");
    assert!(lines[0].contains("\"operation\":\"fetch\""), "{}", lines[0]);
    assert!(lines[0].contains("\"outcome\":\"success\""), "{}", lines[0]);
    assert!(lines[1].contains("\"outcome\":\"failure\""), "{}", lines[1]);
    assert!(
        !out.contains("/etc/hosts") && !out.contains("127.0.0.1"),
        "{out}"
    );
}

/// `REPAIR-9`: `StderrRecorder` を注入しても操作は成功し、panic しない。
#[test]
fn repair_9_stderr_recorder_injects_without_affecting_operation() {
    let rec: Arc<dyn OperationRecorder> = Arc::new(StderrRecorder::stderr());
    let options = ParseOptions::default().with_recorder(rec);
    assert!(parse_document("<p>hi</p>", &options).is_ok());
}

//! 結合テスト用の偽ホスト（TASK-94.5・PLUG-2）。1 接続だけ受けて要求を記録し、固定応答を返す。

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct FakeHost {
    addr: String,
    rx: Receiver<String>,
    handle: JoinHandle<()>,
}

impl FakeHost {
    /// loopback の空きポートで待ち受け、1 接続に `status` / `body` を返す。
    pub fn spawn(status: u16, body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        listener.set_nonblocking(true).expect("nonblocking");
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).expect("blocking");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stream.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if rest.len() >= len {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        Self { addr, rx, handle }
    }

    pub fn addr(&self) -> String {
        self.addr.clone()
    }

    /// 受信した生の要求を返す（接続が無ければ None）。
    pub fn received(&self) -> Option<String> {
        self.rx.recv_timeout(Duration::from_secs(10)).ok()
    }

    pub fn finish(self) {
        // 接続されなかった場合は accept 期限で終わるので待たずに切り離す。
        drop(self.handle);
    }
}

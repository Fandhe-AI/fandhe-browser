//! 結合テスト共通の偽ホスト補助（TASK-94.5・PLUG-2）。
//!
//! バイナリは MCP セッション開始前にホストの `POST /ai/plugins/register` へ自己申告する
//! ため、結合テストの偽ホストは先にその 1 リクエストへ応答しなければならない。
//! 各テストファイルが `mod common;` で取り込む。取り込み先によって未使用になる関数にだけ
//! 個別に `#[allow(dead_code)]` を付ける（モジュール全体の抑止はしない）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

/// 自己申告に対するホストの正常応答（`host-api.schema.json` の登録成功形）。
pub const REGISTER_OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 37\r\nConnection: close\r\n\r\n{\"ok\":true,\"id\":\"fandhe-browser-mcp\"}";

/// `listener` で 1 接続を受け、自己申告の POST を読み切って成功応答を返し、受信全文を返す。
pub fn serve_register(listener: &TcpListener) -> String {
    let Ok((mut s, _)) = listener.accept() else {
        return String::new();
    };
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = s.read(&mut buf).unwrap_or(0);
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw).to_string();
        if n == 0 || body_complete(&text) {
            let _ = s.write_all(REGISTER_OK.as_bytes());
            return text;
        }
    }
}

/// ヘッダ終端まで読み、`Content-Length` 分の本文が揃っているか。
pub fn body_complete(text: &str) -> bool {
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

/// 自己申告だけを受けて listener を閉じる偽ホスト。以後のツール呼び出しは接続拒否になる。
/// stdio_handshake.rs からは使われないため、この関数に限り dead_code を許容する。
#[allow(dead_code)]
pub fn register_then_close() -> String {
    let (addr, _handle) = register_only();
    addr
}

/// 自己申告だけを受ける偽ホスト（以後の接続は受けない）。受信全文を返すハンドルも返す。
pub fn register_only() -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let handle = thread::spawn(move || serve_register(&listener));
    (addr, handle)
}

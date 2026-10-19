//! 結合テスト共通の偽ホスト補助（TASK-94.5・PLUG-2）。
//!
//! バイナリは MCP セッション開始前にホストの `POST /ai/plugins/register` へ自己申告する
//! ため、結合テストの偽ホストは先にその 1 リクエストへ応答しなければならない。
//! 各テストファイルが `mod common;` で取り込む。取り込み先によって未使用になる関数にだけ
//! 個別に `#[allow(dead_code)]` を付ける（モジュール全体の抑止はしない）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
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
    let listener = bind_non_ephemeral_loopback();
    let addr = listener.local_addr().expect("addr").to_string();
    thread::spawn(move || serve_register_with_owned(listener));
    addr
}

/// listener を所有し、成功応答の直前に閉じて（応答後に閉じると、バイナリの次の接続が
/// 閉じる前に accept キューへ入り Io エラーに分類される競合が起きる）自己申告を 1 回だけ処理する。
fn serve_register_with_owned(listener: TcpListener) -> String {
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
            drop(listener);
            let _ = s.write_all(REGISTER_OK.as_bytes());
            return text;
        }
    }
}

/// 自己申告だけを受ける偽ホスト（以後の接続は受けない）。受信全文を返すハンドルも返す。
pub fn register_only() -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let handle = thread::spawn(move || serve_register(&listener));
    (addr, handle)
}

/// OS が `:0` bind や送信元ポートに割り当てるエフェメラル範囲 `(下限, 上限)`（両端含む）。
/// Linux は sysctl を読み、読めない・解釈できない場合と他 OS は既定値（macOS・Windows は 49152 から）を使う。
#[allow(dead_code)]
pub fn ephemeral_port_range() -> (u32, u32) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range") {
            let mut it = text
                .split_whitespace()
                .filter_map(|v| v.parse::<u32>().ok());
            if let (Some(lo), Some(hi)) = (it.next(), it.next())
                && lo <= hi
                && hi <= 65535
            {
                return (lo, hi);
            }
        }
        (32768, 60999)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (49152, 65535)
    }
}

/// エフェメラル範囲の外から loopback の空きポートを 1 つ bind して返す（PLUG-3・PLUG-4・Issue #843）。
///
/// `:0` で取ったポートを閉じて「接続拒否」を再現する方式では、閉じた直後に同じプロセス内の
/// 別テストの `:0` bind へ同じポートが再割り当てされ、接続が成立して `HostError::Io` になる
/// ことがある。`:0` はエフェメラル範囲からしか割り当てないため、範囲外のポートを使えば
/// 再割り当てが起きない。候補が尽きた場合はテストを黙って通さず panic する（fail-closed）。
#[allow(dead_code)]
pub fn bind_non_ephemeral_loopback() -> TcpListener {
    static CURSOR: AtomicU32 = AtomicU32::new(0);
    let (lo, hi) = ephemeral_port_range();
    // 下限側（20000 以上）を優先し、空きが無ければ上限より上を使う。
    let (start, end) = if lo > 20000 {
        (20000u32, lo - 1)
    } else if hi < 65535 {
        (hi + 1, 65535)
    } else {
        panic!("no non-ephemeral port range available (ephemeral {lo}-{hi})");
    };
    let span = end - start + 1;
    // プロセス間の衝突を減らすため、初回は PID 由来のオフセットから始める。
    let _ = CURSOR.compare_exchange(
        0,
        std::process::id() % span + 1,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    for _ in 0..span.min(1000) {
        let n = CURSOR.fetch_add(1, Ordering::SeqCst);
        let port = start + n % span;
        match TcpListener::bind(("127.0.0.1", port as u16)) {
            Ok(l) => return l,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AddrInUse
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::AddrNotAvailable
                ) => {}
            Err(e) => panic!("bind 127.0.0.1:{port}: {e}"),
        }
    }
    panic!("no free non-ephemeral port in {start}-{end}");
}

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

/// listener を所有し、成功応答の直前に閉じて拒否を確認してから応答し（応答後に閉じると、バイナリの次の接続が
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
            close_and_wait_refused(listener);
            let _ = s.write_all(REGISTER_OK.as_bytes());
            return text;
        }
    }
}

/// 閉じたはずのポートに対する 1 回の探査結果（PLUG-3・PLUG-4・Issue #1356）。
/// 真偽値にせず、将来の判定追加（部分的に成立等）に備えて enum にする（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ProbeOutcome {
    /// 接続が拒否された、または確立しなかった（listen していない）。
    Closed,
    /// 接続が確立し、最初の書き込みも拒否されなかった（listen ソケットが生きている）。
    Listening,
}

/// `addr`（エフェメラル範囲の外の loopback）が今 listen しているかを 1 回探査する。
///
/// 「拒否」の判定は `src/host.rs` の分類と同じ基準にする。macOS では閉じたポートへの
/// `connect_timeout` が `Ok` を返し、拒否が `take_error` や最初の書き込みで初めて現れる
/// （Issue #1187）。Windows では SYN の再送で `TimedOut` になり得るが、生きている loopback の
/// listener ならカーネルが即座にハンドシェイクを完了するため、期限内に確立しなければ
/// listen していないとみなす。範囲内のポートは同一プロセスの別テストの接続と衝突するため panic する。
#[allow(dead_code)]
pub fn probe_listening(addr: std::net::SocketAddr) -> ProbeOutcome {
    let (lo, hi) = ephemeral_port_range();
    assert!(
        !(lo..=hi).contains(&u32::from(addr.port())),
        "probe target {addr} is inside the ephemeral range {lo}-{hi}"
    );
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
    else {
        return ProbeOutcome::Closed;
    };
    if !matches!(stream.take_error(), Ok(None)) {
        return ProbeOutcome::Closed;
    }
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(500)));
    match stream.write_all(b"\r\n") {
        Err(_) => ProbeOutcome::Closed,
        Ok(()) => ProbeOutcome::Listening,
    }
}

/// `close_and_wait_refused` が拒否を確認するまで待つ上限。CI の負荷で fork 済み・exec 前の
/// 子プロセスが fd を保持する窓が ms 単位に広がっても収まる余裕を見た値。
const REFUSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// `listener` を閉じ、そのポートが実際に接続を拒否する状態になるまで待って、ポートのアドレスを返す。
///
/// 並列テストの `Command::spawn` は fd テーブルを複製して fork し exec まで子が保持するため、
/// 親が `drop` してもカーネル上の listen ソケットは子の exec まで生き残る。その間に張られた
/// 接続は確立して `HostError::Io` に分類され、接続拒否のテストが不定期に落ちる（Issue #1356。
/// #843・#1187 の再発）。拒否を確認してから先へ進めば、以後の fork は閉じた fd を複製しない。
/// 期限を超えたら黙って通さず panic する（fail-closed）。
#[allow(dead_code)]
pub fn close_and_wait_refused(listener: TcpListener) -> std::net::SocketAddr {
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    let start = std::time::Instant::now();
    let mut attempts = 0u32;
    while probe_listening(addr) == ProbeOutcome::Listening {
        attempts += 1;
        if start.elapsed() >= REFUSE_DEADLINE {
            panic!(
                "port {addr} still accepts connections after {attempts} probes in {:?}; holders: {}",
                start.elapsed(),
                describe_listen_holders(addr.port())
            );
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    if attempts > 0 {
        eprintln!(
            "closed port {addr} stayed listening for {attempts} probes ({:?})",
            start.elapsed()
        );
    }
    addr
}

/// `port` を LISTEN している pid と cmdline を `/proc` から調べた文字列（診断用・Linux のみ）。
#[cfg(target_os = "linux")]
fn describe_listen_holders(port: u16) -> String {
    let hex = format!(":{port:04X} ");
    let mut inodes = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if let (Some(local), Some(st), Some(inode)) = (f.get(1), f.get(3), f.get(9))
                && *st == "0A"
                && format!("{local} ").contains(&hex)
            {
                inodes.push(inode.to_string());
            }
        }
    }
    let mut out = format!("listen inodes {inodes:?}");
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for p in procs.flatten() {
        let name = p.file_name().to_string_lossy().to_string();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(p.path().join("fd")) else {
            continue;
        };
        let held = fds.flatten().any(|fd| {
            std::fs::read_link(fd.path())
                .map(|t| {
                    let t = t.to_string_lossy().to_string();
                    inodes.iter().any(|i| t == format!("socket:[{i}]"))
                })
                .unwrap_or(false)
        });
        if held {
            let cmd = std::fs::read(p.path().join("cmdline")).unwrap_or_default();
            let cmd: String = String::from_utf8_lossy(&cmd).replace('\0', " ");
            out.push_str(&format!(
                "; pid {name} cmdline {}",
                cmd.chars().take(200).collect::<String>()
            ));
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn describe_listen_holders(_port: u16) -> String {
    "unavailable on this OS".to_string()
}

/// wildcard（`0.0.0.0`・`[::]`）で `port` を bind できるか。
///
/// std は unix で `SO_REUSEADDR` を付けて bind するため、macOS / BSD では別プロセスが wildcard で
/// listen 中でも `127.0.0.1:port` の bind が成功する。閉じた後の接続はその wildcard 側へ届き、
/// 「接続拒否」にならない（Issue #1187）。bind できないポートは影があるとみなして避ける。
/// probe は即 drop する（Linux は listen 中の個別 bind があると wildcard bind を拒むため、
/// loopback の bind より前に呼ぶ）。IPv6 が使えない環境は判定不能として通す。
fn wildcard_is_free(port: u16) -> bool {
    use std::io::ErrorKind::{AddrInUse, PermissionDenied};
    match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => drop(l),
        Err(e) if matches!(e.kind(), AddrInUse | PermissionDenied) => return false,
        Err(e) => panic!("probe bind 0.0.0.0:{port}: {e}"),
    }
    match TcpListener::bind(("::", port)) {
        Ok(l) => drop(l),
        Err(e) if matches!(e.kind(), AddrInUse | PermissionDenied) => return false,
        Err(_) => {}
    }
    true
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
/// 再割り当てが起きない。また macOS / BSD では wildcard listener の影がある
/// ポートを避けるため `wildcard_is_free` で事前確認する（Issue #1187）。
/// 候補が尽きた場合はテストを黙って通さず panic する（fail-closed）。
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
        if !wildcard_is_free(port as u16) {
            continue;
        }
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

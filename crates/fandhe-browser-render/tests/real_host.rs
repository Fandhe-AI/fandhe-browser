//! 本物の Servo ホストバイナリと `HostRenderer`（core `Renderer` 実装）を繋ぐ結合テスト
//! （RENDER-3・RENDER-4・SEC-2。TASK-38.8b・#998。設計 `docs/design/servo-host-design.md` §7・§8.1）。
//!
//! 偽ホストを使う `basic_api.rs`（#237）と対になり、ループバックの固定 HTML を配信して
//! load・screenshot・visibility・bounding_box、load タイムアウト、不許可 URL の拒否を実 Servo で確認する。
//! ホストバイナリが要るため `Cargo.toml` で `test = false`（既定の `cargo test` から外す）とし、
//! `servo-host-ubuntu` が次のコマンドで明示実行する。
//! `FANDHE_BROWSER_RENDER_HOST=<絶対パス> cargo test -p fandhe-browser-render --test real_host -- --test-threads=1`
//!
//! 環境変数が未設定・不正なら skip せず panic する（fail-closed。通ったように見せない）。
//! `status`: core の `PageLoad` は `final_url` しか持たず、Servo 0.7.0 のホストも返さないため、
//! `HostProcess` 層で `status() == None` を確認する（作り出さない。REPAIR-3）。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fandhe_browser_core::render::{
    ElementRef, ImageFormat, PageRef, RenderError, Renderer, ScreenshotOptions,
};
use fandhe_browser_render::host_process::{
    HOST_BINARY_ENV, HostProcess, HostProcessConfig, HostProcessError, HostTimeouts,
    resolve_host_binary,
};
use fandhe_browser_render::host_renderer::{HostRenderer, HostRendererConfig};
use fandhe_browser_render::protocol::{HostErrorKind, LoadParams};

/// スクリーンショット用（上半分が赤・下半分が青）。
const SHOT_BODY: &str = "<html><head><title>fixture</title></head><body style=\"margin:0\"><div style=\"height:400px;background:#f00\"></div><div style=\"height:400px;background:#00f\"></div></body></html>";

/// 可視性判定用。
const VIS_BODY: &str = "<html><head><title>vis</title></head><body style=\"margin:0\">\
<div id=\"shown\" style=\"width:100px;height:100px\">a</div>\
<div id=\"none\" style=\"display:none\">b</div>\
<div id=\"vh\" style=\"visibility:hidden;width:50px;height:50px\">c</div>\
<div id=\"off\" style=\"position:absolute;top:-1000px;width:50px;height:50px\">d</div>\
<div data-k=\"x'y\" style=\"width:10px;height:10px\">e</div>\
</body></html>";

/// 境界ボックス用（全要素が絶対配置で期待矩形が既知）。
const BOX_BODY: &str = "<html><head><title>box</title></head><body style=\"margin:0\">\
<div id=\"abs\" style=\"position:absolute;left:10px;top:20px;width:30px;height:40px\"></div>\
<div id=\"frac\" style=\"position:absolute;left:5.5px;top:7.25px;width:12.5px;height:3.75px\"></div>\
<div id=\"off\" style=\"position:absolute;left:0;top:-1000px;width:50px;height:50px\"></div>\
<div id=\"none\" style=\"display:none\"></div>\
</body></html>";

fn host_config(timeouts: HostTimeouts) -> HostProcessConfig {
    let env = std::env::var_os(HOST_BINARY_ENV)
        .unwrap_or_else(|| panic!("{HOST_BINARY_ENV} must be set to the servo host binary"));
    // exe_dir は渡さない（隣接ファイルを誤って拾わない）。
    let bin = resolve_host_binary(None, Some(env.as_os_str()), None)
        .unwrap_or_else(|e| panic!("invalid host binary: {e}"));
    HostProcessConfig::new(bin).with_timeouts(timeouts)
}

fn renderer(timeouts: HostTimeouts, allow_private: bool, load_ms: Option<u32>) -> HostRenderer {
    HostRenderer::new(
        HostRendererConfig::new(host_config(timeouts))
            .with_allow_private_network_access(allow_private)
            .with_load_timeout_ms(load_ms),
    )
}

fn sel(s: &str) -> ElementRef {
    ElementRef::Selector(s.to_owned())
}

fn variant(e: &RenderError) -> &'static str {
    match e {
        RenderError::RenderingDisabled => "RenderingDisabled",
        RenderError::ElementNotFound => "ElementNotFound",
        RenderError::Timeout => "Timeout",
        RenderError::HostUnavailable => "HostUnavailable",
        RenderError::ProtocolMismatch => "ProtocolMismatch",
        RenderError::PageNotLoaded => "PageNotLoaded",
        RenderError::Internal(_) => "Internal",
        _ => "Other",
    }
}

/// ループバックの小さな HTTP サーバー。パスで本文を出し分け、`hang` なら応答しない。
struct Server {
    port: u16,
    stop: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn start(hang: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let (stop, stop_rx) = mpsc::channel::<()>();
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
                        // 無送信の接続で read が無期限に塞がり停止通知を確認できなくなるのを防ぐ
                        let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0u8; 4096];
                        let n = conn.read(&mut buf).unwrap_or(0);
                        let head = String::from_utf8_lossy(buf.get(..n).unwrap_or(&[]));
                        let path = head.split_whitespace().nth(1).unwrap_or("");
                        let (status, body) = match path {
                            "/shot" => ("200 OK", SHOT_BODY),
                            "/vis" => ("200 OK", VIS_BODY),
                            "/box" => ("200 OK", BOX_BODY),
                            _ => ("404 Not Found", "<html><body>nf</body></html>"),
                        };
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = conn.write_all(resp.as_bytes());
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 接続拒否になるループバックのポート番号を返す（`RENDER-3`・`RENDER-4`・`REPAIR-5`・Issue #1153）。
/// `:0` で取って閉じる方式は再割り当てや自己接続で接続が成立しうるため、
/// エフェメラル範囲の外のポートを使う。
fn closed_port() -> u16 {
    let l = bind_non_ephemeral_loopback();
    l.local_addr().expect("addr").port()
}

/// OS が `:0` bind や送信元ポートに割り当てるエフェメラル範囲 `(下限, 上限)`（両端含む）。
/// Linux は sysctl を読み、読めない・解釈できない場合と他 OS は既定値（macOS・Windows は 49152 から）を使う。
fn ephemeral_port_range() -> (u32, u32) {
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

/// 正本は `crates/fandhe-browser-mcp/tests/common/mod.rs`（#1159）。servo-host（別 workspace）と
/// render の real_host.rs にコピーを置くため、変更時は 3 つを同期する。
///
/// エフェメラル範囲の外から loopback の空きポートを 1 つ bind して返す（Issue #843・#1153）。
///
/// `:0` で取ったポートを閉じて「接続拒否」を再現する方式では、閉じた直後に同じプロセス内の
/// 別テストの `:0` bind へ同じポートが再割り当てされ、接続が成立して `HostError::Io` になる
/// ことがある。`:0` はエフェメラル範囲からしか割り当てないため、範囲外のポートを使えば
/// 再割り当てが起きない。候補が尽きた場合はテストを黙って通さず panic する（fail-closed）。
fn bind_non_ephemeral_loopback() -> TcpListener {
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

fn be32(data: &[u8], at: usize) -> u32 {
    let b: [u8; 4] = data
        .get(at..at + 4)
        .expect("png header")
        .try_into()
        .expect("4 bytes");
    u32::from_be_bytes(b)
}

/// テスト用の最小 DEFLATE 伸長（RFC 1951。puff 方式）。依存を増やさず PNG の画素を検証するために置く。
struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    bit: u32,
}

impl Bits<'_> {
    fn one(&mut self) -> u32 {
        let byte = *self.d.get(self.pos).expect("deflate eof");
        let v = (u32::from(byte) >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        v
    }
    fn bits(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |acc, i| acc | (self.one() << i))
    }
}

/// 正準 Huffman 表（長さ別の個数とシンボル列）。
struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Self {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[usize::from(l)] += 1;
        }
        let mut offs = [0u16; 16];
        for i in 1..15 {
            offs[i + 1] = offs[i] + count[i];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[usize::from(offs[usize::from(l)])] = u16::try_from(s).expect("sym");
                offs[usize::from(l)] += 1;
            }
        }
        Self { count, symbol }
    }
    fn decode(&self, b: &mut Bits) -> usize {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= i32::try_from(b.one()).expect("bit");
            let c = i32::from(self.count[len]);
            if code - c < first {
                return usize::from(
                    self.symbol[usize::try_from(index + (code - first)).expect("idx")],
                );
            }
            index += c;
            first = (first + c) << 1;
            code <<= 1;
        }
        panic!("bad huffman code");
    }
}

fn inflate(data: &[u8]) -> Vec<u8> {
    const LBASE: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEXT: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DBASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DEXT: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    let mut b = Bits {
        d: data,
        pos: 0,
        bit: 0,
    };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = b.one();
        match b.bits(2) {
            0 => {
                if b.bit != 0 {
                    b.bit = 0;
                    b.pos += 1;
                }
                let len = usize::from(data[b.pos]) | (usize::from(data[b.pos + 1]) << 8);
                out.extend_from_slice(&data[b.pos + 4..b.pos + 4 + len]);
                b.pos += 4 + len;
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Huff::new(&l), Huff::new(&[5u8; 30]))
                } else {
                    let nlen = usize::try_from(b.bits(5)).expect("n") + 257;
                    let ndist = usize::try_from(b.bits(5)).expect("n") + 1;
                    let ncode = usize::try_from(b.bits(4)).expect("n") + 4;
                    const ORDER: [usize; 19] = [
                        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                    ];
                    let mut cl = [0u8; 19];
                    for &o in ORDER.iter().take(ncode) {
                        cl[o] = u8::try_from(b.bits(3)).expect("len");
                    }
                    let clh = Huff::new(&cl);
                    let mut lens = vec![0u8; nlen + ndist];
                    let mut i = 0;
                    while i < nlen + ndist {
                        let sym = clh.decode(&mut b);
                        if sym < 16 {
                            lens[i] = u8::try_from(sym).expect("len");
                            i += 1;
                        } else {
                            let (v, rep) = match sym {
                                16 => (lens[i - 1], 3 + b.bits(2)),
                                17 => (0, 3 + b.bits(3)),
                                _ => (0, 11 + b.bits(7)),
                            };
                            for _ in 0..rep {
                                lens[i] = v;
                                i += 1;
                            }
                        }
                    }
                    (Huff::new(&lens[..nlen]), Huff::new(&lens[nlen..]))
                };
                loop {
                    let sym = lit.decode(&mut b);
                    if sym < 256 {
                        out.push(u8::try_from(sym).expect("lit"));
                    } else if sym == 256 {
                        break;
                    } else {
                        let s = sym - 257;
                        let len = usize::from(LBASE[s])
                            + usize::try_from(b.bits(u32::from(LEXT[s]))).expect("e");
                        let ds = dist.decode(&mut b);
                        let d = usize::from(DBASE[ds])
                            + usize::try_from(b.bits(u32::from(DEXT[ds]))).expect("e");
                        for _ in 0..len {
                            out.push(out[out.len() - d]);
                        }
                    }
                }
            }
            _ => panic!("bad deflate block"),
        }
        if last == 1 {
            return out;
        }
    }
}

/// 8bit RGBA・非インターレースの PNG を復号して (x, y) の画素を返す。IDAT を連結して zlib を剥がし、
/// 行フィルタ（None/Sub/Up/Average/Paeth）を戻す。
fn png_pixel(png: &[u8], x: usize, y: usize) -> [u8; 4] {
    assert_eq!(
        png.get(24..29),
        Some(&[8u8, 6, 0, 0, 0][..]),
        "RGBA8 non-interlaced"
    );
    let (w, h) = (be32(png, 16) as usize, be32(png, 20) as usize);
    let mut idat = Vec::new();
    let mut at = 8;
    while at + 8 <= png.len() {
        let len = be32(png, at) as usize;
        if png.get(at + 4..at + 8) == Some(&b"IDAT"[..]) {
            idat.extend_from_slice(&png[at + 8..at + 8 + len]);
        }
        at += 12 + len;
    }
    let raw = inflate(&idat[2..]);
    let stride = w * 4;
    assert_eq!(raw.len(), (stride + 1) * h);
    let mut img = vec![0u8; stride * h];
    for r in 0..h {
        let f = raw[r * (stride + 1)];
        for i in 0..stride {
            let cur = raw[r * (stride + 1) + 1 + i];
            let a = if i >= 4 { img[r * stride + i - 4] } else { 0 };
            let b = if r > 0 { img[(r - 1) * stride + i] } else { 0 };
            let c = if r > 0 && i >= 4 {
                img[(r - 1) * stride + i - 4]
            } else {
                0
            };
            let pred = match f {
                0 => 0,
                1 => a,
                2 => b,
                3 => u8::try_from((u16::from(a) + u16::from(b)) / 2).expect("avg"),
                4 => {
                    let p = i32::from(a) + i32::from(b) - i32::from(c);
                    let (pa, pb, pc) = (
                        (p - i32::from(a)).abs(),
                        (p - i32::from(b)).abs(),
                        (p - i32::from(c)).abs(),
                    );
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => panic!("bad filter"),
            };
            img[r * stride + i] = cur.wrapping_add(pred);
        }
    }
    let o = y * stride + x * 4;
    [img[o], img[o + 1], img[o + 2], img[o + 3]]
}

/// RENDER-3: load・screenshot・visibility・bounding_box が実ホストで具体値どおりに返る。
#[test]
fn render3_real_host_roundtrip() {
    let srv = Server::start(false);
    let concrete = renderer(HostTimeouts::default(), true, Some(20_000));
    let r: &dyn Renderer = &concrete;

    let shot_url = srv.url("/shot");
    let loaded = r.load(&PageRef::new(shot_url.as_str())).expect("load");
    assert_eq!(loaded.final_url(), shot_url);

    let shot = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect("screenshot");
    assert_eq!(shot.format, ImageFormat::Png);
    assert_eq!((shot.width, shot.height), (1280, 800));
    assert_eq!(
        shot.data.get(..8),
        Some(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..])
    );
    assert_eq!(shot.data.get(12..16), Some(&b"IHDR"[..]));
    assert_eq!((be32(&shot.data, 16), be32(&shot.data, 20)), (1280, 800));
    // 描画内容: 上部は赤（#f00）、下部は青（#00f）。白紙や前ページの画像を返す回帰を検出する。
    // ソフトウェアレンダラは厳密な色値を保証しないため、ホスト側テストと同じ許容帯で判定する。
    let top = png_pixel(&shot.data, 640, 100);
    let bottom = png_pixel(&shot.data, 640, 700);
    assert!(top[0] > 200 && top[1] < 60 && top[2] < 60, "top {top:?}");
    assert!(
        bottom[2] > 200 && bottom[0] < 60 && bottom[1] < 60,
        "bottom {bottom:?}"
    );

    // 未対応の full_page は成功を装わない。
    let e = r
        .capture_screenshot(&ScreenshotOptions::new().with_full_page(true))
        .expect_err("full_page");
    assert_eq!(variant(&e), "Internal");

    r.load(&PageRef::new(srv.url("/vis"))).expect("load vis");
    for (s, expected) in [
        ("#shown", true),
        ("#none", false),
        ("#vh", false),
        ("#off", false),
        ("div[data-k=\"x'y\"]", true),
    ] {
        let v = r.element_visibility(&sel(s)).expect(s);
        assert_eq!(v.visible, expected, "{s}");
    }
    let e = r.element_visibility(&sel("#missing")).expect_err("missing");
    assert_eq!(variant(&e), "ElementNotFound");

    r.load(&PageRef::new(srv.url("/box"))).expect("load box");
    for (s, exp) in [
        ("#abs", (10.0, 20.0, 30.0, 40.0)),
        ("#frac", (5.5, 7.25, 12.5, 3.75)),
        ("#off", (0.0, -1000.0, 50.0, 50.0)),
        ("#none", (0.0, 0.0, 0.0, 0.0)),
    ] {
        let b = r.bounding_box(&sel(s)).expect(s);
        assert_eq!((b.x, b.y, b.width, b.height), exp, "{s}");
    }

    // 接続拒否の load は失敗し、その後は前のページを返さない。
    let dead = format!("http://127.0.0.1:{}/", closed_port());
    assert!(r.load(&PageRef::new(dead)).is_err());
    let e = r
        .capture_screenshot(&ScreenshotOptions::new())
        .expect_err("after failed load");
    assert_eq!(variant(&e), "PageNotLoaded");
}

/// RENDER-4: 応答しないサーバーへの load は Timeout で失敗し、同じホストで回復できる。
///
/// load の timeout は load ごとにホストへ送られる。cold な CI で初期化・初回フレームが 1 秒を
/// 超えても落ちないよう、ウォームアップと回復の load は 20 秒、ハング検証の load だけ 1 秒にする
/// ため、load ごとに timeout を指定できる HostProcess 層を使う。
#[test]
fn render4_real_host_load_timeout() {
    let hang = Server::start(true);
    let ok = Server::start(false);
    let timeouts = HostTimeouts::default()
        .with_load_default(Duration::from_secs(2))
        .with_load_max(Duration::from_secs(25));
    let mut p = HostProcess::new(host_config(timeouts));

    // ウォームアップ（Servo の初期化を長い timeout で済ませる）。
    let warm = ok.url("/warm");
    p.load(&LoadParams::new(&warm, Some(20_000)).expect("params"), 1)
        .expect("warmup");

    let started = Instant::now();
    let hang_url = hang.url("/");
    let e = p
        .load(&LoadParams::new(&hang_url, Some(1_000)).expect("params"), 2)
        .expect_err("hang must fail");
    assert!(
        matches!(&e, HostProcessError::Timeout(_))
            || matches!(&e, HostProcessError::Host(h) if h.kind() == HostErrorKind::Timeout),
        "{e:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(15));

    let url = ok.url("/shot");
    let loaded = p
        .load(&LoadParams::new(&url, Some(20_000)).expect("params"), 3)
        .expect("recover");
    assert_eq!(loaded.final_url(), url);
}

/// RENDER-3・SEC-2: 許可外 URL はクライアントが拒否し（ホスト起動前）、通常利用は壊れない。
#[test]
fn render3_real_host_rejects_disallowed_urls_client_side() {
    let srv = Server::start(false);
    let r = renderer(HostTimeouts::default(), true, Some(20_000));
    let userinfo = format!("http://user:pw@127.0.0.1:{}/", srv.port);
    for (url, expected) in [
        ("file:///etc/passwd", "Internal"),
        ("data:text/html,x", "Internal"),
        ("javascript:alert(1)", "Internal"),
        (userinfo.as_str(), "Internal"),
        ("about:blank", "PageNotLoaded"),
    ] {
        let e = r.load(&PageRef::new(url)).expect_err(url);
        assert_eq!(variant(&e), expected, "{url}");
        if let Some(d) = e.internal_detail() {
            assert!(!d.contains("passwd") && !d.contains("127.0.0.1"), "{d}");
        }
    }
    let url = srv.url("/shot");
    let loaded = r.load(&PageRef::new(url.as_str())).expect("load");
    assert_eq!(loaded.final_url(), url);

    // ループバックは既定（allow=false）で拒否される。
    let deny = renderer(HostTimeouts::default(), false, Some(20_000));
    let e = deny.load(&PageRef::new(url)).expect_err("loopback denied");
    assert_eq!(variant(&e), "Internal");
}

/// RENDER-3: HostProcess 層。hello の capabilities と、`status` を作り出さないこと（REPAIR-3）。
#[test]
fn render3_real_host_process_layer() {
    let srv = Server::start(false);
    let mut p = HostProcess::new(host_config(HostTimeouts::default()));
    let url = srv.url("/shot");
    let res = p
        .load(&LoadParams::new(&url, Some(20_000)).expect("params"), 1)
        .expect("load");
    assert_eq!(res.final_url(), url);
    assert_eq!(res.status(), None);

    let hello = p.hello().expect("hello");
    let mut caps = hello.capabilities.clone();
    caps.sort();
    assert_eq!(
        caps,
        [
            "bounding_box",
            "load",
            "screenshot",
            "shutdown",
            "visibility"
        ]
    );

    let workdir = p.workdir_path().expect("workdir");
    assert!(workdir.exists());
    p.shutdown();
    assert!(!workdir.exists(), "workdir must be removed after shutdown");
}

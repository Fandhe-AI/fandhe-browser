//! `--stdio` の実バイナリを子プロセスとして起動するブラックボックス結合テスト
//! （`RENDER-3`・`RENDER-4`・`PLUG-1`・`SEC-2`・#993・#231）。
//!
//! フレーム（u32 LE 長さ接頭辞 + UTF-8 JSON）を対話的に読み書きし、hello・`load`（ループバック
//! HTTP サーバー相手。外部ネットワークへ出ない）・`screenshot`（PNG の復号と上下の色。#232）・
//! 未実装メソッドの `unsupported`・`shutdown`・プロトコル違反時の終了コード・stdout の純粋性・
//! ログの秘匿・IntersectionObserver の公開とコールバック（#1294・#1297）・ページから始まる遷移の scheme 制限（`RENDER-10`・#1581）・`navigator.webdriver` の定義（`SEC-5`・TASK-119）を確かめる。このテストバイナリ自体は Servo をリンクしない（`image` は PNG の復号のみ）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(60);

struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Vec<u8>,
    stderr: std::thread::JoinHandle<String>,
}

fn spawn(args: &[&str]) -> Host {
    spawn_with_env(args, &[])
}

/// 環境変数を足して起動する（Cookie が一時領域の外に残らないことの検査で、HOME などを空の場所へ向ける）。
fn spawn_with_env(args: &[&str], envs: &[(&str, &std::path::Path)]) -> Host {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-servo-host"))
        .args(args)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
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
        // 残りの stdout は別スレッドで読む。EOF 待ちで期限判定より前に固まらないため（#1413）。
        let rest_reader = self.child.stdout.take().map(|mut o| {
            std::thread::spawn(move || {
                let mut rest = Vec::new();
                let _ = o.read_to_end(&mut rest);
                rest
            })
        });
        // 終了処理が固まってもテストが固まらないよう上限つきで待つ（#1413）。
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().expect("try_wait") {
                break s;
            }
            if start.elapsed() > LIMIT {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("servo host did not exit within {LIMIT:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // 終了または kill 後は stdout が閉じるため読み取り結果を回収できる。
        let rest = rest_reader
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
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
        serde_json::json!([
            "load",
            "screenshot",
            "visibility",
            "bounding_box",
            "shutdown"
        ])
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
    // 未ロードの screenshot は成功を装わない（REPAIR-3）。
    h.send(r#"{"v":1,"id":4,"method":"screenshot"}"#);
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":4,"error":{"kind":"not_loaded","message":"No page is loaded."}}"#
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
    serve_http_with(SHOT_BODY, hang)
}

/// 可視性判定用の fixture 本文。
const VIS_BODY: &str = "<html><head><title>vis</title></head><body style=\"margin:0\">\
<div id=\"shown\" style=\"width:100px;height:100px\">a</div>\
<div id=\"none\" style=\"display:none\">b</div>\
<div style=\"display:none\"><span id=\"child\">x</span></div>\
<div id=\"vh\" style=\"visibility:hidden;width:50px;height:50px\">c</div>\
<div id=\"zero\" style=\"width:0;height:0\"></div>\
<div id=\"off\" style=\"position:absolute;top:-1000px;width:50px;height:50px\">d</div>\
<div data-k=\"x'y\" style=\"width:10px;height:10px\">e</div>\
</body></html>";

/// 境界ボックス用の fixture 本文（全要素が絶対配置で、期待矩形が既知）。
const BOX_BODY: &str = "<html><head><title>box</title></head><body style=\"margin:0\">\
<div id=\"abs\" style=\"position:absolute;left:10px;top:20px;width:30px;height:40px\"></div>\
<div id=\"frac\" style=\"position:absolute;left:5.5px;top:7.25px;width:12.5px;height:3.75px\"></div>\
<div id=\"off\" style=\"position:absolute;left:0;top:-1000px;width:50px;height:50px\"></div>\
<div id=\"none\" style=\"display:none\"></div>\
<div data-k=\"x'y\" style=\"position:absolute;left:100px;top:200px;width:60px;height:70px\"></div>\
</body></html>";

/// IntersectionObserver 用の fixture 本文（#1294・#1297）。`#exposed` は未公開なら非表示になり、
/// `#fired` は意味のある entry でコールバックが呼ばれたときだけ表示される。
const IO_BODY: &str = "<html><head><title>io</title></head><body style=\"margin:0\">\
<div id=\"exposed\" style=\"position:absolute;left:0;top:0;width:20px;height:20px\"></div>\
<div id=\"target\" style=\"position:absolute;left:100px;top:100px;width:50px;height:50px\"></div>\
<div id=\"fired\" style=\"display:none;position:absolute;left:200px;top:200px;width:30px;height:30px\"></div>\
<script>\
if (typeof IntersectionObserver !== 'function') {\
  document.getElementById('exposed').style.display = 'none';\
} else {\
  try {\
    var io = new IntersectionObserver(function (entries) {\
      if (entries.length > 0 && entries[0].isIntersecting === true && entries[0].target.id === 'target') {\
        document.getElementById('fired').style.display = 'block';\
      }\
    });\
    io.observe(document.getElementById('target'));\
  } catch (e) {}\
}\
</script>\
</body></html>";

/// スクリーンショット用の fixture 本文（上半分が赤・下半分が青）。
const SHOT_BODY: &str = "<html><head><title>fixture</title></head><body style=\"margin:0\"><div style=\"height:400px;background:#f00\"></div><div style=\"height:400px;background:#00f\"></div></body></html>";

/// `serve_http` の本文を指定できる版。
fn serve_http_with(
    body: &'static str,
    hang: bool,
) -> (u16, std::thread::JoinHandle<()>, mpsc::Sender<()>) {
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
    let closed_port = closed_port();

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    let ok_url = format!("http://127.0.0.1:{port}/page?token=abc123");
    h.send(&load_req(1, &ok_url, 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["id"], 1, "{r}");
    assert_eq!(r["result"]["final_url"], ok_url, "{r}");
    // Servo 0.7.0 は HTTP ステータスを返せないため、キーを出さない（REPAIR-3）。
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
    // #1413: Windows では Engine 生成後も後始末を通さず終了する。
    assert_eq!(
        stderr.contains("host exiting before servo teardown"),
        cfg!(windows),
        "{stderr}"
    );
    assert!(
        !stderr.contains("abc123") && !stderr.contains("127.0.0.1"),
        "{stderr}"
    );

    let _ = stop.send(());
    let _ = hang_stop.send(());
    let _ = srv.join();
    let _ = hang_srv.join();
}

/// 標準アルファベットの base64 を復号する（テスト用。パディングは無視）。
fn decode_b64(s: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = alphabet.iter().position(|a| *a == c).expect("alphabet") as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// RENDER-3・RENDER-4: load 後の screenshot は 1280x800 の有効な PNG で、上半分が赤・下半分が青。
/// full_page は unsupported、load 失敗後は not_loaded（前ページの描画を返さない）。
#[test]
fn screenshot_returns_decodable_png_after_load() {
    let (port, srv, stop) = serve_http(false);
    let closed_port = closed_port();
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    h.send(&load_req(
        1,
        &format!("http://127.0.0.1:{port}/shot"),
        20_000,
    ));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");

    h.send(r#"{"v":1,"id":2,"method":"screenshot"}"#);
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["result"]["format"], "png", "{}", r["error"]);
    assert_eq!(r["result"]["width"], 1280);
    assert_eq!(r["result"]["height"], 800);
    let png = decode_b64(r["result"]["data_base64"].as_str().expect("data_base64"));
    let img = image::load_from_memory(&png)
        .expect("decode png")
        .to_rgba8();
    assert_eq!((img.width(), img.height()), (1280, 800));
    let top = img.get_pixel(640, 100).0;
    let bottom = img.get_pixel(640, 700).0;
    assert!(top[0] > 200 && top[1] < 60 && top[2] < 60, "top {top:?}");
    assert!(
        bottom[2] > 200 && bottom[0] < 60 && bottom[1] < 60,
        "bottom {bottom:?}"
    );

    h.send(r#"{"v":1,"id":3,"method":"screenshot","params":{"full_page":true}}"#);
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":3,"error":{"kind":"unsupported","message":"Method is not supported."}}"#
    );

    h.send(&load_req(
        4,
        &format!("http://127.0.0.1:{closed_port}/"),
        20_000,
    ));
    let _ = h.recv();
    h.send(r#"{"v":1,"id":5,"method":"screenshot"}"#);
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":5,"error":{"kind":"not_loaded","message":"No page is loaded."}}"#
    );

    h.send(r#"{"v":1,"id":6,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":6,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    assert!(!stderr.contains("127.0.0.1"), "{stderr}");

    let _ = stop.send(());
    let _ = srv.join();
}

/// RENDER-3: 既知の表示・非表示要素に対し正しい可視性と理由を返す。引用符を含むセレクタは
/// データとして解釈され、不正・制御文字は invalid_params、一致なしは not_found。
#[test]
fn visibility_reports_known_states_after_load() {
    let (port, srv, stop) = serve_http_with(VIS_BODY, false);
    let closed_port = closed_port();
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    let vis = |id: u64, selector_json: &str| {
        format!(
            r#"{{"v":1,"id":{id},"method":"visibility","params":{{"selector":{selector_json}}}}}"#
        )
    };
    h.send(&vis(1, r##""#shown""##));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":1,"error":{"kind":"not_loaded","message":"No page is loaded."}}"#
    );

    h.send(&load_req(
        2,
        &format!("http://127.0.0.1:{port}/vis"),
        20_000,
    ));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");

    let visible = |id: u64| format!(r#"{{"v":1,"id":{id},"result":{{"visible":true}}}}"#);
    let hidden = |id: u64, reason: &str| {
        format!(r#"{{"v":1,"id":{id},"result":{{"visible":false,"reason":"{reason}"}}}}"#)
    };
    let cases: [(&str, String); 7] = [
        (r##""#shown""##, visible(10)),
        (r##""#none""##, hidden(11, "display_none")),
        (r##""#child""##, hidden(12, "display_none")),
        (r##""#vh""##, hidden(13, "visibility_hidden")),
        (r##""#zero""##, hidden(14, "zero_size")),
        (r##""#off""##, hidden(15, "offscreen")),
        (r#""div[data-k=\"x'y\"]""#, visible(16)),
    ];
    for (i, (sel, expected)) in cases.iter().enumerate() {
        h.send(&vis(10 + i as u64, sel));
        assert_eq!(&h.recv(), expected, "selector {sel}");
    }
    h.send(&vis(20, r##""#missing""##));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":20,"error":{"kind":"not_found","message":"No element matches the selector."}}"#
    );
    for (id, sel) in [(21, r#""[[""#), (22, r#""a\u0001""#)] {
        h.send(&vis(id, sel));
        assert_eq!(
            h.recv(),
            format!(
                r#"{{"v":1,"id":{id},"error":{{"kind":"invalid_params","message":"Invalid parameters."}}}}"#
            ),
            "selector {sel}"
        );
    }

    // load 失敗後は前ページの結果を返さない。
    h.send(&load_req(
        30,
        &format!("http://127.0.0.1:{closed_port}/"),
        20_000,
    ));
    let _ = h.recv();
    h.send(&vis(31, r##""#shown""##));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":31,"error":{"kind":"not_loaded","message":"No page is loaded."}}"#
    );

    h.send(r#"{"v":1,"id":32,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":32,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    assert!(
        !stderr.contains("data-k") && !stderr.contains("missing") && !stderr.contains("127.0.0.1"),
        "{stderr}"
    );

    let _ = stop.send(());
    let _ = srv.join();
}

/// RENDER-3・RENDER-5（TASK-38・#1294・#1297・#1273）: IntersectionObserver が公開され、load 後に
/// コールバックが意味のある entry で呼ばれる。servo 0.7.0 で上流のハング修正（servo/servo#47693）を
/// 取り込んだため有効にしている（0.6.0 では #1291 でハングした）。ホストに JS 評価メソッドは無いため、
/// ページ JS の副作用を `visibility` で観測する。外部ネットワークへは出ない（#1003）。
#[test]
fn intersection_observer_is_exposed_and_fires_after_load() {
    let (port, srv, stop) = serve_http_with(IO_BODY, false);
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    h.send(&load_req(1, &format!("http://127.0.0.1:{port}/io"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");

    let vis = |id: u64, sel: &str| {
        format!(r#"{{"v":1,"id":{id},"method":"visibility","params":{{"selector":"{sel}"}}}}"#)
    };
    h.send(&vis(2, "#exposed"));
    assert_eq!(h.recv(), r#"{"v":1,"id":2,"result":{"visible":true}}"#);

    // コールバックは update the rendering 後に呼ばれるため、有限回だけ再試行する。
    let mut last = String::new();
    for i in 0..20u64 {
        let id = 10 + i;
        h.send(&vis(id, "#fired"));
        last = h.recv();
        if last == format!(r#"{{"v":1,"id":{id},"result":{{"visible":true}}}}"#) {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(
        last.ends_with(r#""result":{"visible":true}}"#),
        "IntersectionObserver callback did not fire: {last}"
    );

    h.send(r#"{"v":1,"id":99,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":99,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    assert!(!stderr.contains("127.0.0.1"), "{stderr}");

    let _ = stop.send(());
    let _ = srv.join();
}

/// RENDER-3（TASK-38.5・#234）: 既知のレイアウトの要素に対し正しい境界ボックスを返す。
/// 描画されない要素は (0,0,0,0)、引用符を含むセレクタはデータとして解釈され、不正・制御文字は
/// invalid_params、一致なしは not_found。
#[test]
fn bounding_box_reports_known_layout_after_load() {
    let (port, srv, stop) = serve_http_with(BOX_BODY, false);
    let closed_port = closed_port();
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    let req = |id: u64, selector_json: &str| {
        format!(
            r#"{{"v":1,"id":{id},"method":"bounding_box","params":{{"selector":{selector_json}}}}}"#
        )
    };
    h.send(&req(1, r##""#abs""##));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":1,"error":{"kind":"not_loaded","message":"No page is loaded."}}"#
    );

    h.send(&load_req(
        2,
        &format!("http://127.0.0.1:{port}/box"),
        20_000,
    ));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");

    let cases: [(&str, [f64; 4]); 5] = [
        (r##""#abs""##, [10.0, 20.0, 30.0, 40.0]),
        (r##""#frac""##, [5.5, 7.25, 12.5, 3.75]),
        (r##""#off""##, [0.0, -1000.0, 50.0, 50.0]),
        (r##""#none""##, [0.0, 0.0, 0.0, 0.0]),
        (r#""div[data-k=\"x'y\"]""#, [100.0, 200.0, 60.0, 70.0]),
    ];
    for (i, (sel, expected)) in cases.iter().enumerate() {
        let id = 10 + i as u64;
        h.send(&req(id, sel));
        let v: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
        assert!(v.get("error").is_none(), "selector {sel}: {v}");
        let result = v["result"].as_object().expect("result object");
        let mut keys: Vec<&str> = result.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["height", "width", "x", "y"], "selector {sel}");
        let got = [
            result["x"].as_f64().unwrap(),
            result["y"].as_f64().unwrap(),
            result["width"].as_f64().unwrap(),
            result["height"].as_f64().unwrap(),
        ];
        assert_eq!(&got, expected, "selector {sel}");
    }
    h.send(&req(20, r##""#missing""##));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":20,"error":{"kind":"not_found","message":"No element matches the selector."}}"#
    );
    for (id, sel) in [(21, r#""[[""#), (22, r#""a\u0001""#)] {
        h.send(&req(id, sel));
        assert_eq!(
            h.recv(),
            format!(
                r#"{{"v":1,"id":{id},"error":{{"kind":"invalid_params","message":"Invalid parameters."}}}}"#
            ),
            "selector {sel}"
        );
    }

    // load 失敗後は前ページの結果を返さない。
    h.send(&load_req(
        30,
        &format!("http://127.0.0.1:{closed_port}/"),
        20_000,
    ));
    let _ = h.recv();
    h.send(&req(31, r##""#abs""##));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":31,"error":{"kind":"not_loaded","message":"No page is loaded."}}"#
    );

    h.send(r#"{"v":1,"id":32,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":32,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    assert!(
        !stderr.contains("data-k") && !stderr.contains("missing") && !stderr.contains("127.0.0.1"),
        "{stderr}"
    );

    let _ = stop.send(());
    let _ = srv.join();
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

/// 接続拒否になるループバックのポート番号を返す（`RENDER-3`・`RENDER-4`・`REPAIR-5`・Issue #1153）。
///
/// `:0` で取って閉じる方式では、同じバイナリで並行する `serve_http_with`（`:0`）に同じ番号が
/// 再割り当てされたり自己接続が成立したりして、load が成功してしまう。エフェメラル範囲の外の
/// ポートを bind して閉じれば、`:0` からは再割り当てされない。
fn closed_port() -> u16 {
    let l = bind_non_ephemeral_loopback();
    l.local_addr().expect("addr").port()
}

/// RENDER-3・REPAIR-5・Issue #1153: 接続拒否用ポートはエフェメラル範囲の外にあり、
/// 閉じた後に `:0` bind へ再割り当てされない。
#[test]
fn closed_port_is_outside_ephemeral_range_and_not_reassigned() {
    let (lo, hi) = ephemeral_port_range();
    let l = bind_non_ephemeral_loopback();
    let port = u32::from(l.local_addr().expect("addr").port());
    assert!(port < lo || port > hi, "port {port} is inside {lo}-{hi}");
    drop(l);
    let held: Vec<TcpListener> = (0..100)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    assert!(
        held.iter()
            .all(|l| u32::from(l.local_addr().expect("addr").port()) != port),
        "port {port} was reassigned to a :0 bind"
    );
}

/// hello を受けて shutdown し、終了コード 0 と stdout がフレームのみであることを確かめる。
fn assert_starts_and_shuts_down(args: &[&str]) {
    let mut h = spawn(args);
    let hello: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(hello["event"], "hello", "args: {args:?}");
    h.send(r#"{"v":1,"id":1,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":1,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "args: {args:?} stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
}

/// RENDER-3・SEC-2（#1435）: egress 引数つきでも hello を出し、shutdown で 0 終了する。
#[test]
fn egress_arguments_are_accepted() {
    let port = closed_port().to_string();
    assert_starts_and_shuts_down(&["--stdio", "--egress-proxy-port", &port]);
}

#[cfg(unix)]
#[test]
fn egress_relay_uds_argument_is_accepted() {
    // 中継は bind するだけで UDS へは接続しない（接続は Servo の通信時）ため、不在のパスでよい。
    assert_starts_and_shuts_down(&["--stdio", "--egress-relay-uds", "/nonexistent/proxy.sock"]);
}

/// RENDER-3・SEC-2（#1435）: 不正な egress 引数は失敗終了（コード 1）し、フレームを出さない。
#[test]
fn invalid_egress_arguments_are_rejected_without_frames() {
    for args in [
        &["--stdio", "--egress-proxy-port"][..],
        &["--stdio", "--egress-proxy-port", "0"],
        &["--stdio", "--egress-proxy-port", "65536"],
        &[
            "--stdio",
            "--egress-proxy-port",
            "80",
            "--egress-relay-uds",
            "/a",
        ],
        &["--stdio", "--egress-relay-uds", "rel.sock"],
        &["--stdio", "--egress-relay-uds", "/a/../b.sock"],
        &["--stdio", "--bogus", "1"],
    ] {
        let h = spawn(args);
        let (code, rest, stderr) = h.finish();
        assert_eq!(code, Some(1), "args: {args:?} stderr: {stderr}");
        assert_eq!(rest, 0, "args: {args:?}: no frames expected");
        assert!(stderr.starts_with("error: "), "stderr: {stderr}");
    }
}
/// 受けた接続の先頭行（リクエストライン）を記録する偽 proxy。すべてのリクエストに 502 で答える
/// （CONNECT でも絶対形式 GET でも読み込みは失敗する。成功扱いにしない）。
/// 戻り値は (ポート, スレッド, 停止通知, 記録した先頭行)。
#[allow(clippy::type_complexity)]
fn serve_recording_proxy() -> (
    u16,
    std::thread::JoinHandle<()>,
    mpsc::Sender<()>,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&lines);
    let handle = std::thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            match listener.accept() {
                Ok((mut conn, _)) => {
                    let _ = conn.set_nonblocking(false);
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 4096];
                    let n = conn.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(buf.get(..n).unwrap_or(&[]));
                    let first = head.lines().next().unwrap_or("").to_string();
                    if let Ok(mut v) = seen.lock() {
                        v.push(first);
                    }
                    let resp = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = conn.write_all(resp.as_bytes());
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    });
    (port, handle, stop_tx, lines)
}

/// RENDER-3・SEC-2（#1435）: `--egress-proxy-port` で指定した proxy を Servo の通信が実際に通る。
/// 解決できないホスト宛て（`.invalid`）の読み込みが、直接のホスト解決ではなく proxy への
/// CONNECT として届くことで、引数から proxy pref までの受け渡しと CONNECT プロキシとの互換性を
/// 確かめる（proxy は 502 で拒否するため読み込み自体は失敗する）。外部ネットワークへは出ない。
#[test]
fn load_goes_through_the_egress_proxy() {
    let (proxy_port, srv, stop, lines) = serve_recording_proxy();
    let port = proxy_port.to_string();
    let mut h = spawn(&["--stdio", "--egress-proxy-port", &port]);
    let _hello = h.recv();

    for (id, url) in [
        (1, "https://egress-target.invalid/"),
        (2, "http://egress-plain.invalid/page"),
    ] {
        h.send(&load_req(id, url, 20_000));
        let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
        assert_eq!(r["id"], id, "{r}");
        assert_eq!(r["error"]["kind"], "load_failed", "{r}");
    }

    h.send(r#"{"v":1,"id":3,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":3,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");

    let _ = stop.send(());
    let _ = srv.join();
    let seen = lines.lock().unwrap().clone();
    // https は CONNECT host:443。平文 http は Servo が HTTPS へ昇格するか否かで形が変わるため、
    // 絶対形式 GET・CONNECT :80・CONNECT :443 のいずれかが当該ホスト宛てで届けば proxy 経由とみなす。
    let accepts = |host: &str, l: &str| {
        l.starts_with(&format!("CONNECT {host}:443 "))
            || l.starts_with(&format!("CONNECT {host}:80 "))
            || l.starts_with(&format!("GET http://{host}/"))
    };
    for host in ["egress-target.invalid", "egress-plain.invalid"] {
        assert!(seen.iter().any(|l| accepts(host, l)), "proxy saw: {seen:?}");
    }
    assert!(
        seen.iter()
            .any(|l| l.starts_with("CONNECT egress-target.invalid:443 ")),
        "https must be CONNECT :443, proxy saw: {seen:?}"
    );
}

// ---- 読み込み先の許可リスト（RENDER-3・SEC-2） ----

type Routes = std::sync::Arc<std::collections::HashMap<String, (&'static str, Vec<u8>)>>;

/// ループバックの待ち受けを先に作る（ページ内の URL に使うポートを、ルート登録の前に確定するため）。
fn bind_loopback() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    (listener, port)
}

/// パス別に固定応答を返すループバックサーバー（接続ごとにスレッドで処理）。
/// 種別が [`REDIRECT`] なら本文を `Location` にした 302、[`REFRESH`] なら本文を `Refresh` ヘッダの
/// 移動先にした空の 200 を返す（RENDER-10 の遷移経路の試験用）。
fn serve_routes(listener: TcpListener, routes: Routes) -> mpsc::Sender<()> {
    listener.set_nonblocking(true).expect("nonblocking");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            match listener.accept() {
                Ok((mut conn, _)) => {
                    let routes = routes.clone();
                    std::thread::spawn(move || {
                        let _ = conn.set_nonblocking(false);
                        let mut buf = [0u8; 4096];
                        let n = conn.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let path = req.split(' ').nth(1).unwrap_or("/");
                        let (ct, body) = routes
                            .get(path)
                            .cloned()
                            .unwrap_or(("text/plain", b"nf".to_vec()));
                        if ct == SLOW_HTML {
                            if let Ok(mut at) = SLOW_ARRIVED.lock() {
                                at.get_or_insert_with(std::time::Instant::now);
                            }
                            // テストが解放するまで応答しない（上限つき。解放し忘れでスレッドを残さない）。
                            let until = std::time::Instant::now() + Duration::from_secs(30);
                            while !SLOW_RELEASE.load(std::sync::atomic::Ordering::SeqCst)
                                && std::time::Instant::now() < until
                            {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                        }
                        let to = String::from_utf8_lossy(&body);
                        let (head, body) = match ct {
                            REDIRECT => (
                                format!(
                                    "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                ),
                                Vec::new(),
                            ),
                            REFRESH => (
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nRefresh: 0;url={to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                ),
                                Vec::new(),
                            ),
                            _ => (
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    if ct == SLOW_HTML { "text/html" } else { ct },
                                    body.len()
                                ),
                                body,
                            ),
                        };
                        let _ = conn.write_all(head.as_bytes());
                        let _ = conn.write_all(&body);
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    });
    stop_tx
}

/// [`serve_routes`] の擬似種別: 302 で本文の URL へ移す。
const REDIRECT: &str = "redirect";
/// [`serve_routes`] の擬似種別: `Refresh` ヘッダで本文の URL へ移す。
const REFRESH: &str = "refresh";
/// [`serve_routes`] の擬似種別: [`SLOW_RELEASE`] が立つまで応答を止め、その後に本文を `text/html` で返す（`act` の遷移の
/// 待ちの打ち切りの試験用。実時間の差に頼らず「遷移の完了前に `act` が戻る」ことを確かめる）。
const SLOW_HTML: &str = "slow-html";
/// [`SLOW_HTML`] の応答を解放する。使うのは `interactive_act_edge_paths` だけ。
static SLOW_RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// [`SLOW_HTML`] の要求が最初に届いた時刻（遷移が始まった時刻の近似。クリックの処理の時間を測定から外す）。
static SLOW_ARRIVED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
const RED_HTML: &str = "<body style=\"margin:0;background:#f00\">";
const RED_SVG: &str = "<svg xmlns='http://www.w3.org/2000/svg' width='200' height='200'><rect width='200' height='200' fill='red'/></svg>";
const RED_CSS: &str = "#t{background:#f00}";
const RED_JS: &str = "document.getElementById('t').style.background='#f00'";
const RED_WORKER_JS: &str = "postMessage('red')";
const BODY_MARK: &str = "DISALLOWED_BODY";
const DATA_RED_HTML: &str =
    "data:text/html,%3Cbody%20style%3D%22margin%3A0%3Bbackground%3A%23f00%22%3E";
/// ページ側で対象 `#t` を赤くする JS 式。
const PAINT: &str = "document.getElementById('t').style.background='#f00'";

/// 同じ内容を http（ルート）と file（一時ディレクトリ）の両方で提供する資源。
const FIXTURES: [(&str, &str, &str); 6] = [
    ("red.html", "text/html", RED_HTML),
    ("red.svg", "image/svg+xml", RED_SVG),
    ("red.css", "text/css", RED_CSS),
    ("red.js", "text/javascript", RED_JS),
    ("red.worker.js", "text/javascript", RED_WORKER_JS),
    ("red.txt", "text/plain", BODY_MARK),
];

/// 資源を一時ディレクトリへ書き、http 用のルートの初期値を返す。
fn fixtures(
    tag: &str,
) -> (
    std::path::PathBuf,
    std::collections::HashMap<String, (&'static str, Vec<u8>)>,
) {
    let dir = std::env::temp_dir().join(format!("fandhe_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut routes = std::collections::HashMap::new();
    for (n, ct, body) in FIXTURES {
        std::fs::write(dir.join(n), body).expect("write");
        routes.insert(format!("/{n}"), (ct, body.as_bytes().to_vec()));
    }
    (dir, routes)
}

/// 対象 `#t`（x=440）と対照 2 つ（x=0 の http iframe・x=220 の data: iframe）を並べたページ。
fn page(base: &str, head: &str, target: &str, script: &str) -> String {
    let style = "iframe,#t,img{position:absolute;top:0;width:200px;height:200px;border:0;margin:0}";
    let ctl = format!(
        "<iframe style=\"left:0\" src=\"{base}/red.html\"></iframe><iframe style=\"left:220px\" src=\"{DATA_RED_HTML}\"></iframe>"
    );
    format!(
        "<html><head><style>{style}</style>{head}</head><body style=\"margin:0\">{ctl}{target}<script>{script}</script></body></html>"
    )
}

/// 矩形内で条件を満たす画素の割合。
fn fraction(img: &image::RgbaImage, x0: u32, w: u32, pred: fn([u8; 4]) -> bool) -> f32 {
    let mut n = 0u32;
    for y in 0..200 {
        for x in x0..x0 + w {
            if pred(img.get_pixel(x, y).0) {
                n += 1;
            }
        }
    }
    n as f32 / (200 * w) as f32
}

fn is_red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

fn is_green(p: [u8; 4]) -> bool {
    p[0] < 60 && p[1] > 200 && p[2] < 60
}

fn red_fraction(img: &image::RgbaImage, x0: u32, w: u32) -> f32 {
    fraction(img, x0, w, is_red)
}

fn file_url(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// 要求 id を進めながら `load` を送り、結果が成功であることを確かめる。
fn load_page(h: &mut Host, id: &mut u64, url: &str) {
    *id += 1;
    h.send(&load_req(*id, url, 30_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
}

fn shoot(h: &mut Host, id: &mut u64) -> image::RgbaImage {
    *id += 1;
    h.send(&format!(r#"{{"v":1,"id":{id},"method":"screenshot"}}"#));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    let png = decode_b64(r["result"]["data_base64"].as_str().expect("png"));
    image::load_from_memory(&png).expect("png").to_rgba8()
}

/// 条件を満たすまで撮り直す（期限は 10 秒。超過時は最後の画像を返し、判定は呼び出し側の assert に任せる）。
/// 画像と、条件を満たすまでにかかった時間を返す。
fn poll_shot(
    h: &mut Host,
    id: &mut u64,
    done: impl Fn(&image::RgbaImage) -> bool,
) -> (image::RgbaImage, Duration) {
    let start = Instant::now();
    loop {
        let img = shoot(h, id);
        if done(&img) || start.elapsed() > Duration::from_secs(10) {
            return (img, start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn finish_host(mut h: Host, id: u64) {
    h.send(&format!(r#"{{"v":1,"id":{id},"method":"shutdown"}}"#));
    let _ = h.recv();
    let (code, _, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

type Case = (
    &'static str,
    Box<dyn Fn(&dyn Fn(&str) -> String, &str) -> String>,
);

/// 経路ごとのページ。第 1 引数は `red.*` の URL を作る関数、第 2 引数は http サーバーの base。
/// どの経路がどの delegate を通るかは、各ケースの注記を参照（`WebViewDelegate` は `page.rs`、
/// `ServoDelegate` は `engine.rs` の `set_delegate`）。
fn cases() -> Vec<Case> {
    // 対象 `#t`（x=440）。
    let t = "<div id=t style=\"left:440px\"></div>";
    // blob Worker を作り、`red` を受けたら対象を赤くする。`body` は Worker 内のスクリプト（JS 式の文字列連結用）。
    let blob_worker = |body: String| {
        format!(
            "var b=new Blob([{body}]);var w=new Worker(URL.createObjectURL(b));w.onmessage=function(){{{PAINT}}}"
        )
    };
    vec![
        // WebViewDelegate（子フレーム）。
        (
            "iframe_initial",
            Box::new(|u, b| {
                page(
                    b,
                    "",
                    &format!(
                        "<iframe id=t style=\"left:440px\" src=\"{}\"></iframe>",
                        u("red.html")
                    ),
                    "",
                )
            }),
        ),
        (
            "iframe_js_rewrite",
            Box::new(|u, b| {
                page(
                    b,
                    "",
                    "<iframe id=t style=\"left:440px\" src=\"about:blank\"></iframe>",
                    &format!("document.getElementById('t').src='{}'", u("red.html")),
                )
            }),
        ),
        (
            "img",
            Box::new(|u, b| {
                page(
                    b,
                    "",
                    &format!("<img id=t style=\"left:440px\" src=\"{}\">", u("red.svg")),
                    "",
                )
            }),
        ),
        (
            "css_url",
            Box::new(|u, b| {
                page(
                    b,
                    "",
                    &format!(
                        "<div id=t style=\"left:440px;background:url({})\"></div>",
                        u("red.svg")
                    ),
                    "",
                )
            }),
        ),
        (
            "link_stylesheet",
            Box::new(|u, b| {
                page(
                    b,
                    &format!("<link rel=stylesheet href=\"{}\">", u("red.css")),
                    t,
                    "",
                )
            }),
        ),
        (
            "css_import",
            Box::new(|u, b| {
                page(
                    b,
                    &format!("<style>@import url({});</style>", u("red.css")),
                    t,
                    "",
                )
            }),
        ),
        (
            "script_src",
            Box::new(|u, b| {
                page(
                    b,
                    "",
                    &format!(
                        "<div id=t style=\"left:440px\"></div><script src=\"{}\"></script>",
                        u("red.js")
                    ),
                    "",
                )
            }),
        ),
        (
            "iframe_uppercase_scheme",
            Box::new(|u, b| {
                let s = u("red.html");
                let (sch, rest) = s.split_once(':').unwrap();
                page(
                    b,
                    "",
                    &format!(
                        "<iframe id=t style=\"left:440px\" src=\"{}:{rest}\"></iframe>",
                        sch.to_uppercase()
                    ),
                    "",
                )
            }),
        ),
        // 以降はページから起こす Worker・XHR 経路。実測（Servo 0.7.0）では Worker 内の要求も
        // `WebViewDelegate` を通り、`ServoDelegate` を通る経路はこのテストには無い。
        // 予防: Servo 0.7.0 が元から止めるため、検査を外しても落ちない。上流の挙動が変わったときに検出するために置く。
        (
            "worker_script",
            Box::new(move |u, b| {
                page(
                    b,
                    "",
                    t,
                    &format!(
                        "try{{var w=new Worker('{}');w.onmessage=function(){{{PAINT}}}}}catch(e){{}}",
                        u("red.worker.js")
                    ),
                )
            }),
        ),
        (
            "blob_worker_import_scripts",
            Box::new({
                let bw = blob_worker;
                move |u, b| {
                    page(
                        b,
                        "",
                        t,
                        &bw(format!("\"importScripts('{}')\"", u("red.worker.js"))),
                    )
                }
            }),
        ),
        // 予防: Servo 0.7.0 が元から止めるため、検査を外しても落ちない。上流の挙動が変わったときに検出するために置く。
        (
            "worker_fetch_cors",
            Box::new({
                let bw = blob_worker;
                move |u, b| {
                    page(
                        b,
                        "",
                        t,
                        &bw(format!(
                            "\"fetch('{}').then(function(r){{return r.text()}}).then(function(x){{if(x.indexOf('{BODY_MARK}')>=0)postMessage('red')}}).catch(function(){{}})\"",
                            u("red.txt")
                        )),
                    )
                }
            }),
        ),
        // 予防: Servo 0.7.0 が元から止めるため、検査を外しても落ちない。上流の挙動が変わったときに検出するために置く。
        (
            "worker_fetch_no_cors",
            Box::new({
                let bw = blob_worker;
                move |u, b| {
                    page(
                        b,
                        "",
                        t,
                        &bw(format!(
                            "\"fetch('{}',{{mode:'no-cors'}}).then(function(r){{return r.text()}}).then(function(x){{if(x.indexOf('{BODY_MARK}')>=0)postMessage('red')}}).catch(function(){{}})\"",
                            u("red.txt")
                        )),
                    )
                }
            }),
        ),
        // 予防: Servo 0.7.0 が元から止めるため、検査を外しても落ちない。上流の挙動が変わったときに検出するために置く。
        (
            "xhr",
            Box::new(move |u, b| {
                page(
                    b,
                    "",
                    t,
                    &format!(
                        "try{{var x=new XMLHttpRequest();x.open('GET','{}');x.onload=function(){{if(x.responseText.indexOf('{BODY_MARK}')>=0){{{PAINT}}}}};x.send()}}catch(e){{}}",
                        u("red.txt")
                    ),
                )
            }),
        ),
    ]
}

/// RENDER-3・SEC-2: 許可外 scheme（例として `file:`）を読み込み先にした各経路は空の応答で終わって何も描かれず、同じページの http・data: の
/// 対照と、同じ経路の http 版は赤く描かれる（対照が赤いので、読み込み完了後に撮れていることも保証する）。
/// 撮影は条件を満たすまで撮り直す。file 版は、同じ経路の http 版が赤くなるまでにかかった時間
/// （下限 300ms）だけ、対照が赤くなった後も待ってから最終判定の画像を撮る。
#[test]
fn load_allowlist_rejects_disallowed_scheme() {
    let (dir, mut routes) = fixtures("allowlist");
    let (listener, port) = bind_loopback();
    let base = format!("http://127.0.0.1:{port}");

    // 経路 × (http, file) のページを 1 つのサーバーへ登録する。
    let cases = cases();
    for (name, build) in &cases {
        for kind in ["http", "file"] {
            let u: Box<dyn Fn(&str) -> String> = if kind == "http" {
                let base = base.clone();
                Box::new(move |n| format!("{base}/{n}"))
            } else {
                let dir = dir.clone();
                Box::new(move |n| file_url(&dir.join(n)))
            };
            routes.insert(
                format!("/p_{name}_{kind}"),
                ("text/html", build(&*u, &base).into_bytes()),
            );
        }
    }
    let stop = serve_routes(listener, std::sync::Arc::new(routes));

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 0;
    for (name, _) in &cases {
        let mut http_time = Duration::from_millis(300);
        for (kind, want_red) in [("http", true), ("file", false)] {
            load_page(&mut h, &mut id, &format!("{base}/p_{name}_{kind}"));
            let ctl = |i: &image::RgbaImage| {
                red_fraction(i, 0, 200) > 0.9 && red_fraction(i, 220, 200) > 0.9
            };
            let img = if want_red {
                let (img, t) = poll_shot(&mut h, &mut id, |i| {
                    ctl(i) && red_fraction(i, 440, 200) > 0.9
                });
                http_time = http_time.max(t);
                img
            } else {
                poll_shot(&mut h, &mut id, ctl);
                std::thread::sleep(http_time);
                shoot(&mut h, &mut id)
            };
            let ctl_http = red_fraction(&img, 0, 200);
            let ctl_data = red_fraction(&img, 220, 200);
            let target = red_fraction(&img, 440, 200);
            assert!(ctl_http > 0.9, "{name}/{kind}: http control {ctl_http}");
            assert!(ctl_data > 0.9, "{name}/{kind}: data control {ctl_data}");
            if want_red {
                assert!(
                    target > 0.9,
                    "{name}/{kind}: target should be red: {target}"
                );
            } else {
                assert!(
                    target < 0.01,
                    "{name}/{kind}: disallowed target unexpectedly loaded: {target}"
                );
            }
        }
    }
    finish_host(h, id + 1);
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
}

/// RENDER-3・SEC-2・RENDER-10: 主フレームを `location` で許可外 scheme（例として `file:`）へ移しても読み込まれない。
/// 初期ページは緑。http 版は赤い全面ページへ遷移し、file 版は遷移の検査（`request_navigation`。TASK-120）が
/// 先に拒否して緑のまま残る。TASK-120 より前は、遷移が始まって読み込み先の許可リストが空の応答で白くしていた。
/// 許可リストは主フレームにも効くが、この経路では遷移の検査の後ろの二重化になる。
#[test]
fn load_allowlist_rejects_disallowed_main_frame() {
    let (dir, mut routes) = fixtures("mainframe");
    let (listener, port) = bind_loopback();
    let base = format!("http://127.0.0.1:{port}");
    for (kind, url) in [
        ("http", format!("{base}/red.html")),
        ("file", file_url(&dir.join("red.html"))),
    ] {
        routes.insert(
            format!("/p_{kind}"),
            (
                "text/html",
                format!(
                    "<html><body style=\"margin:0;background:#0f0\"><script>setTimeout(function(){{location='{url}'}},200)</script></body></html>"
                )
                .into_bytes(),
            ),
        );
    }
    let stop = serve_routes(listener, std::sync::Arc::new(routes));

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 0;
    let mut http_time = Duration::from_millis(300);
    for kind in ["http", "file"] {
        load_page(&mut h, &mut id, &format!("{base}/p_{kind}"));
        let img = if kind == "http" {
            // 緑でなくなる（遷移が済む）まで待つ。
            let (img, t) = poll_shot(&mut h, &mut id, |i| fraction(i, 0, 200, is_green) < 0.5);
            http_time = http_time.max(t);
            img
        } else {
            // http 版が遷移を終えるまでにかかった時間と 1 秒の大きい方だけ待ってから撮る。
            std::thread::sleep(http_time.max(Duration::from_secs(1)));
            shoot(&mut h, &mut id)
        };
        let red = red_fraction(&img, 0, 200);
        if kind == "http" {
            assert!(red > 0.9, "{kind}: should navigate to red page: {red}");
        } else {
            assert!(red < 0.01, "{kind}: main frame unexpectedly loaded: {red}");
            assert!(
                fraction(&img, 0, 200, is_green) > 0.9,
                "{kind}: navigation should be denied before loading"
            );
        }
    }
    finish_host(h, id + 1);
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
}

/// RENDER-3: ページからの WebSocket 接続はホストが拒否し、サーバーへ届かない
/// （egress 隔離ありで WebSocket が使えない挙動と揃える）。
/// ページは `WebSocket` の生成が例外なく済んだこと（または `onerror`・`onclose`）で DOM を赤くする
/// （スクリプトが実行され生成まで進んだ証明。拒否時は応答が空のためイベントは来ない）。
/// ホストの検査を外すとハンドシェイクがサーバーへ届き、`seen` が真になって失敗する（確認済み）。
#[test]
fn websocket_connections_are_refused() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let ws = TcpListener::bind("127.0.0.1:0").expect("bind");
    let ws_port = ws.local_addr().expect("addr").port();
    ws.set_nonblocking(true).expect("nonblocking");
    let seen = Arc::new(AtomicBool::new(false));
    let seen_w = seen.clone();
    let (ws_stop_tx, ws_stop_rx) = mpsc::channel::<()>();
    let ws_thread = std::thread::spawn(move || {
        while ws_stop_rx.try_recv().is_err() {
            if let Ok((mut c, _)) = ws.accept() {
                let _ = c.set_nonblocking(false);
                let _ = c.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let n = c.read(&mut buf).unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n])
                    .to_ascii_lowercase()
                    .contains("upgrade: websocket")
                {
                    seen_w.store(true, Ordering::SeqCst);
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    let html = format!(
        "<html><body style=\"margin:0\"><div id=t style=\"width:200px;height:200px;background:#0f0\"></div><script>\
         function f(){{document.getElementById('t').style.background='#f00'}}\
         try{{var s=new WebSocket('ws://127.0.0.1:{ws_port}/');s.onerror=f;s.onclose=f;f()}}catch(e){{}}</script></body></html>"
    );
    let mut routes = std::collections::HashMap::new();
    routes.insert("/ws".to_owned(), ("text/html", html.into_bytes()));
    let (listener, port) = bind_loopback();
    let stop = serve_routes(listener, Arc::new(routes));

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 0;
    load_page(&mut h, &mut id, &format!("http://127.0.0.1:{port}/ws"));
    let (img, _) = poll_shot(&mut h, &mut id, |i| red_fraction(i, 0, 200) > 0.9);
    assert!(
        red_fraction(&img, 0, 200) > 0.9,
        "websocket attempt was not observed (no onerror/onclose)"
    );
    // 届くなら数 ms で届く。撮影の直後に取りこぼさないよう少し待つ。
    std::thread::sleep(Duration::from_millis(500));
    finish_host(h, id + 1);
    assert!(
        !seen.load(Ordering::SeqCst),
        "websocket handshake reached the server"
    );
    let _ = ws_stop_tx.send(());
    let _ = ws_thread.join();
    let _ = stop.send(());
}

/// 主フレームを `data:`・`blob:`・`file:` へ移そうとする経路と、その移動先を受け取ってページを返す関数。
/// 第 1 引数は移動先 URL（JS 文字列リテラルに入れる）。初期ページは全面が緑。
type NavCase = (&'static str, fn(&str) -> String);

/// 全面を緑にし、`script` を 200ms 後に実行するページ。
fn green_page(head: &str, script: &str) -> String {
    format!(
        "<html><head>{head}</head><body style=\"margin:0;background:#0f0\"><script>setTimeout(function(){{{script}}},200)</script></body></html>"
    )
}

/// RENDER-10 の遷移経路。`{to}` を移動先として使う。
fn nav_cases() -> Vec<NavCase> {
    vec![
        ("location", |to| green_page("", &format!("location='{to}'"))),
        ("window_open_self", |to| {
            green_page("", &format!("window.open('{to}','_self')"))
        }),
        ("top_location_from_blob_iframe", |to| {
            // blob: の iframe（読み込みは許す）の中から主フレームを移す。
            green_page(
                "",
                &format!(
                    "var b=new Blob([\"<script>top.location='{to}'<\\/script>\"],{{type:'text/html'}});\
                     var f=document.createElement('iframe');f.style.display='none';f.src=URL.createObjectURL(b);document.body.appendChild(f)"
                ),
            )
        }),
        ("target_blank", |to| {
            green_page(
                "",
                &format!(
                    "var a=document.createElement('a');a.href='{to}';a.target='_blank';document.body.appendChild(a);a.click()"
                ),
            )
        }),
        ("meta_refresh", |to| {
            green_page(
                &format!("<meta http-equiv=refresh content=\"0;url={to}\">"),
                "",
            )
        }),
        ("redirect", |_| String::new()), // 本文は使わない（/r_* の 302 を `location` で踏む）。
        ("refresh_header", |_| String::new()), // 同上（/h_* の Refresh ヘッダ）。
    ]
}

/// RENDER-10（TASK-120・#1581）: ページから始まる主フレームの遷移は、移動先が `data:`・`blob:`・`file:`
/// なら起きず、赤い移動先は描かれない。同じ経路で http の移動先へは遷移する（対照。経路が実際に
/// 動いたことと待ち時間の目安を与える）。`blob:` の移動先はページ内で作る（`location` 経路のみ）。
/// 302・`Refresh` ヘッダ・`meta refresh` の移動先の拒否は Servo 内部の制限に頼る経路で、servo の
/// 版更新で黙って変わらないよう固定する（TASK-117 の ★2 の読み替えの条件）。
/// 実測（2026-11-18・Servo 0.7.0）: ホストの遷移の検査を外すと、`location`・`window.open(_self)`・
/// `top.location`・`meta refresh`・`Refresh` ヘッダで `data:`・`blob:` へ移れてしまう（Servo 自身は止めない）。
/// `file:` は読み込み先の許可リストが、302 の移動先は Servo の fetch が止める。`target=_blank` と別文書への
/// `history.back()` は `request_navigation` を通らない（前者は新しいウィンドウを作らず、後者は検査済みの
/// 履歴にしか戻れない）。
#[test]
fn main_frame_navigation_to_disallowed_scheme_is_blocked() {
    let (dir, mut routes) = fixtures("navscheme");
    let (listener, port) = bind_loopback();
    let base = format!("http://127.0.0.1:{port}");
    let targets = [
        ("http", format!("{base}/red.html")),
        ("data", DATA_RED_HTML.to_owned()),
        ("file", file_url(&dir.join("red.html"))),
    ];
    let cases = nav_cases();
    for (name, build) in &cases {
        for (kind, to) in &targets {
            let body = match *name {
                "redirect" => {
                    routes.insert(format!("/r_{kind}"), (REDIRECT, to.clone().into_bytes()));
                    green_page("", &format!("location='/r_{kind}'"))
                }
                "refresh_header" => {
                    routes.insert(format!("/h_{kind}"), (REFRESH, to.clone().into_bytes()));
                    green_page("", &format!("location='/h_{kind}'"))
                }
                _ => build(to),
            };
            routes.insert(
                format!("/p_{name}_{kind}"),
                ("text/html", body.into_bytes()),
            );
        }
    }
    // blob: の移動先（ページ内で赤い文書の blob URL を作って移る）。
    routes.insert(
        "/p_location_blob".to_owned(),
        (
            "text/html",
            green_page(
                "",
                &format!(
                    "location=URL.createObjectURL(new Blob(['{RED_HTML}'],{{type:'text/html'}}))"
                ),
            )
            .into_bytes(),
        ),
    );
    let stop = serve_routes(listener, std::sync::Arc::new(routes));

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 0;
    let mut runs: Vec<(String, bool)> = Vec::new();
    for (name, _) in &cases {
        for (kind, _) in &targets {
            // 新しいウィンドウは作られない（RENDER-10 の `unsupported`）ため、主フレームは http でも移らない。
            runs.push((
                format!("{name}_{kind}"),
                *kind == "http" && *name != "target_blank",
            ));
        }
    }
    runs.push(("location_blob".to_owned(), false));
    let mut wait = Duration::from_millis(300);
    for (run, want_red) in runs {
        load_page(&mut h, &mut id, &format!("{base}/p_{run}"));
        let img = if want_red {
            let (img, t) = poll_shot(&mut h, &mut id, |i| red_fraction(i, 0, 200) > 0.9);
            wait = wait.max(t);
            img
        } else {
            // 拒否されると緑のまま、Servo が失敗させると白い空ページやエラーページになる。
            // どちらでも赤くならないことを、http 版が赤くなるまでの最長時間と 1 秒の大きい方まで見る。
            let start = Instant::now();
            loop {
                let img = shoot(&mut h, &mut id);
                if start.elapsed() > wait.max(Duration::from_secs(1)) {
                    break img;
                }
                assert!(
                    red_fraction(&img, 0, 200) < 0.01,
                    "{run}: navigated to disallowed target"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        let red = red_fraction(&img, 0, 200);
        if want_red {
            assert!(
                red > 0.9,
                "{run}: control should navigate to red page: {red}"
            );
        } else {
            assert!(red < 0.01, "{run}: navigated to disallowed target: {red}");
        }
    }
    finish_host(h, id + 1);
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
}

/// RENDER-10（TASK-120）: 主フレームの初回の `load` で 302 の移動先が `data:`・`file:` なら、移動先へは
/// 移らない（Servo 内部の制限に頼る経路の固定）。Servo 0.7.0 は `data:` への 302 を失敗にせず、元の
/// URL の空のページで止める（実測）ため、`load_failed` か「最終 URL が元の URL のまま赤く描かれない」の
/// どちらかを合格にする。http の移動先は成功し、最終 URL が移動先になる（対照）。
#[test]
fn initial_load_redirect_to_disallowed_scheme_does_not_reach_target() {
    let (dir, mut routes) = fixtures("loadredirect");
    let (listener, port) = bind_loopback();
    let base = format!("http://127.0.0.1:{port}");
    for (kind, to) in [
        ("http", format!("{base}/red.html")),
        ("data", DATA_RED_HTML.to_owned()),
        ("file", file_url(&dir.join("red.html"))),
    ] {
        routes.insert(format!("/r_{kind}"), (REDIRECT, to.into_bytes()));
    }
    let stop = serve_routes(listener, std::sync::Arc::new(routes));

    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 0;
    for kind in ["http", "data", "file"] {
        id += 1;
        let start = format!("{base}/r_{kind}");
        h.send(&load_req(id, &start, 30_000));
        let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
        if kind == "http" {
            assert_eq!(r["result"]["final_url"], format!("{base}/red.html"), "{r}");
        } else if r["error"]["kind"] != "load_failed" {
            assert_eq!(r["result"]["final_url"], start.as_str(), "{kind}: {r}");
            let red = red_fraction(&shoot(&mut h, &mut id), 0, 200);
            assert!(red < 0.01, "{kind}: redirect target rendered: {red}");
        }
    }
    finish_host(h, id + 1);
    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 操作モードの DOM の取得（RENDER-11・TASK-122） ----

/// ループバック宛てだけを転送する最小の egress プロキシ（`--interactive` は egress の指定が必須のため）。
/// CONNECT はトンネルにし、絶対形式の GET は経路形式に直して転送する。外部ネットワークへは出ない。
fn serve_forwarding_proxy() -> (u16, mpsc::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            let Ok((mut conn, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            std::thread::spawn(move || {
                let _ = conn.set_nonblocking(false);
                let mut buf = vec![0u8; 65536];
                let n = conn.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut words = head.split(' ');
                let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                let (authority, rest) = if method == "CONNECT" {
                    (target.to_owned(), None)
                } else {
                    let t = target.strip_prefix("http://").unwrap_or(target);
                    let (a, p) = t.split_at(t.find('/').unwrap_or(t.len()));
                    (
                        a.to_owned(),
                        Some(head.replacen(target, if p.is_empty() { "/" } else { p }, 1)),
                    )
                };
                if !authority.starts_with("127.0.0.1:") {
                    let _ = conn.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                let Ok(mut up) = std::net::TcpStream::connect(&authority) else {
                    return;
                };
                match rest {
                    None => {
                        let _ = conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n");
                    }
                    Some(req) => {
                        let _ = up.write_all(req.as_bytes());
                    }
                }
                let (mut a, mut b) = (conn.try_clone().unwrap(), up.try_clone().unwrap());
                let t = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut a, &mut b);
                    let _ = b.shutdown(std::net::Shutdown::Write);
                });
                let _ = std::io::copy(&mut up, &mut conn);
                let _ = conn.shutdown(std::net::Shutdown::Both);
                let _ = t.join();
            });
        }
    });
    (port, stop_tx)
}

/// 操作モードのホストを起動し、hello を読む。
fn spawn_interactive(proxy_port: u16) -> (Host, serde_json::Value) {
    spawn_interactive_with_env(proxy_port, &[])
}

fn spawn_interactive_with_env(
    proxy_port: u16,
    envs: &[(&str, &std::path::Path)],
) -> (Host, serde_json::Value) {
    let port = proxy_port.to_string();
    let mut h = spawn_with_env(
        &["--stdio", "--interactive", "--egress-proxy-port", &port],
        envs,
    );
    let hello: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    (h, hello)
}

fn request(h: &mut Host, id: u64, method: &str) -> serde_json::Value {
    h.send(&format!(r#"{{"v":1,"id":{id},"method":"{method}"}}"#));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["id"], id, "{r}");
    r
}

/// `data-fandhe-n` の値を出現順に集める。
fn numbers(html: &str) -> Vec<u64> {
    html.match_indices("data-fandhe-n=\"")
        .map(|(i, m)| {
            let rest = &html[i + m.len()..];
            rest[..rest.find('"').unwrap()].parse().unwrap()
        })
        .collect()
}

/// RENDER-11・TASK-122: 番号つきの HTML・現在値・伏せる値（第 1 層）・iframe の中身の除外・ページの
/// 同名属性の無視を、実ホストで確かめる。egress の指定なしの `--interactive` は起動しない（設計 §14.1）。
#[test]
fn interactive_dom_snapshot_numbers_elements_and_redacts() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    let page = concat!(
        "<!doctype html><html><head><title>t</title></head><body>",
        "<input id=q name=q value=initial>",
        "<input id=pw type=password value=pw-attr-secret>",
        "<input id=cc autocomplete=\"section-a cc-number\" value=cc-attr-secret>",
        "<textarea id=ta>orig</textarea>",
        "<input type=checkbox id=cb>",
        "<select id=s><option value=a selected>A<option value=b>B</select>",
        "<div data-fandhe-n=\"999\" id=spoof>x</div>",
        "<iframe id=f1 srcdoc=\"<p>SRCDOC_SECRET</p>\"></iframe>",
        "<iframe id=f2 src=\"data:text/html,DATAURL_SECRET\"></iframe>",
        "<iframe id=f3 src=\"/rel\"></iframe>",
        "<object id=ob data=\"data:text/html,OBJECT_SECRET\"></object>",
        "<select id=ccm autocomplete=cc-exp-month><option value=m12 selected>12</option><option value=m01>01</option></select>",
        "<p id=e>a &lt; b &amp; c</p>",
        "<input type=radio id=rx name=size value=express checked>",
        "<constructor id=k><b>kid</b></constructor>",
        "<svg><script>if(1<2){}</script></svg>",
        "<script>",
        "document.getElementById('q').value='typed-value';",
        "document.getElementById('pw').value='typed-'+'pw';",
        "document.getElementById('ta').value='ta-value';",
        "document.getElementById('cb').checked=true;",
        "document.getElementById('s').value='b';",
        "var f=document.createElement('iframe');",
        "f.src=URL.createObjectURL(new Blob(['<p>BLOB_'+'SECRET</p>'],{type:'text/html'}));",
        "document.body.appendChild(f);",
        "</script></body></html>"
    );
    m.insert("/p".to_owned(), ("text/html", page.as_bytes().to_vec()));
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();

    // egress の指定なしの --interactive はフレームを出さずに失敗する。
    let (code, rest, _) = spawn(&["--stdio", "--interactive"]).finish();
    assert_ne!(code, Some(0));
    assert_eq!(rest, 0);

    let (mut h, hello) = spawn_interactive(proxy);
    let caps = hello["data"]["capabilities"].as_array().unwrap();
    assert!(caps.contains(&serde_json::json!("dom_snapshot")), "{hello}");
    assert_eq!(
        hello["data"]["limits"]["dom_snapshot_html_bytes"],
        8_388_608
    );

    let url = format!("http://127.0.0.1:{port}/p");
    h.send(&load_req(1, &url, 30_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    let page_id = r["result"]["page"].clone();
    assert_eq!(page_id["session"].as_str().unwrap().len(), 32, "{r}");
    assert!(page_id["doc_seq"].as_u64().unwrap() >= 1, "{r}");

    let r = request(&mut h, 2, "dom_snapshot");
    let res = &r["result"];
    let html = res["html"].as_str().unwrap_or_else(|| panic!("{r}"));
    assert_eq!(res["url"], url.as_str());
    assert_eq!(res["page"]["session"], page_id["session"]);
    assert_eq!(res["snapshot"], "s1");
    // 番号は前順の 0 起点で重複なく振られ、ページ自身の同名属性（999）は出ない。
    let ns = numbers(html);
    assert_eq!(ns, (0..ns.len() as u64).collect::<Vec<_>>());
    assert_eq!(res["element_count"], ns.len() as u64);
    assert!(
        html.starts_with("<!DOCTYPE html><html data-fandhe-n=\"0\">"),
        "{html}"
    );
    // 現在値を書く。
    assert!(
        html.contains("id=\"q\" name=\"q\" value=\"typed-value\""),
        "{html}"
    );
    assert!(html.contains(">ta-value</textarea>"), "{html}");
    assert!(html.contains("id=\"cb\" value=\"on\" checked>"), "{html}");
    // checkbox・radio も value を残す（選択肢を見分けるため）。
    assert!(
        html.contains("id=\"rx\" name=\"size\" value=\"express\" checked>"),
        "{html}"
    );
    // 要素名が Object.prototype の名前と同じでも空要素扱いにしない。
    assert!(html.contains("id=\"k\"><b data-fandhe-n="), "{html}");
    assert!(html.contains(">kid</b></constructor>"), "{html}");
    assert!(html.contains("value=\"b\" selected>"), "{html}");
    assert!(!html.contains("value=\"a\" selected"), "{html}");
    // 伏せる対象の値（属性・現在値とも）は出さず、数を返す。
    for secret in ["pw-attr-secret", "typed-pw", "cc-attr-secret"] {
        assert!(!html.contains(secret), "{secret} leaked: {html}");
    }
    assert!(!html.contains("value=\"m12\" selected"), "{html}");
    assert_eq!(res["redacted"], 3);
    // テキストはエスケープし、SVG の script は HTML の生テキスト扱いにしない（再パースで偽の要素を作らない）。
    assert!(html.contains("a &lt; b &amp; c"), "{html}");
    assert!(html.contains("if(1&lt;2){}"), "{html}");
    // 相対 URL の iframe の src は http(s) に解決されるため残す。
    assert!(html.contains("id=\"f3\" src=\"/rel\""), "{html}");
    // iframe の中身（srcdoc・data:・blob:）は出さず、数を返す（#1581 の引き継ぎ）。
    for secret in [
        "SRCDOC_SECRET",
        "DATAURL_SECRET",
        "BLOB_SECRET",
        "OBJECT_SECRET",
    ] {
        assert!(!html.contains(secret), "{secret} leaked: {html}");
    }
    assert_eq!(
        res["excluded"],
        serde_json::json!({"iframes": 4, "shadow_roots": 0})
    );
    // 2 回目は新しい識別子。
    assert_eq!(
        request(&mut h, 3, "dom_snapshot")["result"]["snapshot"],
        "s2"
    );

    h.send(r#"{"v":1,"id":4,"method":"shutdown"}"#);
    assert_eq!(h.recv(), r#"{"v":1,"id":4,"result":{}}"#);
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    for secret in ["typed-value", "pw-attr-secret", "SRCDOC_SECRET"] {
        assert!(!stderr.contains(secret), "{secret} in log: {stderr}");
    }
}

/// RENDER-11・TASK-121 の確定条件 3: ページ自身の `pushState` で doc_seq が上がり、page なしの
/// dom_snapshot で取り直せる。主フレームが `about:blank` へ移ったら invalid_state（#1581 の引き継ぎ）。
#[test]
fn interactive_dom_snapshot_follows_document_changes() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    let page = "<p id=x>x</p><script>setTimeout(function(){history.pushState({},'','/p2')},300);\
                setTimeout(function(){location='about:blank'},2500)</script>";
    m.insert("/p".to_owned(), ("text/html", page.as_bytes().to_vec()));
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, _) = spawn_interactive(proxy);
    h.send(&load_req(1, &format!("http://127.0.0.1:{port}/p"), 30_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    let first = r["result"]["page"]["doc_seq"].as_u64().unwrap();

    let mut id = 1;
    let start = Instant::now();
    let moved = loop {
        id += 1;
        let r = request(&mut h, id, "dom_snapshot");
        if r["result"]["url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/p2"))
        {
            break r;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "pushState not observed: {r}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        moved["result"]["page"]["doc_seq"].as_u64().unwrap() > first,
        "{moved}"
    );

    let start = Instant::now();
    loop {
        id += 1;
        let r = request(&mut h, id, "dom_snapshot");
        if r["error"]["kind"] == "invalid_state" {
            assert_eq!(
                r["error"]["message"],
                "Current document is not an http(s) page."
            );
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "about:blank not observed: {r}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    id += 1;
    h.send(&format!(r#"{{"v":1,"id":{id},"method":"shutdown"}}"#));
    let _ = h.recv();
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0);
}

/// RENDER-11: HTML が 8 MiB を超えるページは、切り詰めずに too_large。
#[test]
fn interactive_dom_snapshot_rejects_oversized_html() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    let mut page = String::from("<p>");
    page.push_str(&"a".repeat(8 * 1024 * 1024 + 16));
    page.push_str("</p>");
    m.insert("/big".to_owned(), ("text/html", page.into_bytes()));
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, _) = spawn_interactive(proxy);
    h.send(&load_req(
        1,
        &format!("http://127.0.0.1:{port}/big"),
        60_000,
    ));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
    let r = request(&mut h, 2, "dom_snapshot");
    assert_eq!(r["error"]["kind"], "too_large", "{r}");
    h.send(r#"{"v":1,"id":3,"method":"shutdown"}"#);
    let _ = h.recv();
    let (code, rest, _) = h.finish();
    assert_eq!(code, Some(0));
    assert_eq!(rest, 0);
}

// ---- navigator.webdriver の定義（SEC-5・TASK-119） ----

/// SEC-5: head 内の最初のインラインスクリプトの時点の検査結果を、`#wd` の `data-wd` に書く。
/// 順に: 値が `true`・再定義が `TypeError`・`delete` が `false`・記述子が getter だけ（setter なし・
/// `configurable: false`・`enumerable: true`）・個体に自前の項目なし・最後にもう一度 `true`。
const WEBDRIVER_PAGE: &str = concat!(
    "<!doctype html><html><head><script>",
    "var r=[navigator.webdriver===true];",
    "try{Object.defineProperty(Navigator.prototype,'webdriver',{get:function(){return false}});r.push(false)}",
    "catch(e){r.push(e instanceof TypeError)}",
    "r.push(delete Navigator.prototype.webdriver===false);",
    "var d=Object.getOwnPropertyDescriptor(Navigator.prototype,'webdriver');",
    "r.push(!!d&&typeof d.get==='function'&&d.set===undefined&&d.configurable===false&&d.enumerable===true);",
    "r.push(!Object.prototype.hasOwnProperty.call(navigator,'webdriver'));",
    "r.push(navigator.webdriver===true);",
    "window.__wd=r.join(',');",
    "</script></head><body>",
    "<div id=wd style=\"width:20px;height:20px;background:#0f0\"></div>",
    "<script>document.getElementById('wd').setAttribute('data-wd',window.__wd);</script>",
    "</body></html>"
);

/// `WEBDRIVER_PAGE` の検査がすべて成り立つときの `data-wd`。
const WEBDRIVER_ALL_TRUE: &str = "true,true,true,true,true,true";

/// SEC-5・TASK-119: 撮影（静的モード）と操作モードの両方のホストで、定義が最初のスクリプトから効き、消えない。
#[test]
fn navigator_webdriver_is_true_from_first_script_in_both_modes() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    m.insert(
        "/wd".to_owned(),
        ("text/html", WEBDRIVER_PAGE.as_bytes().to_vec()),
    );
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let url = format!("http://127.0.0.1:{port}/wd");

    // 撮影（静的モード）: 検査結果を書いた要素が、可視性の判定で見つかる。
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 1;
    load_page(&mut h, &mut id, &url);
    id += 1;
    h.send(&format!(
        r##"{{"v":1,"id":{id},"method":"visibility","params":{{"selector":"#wd[data-wd='{WEBDRIVER_ALL_TRUE}']"}}}}"##
    ));
    assert_eq!(
        h.recv(),
        format!(r#"{{"v":1,"id":{id},"result":{{"visible":true}}}}"#)
    );
    finish_host(h, id + 1);

    // 操作モード: DOM の取得に検査結果が出る。
    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, _hello) = spawn_interactive(proxy);
    h.send(&load_req(1, &url, 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
    let snap = request(&mut h, 2, "dom_snapshot");
    let html = snap["result"]["html"].as_str().unwrap();
    assert!(
        html.contains(&format!("data-wd=\"{WEBDRIVER_ALL_TRUE}\"")),
        "{html}"
    );
    finish_host(h, 3);
}

/// SEC-5・#1609（2026-11-19 の判断。案 2）: 操作モードでは、主フレームで `navigator.webdriver` が `true` でない
/// 文書（`head` の無い XHTML）を `load`・`dom_snapshot` で `unsupported` として断り、ページを捨てる。遷移した
/// 先の文書も `dom_snapshot` で断る。撮影（静的モード）では読み込める。
#[test]
fn interactive_mode_rejects_documents_without_navigator_webdriver() {
    const XHTML: &str = "application/xhtml+xml";
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    m.insert(
        "/nohead".to_owned(),
        (
            XHTML,
            b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>x</p></body></html>".to_vec(),
        ),
    );
    m.insert(
        "/head".to_owned(),
        (XHTML, b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><head><title>h</title></head><body><p>x</p></body></html>".to_vec()),
    );
    m.insert(
        "/override".to_owned(),
        ("text/html", b"<!doctype html><head><script>Object.defineProperty(navigator,'webdriver',{value:false})</script></head><p>x</p>".to_vec()),
    );
    m.insert(
        "/from".to_owned(),
        ("text/html", b"<!doctype html><button id=ps onclick=\"history.pushState(null,'','/from?x')\" style=\"display:block;width:200px;height:30px\">P</button><a id=go href=\"/nohead\" style=\"display:block;width:200px;height:30px\">go</a>".to_vec()),
    );
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let url = |path: &str| format!("http://127.0.0.1:{port}{path}");

    // 撮影（静的モード）: 断らない。
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 0;
    load_page(&mut h, &mut id, &url("/nohead"));
    finish_host(h, id + 1);

    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, _hello) = spawn_interactive(proxy);
    // head の無い XHTML は load で断り、ページを捨てる。
    h.send(&load_req(1, &url("/nohead"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["error"]["kind"], "unsupported", "{r}");
    let r = request(&mut h, 2, "dom_snapshot");
    assert_eq!(r["error"]["kind"], "not_loaded", "{r}");
    // head のある XHTML は使える。
    h.send(&load_req(3, &url("/head"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
    let r = request(&mut h, 4, "dom_snapshot");
    assert!(r["result"].is_object(), "{r}");
    // 遷移した先が head の無い XHTML なら、dom_snapshot で断ってページを捨てる。
    h.send(&load_req(5, &url("/from"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
    let snap = request(&mut h, 6, "dom_snapshot")["result"].clone();
    let html = snap["html"].as_str().unwrap().to_owned();
    // 同じ文書の中の URL の変更の後も、前の取得の要素へは操作を届けない（取得した文書の番号の照合）。
    let r = act_req(
        &mut h,
        20,
        &snap,
        node_of(&html, "ps"),
        r#"{"tag":"button","id":"ps"}"#,
        r#"{"kind":"click"}"#,
    );
    assert!(r["result"].is_object(), "{r}");
    let mut old = snap.clone();
    old["page"] = r["result"]["page"].clone();
    let a = act_req(
        &mut h,
        21,
        &old,
        node_of(&html, "ps"),
        r#"{"tag":"button","id":"ps"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(a["error"]["kind"], "stale", "{a}");
    let snap = request(&mut h, 22, "dom_snapshot")["result"].clone();
    let html = snap["html"].as_str().unwrap().to_owned();
    let r = act_req(
        &mut h,
        7,
        &snap,
        node_of(&html, "go"),
        r#"{"tag":"a","id":"go"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(r["result"]["navigated"], true, "{r}");
    let new_page = r["result"]["page"].to_string();
    h.send(&format!(
        r#"{{"v":1,"id":8,"method":"wait","params":{{"page":{new_page},"condition":{{"kind":"load_complete"}},"timeout_ms":5000}}}}"#
    ));
    let w: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(w["result"].is_object(), "{w}");
    // 遷移の前の取得の要素へは、新しい page と組んでも操作を届けない（確かめていない文書）。
    let mut old = snap.clone();
    old["page"] = w["result"]["page"].clone();
    let a = act_req(
        &mut h,
        9,
        &old,
        node_of(&html, "go"),
        r#"{"tag":"a","id":"go"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(a["error"]["kind"], "stale", "{a}");
    let r = request(&mut h, 10, "dom_snapshot");
    assert_eq!(r["error"]["kind"], "unsupported", "{r}");
    let r = request(&mut h, 11, "dom_snapshot");
    assert_eq!(r["error"]["kind"], "not_loaded", "{r}");
    // ページが navigator の個体を上書きして true でなくした文書も断る。
    h.send(&load_req(12, &url("/override"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["error"]["kind"], "unsupported", "{r}");
    finish_host(h, 13);
}

// ---- 操作モードの操作（act）と待機（wait）（RENDER-10・AISNAP-18・TASK-126） ----

/// 操作の試験ページ。結果は `#out` の文字列に書く（`dom_snapshot` で読む）。
const ACT_PAGE: &str = concat!(
    "<!doctype html><html><head><title>act</title><style>body{margin:0}button,input,select{display:block;width:200px;height:30px}</style></head><body>",
    "<p id=out>none</p>",
    "<button id=b onclick=\"document.getElementById('out').textContent='clicked:'+event.isTrusted\">B</button>",
    "<input id=q name=q value=old onkeydown=\"if(event.key==='Enter')document.getElementById('out').textContent='enter:'+this.value\">",
    "<input id=jump oninput=\"document.getElementById('out').textContent='jump:'+this.value;document.getElementById('sink').focus()\">",
    "<input id=sink oninput=\"document.getElementById('out').textContent='sink:'+this.value\">",
    "<select id=s name=s onchange=\"document.getElementById('out').textContent='sel:'+this.value\"><option value=a selected>A</option><option value=b>B</option><option value=c disabled>C</option></select>",
    "<button id=pop onclick=\"window.open('/x')\">P</button>",
    "<div style=\"position:relative;width:200px;height:30px\"><input id=fq onkeydown=\"document.getElementById('out').textContent='fq'\"></div>",
    "<button id=coverfq onclick=\"var f=document.getElementById('fq');var d=document.createElement('div');d.style.cssText='position:absolute;left:0;top:0;width:200px;height:30px;background:#fff';f.parentNode.appendChild(d);f.focus()\">V</button>",
    "<iframe id=fr srcdoc=\"<p>f</p>\" style=\"width:100px;height:30px\"></iframe>",
    "<button id=frnav onclick=\"document.getElementById('fr').src='/act?frame';document.getElementById('out').textContent='frnav'\">N</button>",
    "<button id=dis disabled onclick=\"document.getElementById('out').textContent='dis'\">D</button>",
    "<div style=\"position:relative;width:200px;height:30px\"><button id=cov onclick=\"document.getElementById('out').textContent='cov'\">C</button>",
    "<div style=\"position:absolute;left:0;top:0;width:200px;height:30px;background:#fff\"></div></div>",
    "<div style=\"position:relative;width:200px;height:30px\"><button id=ifr onclick=\"document.getElementById('out').textContent='ifr'\">I</button>",
    "<iframe srcdoc=\"<p>x</p>\" style=\"position:absolute;left:0;top:0;width:200px;height:30px;border:0\"></iframe></div>",
    "<div style=\"position:relative;width:200px;height:30px\"><input id=covq value=keep>",
    "<div style=\"position:absolute;left:0;top:0;width:200px;height:30px;background:#fff\"></div></div>",
    "<select id=hid style=\"display:none\"><option value=a>A</option></select>",
    "<button id=edge style=\"position:fixed;left:300px;top:790px\" onclick=\"document.getElementById('out').textContent='edge'\">E</button>",
    "<button id=far style=\"margin-top:3000px\" onclick=\"document.getElementById('out').textContent='far'\">F</button>",
    "<script>setTimeout(function(){var d=document.createElement('div');d.id='late';document.body.appendChild(d);},300);</script>",
    "</body></html>"
);

/// `dom_snapshot` の HTML から、属性 `id="<id>"` を持つ要素の番号を引く。
fn node_of(html: &str, id: &str) -> u64 {
    let needle = format!(" id=\"{id}\"");
    html.split('<')
        .find(|tag| tag.split('>').next().is_some_and(|t| t.contains(&needle)))
        .and_then(|tag| tag.split("data-fandhe-n=\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no element with id {id}: {html}"))
}

/// 操作モードのホストでページを読み込み、`dom_snapshot` を取る。戻り値は (ホスト, 次の id, 取得結果)。
fn interactive_page(url: &str, proxy: u16) -> (Host, u64, serde_json::Value) {
    let (mut h, hello) = spawn_interactive(proxy);
    let caps = hello["data"]["capabilities"].as_array().unwrap();
    assert!(caps.contains(&serde_json::json!("act")), "{hello}");
    assert!(caps.contains(&serde_json::json!("wait")), "{hello}");
    assert_eq!(hello["data"]["limits"]["act_timeout_ms"], 10_000);
    h.send(&load_req(1, url, 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
    let snap = request(&mut h, 2, "dom_snapshot");
    (h, 3, snap["result"].clone())
}

/// `act` を送って応答を返す。
fn act_req(
    h: &mut Host,
    id: u64,
    snap: &serde_json::Value,
    node: u64,
    expect: &str,
    action: &str,
) -> serde_json::Value {
    let page = snap["page"].to_string();
    let s = snap["snapshot"].as_str().unwrap();
    h.send(&format!(
        r#"{{"v":1,"id":{id},"method":"act","params":{{"page":{page},"snapshot":"{s}","node":{node},"expect":{expect},"action":{action}}}}}"#
    ));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(r["id"], id, "{r}");
    r
}

fn out_text(h: &mut Host, id: u64) -> (String, serde_json::Value) {
    let snap = request(h, id, "dom_snapshot");
    let html = snap["result"]["html"].as_str().unwrap().to_owned();
    let start = html
        .find(" id=\"out\">")
        .map(|i| i + " id=\"out\">".len())
        .unwrap();
    let text = html[start..].split('<').next().unwrap().to_owned();
    (text, snap["result"].clone())
}

/// RENDER-10・TASK-126: クリック・文字入力・選択・キーを入力イベント（`isTrusted`）で渡し、照合値・`page`・
/// `snapshot` が違えば `stale`、`disabled`・他の要素や iframe に隠れた要素は `not_actionable`。画面の外の要素は
/// スクロールして押す。
#[test]
fn interactive_act_operates_elements_and_rejects_unsafe_targets() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    m.insert(
        "/act".to_owned(),
        ("text/html", ACT_PAGE.as_bytes().to_vec()),
    );
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, mut id, mut snap) =
        interactive_page(&format!("http://127.0.0.1:{port}/act"), proxy);
    let html = snap["html"].as_str().unwrap().to_owned();
    let ok = |r: &serde_json::Value| {
        assert!(r["result"]["page"].is_object(), "{r}");
        assert_eq!(r["result"]["navigated"], false, "{r}");
    };
    let kind = |r: &serde_json::Value| r["error"]["kind"].as_str().unwrap_or("").to_owned();

    // クリック（入力イベントなので isTrusted は true）。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "b"),
        r#"{"tag":"button","id":"b"}"#,
        r#"{"kind":"click"}"#,
    );
    ok(&r);
    id += 1;
    let old = snap.clone();
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "clicked:true");
    }
    id += 1;

    // 取り直した後は、前の取得の snapshot では stale。
    let r = act_req(
        &mut h,
        id,
        &old,
        node_of(&html, "b"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(kind(&r), "stale", "{r}");
    id += 1;

    // 文字入力（既存の値を消してから入れる）→ Enter。
    let q = node_of(&html, "q");
    let r = act_req(
        &mut h,
        id,
        &snap,
        q,
        r#"{"tag":"input","name":"q"}"#,
        r#"{"kind":"fill","text":"hi あ","sensitive":false}"#,
    );
    ok(&r);
    id += 1;
    let r = act_req(
        &mut h,
        id,
        &snap,
        q,
        r#"{"tag":"input"}"#,
        r#"{"kind":"press","key":"Enter"}"#,
    );
    ok(&r);
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "enter:hi あ");
    }
    id += 1;

    // 文字入力の途中でページがフォーカスを移すと、残りの文字は移った先へ入りうる（既知の制約。設計 §14.4。
    // どちらへ入るかは入力の処理の時機による）。最後のフォーカスの確認で外れたことを `not_actionable` で返す。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "jump"),
        r#"{"tag":"input"}"#,
        r#"{"kind":"fill","text":"abc"}"#,
    );
    assert_eq!(kind(&r), "not_actionable", "{r}");
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert!(
            matches!(t.as_str(), "jump:abc" | "sink:bc" | "sink:c"),
            "{t}"
        );
    }
    id += 1;

    // 選択。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "s"),
        r#"{"tag":"select"}"#,
        r#"{"kind":"select","values":["b"]}"#,
    );
    ok(&r);
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "sel:b");
    }
    id += 1;

    // 選択: 一致する選択肢が無い・単一選択に 2 つ・表示されていない select は選ばない。
    for (target, values, want) in [
        ("s", r#"["zz"]"#, "not_actionable"),
        ("s", r#"["c"]"#, "not_actionable"),
        ("s", r#"["a","b"]"#, "invalid_params"),
        ("hid", r#"["a"]"#, "not_actionable"),
    ] {
        let r = act_req(
            &mut h,
            id,
            &snap,
            node_of(&html, target),
            r#"{"tag":"select"}"#,
            &format!(r#"{{"kind":"select","values":{values}}}"#),
        );
        assert_eq!(kind(&r), want, "{target} {values}: {r}");
        id += 1;
    }

    // 新しいウィンドウは開かず unsupported。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "pop"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(kind(&r), "unsupported", "{r}");
    id += 1;

    // iframe の遷移を許可しても、読み込みを待ち続けない（期限までに返る）。ホストの文書番号はフレームを区別せず
    // 上がるため、navigated は真になりうる（取り直して続ける）。
    let started = std::time::Instant::now();
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "frnav"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    assert!(r["result"]["page"].is_object(), "{r}");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "frnav");
    }
    id += 1;

    // フォーカスがあっても、覆われた入力欄へはキーを送らない（覆ってからフォーカスを戻すボタンで作る）。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "coverfq"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    ok(&r);
    id += 1;
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "fq"),
        r#"{"tag":"input"}"#,
        r#"{"kind":"press","key":"Enter"}"#,
    );
    assert_eq!(kind(&r), "not_actionable", "{r}");
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_ne!(t, "fq", "a key reached a covered input");
    }
    id += 1;
    // 覆いの要素が増えて番号がずれたため、以後は取り直した HTML から番号を引く。
    let html = snap["html"].as_str().unwrap().to_owned();

    // 隠れた入力欄は、値を消す前に断る（既存の値が残る）。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "covq"),
        r#"{"tag":"input"}"#,
        r#"{"kind":"fill","text":"zz"}"#,
    );
    assert_eq!(kind(&r), "not_actionable", "{r}");
    id += 1;
    {
        let (_, s2) = out_text(&mut h, id);
        snap = s2;
        assert!(
            snap["html"]
                .as_str()
                .unwrap()
                .contains(r#"id="covq" value="keep""#),
            "{}",
            snap["html"]
        );
    }
    id += 1;

    // 画面の下端にまたがる要素は、画面の中に入っている部分の中心を押す（判定した点と同じ点）。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "edge"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    ok(&r);
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "edge");
    }
    id += 1;

    // 画面の外の要素はスクロールして押す。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "far"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    ok(&r);
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "far");
    }
    id += 1;

    // 押さない: disabled・他の要素に隠れている・iframe に隠れている。
    for (target, want) in [
        ("dis", "not_actionable"),
        ("cov", "not_actionable"),
        ("ifr", "not_actionable"),
    ] {
        let r = act_req(
            &mut h,
            id,
            &snap,
            node_of(&html, target),
            r#"{"tag":"button"}"#,
            r#"{"kind":"click"}"#,
        );
        assert_eq!(kind(&r), want, "{target}: {r}");
        id += 1;
    }
    assert_eq!(
        out_text(&mut h, id).0,
        "far",
        "a rejected target must not be clicked"
    );
    id += 1;

    // stale: 照合値の違い・番号の範囲外・page の違い。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "b"),
        r#"{"tag":"input"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(kind(&r), "stale", "{r}");
    id += 1;
    let r = act_req(
        &mut h,
        id,
        &snap,
        199_999,
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(kind(&r), "stale", "{r}");
    id += 1;
    let mut other = snap.clone();
    other["page"]["doc_seq"] = 999.into();
    let r = act_req(
        &mut h,
        id,
        &other,
        node_of(&html, "b"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(kind(&r), "stale", "{r}");
    id += 1;
    // 解釈できないキーは invalid_params。
    let r = act_req(
        &mut h,
        id,
        &snap,
        q,
        r#"{"tag":"input"}"#,
        r#"{"kind":"press","key":"NotAKey"}"#,
    );
    assert_eq!(kind(&r), "invalid_params", "{r}");
    id += 1;

    // 待機: 遅れて現れる要素・すでに成立している条件・時間切れ・不正なセレクタ。
    let page = snap["page"].to_string();
    let wait = |h: &mut Host, id: u64, cond: &str, ms: u64| -> serde_json::Value {
        h.send(&format!(
            r#"{{"v":1,"id":{id},"method":"wait","params":{{"page":{page},"condition":{cond},"timeout_ms":{ms}}}}}"#
        ));
        serde_json::from_str(&h.recv()).unwrap()
    };
    let r = wait(
        &mut h,
        id,
        r##"{"kind":"selector_present","selector":"#late"}"##,
        5000,
    );
    assert!(r["result"]["page"].is_object(), "{r}");
    id += 1;
    let r = wait(&mut h, id, r#"{"kind":"text_present","text":"far"}"#, 1000);
    assert!(r["result"]["page"].is_object(), "{r}");
    id += 1;
    let r = wait(&mut h, id, r#"{"kind":"load_complete"}"#, 1000);
    assert!(r["result"]["page"].is_object(), "{r}");
    id += 1;
    let r = wait(
        &mut h,
        id,
        r##"{"kind":"selector_absent","selector":"#late"}"##,
        300,
    );
    assert_eq!(kind(&r), "timeout", "{r}");
    id += 1;
    let r = wait(
        &mut h,
        id,
        r#"{"kind":"selector_present","selector":"[["}"#,
        1000,
    );
    assert_eq!(kind(&r), "invalid_params", "{r}");
    id += 1;

    h.send(&format!(r#"{{"v":1,"id":{id},"method":"shutdown"}}"#));
    let _ = h.recv();
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0);
    assert!(
        !stderr.contains('あ'),
        "input text leaked to the log: {stderr}"
    );
}

/// `RENDER-10`・TASK-126: 操作モードで Cookie（永続の `max-age`）を作って操作しても、ホストの終了後にその値を含むファイルが
/// ホームや一時ディレクトリに残らない（`temporary_storage`。操作モードでは永続 Cookie を使わない。
/// `rendering-layer.md` の判断 5）。HOME・TMPDIR・XDG の各ディレクトリを空の場所へ向けて確かめる。
#[test]
fn interactive_cookies_do_not_outlive_the_host() {
    let marker = format!("cookie-marker-{}", std::process::id());
    let root = std::env::temp_dir().join(format!("fandhe-host-cookie-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dirs: Vec<(&str, std::path::PathBuf)> = [
        "HOME",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ]
    .into_iter()
    .map(|k| (k, root.join(k.to_ascii_lowercase())))
    .collect();
    for (_, d) in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }
    let page = format!(
        "<!doctype html><html><body><script>document.cookie='k={marker}; max-age=3600; path=/';</script>\
<input id=q name=q style=\"display:block;width:200px;height:30px\"></body></html>"
    );
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    m.insert("/c".to_owned(), ("text/html", page.into_bytes()));
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();
    let envs: Vec<(&str, &std::path::Path)> = dirs.iter().map(|(k, d)| (*k, d.as_path())).collect();
    let (mut h, _hello) = spawn_interactive_with_env(proxy, &envs);
    h.send(&load_req(1, &format!("http://127.0.0.1:{port}/c"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");
    let snap = request(&mut h, 2, "dom_snapshot")["result"].clone();
    let html = snap["html"].as_str().unwrap().to_owned();
    let r = act_req(
        &mut h,
        3,
        &snap,
        node_of(&html, "q"),
        r#"{"tag":"input"}"#,
        r#"{"kind":"fill","text":"x"}"#,
    );
    assert!(r["result"].is_object(), "{r}");
    finish_host(h, 4);

    fn scan(dir: &std::path::Path, marker: &str, hits: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                scan(&p, marker, hits);
            } else if std::fs::read(&p)
                .is_ok_and(|b| b.windows(marker.len()).any(|w| w == marker.as_bytes()))
            {
                hits.push(p);
            }
        }
    }
    let mut hits = Vec::new();
    scan(&root, &marker, &mut hits);
    let _ = std::fs::remove_dir_all(&root);
    assert!(hits.is_empty(), "cookie value persisted: {hits:?}");
}

/// RENDER-10・TASK-126: クリックで遷移したら `navigated` が真になり、`page` の `doc_seq` が進む。
#[test]
fn interactive_act_reports_navigation() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    m.insert(
        "/from".to_owned(),
        ("text/html", b"<!doctype html><a id=go href=\"/to\" style=\"display:block;width:200px;height:30px\">go</a>".to_vec()),
    );
    m.insert(
        "/to".to_owned(),
        ("text/html", b"<!doctype html><p id=arrived>ok</p>".to_vec()),
    );
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, id, snap) = interactive_page(&format!("http://127.0.0.1:{port}/from"), proxy);
    let html = snap["html"].as_str().unwrap().to_owned();
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "go"),
        r#"{"tag":"a","id":"go"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(r["result"]["navigated"], true, "{r}");
    let before = snap["page"]["doc_seq"].as_u64().unwrap();
    assert!(
        r["result"]["page"]["doc_seq"].as_u64().unwrap() > before,
        "{r}"
    );
    // 遷移の後は、返った page で待てる。前の page では待てない。
    let new_page = r["result"]["page"].to_string();
    h.send(&format!(
        r#"{{"v":1,"id":{},"method":"wait","params":{{"page":{new_page},"condition":{{"kind":"load_complete"}},"timeout_ms":5000}}}}"#,
        id + 10
    ));
    let w: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(w["result"]["page"].is_object(), "{w}");
    let page = snap["page"].to_string();
    h.send(&format!(
        r#"{{"v":1,"id":{},"method":"wait","params":{{"page":{page},"condition":{{"kind":"load_complete"}},"timeout_ms":5000}}}}"#,
        id + 1
    ));
    let w: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert_eq!(w["error"]["kind"], "stale", "{w}");
    finish_host(h, id + 2);
}

/// RENDER-10・TASK-126（#1592 の残り）: フォーカスの無い欄への `press` はクリックしてから送る、折り返したリンクは
/// 最初の行を押す、遷移先の応答が 2 秒以上来なければ待ちを打ち切って `navigated` を偽で返す、主フレームが
/// `about:blank` へ移った後の `act` は `invalid_state`。
#[test]
fn interactive_act_edge_paths() {
    let (l, port) = bind_loopback();
    let out = "document.getElementById('out').textContent";
    let page = format!(
        "<!doctype html><html><head><style>body{{margin:0}}input,button{{display:block;width:200px;height:30px}}</style></head><body>\
<p id=out>none</p>\
<input id=nf onkeydown=\"{out}='nf:'+event.key+':'+(document.activeElement===this)\">\
<div style=\"width:90px;font:16px monospace\">xxxxx <a id=wrap href=\"#\" onclick=\"var r=this.getClientRects();{out}='wrap:'+r.length+':'+(event.clientY>=r[0].top&&event.clientY<r[0].bottom);return false\">aaa bbb</a></div>\
<a id=slow href=\"/slow\" style=\"display:block;width:200px;height:30px\">slow</a>\
</body></html>"
    );
    let mut m = std::collections::HashMap::new();
    m.insert("/edge".to_owned(), ("text/html", page.into_bytes()));
    m.insert(
        "/slow".to_owned(),
        (
            SLOW_HTML,
            b"<!doctype html><button id=blank style=\"display:block;width:200px;height:30px\" onclick=\"location='about:blank'\">B</button>".to_vec(),
        ),
    );
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let (proxy, _pstop) = serve_forwarding_proxy();
    let (mut h, mut id, mut snap) =
        interactive_page(&format!("http://127.0.0.1:{port}/edge"), proxy);
    let html = snap["html"].as_str().unwrap().to_owned();

    // フォーカスが無ければクリックでフォーカスを移してからキーを送る。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "nf"),
        r#"{"tag":"input"}"#,
        r#"{"kind":"press","key":"a"}"#,
    );
    assert_eq!(r["result"]["navigated"], false, "{r}");
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        assert_eq!(t, "nf:a:true");
    }
    id += 1;

    // 行の途中から始まって折り返したリンク（外接矩形の中心はリンクの文字から外れる）も、最初の行の矩形の中心で押せる。
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "wrap"),
        r#"{"tag":"a","id":"wrap"}"#,
        r#"{"kind":"click"}"#,
    );
    assert!(r["result"]["page"].is_object(), "{r}");
    id += 1;
    {
        let (t, s2) = out_text(&mut h, id);
        snap = s2;
        // 2 行に折り返していて、押した点は最初の行の矩形の中。
        assert_eq!(t, "wrap:2:true");
    }
    id += 1;

    // 遷移先の応答が来ない: 2 秒で待ちを打ち切り、navigated は偽（文書番号はまだ変わっていない）。応答は `act` が
    // 戻ってから解放する。
    let started = std::time::Instant::now();
    let r = act_req(
        &mut h,
        id,
        &snap,
        node_of(&html, "slow"),
        r#"{"tag":"a","id":"slow"}"#,
        r#"{"kind":"click"}"#,
    );
    let took = started.elapsed();
    let since_request = SLOW_ARRIVED
        .lock()
        .unwrap()
        .expect("navigation request reached the server")
        .elapsed();
    assert_eq!(r["result"]["navigated"], false, "{r}");
    assert_eq!(r["result"]["page"], snap["page"], "{r}");
    // 下限: 全体で待ちの打ち切り（2 秒）以上。上限: 遷移の要求が届いてから応答までを測り（クリックの処理を外す）、
    // 打ち切りの 2 秒に応答の IPC の余裕を足した値未満（打ち切りが延びる回帰を拾う）。
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(
        since_request < Duration::from_millis(3500),
        "{since_request:?} (total {took:?})"
    );
    SLOW_RELEASE.store(true, std::sync::atomic::Ordering::SeqCst);
    id += 1;

    // 遅れて届いた文書へ移ったら、その文書で about:blank へ移らせる。
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let slow = loop {
        let s = request(&mut h, id, "dom_snapshot");
        id += 1;
        if s["result"]["url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/slow"))
        {
            break s["result"].clone();
        }
        assert!(std::time::Instant::now() < deadline, "{s}");
        std::thread::sleep(Duration::from_millis(200));
    };
    let shtml = slow["html"].as_str().unwrap().to_owned();
    let r = act_req(
        &mut h,
        id,
        &slow,
        node_of(&shtml, "blank"),
        r#"{"tag":"button","id":"blank"}"#,
        r#"{"kind":"click"}"#,
    );
    assert!(r["result"]["page"].is_object(), "{r}");
    id += 1;
    // クリックは 1 回だけ。about:blank へ移ったことは読み取りだけの dom_snapshot で待つ。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = request(&mut h, id, "dom_snapshot");
        id += 1;
        if r["error"]["kind"] == "invalid_state" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{r}");
        std::thread::sleep(Duration::from_millis(200));
    }
    // about:blank の上では操作しない（前の取得の page・snapshot のままでも、照合より先に invalid_state で断る）。
    let r = act_req(
        &mut h,
        id,
        &slow,
        node_of(&shtml, "blank"),
        r#"{"tag":"button"}"#,
        r#"{"kind":"click"}"#,
    );
    assert_eq!(r["error"]["kind"], "invalid_state", "{r}");
    finish_host(h, id + 1);
}

// ---- ページの alert()（#1597） ----

/// `RENDER-3`・`RENDER-4`・#1597・設計 §2.1: ページが `alert()` を呼んでも読み込みは止まらず、Servo が書く文字列はフレームの流れにも
/// ログにも出ない（stdout はフレーム専用のまま）。Windows は付け替えないため対象外（既知の制約）。
#[cfg(unix)]
#[test]
fn page_alert_does_not_block_load_or_pollute_stdout() {
    let (l, port) = bind_loopback();
    let mut m = std::collections::HashMap::new();
    let page = concat!(
        "<!doctype html><html><head><script>alert('ALERT_MARKER_1597')</script></head>",
        "<body style=\"margin:0;background:#00ff00\"><script>alert('SECOND_ALERT_1597')</script></body></html>"
    );
    m.insert("/alert".to_owned(), ("text/html", page.as_bytes().to_vec()));
    let _stop = serve_routes(l, std::sync::Arc::new(m));
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();
    let mut id = 1;
    load_page(&mut h, &mut id, &format!("http://127.0.0.1:{port}/alert"));
    let img = shoot(&mut h, &mut id);
    assert!(fraction(&img, 0, img.width(), is_green) > 0.9);
    // 同じホストで続けて読み込める。
    load_page(&mut h, &mut id, &format!("http://127.0.0.1:{port}/alert"));
    id += 1;
    h.send(&format!(r#"{{"v":1,"id":{id},"method":"shutdown"}}"#));
    assert_eq!(h.recv(), format!(r#"{{"v":1,"id":{id},"result":{{}}}}"#));
    let (code, rest, stderr) = h.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(rest, 0, "stdout must contain frames only");
    assert!(!stderr.contains("ALERT_MARKER_1597"), "{stderr}");
    assert!(!stderr.contains("SECOND_ALERT_1597"), "{stderr}");
}

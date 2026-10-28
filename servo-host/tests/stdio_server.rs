//! `--stdio` の実バイナリを子プロセスとして起動するブラックボックス結合テスト
//! （`RENDER-3`・`RENDER-4`・`PLUG-1`・`SEC-2`・#993・#231）。
//!
//! フレーム（u32 LE 長さ接頭辞 + UTF-8 JSON）を対話的に読み書きし、hello・`load`（ループバック
//! HTTP サーバー相手。外部ネットワークへ出ない）・`screenshot`（PNG の復号と上下の色。#232）・
//! 未実装メソッドの `unsupported`・`shutdown`・プロトコル違反時の終了コード・stdout の純粋性・
//! ログの秘匿・IntersectionObserver が無効のままであること（#1291・#1297。servo 0.7.0 で有効に戻すときに
//! 公開とコールバックを確かめる形へ戻す）を確かめる。このテストバイナリ自体は Servo をリンクしない（`image` は PNG の復号のみ）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
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

/// IntersectionObserver 用の fixture 本文（#1294・#1291・#1297）。`#exposed` は未公開なら非表示になり、
/// `#fired` は意味のある entry でコールバックが呼ばれたときだけ表示される。servo 0.6.0 では無効の
/// ままなので、現在は `#exposed` が `display:none` になり `#fired` も表示されない（0.7.0 で有効に戻す
/// ときに期待値を元の形へ戻す）。
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

/// RENDER-3・RENDER-5（TASK-38・#1291・#1297）: servo 0.6.0 では IntersectionObserver が無効の
/// ままである。有効にすると Script スレッドがハングする（上流 servo/servo#45915）ため無効に戻した。
/// 未公開なら fixture の `#exposed` が `display:none` になり、`#fired` も表示されない。
/// 0.7.0 で有効に戻すときは、公開され load 後にコールバックが呼ばれることを確かめる元の形
/// （`#exposed` が可視・`#fired` が有限回の再試行内に可視）へ戻す。ホストに JS 評価メソッドは
/// 無いため、ページ JS の副作用を `visibility` で観測する。外部ネットワークへは出ない（#1003）。
#[test]
fn intersection_observer_stays_disabled_on_servo_0_6() {
    let (port, srv, stop) = serve_http_with(IO_BODY, false);
    let mut h = spawn(&["--stdio"]);
    let _hello = h.recv();

    h.send(&load_req(1, &format!("http://127.0.0.1:{port}/io"), 20_000));
    let r: serde_json::Value = serde_json::from_str(&h.recv()).unwrap();
    assert!(r["result"].is_object(), "{r}");

    let vis = |id: u64, sel: &str| {
        format!(r#"{{"v":1,"id":{id},"method":"visibility","params":{{"selector":"{sel}"}}}}"#)
    };
    // 未公開の分岐は `display:none` にするため、不可視で理由は `display_none`。
    h.send(&vis(2, "#exposed"));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":2,"result":{"visible":false,"reason":"display_none"}}"#
    );
    // コールバックが呼ばれる経路が無いため、`#fired` は初期の `display:none` のまま。
    h.send(&vis(3, "#fired"));
    assert_eq!(
        h.recv(),
        r#"{"v":1,"id":3,"result":{"visible":false,"reason":"display_none"}}"#
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

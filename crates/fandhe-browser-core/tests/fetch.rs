//! `fandhe_browser_core::fetch`（TASK-24.2・#36・ビヘイビア `CORE-1`）の
//! 受け入れ基準を検証する結合テスト。
//!
//! 外部ネットワークには依存せず、`127.0.0.1` のループバックへ最小の
//! HTTP/1.1 応答を返すテストサーバーだけを使う（CI の決定性のため）。
//! `parse` モジュール（#38）等の網羅的なケースは #37（TASK-24.3）に委ね、
//! ここでは受け入れ基準（タイムアウト・リダイレクト上限・本文上限・
//! scheme 拒否・正常系）の最低限を確認する。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use fandhe_browser_core::error::Error;
use fandhe_browser_core::fetch::{FetchOptions, Fetcher};

/// 接続からリクエストヘッダ（`\r\n\r\n` まで）を読み捨てる。
///
/// クライアント（reqwest）がリクエストを送り切る前に応答を書き始めると
/// 一部プラットフォームで接続がリセットされ得るため、各テストサーバーは
/// 応答前に必ずこれを呼ぶ。
fn drain_request_head(stream: &mut TcpStream) {
    let mut buf = [0u8; 1024];
    let mut data = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if data.len() > 8192 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// `127.0.0.1` の空きポートへ bind し、接続ごとに `handler` を新しい
/// スレッドで呼び出すループバックサーバーを起動する。戻り値は bind した
/// ポート番号（`http://127.0.0.1:<port>/` の組み立てに使う）。
fn spawn_loopback_server<F>(handler: F) -> u16
where
    F: Fn(TcpStream) + Send + 'static + Clone,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            thread::spawn(move || handler(stream));
        }
    });
    port
}

/// CORE-1（#36）: 全体タイムアウトを超過すると `Error::Timeout` を返す。
/// サーバーは接続だけ受けて応答を返さず、クライアントのタイムアウト
/// （200ms）より十分長く（3 秒）沈黙する。
#[tokio::test]
async fn core_1_fetch_errors_on_timeout() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        thread::sleep(Duration::from_secs(3));
    });

    let options = FetchOptions::new().with_timeout(Duration::from_millis(200));
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("応答なしのサーバーはタイムアウトになるはず");
    assert!(
        matches!(err, Error::Timeout { limit } if limit == Duration::from_millis(200)),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: リダイレクト回数が上限を超えると `Error::TooManyRedirects`
/// を返す。サーバーは自分自身への `302` を返し続ける。
#[tokio::test]
async fn core_1_fetch_errors_on_too_many_redirects() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                drain_request_head(&mut stream);
                let body = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(body.as_bytes());
            });
        }
    });

    let options = FetchOptions::new().with_max_redirects(3);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("無限リダイレクトは上限で打ち切られるはず");
    assert!(
        matches!(err, Error::TooManyRedirects { limit: 3 }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: `Content-Length` ヘッダが上限を超える場合、本文を読まずに
/// `Error::ResponseTooLarge` を返す（事前検査）。
#[tokio::test]
async fn core_1_fetch_rejects_oversized_content_length() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        // 宣言だけで実際の本文は送らない（事前検査は本文を読まずに失敗する
        // ことの確認が目的のため）。
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
    });

    let options = FetchOptions::new().with_max_body_bytes(1024);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("Content-Length が上限超過なら Err になるはず");
    assert!(
        matches!(err, Error::ResponseTooLarge { limit: 1024 }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: `Content-Length` が無い（chunked でも無い）応答でも、
/// ストリーミング読み込み中の逐次検査で `Error::ResponseTooLarge` を返す。
#[tokio::test]
async fn core_1_fetch_rejects_oversized_streamed_body() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        // Content-Length ヘッダを付けず、上限（1024 バイト）を超える本文を
        // 送ってから接続を閉じる（close-delimited body）。
        let body = vec![b'a'; 4096];
        let _ = stream.write_all(&body);
    });

    let options = FetchOptions::new().with_max_body_bytes(1024);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("ヘッダ欠如でもストリーミング検査で上限超過になるはず");
    assert!(
        matches!(err, Error::ResponseTooLarge { limit: 1024 }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: `http`/`https` 以外の scheme は送信前に拒否する
/// （security.md「SSRF」）。
#[tokio::test]
async fn core_1_fetch_rejects_non_http_scheme() {
    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get("file:///etc/passwd")
        .await
        .expect_err("file: scheme は拒否されるはず");
    assert!(
        matches!(err, Error::DisallowedScheme { ref scheme } if scheme == "file"),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: 200 応答は status・body・content_type を具体値で返す。
/// 404 応答も `Err` にならず `Ok` で返る（4xx/5xx をエラー扱いしない設計）。
#[tokio::test]
async fn core_1_fetch_ok_returns_status_and_body() {
    let ok_port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let body = b"hello world";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body);
    });
    let not_found_port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
    });

    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{ok_port}/"))
        .await
        .expect("200 応答は Ok になるはず");
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.body(), b"hello world");
    assert_eq!(ok.body_text_lossy(), "hello world");
    assert_eq!(ok.content_type(), Some("text/plain"));
    assert_eq!(ok.final_url(), format!("http://127.0.0.1:{ok_port}/"));

    let not_found = fetcher
        .get(&format!("http://127.0.0.1:{not_found_port}/"))
        .await
        .expect("404 応答も Ok になるはず（4xx/5xx はエラー扱いしない）");
    assert_eq!(not_found.status(), 404);
    assert_eq!(not_found.body(), b"");
}

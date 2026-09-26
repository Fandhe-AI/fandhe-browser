//! `fandhe_browser_core::fetch`（TASK-24.2・#36・ビヘイビア `CORE-1`）の
//! 受け入れ基準を検証する結合テスト。
//!
//! 外部ネットワークには依存せず、`127.0.0.1` のループバックへ最小の
//! HTTP/1.1 応答を返すテストサーバーだけを使う（CI の決定性のため）。
//! `parse` モジュール（#38）等の網羅的なケースは #37（TASK-24.3）に委ね、
//! ここでは受け入れ基準（タイムアウト・リダイレクト上限・本文上限・
//! scheme 拒否・内部アドレス拒否・正常系）の最低限を確認する。
//!
//! ループバック（`127.0.0.1`）はそれ自体が内部アドレスであるため、既定
//! （`FetchOptions::allow_private_network_access == false`）ではテスト
//! サーバーへの接続自体が拒否されてしまう。ループバック接続の正常系を
//! 検証するテストはすべて [`loopback_allowed_options`] で明示的に
//! opt-in する（内部アドレス拒否そのものを検証するテストは opt-in せず、
//! 既定値のまま `Error::DisallowedAddress` を確認する）。

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

/// ループバック（内部アドレス）への接続を明示的に許可した [`FetchOptions`]
/// を返す。本ファイルのテストサーバーはすべて `127.0.0.1` で待ち受けるため、
/// SSRF 拒否そのものを検証するテスト以外はこれを起点に組み立てる。
fn loopback_allowed_options() -> FetchOptions {
    FetchOptions::new().with_allow_private_network_access(true)
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

    let options = loopback_allowed_options().with_timeout(Duration::from_millis(200));
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

    let options = loopback_allowed_options().with_max_redirects(3);
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

    let options = loopback_allowed_options().with_max_body_bytes(1024);
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

    let options = loopback_allowed_options().with_max_body_bytes(1024);
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

/// CORE-1（#36）: リダイレクト先が `http`/`https` 以外の scheme（`file:`）
/// になる場合、要求前の検査だけでなくリダイレクト経由の経路でも
/// `Error::DisallowedScheme` を返す（モジュール doc「要求前・リダイレクト先の
/// 両方で拒否する」の後半を検証する）。
#[tokio::test]
async fn core_1_fetch_rejects_disallowed_scheme_via_redirect() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let body = "HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(body.as_bytes());
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("file: へのリダイレクトは拒否されるはず");
    assert!(
        matches!(err, Error::DisallowedScheme { ref scheme } if scheme == "file"),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: リダイレクト先が authority を持つ非 http/https scheme
/// （`ftp://127.0.0.1:1/`）の場合、`Fetcher::new` の `Policy::custom`
/// （scheme 拒否分岐・`RedirectMarker::DisallowedScheme` の downcast 経路）が
/// 実際に通ることを確認する。`file:///...`（authority を持たない）は
/// `reject_unresolved_disallowed_redirect` 側の未解決経路で処理されるため
/// この経路を検証できない（fetch.rs モジュール doc のコメント参照）。
#[tokio::test]
async fn core_1_fetch_rejects_disallowed_scheme_via_redirect_policy() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let body = "HTTP/1.1 302 Found\r\nLocation: ftp://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(body.as_bytes());
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("ftp: へのリダイレクトは Policy::custom で拒否されるはず");
    assert!(
        matches!(err, Error::DisallowedScheme { ref scheme } if scheme == "ftp"),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: `max_redirects` ちょうどの回数までは追跡に成功し `Ok` を
/// 返す（境界値検証。`attempt.previous().len() > max_redirects` の判定が
/// 「N 回まで許可」の意味と一致することの確認）。サーバーはリクエストの
/// 到達回数を数え、`max_redirects` 回だけ 302 を返してから 200 を返す。
#[tokio::test]
async fn core_1_fetch_ok_when_redirects_equal_max_redirects() {
    const MAX_REDIRECTS: usize = 2;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let counter = counter.clone();
            thread::spawn(move || {
                let mut stream = stream;
                drain_request_head(&mut stream);
                let hop = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if hop < MAX_REDIRECTS {
                    let body = format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(body.as_bytes());
                } else {
                    let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(b"ok");
                }
            });
        }
    });

    let options = loopback_allowed_options().with_max_redirects(MAX_REDIRECTS);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("ちょうど max_redirects 回のリダイレクトは Ok になるはず");
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.body(), b"ok");
}

/// CORE-1（#36）: `max_redirects = 0` では最初のリダイレクトで即座に
/// `Error::TooManyRedirects { limit: 0 }` を返す（境界値検証）。
#[tokio::test]
async fn core_1_fetch_errors_immediately_when_max_redirects_is_zero() {
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

    let options = loopback_allowed_options().with_max_redirects(0);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("max_redirects = 0 は最初のリダイレクトで打ち切られるはず");
    assert!(
        matches!(err, Error::TooManyRedirects { limit: 0 }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36）: `Fetcher::new` が rustls の crypto provider（ring）を
/// 正しく install することを確認する（TLS 配線の回帰検出。実際の TLS
/// ハンドシェイクを行う結合テストは証明書生成の dev 依存が必要なため
/// 本 Issue の範囲外。PR 本文に切り出し候補として記載する）。
#[test]
fn core_1_fetcher_new_installs_rustls_crypto_provider() {
    let _fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");
    assert!(
        rustls::crypto::CryptoProvider::get_default().is_some(),
        "rustls の crypto provider が install されているはず"
    );
}

/// CORE-1（#36）: `FetchOptions::validate`（`Fetcher::new` 経由）はゼロ値の
/// タイムアウト・接続タイムアウト・本文上限を `Error::InvalidInput` として
/// 早期に拒否する。
/// `Fetcher::new(options)` が `Error::InvalidInput` で失敗することを確認する
/// （`Fetcher` は `Debug` を実装しないため `expect_err` は使えない）。
fn assert_invalid_input(options: FetchOptions) {
    match Fetcher::new(options) {
        Ok(_) => panic!("InvalidInput になるはずが Fetcher::new が成功した"),
        Err(err) => assert!(
            matches!(err, Error::InvalidInput { .. }),
            "unexpected error: {err:?}"
        ),
    }
}

#[test]
fn core_1_fetcher_new_rejects_zero_timeout() {
    assert_invalid_input(FetchOptions::new().with_timeout(Duration::from_secs(0)));
}

#[test]
fn core_1_fetcher_new_rejects_zero_connect_timeout() {
    assert_invalid_input(FetchOptions::new().with_connect_timeout(Duration::from_secs(0)));
}

#[test]
fn core_1_fetcher_new_rejects_zero_max_body_bytes() {
    assert_invalid_input(FetchOptions::new().with_max_body_bytes(0));
}

/// CORE-1（#36）: 解析できない URL（scheme を欠く等）を渡すと
/// `Error::InvalidInput` を返す（`Url::parse` 失敗の経路）。
#[tokio::test]
async fn core_1_fetch_rejects_unparseable_url() {
    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get("not a url")
        .await
        .expect_err("解析できない URL は InvalidInput のはず");
    assert!(
        matches!(err, Error::InvalidInput { .. }),
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

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

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

/// CORE-1（#36。PR #430 コードレビュー指摘）: `allow_private_network_access`
/// が既定（`false`）のとき、取得先がループバック（`127.0.0.1`）へ解決される
/// 場合は接続前に `Error::DisallowedAddress` を返す（security.md「SSRF」）。
#[tokio::test]
async fn core_1_fetch_rejects_loopback_address_by_default() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
    });

    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("既定ではループバックへの接続は拒否されるはず");
    assert!(
        matches!(err, Error::DisallowedAddress { ref address } if address == "127.0.0.1"),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#36。PR #430 コードレビュー指摘）: `allow_private_network_access`
/// を `true` にすると、ループバックへの接続が既定の拒否を回避して成功する
/// （opt-in の動作確認）。
#[tokio::test]
async fn core_1_fetch_allows_loopback_when_opted_in() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(b"ok");
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("opt-in 時はループバックへの接続が成功するはず");
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.body(), b"ok");
}

/// CORE-1（#36。PR #430 コードレビュー指摘）: IP リテラル host（DNS を
/// 介さない）でも `SafeResolver` が同じ拒否を適用する（`Url::parse` を通した
/// IPv4 リテラルが `ToSocketAddrs` でそのまま解決される経路の確認）。
#[tokio::test]
async fn core_1_fetch_rejects_ipv4_literal_loopback_by_default() {
    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get("http://127.0.0.1:1/")
        .await
        .expect_err("IP リテラルのループバックも既定では拒否されるはず");
    assert!(
        matches!(err, Error::DisallowedAddress { ref address } if address == "127.0.0.1"),
        "unexpected error: {err:?}"
    );
}

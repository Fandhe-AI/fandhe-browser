//! `fandhe_browser_core::fetch`（TASK-24.2・#36・ビヘイビア `CORE-1`）の
//! 受け入れ基準を検証する結合テスト。
//!
//! 外部ネットワークには依存せず、`127.0.0.1` のループバックへ最小の
//! HTTP/1.1 応答を返すテストサーバーだけを使う（CI の決定性のため）。
//! #36（TASK-24.2）で用意した最低限（タイムアウト・リダイレクト上限・
//! 本文上限・scheme 拒否・内部アドレス拒否・正常系）に加え、本ファイルは
//! #37（TASK-24.3・MS-1）でタイムアウト（本文受信中）・リダイレクト（相対
//! `Location`・userinfo 除去・`Location` 欠如の 3xx）・異常系（接続断・
//! 不正な応答・truncated body・接続拒否・秘密情報除去）・本文上限の境界
//! （ちょうど・+1・chunked）・送信前拒否（scheme・内部 IP リテラルの
//! テーブル駆動）を拡充した（受け入れ条件「タイムアウト・リダイレクト
//! 上限・異常系のテストが具体値で検証される」）。
//!
//! ループバック（`127.0.0.1`）はそれ自体が内部アドレスであるため、既定
//! （`FetchOptions::allow_private_network_access == false`）ではテスト
//! サーバーへの接続自体が拒否されてしまう。ループバック接続の正常系を
//! 検証するテストはすべて [`loopback_allowed_options`] で明示的に
//! opt-in する（内部アドレス拒否そのものを検証するテストは opt-in せず、
//! 既定値のまま `Error::DisallowedAddress` を確認する）。
//!
//! ## 既知の未検証経路（#37 の範囲外。将来仕様）
//!
//! - `Policy::custom` の `RedirectMarker::DisallowedAddress` 分岐:
//!   ループバックサーバーは既定（`allow_private_network_access == false`）
//!   では初回接続の時点で拒否され、`true` にするとリダイレクト先も
//!   一律に許可されるため、「初回は許可・リダイレクト先だけ内部アドレス」
//!   という組み合わせをループバックサーバーだけでは再現できない
//! - `connect_timeout` 単体の検証: 到達不能な非ルーティングアドレスへの
//!   接続は内部アドレス扱いで送信前に拒否され、`allow_private_network_access`
//!   を opt-in しても実際の到達性は環境（CI ランナーのネットワーク構成）に
//!   依存しフレーキーになるため見送る
//! - 実 TLS ハンドシェイクの結合テスト: 証明書生成の dev 依存が必要
//!   （dependency-policy.md によりユーザー承認なしに追加できない）
//! - `map_reqwest_error` の `TooManyConcurrentDnsResolutions` 経由の結合
//!   経路: 単体テスト（`fetch.rs` の `mod tests`）で確認済み

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

/// CORE-1（#37。TASK-24.3・MS-1）: 全体タイムアウトは、応答ヘッダ受信後・本文
/// ストリーミング中の沈黙でも打ち切る（`core_1_fetch_errors_on_timeout`
/// はヘッダ受信前の沈黙のみを確認していたため、`chunk()` のエラー経路
/// （`map_reqwest_error` の `is_timeout()` 分岐）をここで補強する）。
#[tokio::test]
async fn core_1_fetch_errors_on_timeout_during_body_streaming() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&[b'a'; 10]);
        thread::sleep(Duration::from_secs(5));
    });

    let options = loopback_allowed_options().with_timeout(Duration::from_millis(300));
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let started = std::time::Instant::now();
    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("本文受信中の沈黙もタイムアウトになるはず");
    assert!(
        matches!(err, Error::Timeout { limit } if limit == Duration::from_millis(300)),
        "unexpected error: {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "タイムアウトが実際に打ち切っているはず（elapsed: {:?}）",
        started.elapsed()
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: `max_redirects` を超えるリダイレクトは
/// `Error::TooManyRedirects` になり、サーバーへの到達回数は
/// `max_redirects + 1`（初回 + 追跡した `max_redirects` 回）で打ち切られる
/// （`attempt.previous().len() > max_redirects` の判定を「追跡した回数」で
/// 具体値検証する）。
#[tokio::test]
async fn core_1_fetch_too_many_redirects_request_count() {
    const MAX_REDIRECTS: usize = 3;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter_for_server = counter.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let counter = counter_for_server.clone();
            thread::spawn(move || {
                let mut stream = stream;
                drain_request_head(&mut stream);
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(body.as_bytes());
            });
        }
    });

    let options = loopback_allowed_options().with_max_redirects(MAX_REDIRECTS);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("無限リダイレクトは上限で打ち切られるはず");
    assert!(
        matches!(
            err,
            Error::TooManyRedirects {
                limit: MAX_REDIRECTS
            }
        ),
        "unexpected error: {err:?}"
    );
    // サーバーへの到達回数が安定するまで（打ち切り後のクライアント側の
    // 後始末が非同期のため）短時間ポーリングする。
    let mut reached = counter.load(std::sync::atomic::Ordering::SeqCst);
    for _ in 0..50 {
        if reached > MAX_REDIRECTS {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        reached = counter.load(std::sync::atomic::Ordering::SeqCst);
    }
    assert_eq!(
        reached,
        MAX_REDIRECTS + 1,
        "初回 + max_redirects 回の追跡でサーバーへ到達するはず"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: 相対パスの `Location`（`/next`）を基準 URL に
/// 対して解決してから追跡し、`final_url()` が絶対 URL の追跡先と一致する。
#[tokio::test]
async fn core_1_fetch_follows_relative_location_and_reports_final_url() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
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
                        }
                        Err(_) => break,
                    }
                }
                let request_line = String::from_utf8_lossy(&data);
                if request_line.starts_with("GET /next ") {
                    let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(b"ok");
                } else {
                    let body = "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(body.as_bytes());
                }
            });
        }
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("相対 Location の追跡は成功するはず");
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.body(), b"ok");
    assert_eq!(ok.final_url(), format!("http://127.0.0.1:{port}/next"));
}

/// CORE-1（#37。TASK-24.3・MS-1）: リダイレクト元 URL に含まれる userinfo
/// （`user:pass@host`）は、リダイレクト追跡後の `final_url()` からも常に
/// 取り除かれる（security.md 秘密情報混入防止）。相対 `Location` への
/// リダイレクトは authority（userinfo を含む）を引き継いで解決される
/// ため、初回リクエストへ 200 を返すだけでは追跡後の挙動を検証できない。
/// 302 + 相対 `Location` で追跡先へ導き、そこで 200 を返す構成にする。
#[tokio::test]
async fn core_1_fetch_strips_userinfo_from_final_url() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
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
                        }
                        Err(_) => break,
                    }
                }
                let request_line = String::from_utf8_lossy(&data);
                if request_line.starts_with("GET /next ") {
                    let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(b"ok");
                } else {
                    let body = "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(body.as_bytes());
                }
            });
        }
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://user:dummy-secret@127.0.0.1:{port}/"))
        .await
        .expect("userinfo 付き URL の取得は成功するはず");
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.body(), b"ok");
    assert_eq!(ok.final_url(), format!("http://127.0.0.1:{port}/next"));
    assert!(!ok.final_url().contains("dummy-secret"));
    assert!(!ok.final_url().contains('@'));
}

/// CORE-1（#37。TASK-24.3・MS-1）: `Location` ヘッダを伴わない 3xx 応答は
/// リダイレクトとして扱われず、そのまま `Ok` として返る
/// （`status()` がそのステータスコードになる）。
#[tokio::test]
async fn core_1_fetch_returns_3xx_without_location_as_ok() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("Location の無い 3xx は Ok になるはず");
    assert_eq!(ok.status(), 302);
}

/// CORE-1（#37。TASK-24.3・MS-1）: リクエストヘッダを読み切った後に応答を送らず
/// 接続を閉じたサーバーへの取得は `Error::Network` になる。
#[tokio::test]
async fn core_1_fetch_network_error_when_server_closes_without_response() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        // 応答を送らずそのまま接続を閉じる（drop）。
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("応答なしで接続を閉じるサーバーは Network エラーになるはず");
    assert!(
        matches!(err, Error::Network { .. }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: HTTP として解釈できない応答（ステータス行が
/// 存在しない）を返すサーバーへの取得は `Error::Network` になる。
#[tokio::test]
async fn core_1_fetch_network_error_on_malformed_response() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let _ = stream.write_all(b"garbage\r\n\r\n");
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("不正な応答は Network エラーになるはず");
    assert!(
        matches!(err, Error::Network { .. }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: `Content-Length` で宣言した本文サイズより
/// 少ないバイト数だけ送って接続を閉じた場合（truncated body）は
/// `Error::Network` になる。
#[tokio::test]
async fn core_1_fetch_network_error_on_truncated_body() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&[b'a'; 10]);
        // 残り 90 バイトを送らずに接続を閉じる。
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("本文が途中で打ち切られた場合は Network エラーになるはず");
    assert!(
        matches!(err, Error::Network { .. }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: 接続を受け付けるプロセスが存在しないポートへの
/// 取得は `Error::Network`（接続拒否）になる。Windows の SYN 再送で
/// 数秒かかり得るため、タイムアウトは既定（30 秒）を維持する。
#[tokio::test]
async fn core_1_fetch_network_error_on_connection_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("listener を閉じたポートへの接続は失敗するはず");
    assert!(
        matches!(err, Error::Network { .. }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1・security.md 秘密情報混入防止）:
/// `Error::Network` の `message` は、取得元 URL に含まれる userinfo・
/// クエリパラメータの秘密情報を含まない（`reqwest::Error::without_url()`
/// の契約確認）。メッセージ全文の一致は reqwest 更新で壊れやすいため
/// assert しない。
#[tokio::test]
async fn core_1_fetch_network_error_does_not_leak_secrets_in_message() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!(
            "http://user:dummy-secret@127.0.0.1:{port}/?token=dummy-token"
        ))
        .await
        .expect_err("listener を閉じたポートへの接続は失敗するはず");
    match err {
        Error::Network { message } => {
            assert!(
                !message.contains("dummy-secret"),
                "message に秘密情報が漏れている: {message}"
            );
            assert!(
                !message.contains("dummy-token"),
                "message に秘密情報が漏れている: {message}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

/// CORE-1（#37。TASK-24.3・MS-1）: `Content-Length` がちょうど `max_body_bytes` と
/// 等しい応答は `Ok` になり、`body()` の長さも一致する（事前検査の境界値）。
#[tokio::test]
async fn core_1_fetch_ok_when_content_length_equals_max_body_bytes() {
    const LIMIT: usize = 1024;
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {LIMIT}\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&vec![b'x'; LIMIT]);
    });

    let options = loopback_allowed_options().with_max_body_bytes(LIMIT as u64);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("Content-Length == max_body_bytes は Ok になるはず");
    assert_eq!(ok.body().len(), LIMIT);
}

/// CORE-1（#37。TASK-24.3・MS-1）: `Content-Length` ヘッダを持たない
/// close-delimited body で、ちょうど `max_body_bytes` バイトなら `Ok`、
/// 1 バイト超えると `Error::ResponseTooLarge` になる（ストリーミング検査の
/// 境界値。事前検査ではなく逐次検査側の境界を確認する）。
#[tokio::test]
async fn core_1_fetch_ok_when_streamed_body_equals_max_body_bytes() {
    const LIMIT: usize = 1024;
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&vec![b'x'; LIMIT]);
    });

    let options = loopback_allowed_options().with_max_body_bytes(LIMIT as u64);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("ストリーミング検査でちょうど上限のときは Ok になるはず");
    assert_eq!(ok.body().len(), LIMIT);
}

/// [`core_1_fetch_ok_when_streamed_body_equals_max_body_bytes`] の +1 バイト
/// 版: close-delimited body が上限を 1 バイト超えると
/// `Error::ResponseTooLarge` になる。
#[tokio::test]
async fn core_1_fetch_rejects_streamed_body_one_byte_over_max_body_bytes() {
    const LIMIT: usize = 1024;
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&vec![b'x'; LIMIT + 1]);
    });

    let options = loopback_allowed_options().with_max_body_bytes(LIMIT as u64);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("上限を 1 バイト超える close-delimited body は Err になるはず");
    assert!(
        matches!(err, Error::ResponseTooLarge { limit } if limit == LIMIT as u64),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: `Transfer-Encoding: chunked` で送られた本文が
/// `max_body_bytes` を超える場合も `Error::ResponseTooLarge` になる
/// （`Content-Length` を持たないもう一つの実応答形状のストリーミング検査）。
/// 上限内では連結後の本文が具体値で一致することも併せて確認する。
#[tokio::test]
async fn core_1_fetch_rejects_oversized_chunked_body() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        // 8 バイトのチャンクを 3 回（計 24 バイト）送る。
        for _ in 0..3 {
            let _ = stream.write_all(b"8\r\naaaaaaaa\r\n");
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });

    let options = loopback_allowed_options().with_max_body_bytes(16);
    let fetcher = Fetcher::new(options).expect("Fetcher::new が失敗しないこと");

    let err = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect_err("chunked 本文の合計が上限を超える場合は Err になるはず");
    assert!(
        matches!(err, Error::ResponseTooLarge { limit: 16 }),
        "unexpected error: {err:?}"
    );
}

/// CORE-1（#37。TASK-24.3・MS-1）: 上限内の chunked 本文は連結後の本文が具体値で
/// 一致する（chunked デコード自体の回帰検出）。
#[tokio::test]
async fn core_1_fetch_ok_reassembles_chunked_body() {
    let port = spawn_loopback_server(|mut stream| {
        drain_request_head(&mut stream);
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(b"5\r\nhello\r\n");
        let _ = stream.write_all(b"1\r\n \r\n");
        let _ = stream.write_all(b"5\r\nworld\r\n");
        let _ = stream.write_all(b"0\r\n\r\n");
    });

    let fetcher = Fetcher::new(loopback_allowed_options()).expect("Fetcher::new が失敗しないこと");

    let ok = fetcher
        .get(&format!("http://127.0.0.1:{port}/"))
        .await
        .expect("上限内の chunked 本文は Ok になるはず");
    assert_eq!(ok.body(), b"hello world");
}

/// CORE-1（#37。TASK-24.3・MS-1）: `http`/`https` 以外の scheme はネットワークに
/// 触れずに送信前で拒否される（テーブル駆動。`core_1_fetch_rejects_non_http_scheme`
/// の `file:` 単体を拡張し、拒否対象 scheme を網羅する）。
#[tokio::test]
async fn core_1_fetch_rejects_disallowed_schemes_before_send() {
    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let cases = [
        ("data:text/html,x", "data"),
        ("javascript:alert(1)", "javascript"),
        ("ftp://example.com/", "ftp"),
        ("ws://example.com/", "ws"),
        ("about:blank", "about"),
    ];
    for (url, expected_scheme) in cases {
        let err = fetcher
            .get(url)
            .await
            .expect_err(&format!("{url} は拒否されるはず"));
        assert!(
            matches!(err, Error::DisallowedScheme { ref scheme } if scheme == expected_scheme),
            "unexpected error for {url}: {err:?}"
        );
    }
}

/// CORE-1（#37。TASK-24.3・MS-1・security.md 「SSRF」）: 内部アドレスの IP
/// リテラル host（IPv6 ループバック・リンクローカルのメタデータサービス
/// アドレス・プライベートアドレス・WHATWG の 10 進/16 進正規化表記）は、
/// いずれも正規化後の表示形（`is_disallowed_address` が判定に使う
/// `IpAddr` の文字列表現）で `Error::DisallowedAddress` になる。
#[tokio::test]
async fn core_1_fetch_rejects_internal_ip_literals_before_send() {
    let fetcher = Fetcher::new(FetchOptions::new()).expect("Fetcher::new が失敗しないこと");

    let cases = [
        ("http://[::1]:1/", "::1"),
        (
            "http://169.254.169.254/latest/meta-data/",
            "169.254.169.254",
        ),
        ("http://10.0.0.1/", "10.0.0.1"),
        // WHATWG URL の host 正規化により 10 進数表記 2130706433 は
        // 127.0.0.1 に、16 進数表記 0x7f.1 は 127.0.0.1 に解決される。
        ("http://2130706433/", "127.0.0.1"),
        ("http://0x7f.1/", "127.0.0.1"),
    ];
    for (url, expected_address) in cases {
        let err = fetcher
            .get(url)
            .await
            .expect_err(&format!("{url} は拒否されるはず"));
        assert!(
            matches!(err, Error::DisallowedAddress { ref address } if address == expected_address),
            "unexpected error for {url}: {err:?}"
        );
    }
}

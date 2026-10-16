//! `commands` 配下のテスト共通補助（`#[cfg(test)]` 限定）。
//!
//! navigate・query の単体テストが共有する。ネットワーク取得を伴うテストを外部に依存させないため、
//! ループバックの使い捨てサーバーと、ループバックを許可した [`Fetcher`] を提供する。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use fandhe_browser_core::{FetchOptions, Fetcher};

/// current_thread ランタイムで future を完了まで実行する。
pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime builds")
        .block_on(f)
}

/// 1 接続だけ受け付け、固定の応答を返すループバックサーバー。ポート番号を返す。
pub(crate) fn serve_once(response: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(response.as_bytes());
        }
    });
    port
}

/// ループバック宛ての取得を許可した Fetcher（テスト専用。本番は既定で拒否）。
pub(crate) fn loopback_fetcher() -> Fetcher {
    Fetcher::new(FetchOptions::new().with_allow_private_network_access(true))
        .expect("fetcher builds")
}

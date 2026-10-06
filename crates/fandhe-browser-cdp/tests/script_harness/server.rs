//! テスト用 CDP サーバーの起動と `/json/version` 取得（`Profile::open` を使うため unix 限定の
//! テストから使う。契約テストは取り込まない）。

use std::future::Future;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use fandhe_backend_core::server::Server;
use fandhe_browser_cdp::{CdpState, endpoints};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::Value;

use super::MAX_STREAM_BYTES;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除）。プロファイル保管場所をここへ閉じ込める。
pub struct TempDir(pub PathBuf);

impl TempDir {
    /// 排他的に（`create_dir` で）新規作成できたパスだけを所有する。同名の既存ディレクトリ
    /// （PID 再利用・前回の異常終了の残骸）は流用も削除もせず、別名で作り直す。
    pub fn new() -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        for _ in 0..1000 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("fandhe-cdp-script-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create temp dir: {e}"),
            }
        }
        panic!("could not create a unique temp dir");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// サーバー停止要求の共有状態（`stop` フラグと、待機中 Future の waker）。
#[derive(Default)]
struct StopSignal {
    inner: Mutex<(bool, Option<Waker>)>,
}

impl StopSignal {
    fn trigger(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.0 = true;
            if let Some(w) = g.1.take() {
                w.wake();
            }
        }
    }
}

/// `BoundServer::run_until` へ渡す停止 Future（workspace の tokio は `sync` / `time` feature を
/// 持たないため、標準ライブラリだけで実装する）。
struct StopFuture(Arc<StopSignal>);

impl Future for StopFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.0.inner.lock() {
            Ok(mut g) => {
                if g.0 {
                    Poll::Ready(())
                } else {
                    g.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
            // ロック汚染時は停止扱いにして待機を打ち切る。
            Err(_) => Poll::Ready(()),
        }
    }
}

/// サーバースレッド終了を待つ上限（超過時はプロファイルを削除せず保持して失敗にする）。
const SERVER_STOP_GRACE: Duration = Duration::from_secs(10);

/// 起動済みテストサーバー。drop 時にサーバーを停止してスレッド終了を待ち、その後に
/// `_dir`（プロファイル）を削除する（停止前に削除するとサーバーが保持中のプロファイルを
/// 消してしまうため。フィールドは宣言順に drop されるので `_dir` は最後）。
pub struct TestServer {
    pub addr: SocketAddr,
    /// サーバーが保持する CDP 状態（未実装メソッドの受信記録など、層 B の検証用）。
    #[allow(dead_code)]
    pub state: Arc<CdpState>,
    stop: Arc<StopSignal>,
    stopped: Receiver<()>,
    _dir: Option<TempDir>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.trigger();
        // サーバースレッドが終了時に送る完了通知を待つ（送信側が drop されて Err(Disconnected)
        // になるのはスレッド終了済みの場合なので削除してよい）。時間切れは稼働中の可能性が
        // あるため、プロファイルを削除せず（leak して）失敗として報告する。
        if matches!(
            self.stopped.recv_timeout(SERVER_STOP_GRACE),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            std::mem::forget(self._dir.take());
            if !std::thread::panicking() {
                panic!("test server did not stop within {SERVER_STOP_GRACE:?}; profile dir kept");
            }
        }
    }
}

/// 実サーバーを専用スレッドで起動する。
///
/// 子プロセスの待機はブロッキングなので、テスト側ランタイムとは別の current_thread
/// ランタイムで動かす（workspace の tokio は `rt-multi-thread` を持たず、feature 追加は
/// 依存変更になるため行わない）。スレッドは [`TestServer`] の drop で停止・終了を待つ。
pub fn start_server() -> TestServer {
    let dir = TempDir::new();
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    let st = Arc::new(CdpState::new(app));
    let state = Arc::clone(&st);
    let stop = Arc::new(StopSignal::default());
    let stop_for_thread = Arc::clone(&stop);
    let (tx, rx) = std::sync::mpsc::channel();
    let (stopped_tx, stopped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _ = rt.block_on(async move {
            let (router, ws_config) = endpoints(&st).expect("endpoints").into_parts();
            let bound = Server::new()
                .handler(router)
                .websocket(ws_config)
                .bind("127.0.0.1:0")
                .await
                .expect("bind");
            tx.send(bound.local_addr().expect("local_addr")).ok();
            bound.run_until(StopFuture(stop_for_thread)).await
        });
        // ランタイム（接続タスク含む）を落としてから完了を通知する。
        drop(rt);
        stopped_tx.send(()).ok();
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server did not start");
    TestServer {
        addr,
        state,
        stop,
        stopped,
        _dir: Some(dir),
    }
}

/// `GET /json/version` から `webSocketDebuggerUrl` を取得する（`puppeteer.connect` の入力相当）。
pub fn browser_ws_endpoint(addr: SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes()).expect("write");
    let mut raw = Vec::new();
    s.take(MAX_STREAM_BYTES as u64)
        .read_to_end(&mut raw)
        .expect("read");
    let text = String::from_utf8_lossy(&raw);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let json: Value = serde_json::from_str(body.trim()).expect("version json");
    let url = json
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .expect("webSocketDebuggerUrl")
        .to_string();
    let prefix = format!("ws://127.0.0.1:{}/devtools/browser/", addr.port());
    assert!(url.starts_with(&prefix), "unexpected endpoint: {url}");
    url
}

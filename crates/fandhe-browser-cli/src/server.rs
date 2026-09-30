//! cli のサーバー組み立て（TASK-41（41.5）・MS-3・`CDP-1`・`AISNAP-6`）。
//!
//! `main` から呼ばれ、プロファイルを開いて core の `AppState` を 1 つ生成し、cdp の
//! ルータ・WS 受け口（`fandhe_browser_cdp::endpoints`）を同一リスナーへ載せて待ち受ける。
//! 依存方向は cli → cdp / core / profile の一方向（`self-repair-design.md` 決定 3）。
//! AI API ルータ（TASK-19）は [`RouterFactory`] 経由で同じ `Arc<AppState>` から合成する。
//!
//! # スタブについて
//!
//! 以下は未実装で、実装済みを装わない（REPAIR-3）。
//!
//! - graceful shutdown: 現状は `BoundServer::run` で動かし続け、Ctrl-C は OS 既定の
//!   シグナル動作で終了する（advisory lock は OS が解放する）。`run_until` による
//!   正常停止には tokio の `signal` feature（依存変更・要承認）が要る
//! - ポート・bind 先・プロファイルの明示指定: TASK-47（`CLI-1`）・TASK-60.4 で追加する。
//!   現状は [`DEFAULT_ADDR`] 固定
//! - feature `rendering` 時の描画実装の注入: render crate が `Renderer` を実装した時点
//!   （TASK-33・`RENDER-1`）で `AppState::new` へ渡す。現状は常に無効レンダラー
//! - AI API ルータ: TASK-19 で `fandhe-browser-ai` を依存に加え、[`RouterFactory`] として渡す
//!
//! # 対応 OS
//!
//! Windows では profile crate の ACL 実装（`XOS-7`〜`XOS-10`）待ちのため起動できない
//! （fail-closed）。`Profile::open` が `ProfileError::Unsupported` を返し、cli は
//! [`UNSUPPORTED_OS_MESSAGE`] を出して非ゼロ終了する。成功を装うことはしない。
//!
//! # セキュリティ
//!
//! CDP は任意操作を許すため bind 先は loopback に限定し、それ以外は bind 前に拒否する
//! （`SEC-2` の観点・OWASP A01/A05）。接続上限・read timeout・body 上限は core の
//! `Server` 既定値（DoS 安全側）のまま使い、ここでは緩めない。

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use fandhe_backend_core::server::{BoundServer, Server};
use fandhe_backend_routes::{Router, RouterMergeError};
use fandhe_browser_cdp::{CdpState, WsConfigError, endpoints};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::{ProfileError, ProfileStore};

/// 既定の待ち受けアドレス。9333 は spec `api-cdp.md` のレスポンス例（`CDP-1`）に合わせた値。
pub(crate) const DEFAULT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9333);

/// 追加ルータ（AI API 等）を、CDP と同じ `Arc<AppState>` から組み立てるファクトリ。
///
/// 組み立て済みの `Router` ではなくファクトリを渡させることで、`AppState` の共有
/// （`AISNAP-6`）を型の上で強制する。TASK-19 では `Box::new(|app| ai_router(app))` の形で渡す。
pub(crate) type RouterFactory = Box<dyn FnOnce(Arc<AppState>) -> Router>;

/// プロファイル隔離が未対応の OS（現状 Windows）で起動に失敗した際の固定文言。
///
/// profile crate の Windows ACL（`XOS-7`〜`XOS-10`）が未実装のため、既定 ACL で機密データを
/// 書く偽装成功を避けて `Profile::open` が `Unsupported` を返す。その意図した fail-closed を
/// 利用者へ伝える。`main` が `error: ` を前置して stderr へ出し、非ゼロで終了する。
pub(crate) const UNSUPPORTED_OS_MESSAGE: &str = "profile isolation is not yet supported on this OS (Windows ACL not implemented; XOS-7..XOS-10)";

/// 起動時のエラー。`Display` は固定の英語文言で、入力値（パス・アドレス等）を埋め込まない。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum StartupError {
    /// プロファイルのルート解決・open の失敗（Windows の `Unsupported`・二重起動の `Locked` を含む）。
    Profile(ProfileError),
    /// CDP の WebSocket 設定の構築失敗。
    WebSocketConfig(WsConfigError),
    /// 追加ルータが既存ルートと衝突した。
    RouteConflict(RouterMergeError),
    /// loopback 以外のアドレスを拒否した。
    NonLoopbackAddr,
    /// bind の失敗。
    Bind(io::Error),
    /// 非同期ランタイムの構築失敗。
    Runtime(io::Error),
    /// 待ち受けループの異常終了。
    Serve(io::Error),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Profile(ProfileError::Unsupported { .. }) => f.write_str(UNSUPPORTED_OS_MESSAGE),
            Self::Profile(_) => f.write_str("failed to open profile"),
            Self::WebSocketConfig(_) => f.write_str("failed to build websocket config"),
            Self::RouteConflict(_) => f.write_str("router merge conflict"),
            Self::NonLoopbackAddr => f.write_str("refusing to bind a non-loopback address"),
            Self::Bind(_) => f.write_str("failed to bind listener"),
            Self::Runtime(_) => f.write_str("failed to build async runtime"),
            Self::Serve(_) => f.write_str("server terminated abnormally"),
        }
    }
}

impl std::error::Error for StartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Profile(e) => Some(e),
            Self::WebSocketConfig(e) => Some(e),
            Self::RouteConflict(e) => Some(e),
            Self::NonLoopbackAddr => None,
            Self::Bind(e) | Self::Runtime(e) | Self::Serve(e) => Some(e),
        }
    }
}

/// ストアからプロファイルを開き、`AppState` を 1 つだけ生成する（`AISNAP-6`・`PROF-1`）。
///
/// `run` から呼ばれる。描画実装は未注入（スタブ節参照）のため無効レンダラーを使う。
pub(crate) fn open_app_state(store: &dyn ProfileStore) -> Result<Arc<AppState>, StartupError> {
    let profile = store.open_or_create().map_err(StartupError::Profile)?;
    Ok(Arc::new(AppState::with_disabled_renderer(Arc::new(
        profile,
    ))))
}

/// loopback 以外（`0.0.0.0`・`::`・LAN アドレス）を bind 前に拒否する。
fn ensure_loopback(addr: SocketAddr) -> Result<SocketAddr, StartupError> {
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(StartupError::NonLoopbackAddr)
    }
}

/// CDP のルータ・WS 受け口と追加ルータを合成した `Server` を組み立てる。
///
/// cdp の `endpoints` は `(router, ws)` を必ず対で `Server` へ渡す契約。追加ルータは
/// 同じ `Arc<AppState>` を渡したファクトリから作り、ルート重複は `RouteConflict` にする。
fn assemble(
    app: &Arc<AppState>,
    extra: Vec<RouterFactory>,
) -> Result<(Server, Arc<CdpState>), StartupError> {
    let cdp = Arc::new(CdpState::new(Arc::clone(app)));
    let (mut router, ws) = endpoints(&cdp)
        .map_err(StartupError::WebSocketConfig)?
        .into_parts();
    for factory in extra {
        router = router
            .merge(factory(Arc::clone(app)))
            .map_err(StartupError::RouteConflict)?;
    }
    Ok((Server::new().handler(router).websocket(ws), cdp))
}

/// loopback 検査 → 組み立て → bind。`run` とテストから呼ばれる。
async fn bind(
    app: &Arc<AppState>,
    addr: SocketAddr,
    extra: Vec<RouterFactory>,
) -> Result<(BoundServer, Arc<CdpState>), StartupError> {
    let addr = ensure_loopback(addr)?;
    let (server, cdp) = assemble(app, extra)?;
    let bound = server.bind(addr).await.map_err(StartupError::Bind)?;
    Ok((bound, cdp))
}

/// プロファイルを開いてサーバーを起動し、終了まで待つ（`main` から呼ばれる）。
///
/// stderr へ Chromium 互換の `DevTools listening on ws://...` を 1 行出す
/// （Puppeteer 等が launch 時に読む形式）。プロファイルのパスは出さない。
pub(crate) async fn run(
    store: &dyn ProfileStore,
    addr: SocketAddr,
    extra: Vec<RouterFactory>,
) -> Result<(), StartupError> {
    let app = open_app_state(store)?;
    let (bound, cdp) = bind(&app, addr, extra).await?;
    let local = bound.local_addr().map_err(StartupError::Bind)?;
    eprintln!(
        "DevTools listening on ws://{local}/devtools/browser/{}",
        cdp.browser_id().as_str()
    );
    bound.run().await.map_err(StartupError::Serve)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sec_ensure_loopback_rejects_non_loopback() {
        for bad in ["0.0.0.0:0", "[::]:0", "192.168.0.1:9333"] {
            let addr: SocketAddr = bad.parse().unwrap();
            let err = ensure_loopback(addr).unwrap_err();
            assert!(matches!(err, StartupError::NonLoopbackAddr));
            assert_eq!(err.to_string(), "refusing to bind a non-loopback address");
        }
        for ok in ["127.0.0.1:0", "[::1]:0"] {
            let addr: SocketAddr = ok.parse().unwrap();
            assert_eq!(ensure_loopback(addr).unwrap(), addr);
        }
    }

    /// `XOS-7`〜`XOS-10`: `Unsupported` だけが固定の理由文言になり、他のプロファイル
    /// エラーの表示は変わらない（OS 非依存で 3 OS とも実行）。
    #[test]
    fn xos7_unsupported_profile_error_displays_fixed_message() {
        let err = StartupError::Profile(ProfileError::Unsupported { reason: "x" });
        assert_eq!(
            err.to_string(),
            "profile isolation is not yet supported on this OS (Windows ACL not implemented; XOS-7..XOS-10)"
        );
        let other = StartupError::Profile(ProfileError::Locked {
            path: std::path::PathBuf::from("p"),
        });
        assert_eq!(other.to_string(), "failed to open profile");
    }

    #[test]
    fn cdp1_default_addr_is_loopback_9333() {
        assert_eq!(DEFAULT_ADDR.to_string(), "127.0.0.1:9333");
    }

    #[cfg(not(unix))]
    mod non_unix {
        use super::super::*;
        use crate::server::tests::support::{FixedStore, TempDir};

        #[test]
        fn cdp1_open_app_state_is_unsupported_on_non_unix() {
            let dir = TempDir::new();
            let err = open_app_state(&FixedStore(dir.0.clone())).unwrap_err();
            assert!(matches!(
                err,
                StartupError::Profile(ProfileError::Unsupported { .. })
            ));
            assert_eq!(err.to_string(), UNSUPPORTED_OS_MESSAGE);
        }
    }

    // `Profile::open` は非 unix では `Unsupported` を返す仕様のため、プロファイルを
    // 開く必要があるテストは unix 限定（任意の skip ではない。cdp の結合テストと同じ扱い）。
    #[cfg(unix)]
    mod unix {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        use fandhe_backend_http::response::Response;
        use serde_json::Value;

        use super::super::*;
        use crate::server::tests::support::{FixedStore, TempDir};

        const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
        const WS_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

        /// 応答ヘッドと本文を文字列で返す簡易 HTTP クライアント（接続ごとに 1 要求）。
        fn http(addr: SocketAddr, request: String) -> (u16, String, String) {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            s.write_all(request.as_bytes()).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // ヘッド終端まで読み、Content-Length 分の本文（WS の 101 は本文なし）も読む。
            loop {
                if let Some(pos) = find(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let want = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + want {
                        let body = String::from_utf8_lossy(&buf[pos + 4..pos + 4 + want]);
                        let status = head
                            .split_whitespace()
                            .nth(1)
                            .and_then(|c| c.parse().ok())
                            .expect("status");
                        return (status, head, body.to_string());
                    }
                }
                let n = s.read(&mut chunk).expect("read");
                assert!(n > 0, "unexpected EOF: {:?}", String::from_utf8_lossy(&buf));
                buf.extend_from_slice(&chunk[..n]);
            }
        }

        fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
            hay.windows(needle.len()).position(|w| w == needle)
        }

        fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
            http(
                addr,
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                    addr.port()
                ),
            )
        }

        fn upgrade(addr: SocketAddr, path: &str) -> (u16, String, String) {
            http(
                addr,
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\n\
                     Connection: Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\n\
                     Sec-WebSocket-Version: 13\r\n\r\n",
                    addr.port()
                ),
            )
        }

        async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
            tokio::task::spawn_blocking(f).await.expect("join")
        }

        #[test]
        fn aisnap6_open_app_state_uses_store_root() {
            let dir = TempDir::new();
            let store = FixedStore(dir.0.clone());
            let app = open_app_state(&store).expect("open");
            assert_eq!(app.profile().root(), dir.0.as_path());
            // 同じルートを二重に開けない（PROF-1）。
            let err = open_app_state(&store).unwrap_err();
            assert!(matches!(
                err,
                StartupError::Profile(ProfileError::Locked { .. })
            ));
        }

        #[tokio::test]
        async fn cdp1_serves_json_version_and_list_on_same_listener() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.0.clone())).unwrap();
            let (bound, cdp) = bind(&app, "127.0.0.1:0".parse().unwrap(), vec![])
                .await
                .unwrap();
            let addr = bound.local_addr().unwrap();
            tokio::spawn(bound.run());
            let id = cdp.browser_id().as_str().to_string();

            let (status, _, body) = blocking(move || get(addr, "/json/version")).await;
            assert_eq!(status, 200);
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(
                v["webSocketDebuggerUrl"],
                format!("ws://127.0.0.1:{}/devtools/browser/{id}", addr.port())
            );
            let (status, _, body) = blocking(move || get(addr, "/json/list")).await;
            assert_eq!(status, 200);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap(),
                serde_json::json!([])
            );
        }

        #[tokio::test]
        async fn cdp1_devtools_browser_upgrade_on_same_listener() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.0.clone())).unwrap();
            let (bound, cdp) = bind(&app, "127.0.0.1:0".parse().unwrap(), vec![])
                .await
                .unwrap();
            let addr = bound.local_addr().unwrap();
            tokio::spawn(bound.run());
            let id = cdp.browser_id().as_str().to_string();

            let path = format!("/devtools/browser/{id}");
            let (status, head, _) = blocking(move || upgrade(addr, &path)).await;
            assert_eq!(status, 101);
            assert!(
                head.contains(&format!("Sec-WebSocket-Accept: {WS_ACCEPT}")),
                "{head}"
            );

            let (status, _, _) =
                blocking(move || upgrade(addr, "/devtools/browser/no-such-id")).await;
            assert_ne!(status, 101);
        }

        #[tokio::test]
        async fn aisnap6_extra_router_factory_shares_app_state() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.0.clone())).unwrap();
            let received: Arc<std::sync::Mutex<Option<Arc<AppState>>>> = Arc::default();
            let slot = Arc::clone(&received);
            let factory: RouterFactory = Box::new(move |a| {
                *slot.lock().unwrap() = Some(a);
                Router::new().route("GET", "/ai/ping", |_, _| {
                    Response::new(200, b"pong".to_vec())
                })
            });
            let (bound, cdp) = bind(&app, "127.0.0.1:0".parse().unwrap(), vec![factory])
                .await
                .unwrap();
            let addr = bound.local_addr().unwrap();
            tokio::spawn(bound.run());

            let got = received.lock().unwrap().take().expect("factory called");
            assert!(Arc::ptr_eq(&got, &app));
            assert!(Arc::ptr_eq(&got, cdp.app_state()));
            let (status, _, body) = blocking(move || get(addr, "/ai/ping")).await;
            assert_eq!((status, body.as_str()), (200, "pong"));
            let (status, _, _) = blocking(move || get(addr, "/json/version")).await;
            assert_eq!(status, 200);
        }

        #[tokio::test]
        async fn cdp1_extra_router_conflict_is_rejected() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.0.clone())).unwrap();
            let factory: RouterFactory = Box::new(|_| {
                Router::new().route("GET", "/json/version", |_, _| {
                    Response::new(200, Vec::new())
                })
            });
            let err = bind(&app, "127.0.0.1:0".parse().unwrap(), vec![factory])
                .await
                .err()
                .expect("conflict");
            assert!(matches!(err, StartupError::RouteConflict(_)));
        }
    }

    mod support {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use fandhe_browser_profile::{ProfileError, ProfileStore, ResolvedRoot, RootSource};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        /// 一時ディレクトリ（drop で再帰削除。外部依存は追加しない）。
        pub(super) struct TempDir(pub(super) PathBuf);

        impl TempDir {
            pub(super) fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                Self(base.join(format!("fandhe-cli-test-{}-{n}", std::process::id())))
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// 固定パスを返すストア（テスト専用）。
        pub(super) struct FixedStore(pub(super) PathBuf);

        impl ProfileStore for FixedStore {
            fn resolve_root(&self) -> Result<ResolvedRoot, ProfileError> {
                Ok(ResolvedRoot::new(self.0.clone(), RootSource::Explicit))
            }
        }
    }
}

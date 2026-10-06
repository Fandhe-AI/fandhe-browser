//! cli のサーバー組み立て（TASK-41（41.5）・MS-3・`CDP-1`・`AISNAP-6`）。
//!
//! `main` から呼ばれ、プロファイルを開いて core の `AppState` を 1 つ生成し、cdp の
//! ルータ・WS 受け口（`fandhe_browser_cdp::endpoints`）を同一リスナーへ載せて待ち受ける。
//! 依存方向は cli → cdp / ai / core / profile の一方向（`self-repair-design.md` 決定 3）。
//! AI API ルータ（ai の `/ai/*`。TASK-19.3・#225）は [`default_router_factories`] が返す
//! [`RouterFactory`] 経由で同じ `Arc<AppState>` から合成する。
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
/// （`AISNAP-6`）を型の上で強制する。既定の一覧は [`default_router_factories`]。
pub(crate) type RouterFactory = Box<dyn FnOnce(Arc<AppState>) -> Router>;

/// 既定で CDP のルータへ合成する追加ルータ（現状は ai の `/ai/*` のみ。`AISNAP-6`・TASK-19.3）。
///
/// `main` が [`run`] へ渡す。ai の `router` は cdp と互いに依存せず、`Arc<AppState>` だけを共有する。
/// ルート衝突は [`StartupError::RouteConflict`] で起動を失敗させる（fail-closed）。
pub(crate) fn default_router_factories() -> Vec<RouterFactory> {
    vec![Box::new(fandhe_browser_ai::api::router)]
}

/// プロファイル隔離が未対応の OS（現状 Windows）で起動に失敗した際の固定文言。
///
/// profile crate の Windows ACL（`XOS-7`〜`XOS-10`）が未実装のため、既定 ACL で機密データを
/// 書く偽装成功を避けて `Profile::open` が `Unsupported` を返す。その意図した fail-closed を
/// 利用者へ伝える。`main` が `error: ` を前置して stderr へ出し、非ゼロで終了する。
pub(crate) const UNSUPPORTED_OS_MESSAGE: &str = "profile isolation is not yet supported on this OS (Windows ACL not implemented; XOS-7..XOS-10)";

/// `FANDHE_BROWSER_CONFIG` が空文字だった場合の固定文言（`startup_config` が返す）。
pub(crate) const CONFIG_PATH_EMPTY_MESSAGE: &str = "FANDHE_BROWSER_CONFIG is set but empty";

/// 設定の `[profile] root` が指定されたが cli へ未配線の場合の固定文言。
///
/// 黙って無視すると既定プロファイルへ書き込む（設定が効いているように装う）ため拒否する。
pub(crate) const PROFILE_ROOT_NOT_WIRED_MESSAGE: &str =
    "profile.root in the config file is not yet supported by the CLI (TASK-47/TASK-60.4)";

/// 設定ファイルの I/O エラー（`ConfigError::Io`）の固定文言。core のメッセージは
/// `path.display()` を含むため、表示せずこの文言へ写像する。
pub(crate) const CONFIG_IO_MESSAGE: &str = "failed to read the config file";

/// `profile.root` の値検証エラー（`ConfigError::InvalidValue`）の固定文言。core の
/// メッセージは「設定ファイルの親ディレクトリの絶対パス」を含み得るため、表示せずこの文言へ写像する。
pub(crate) const CONFIG_PROFILE_ROOT_INVALID_MESSAGE: &str =
    "profile.root in the config file is invalid";

/// 起動時のエラー。`Display` は固定の英語文言で、入力値（パス・アドレス等）を埋め込まない。
///
/// 例外は [`StartupError::Config`] のみで、core の設定エラー文言（指定値・同梱エンジン一覧・
/// 必要な feature）を透過する（spec `JS-1` 切替方式ケース (3) の要求。値は運用者自身の
/// ローカル設定由来）。ただしファイルパスを含み得る I/O エラー（`ConfigError::Io`）と
/// `profile.root` の値検証エラー（親ディレクトリの絶対パスを含み得る）は
/// [`CONFIG_IO_MESSAGE`]・[`CONFIG_PROFILE_ROOT_INVALID_MESSAGE`] の固定文言へ写像し、パスを表示しない。
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum StartupError {
    /// 設定ファイルの読み込み・検証の失敗（未同梱エンジン指定を含む。`JS-2`・TASK-30.5）。
    Config(fandhe_browser_core::Error),
    /// `FANDHE_BROWSER_CONFIG` が空文字。
    ConfigPathEmpty,
    /// `[profile] root` 指定は cli 未配線のため拒否した。
    ProfileRootNotWired,
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
            Self::Config(fandhe_browser_core::Error::Config(
                fandhe_browser_core::config::ConfigError::Io { .. },
            )) => f.write_str(CONFIG_IO_MESSAGE),
            Self::Config(fandhe_browser_core::Error::Config(
                fandhe_browser_core::config::ConfigError::InvalidValue {
                    key: "profile.root",
                    ..
                },
            )) => f.write_str(CONFIG_PROFILE_ROOT_INVALID_MESSAGE),
            Self::Config(e) => write!(f, "{e}"),
            Self::ConfigPathEmpty => f.write_str(CONFIG_PATH_EMPTY_MESSAGE),
            Self::ProfileRootNotWired => f.write_str(PROFILE_ROOT_NOT_WIRED_MESSAGE),
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
            Self::Config(e) => Some(e),
            Self::ConfigPathEmpty | Self::ProfileRootNotWired => None,
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

    /// 設定ファイルの I/O エラーは固定文言へ写像され、パスが表示に漏れない。
    #[test]
    fn config_io_error_displays_fixed_message_without_path() {
        let io = fandhe_browser_core::Error::Config(fandhe_browser_core::config::ConfigError::Io {
            message: "failed to open /secret/dir/cfg.toml: No such file".to_string(),
        });
        let shown = StartupError::Config(io).to_string();
        assert_eq!(shown, "failed to read the config file");
        assert!(!shown.contains("/secret"));
    }

    /// `profile.root` の値検証エラーは固定文言へ写像され、親ディレクトリのパスが漏れない。
    #[test]
    fn config_profile_root_invalid_displays_fixed_message_without_path() {
        let e = fandhe_browser_core::Error::Config(
            fandhe_browser_core::config::ConfigError::InvalidValue {
                key: "profile.root",
                message: "profile.root \"../x\" must resolve under (/secret/dir)".to_string(),
            },
        );
        let shown = StartupError::Config(e).to_string();
        assert_eq!(shown, "profile.root in the config file is invalid");
        assert!(!shown.contains("/secret"));
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
            let err = open_app_state(&FixedStore(dir.path.clone())).unwrap_err();
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
            let store = FixedStore(dir.path.clone());
            let app = open_app_state(&store).expect("open");
            assert_eq!(app.profile().root(), dir.path.as_path());
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
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
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
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
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
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
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

        /// 合成済みの実 ai ルータ（`main` と同じ `default_router_factories`）で、`/json/*` と
        /// `/ai/snapshot` が同一リスナーから返ること（`AISNAP-6`・TASK-19.3）。
        #[tokio::test]
        async fn aisnap6_serves_json_and_ai_snapshot_on_same_listener() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
            let (bound, cdp) = bind(
                &app,
                "127.0.0.1:0".parse().unwrap(),
                default_router_factories(),
            )
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
            let (status, _, _) = blocking(move || get(addr, "/json/list")).await;
            assert_eq!(status, 200);

            // 未ナビゲートでも 404 ではなく 409 が返る = ai ルータへ到達している（AISNAP-14）。
            let (status, _, body) = blocking(move || get(addr, "/ai/snapshot")).await;
            assert_eq!(status, 409);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap(),
                serde_json::json!({
                    "code": "no_navigation",
                    "message": "no navigation has been performed yet"
                })
            );
        }

        #[tokio::test]
        async fn aisnap6_ai_snapshot_returns_200_after_navigation_commit() {
            use fandhe_browser_core::NavigationResult;

            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
            let (bound, _cdp) = bind(
                &app,
                "127.0.0.1:0".parse().unwrap(),
                default_router_factories(),
            )
            .await
            .unwrap();
            let addr = bound.local_addr().unwrap();
            tokio::spawn(bound.run());

            let nav = app.navigation();
            let g = nav.begin_navigation().expect("begin");
            nav.commit_navigation(
                g,
                NavigationResult::new(
                    "https://example.com/",
                    "<title>Example Domain</title><h1>Example Domain</h1>",
                ),
            )
            .expect("commit");

            let (status, head, body) = blocking(move || get(addr, "/ai/snapshot")).await;
            assert_eq!(status, 200);
            assert!(
                head.to_ascii_lowercase()
                    .contains("content-type: application/json; charset=utf-8"),
                "{head}"
            );
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["url"], "https://example.com/");
            assert_eq!(v["tree"]["role"], "document");
            assert_eq!(v["tree"]["name"], "Example Domain");
        }

        /// 合成後も DNS rebinding 対策（Host 検証）が効くこと（SEC 系・OWASP A01/A05）。
        #[tokio::test]
        async fn sec_ai_snapshot_rejects_disallowed_host_on_composed_listener() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
            let (bound, _cdp) = bind(
                &app,
                "127.0.0.1:0".parse().unwrap(),
                default_router_factories(),
            )
            .await
            .unwrap();
            let addr = bound.local_addr().unwrap();
            tokio::spawn(bound.run());

            let (status, _, body) = blocking(move || {
                http(
                    addr,
                    "GET /ai/snapshot HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n"
                        .to_string(),
                )
            })
            .await;
            assert_eq!(status, 403);
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["code"], "host_not_allowed");
        }
        // ---- CDP と独自 API の AppState 共有結合テスト（AISNAP-6・TASK-19.4・Issue #226） ----
        //
        // ai ⇔ cdp の crate 間依存は AGENTS.md で禁止のため、両者を合成できる cli に置く。
        // 実 HTML を取得する `Page.navigate` は、本番 `Fetcher` が内部アドレスを拒否する既定
        // （SEC-2）のためここでは駆動しない（cdp crate 内テストが担う）。ここでは取得を伴わず
        // 確定する `about:blank` と、拒否される遷移で「CDP が書いた共有状態を `/ai/snapshot`
        // が読む」経路を固定する。

        /// `/devtools/browser/{id}` へ WS 接続する（応答ヘッドは 1 バイトずつ読む）。
        fn ws_connect(addr: SocketAddr, browser_id: &str) -> TcpStream {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let req = format!(
                "GET /devtools/browser/{browser_id} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
                 Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {WS_KEY}\r\n\
                 Sec-WebSocket-Version: 13\r\n\r\n",
                addr.port()
            );
            s.write_all(req.as_bytes()).unwrap();
            let mut buf = Vec::new();
            let mut b = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n") {
                assert_eq!(s.read(&mut b).expect("read head"), 1, "unexpected EOF");
                buf.push(b[0]);
            }
            let head = String::from_utf8(buf).unwrap();
            assert!(head.starts_with("HTTP/1.1 101"), "{head}");
            assert!(
                head.contains(&format!("Sec-WebSocket-Accept: {WS_ACCEPT}")),
                "{head}"
            );
            s
        }

        /// マスク付きテキストフレームを送る（125 バイト以下の 7 bit 長のみ）。
        fn ws_send_text(s: &mut TcpStream, text: &str) {
            let payload = text.as_bytes();
            assert!(
                payload.len() < 126,
                "test helper supports short frames only"
            );
            let key = [0x11u8, 0x22, 0x33, 0x44];
            let mut frame = vec![0x81, 0x80 | payload.len() as u8];
            frame.extend_from_slice(&key);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
            s.write_all(&frame).unwrap();
        }

        /// サーバー発の非マスクテキストフレームを 1 つ読んで JSON にする（7 bit / 16 bit 長）。
        fn ws_read_json(s: &mut TcpStream) -> Value {
            let mut h = [0u8; 2];
            s.read_exact(&mut h).expect("frame header");
            assert_eq!(h[0], 0x81, "expected FIN + text frame");
            assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
            let mut len = usize::from(h[1] & 0x7f);
            assert!(len <= 126, "unsupported frame length");
            if len == 126 {
                let mut ext = [0u8; 2];
                s.read_exact(&mut ext).expect("extended length");
                len = usize::from(u16::from_be_bytes(ext));
            }
            let mut payload = vec![0u8; len];
            s.read_exact(&mut payload).expect("payload");
            serde_json::from_slice(&payload).expect("json")
        }

        /// 合成サーバーを起動し、`https://example.com/` を確定済みにして返す。
        async fn spawn_with_committed_example() -> (SocketAddr, String, Arc<AppState>, TempDir) {
            use fandhe_browser_core::NavigationResult;

            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
            let (bound, cdp) = bind(
                &app,
                "127.0.0.1:0".parse().unwrap(),
                default_router_factories(),
            )
            .await
            .unwrap();
            let addr = bound.local_addr().unwrap();
            tokio::spawn(bound.run());
            assert!(Arc::ptr_eq(cdp.app_state(), &app));

            let nav = app.navigation();
            let g = nav.begin_navigation().expect("begin");
            nav.commit_navigation(
                g,
                NavigationResult::new(
                    "https://example.com/",
                    "<title>Example Domain</title><h1>Example Domain</h1>",
                ),
            )
            .expect("commit");
            let id = cdp.browser_id().as_str().to_string();
            (addr, id, app, dir)
        }

        async fn snapshot_json(addr: SocketAddr) -> Value {
            let (status, _, body) = blocking(move || get(addr, "/ai/snapshot")).await;
            assert_eq!(status, 200, "{body}");
            serde_json::from_str(&body).unwrap()
        }

        /// CDP `Page.navigate`（WS 経由）が書いた共有状態を `/ai/snapshot` が読むこと。
        #[tokio::test]
        async fn aisnap6_cdp_page_navigate_is_visible_to_ai_snapshot() {
            let (addr, id, app, _dir) = spawn_with_committed_example().await;
            let before = snapshot_json(addr).await;
            assert_eq!(before["url"], "https://example.com/");
            assert_eq!(before["tree"]["name"], "Example Domain");

            let frames = blocking(move || {
                let mut s = ws_connect(addr, &id);
                ws_send_text(
                    &mut s,
                    r#"{"id":1,"method":"Page.navigate","params":{"url":"about:blank"}}"#,
                );
                [
                    ws_read_json(&mut s),
                    ws_read_json(&mut s),
                    ws_read_json(&mut s),
                ]
            })
            .await;
            assert_eq!(frames[0]["id"], 1);
            assert_eq!(frames[0]["result"]["frameId"], "main");
            assert!(
                frames[0]["result"].get("errorText").is_none(),
                "{}",
                frames[0]
            );
            assert_eq!(frames[1]["method"], "Page.frameNavigated");
            assert_eq!(frames[1]["params"]["frame"]["url"], "about:blank");
            assert_eq!(frames[2]["method"], "Page.loadEventFired");

            let after = snapshot_json(addr).await;
            assert_eq!(after["url"], "about:blank");
            assert_eq!(after["tree"]["role"], "document");
            assert!(
                !after.to_string().contains("Example Domain"),
                "stale content: {after}"
            );
            assert_eq!(
                app.navigation().latest().expect("latest").url(),
                "about:blank"
            );
        }

        /// 拒否された CDP 遷移（内部アドレス）は共有状態を変えず、`/ai/snapshot` も不変（SEC-2）。
        #[tokio::test]
        async fn aisnap6_rejected_cdp_navigate_keeps_ai_snapshot_unchanged() {
            let (addr, id, _app, _dir) = spawn_with_committed_example().await;

            let (nav_resp, next) = blocking(move || {
                let mut s = ws_connect(addr, &id);
                ws_send_text(
                    &mut s,
                    r#"{"id":1,"method":"Page.navigate","params":{"url":"http://127.0.0.1:1/"}}"#,
                );
                let nav_resp = ws_read_json(&mut s);
                // イベントが出ていなければ、次に届くのは続けて送った未実装メソッドの応答。
                ws_send_text(&mut s, r#"{"id":2,"method":"Browser.getVersion"}"#);
                (nav_resp, ws_read_json(&mut s))
            })
            .await;
            assert_eq!(nav_resp["id"], 1);
            assert_eq!(nav_resp["result"]["errorText"], "net::ERR_ACCESS_DENIED");
            assert_eq!(next["id"], 2);
            assert_eq!(next["error"]["code"], -32601);

            let after = snapshot_json(addr).await;
            assert_eq!(after["url"], "https://example.com/");
            assert_eq!(after["tree"]["name"], "Example Domain");
        }

        #[tokio::test]
        async fn cdp1_extra_router_conflict_is_rejected() {
            let dir = TempDir::new();
            let app = open_app_state(&FixedStore(dir.path.clone())).unwrap();
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
        ///
        /// `.0` は自前で新規作成した親ディレクトリ配下の未作成パス（プロファイルルート用）。
        /// 親は `create_dir` の排他的作成に成功したものだけを使い、既存パスと衝突した場合は
        /// 連番を進めて別名を選ぶ。drop では自分が作った親だけを削除する。
        pub(super) struct TempDir {
            pub(super) owned: PathBuf,
            pub(super) path: PathBuf,
        }

        impl TempDir {
            pub(super) fn new() -> Self {
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                loop {
                    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                    let owned = base.join(format!("fandhe-cli-test-{}-{n}", std::process::id()));
                    match std::fs::create_dir(&owned) {
                        Ok(()) => {
                            let path = owned.join("profile");
                            return Self { owned, path };
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) => panic!("failed to create test temp dir: {e}"),
                    }
                }
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.owned);
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

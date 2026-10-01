//! CDP `Page` ドメインのハンドラ（TASK-42（42.2）・#240、ビヘイビア `CDP-1`・`SEC-2`・MS-4）。
//!
//! `crate::protocol::builtin_handlers` が [`PageNavigate`] を `Page.navigate` として登録し、
//! ws.rs 経由のディスパッチャから呼ばれる。本モジュールは core の
//! [`Fetcher`]（HTTP 取得・SSRF 防御・サイズ／時間上限）で URL を取得し、
//! core の共有状態 [`NavigationState`]（`AppState::navigation()`）の直近結果
//! （最終 URL と HTML）を世代付きで更新する。後続の `DOM.getDocument`（42.4）などが
//! 同じ状態を読む。
//!
//! # スタブ・範囲外（`REPAIR-3`）
//!
//! - `Page.frameNavigated`・`Page.loadEventFired` 等のイベント送出は TASK-42.3（#241）。
//!   本ハンドラの出力は `events` が空。
//! - HTML は `FetchResponse::body_text_lossy`（UTF-8 lossy）で文字列化する簡易実装。
//!   文字コード判定は `CORE-5`・TASK-25 で置き換える。
//! - `frameId` は [`MAIN_FRAME_ID`] の固定値。セッション／ターゲットに基づく決定は
//!   TASK-43（`CDP-2`）で行う。
//! - `transitionType`・`referrer`・`frameId` 等の他パラメータは現時点では無視する。
//!
//! # 失敗時の応答方針（`SEC-2`）
//!
//! パラメータ不正は JSON-RPC エラー（`-32602`）。取得失敗は CDP 仕様どおり `result` に
//! `errorText` を付けて返す。これは仕様上の失敗通知であり、Puppeteer / Playwright は
//! 失敗として扱う（一律 success を返して検出を回避する挙動ではない）。`errorText` は
//! [`fetch_error_text`] の閉じた固定文言表のみで、URL・アドレス等の入力由来文字列を含めない。

use fandhe_browser_core::{
    Error, FetchOptions, Fetcher, NavigationResult, NavigationState, StateError,
};
use serde_json::{Value, json};

use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, HandlerOutput};
use crate::target::MAX_URL_LEN;

/// メインフレームの `frameId`（スタブ。将来はセッション → ターゲット ID。`CDP-2`・TASK-43）。
pub(crate) const MAIN_FRAME_ID: &str = "main";

/// [`navigate`] の結果。将来の拡張（イベント用情報など）に備えて構造体にする（`REPAIR-4`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavigateOutcome {
    /// 払い出した世代番号から決定的に作る `loaderId`。
    pub loader_id: String,
    /// 取得失敗・中断時の CDP `errorText`（固定文言）。成功時は `None`。
    pub error_text: Option<&'static str>,
}

/// 取得エラーを CDP の `errorText`（固定文言）へ写像する閉じた表。
///
/// `Error` は `#[non_exhaustive]` のためワイルドカード腕を持つ。入力由来の文字列は返さない。
pub(crate) fn fetch_error_text(err: &Error) -> &'static str {
    match err {
        Error::Timeout { .. } => "net::ERR_TIMED_OUT",
        Error::TooManyRedirects { .. } => "net::ERR_TOO_MANY_REDIRECTS",
        Error::ResponseTooLarge { .. } => "net::ERR_FILE_TOO_BIG",
        Error::DisallowedScheme { .. } | Error::DisallowedAddress { .. } => {
            "net::ERR_ACCESS_DENIED"
        }
        Error::InvalidInput { .. } => "net::ERR_INVALID_URL",
        _ => "net::ERR_FAILED",
    }
}

fn state_error(_: StateError) -> CdpError {
    CdpError::SERVER_ERROR
}

/// `url` へ遷移し、`nav` の直近結果を更新する。ハンドラ本体（`Page.navigate`）の中核で、
/// `CdpState` に依存しないため 3 OS で単体テストできる。
///
/// 4xx/5xx も完了したナビゲーションとして commit する（core の `Fetcher::get` は判断を
/// 呼び出し側へ委ねる）。取得失敗時は直近結果が `None` のまま（begin で無効化済み）。
pub(crate) async fn navigate(
    fetcher: &Fetcher,
    nav: &NavigationState,
    url: &str,
) -> Result<NavigateOutcome, CdpError> {
    // 共有状態を変更する前に URL の形式・scheme を検証する。不正 URL や `file:` で
    // 保存済みの URL / HTML を失わせない（`about:blank` は下で個別に扱う）。
    if url != "about:blank"
        && let Err(e) = fetcher.validate_url(url)
    {
        return Ok(NavigateOutcome {
            loader_id: format!("{:x}", nav.current_generation().get()),
            error_text: Some(fetch_error_text(&e)),
        });
    }
    let generation = nav.begin_navigation().map_err(state_error)?;
    let mut outcome = NavigateOutcome {
        loader_id: format!("{:x}", generation.get()),
        error_text: None,
    };

    if url == "about:blank" {
        // clear_navigation は成功時に自身で世代を 1 つ進める。返す `loaderId` は進めた後の
        // 世代（`current_generation`）に合わせ、次の遷移で番号が飛ばないようにする。
        return match nav.clear_navigation(generation) {
            Ok(()) => {
                outcome.loader_id = format!("{:x}", generation.get().saturating_add(1));
                Ok(outcome)
            }
            Err(StateError::Superseded { .. }) => {
                outcome.error_text = Some("net::ERR_ABORTED");
                Ok(outcome)
            }
            Err(e) => Err(state_error(e)),
        };
    }

    match fetcher.get(url).await {
        Ok(resp) => {
            let result = NavigationResult::new(resp.final_url(), resp.body_text_lossy());
            match nav.commit_navigation(generation, result) {
                Ok(()) => {}
                Err(StateError::Superseded { .. }) => {
                    outcome.error_text = Some("net::ERR_ABORTED");
                }
                Err(e) => return Err(state_error(e)),
            }
        }
        Err(e) => outcome.error_text = Some(fetch_error_text(&e)),
    }
    Ok(outcome)
}

/// `Page.navigate` ハンドラ。`Fetcher`（接続プール）を所有し、起動時に 1 回だけ構築する。
pub(crate) struct PageNavigate {
    fetcher: Fetcher,
}

impl PageNavigate {
    /// `options` で `Fetcher` を構築する。本番は `FetchOptions::default()`
    /// （内部アドレス拒否の安全側既定）、テストのみループバックを許可する。
    pub fn new(options: FetchOptions) -> Result<Self, Error> {
        Ok(Self {
            fetcher: Fetcher::new(options)?,
        })
    }
}

impl CommandHandler for PageNavigate {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            // 状態変更（世代の払い出し）より前に検証する。`sessionId` が付いている場合は
            // レジストリに登録済みであることを要求する（未登録 ID で共有状態を書き換えさせない。
            // `CDP-1`・`SEC-2`）。`sessionId` なし（ブラウザレベル）はセッション／ターゲット対応
            // （TASK-43・`CDP-2`）が入るまで従来どおり許容する（スタブ）。
            let target_id = match ctx.session_id {
                Some(sid) => Some(
                    ctx.state
                        .registry()
                        .session_target(sid)
                        .ok_or(CdpError::INVALID_PARAMS)?,
                ),
                None => None,
            };
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty() && u.len() <= MAX_URL_LEN)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let nav = ctx.state.app_state().navigation();
            let out = navigate(&self.fetcher, nav, url).await?;
            // 成功時のみ、ターゲット表の URL を確定した最終 URL へ更新する（`CDP-1`）。
            // 取得中にターゲットが閉じられた場合（UnknownTarget）は無視してよい。
            if out.error_text.is_none()
                && let (Some(tid), Some(latest)) = (&target_id, nav.latest())
            {
                let _ = ctx.state.registry().set_target_url(tid, latest.url());
            }
            let mut result = json!({
                "frameId": MAIN_FRAME_ID,
                "loaderId": out.loader_id,
            });
            if let (Some(text), Some(obj)) = (out.error_text, result.as_object_mut()) {
                obj.insert("errorText".into(), json!(text));
            }
            Ok(HandlerOutput::result(result))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn drain_request_head(stream: &mut TcpStream) {
        let mut buf = [0u8; 1024];
        let mut data = Vec::new();
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 8192 {
                break;
            }
        }
    }

    /// 接続ごとに `status` と `body` を返すループバックサーバー。ポート番号を返す。
    fn serve(status: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                thread::spawn(move || {
                    drain_request_head(&mut s);
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        port
    }

    fn allowed_fetcher() -> Fetcher {
        Fetcher::new(FetchOptions::new().with_allow_private_network_access(true)).unwrap()
    }

    #[tokio::test]
    async fn cdp1_navigate_commits_fetched_html_and_final_url() {
        let port = serve("200 OK", "<h1>hello</h1>");
        let nav = NavigationState::new();
        let url = format!("http://127.0.0.1:{port}/");
        let out = navigate(&allowed_fetcher(), &nav, &url).await.unwrap();
        assert_eq!(out.error_text, None);
        assert_eq!(out.loader_id, "1");
        let latest = nav.latest().unwrap();
        assert_eq!(latest.url(), url);
        assert_eq!(latest.html(), "<h1>hello</h1>");
        assert_eq!(nav.current_generation().get(), 1);
    }

    #[tokio::test]
    async fn cdp1_navigate_twice_replaces_result_and_advances_generation() {
        let p1 = serve("200 OK", "first");
        let p2 = serve("200 OK", "second");
        let nav = NavigationState::new();
        let f = allowed_fetcher();
        navigate(&f, &nav, &format!("http://127.0.0.1:{p1}/"))
            .await
            .unwrap();
        let out = navigate(&f, &nav, &format!("http://127.0.0.1:{p2}/"))
            .await
            .unwrap();
        assert_eq!(out.loader_id, "2");
        assert_eq!(nav.latest().unwrap().html(), "second");
        assert_eq!(nav.current_generation().get(), 2);
    }

    #[tokio::test]
    async fn cdp1_navigate_commits_4xx_page() {
        let port = serve("404 Not Found", "missing");
        let nav = NavigationState::new();
        let out = navigate(
            &allowed_fetcher(),
            &nav,
            &format!("http://127.0.0.1:{port}/"),
        )
        .await
        .unwrap();
        assert_eq!(out.error_text, None);
        assert_eq!(nav.latest().unwrap().html(), "missing");
    }

    #[tokio::test]
    async fn cdp1_navigate_about_blank_clears_without_fetch() {
        let nav = NavigationState::new();
        let out = navigate(&allowed_fetcher(), &nav, "about:blank")
            .await
            .unwrap();
        assert_eq!(out.error_text, None);
        let latest = nav.latest().unwrap();
        assert_eq!(latest.url(), "about:blank");
        assert_eq!(latest.html(), "");
        assert_eq!(out.loader_id, "2");
        assert_eq!(
            out.loader_id,
            format!("{:x}", nav.current_generation().get())
        );
    }

    #[tokio::test]
    async fn cdp1_navigate_fetch_failure_sets_error_text_and_no_result() {
        let port = serve("200 OK", "secret");
        let strict = Fetcher::new(FetchOptions::default()).unwrap();
        let nav = NavigationState::new();
        let out = navigate(&strict, &nav, &format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        assert_eq!(out.error_text, Some("net::ERR_ACCESS_DENIED"));
        assert!(nav.latest().is_none());

        let out = navigate(&strict, &nav, "file:///etc/passwd").await.unwrap();
        assert_eq!(out.error_text, Some("net::ERR_ACCESS_DENIED"));
        assert!(nav.latest().is_none());
    }

    #[tokio::test]
    async fn cdp1_navigate_invalid_url_keeps_previous_result() {
        let port = serve("200 OK", "keep");
        let nav = NavigationState::new();
        let f = allowed_fetcher();
        let url = format!("http://127.0.0.1:{port}/");
        navigate(&f, &nav, &url).await.unwrap();
        let before = nav.current_generation().get();

        let out = navigate(&f, &nav, "file:///etc/passwd").await.unwrap();
        assert_eq!(out.error_text, Some("net::ERR_ACCESS_DENIED"));
        let out = navigate(&f, &nav, "not a url").await.unwrap();
        assert_eq!(out.error_text, Some("net::ERR_INVALID_URL"));

        let latest = nav.latest().unwrap();
        assert_eq!(latest.url(), url);
        assert_eq!(latest.html(), "keep");
        assert_eq!(nav.current_generation().get(), before);
    }

    #[test]
    fn cdp1_fetch_error_text_is_fixed_table() {
        use std::time::Duration;
        let cases: Vec<(Error, &str)> = vec![
            (
                Error::Timeout {
                    limit: Duration::from_secs(1),
                },
                "net::ERR_TIMED_OUT",
            ),
            (
                Error::TooManyRedirects { limit: 3 },
                "net::ERR_TOO_MANY_REDIRECTS",
            ),
            (
                Error::ResponseTooLarge { limit: 1 },
                "net::ERR_FILE_TOO_BIG",
            ),
            (
                Error::DisallowedScheme {
                    scheme: "file".into(),
                },
                "net::ERR_ACCESS_DENIED",
            ),
            (
                Error::DisallowedAddress {
                    address: "127.0.0.1".into(),
                },
                "net::ERR_ACCESS_DENIED",
            ),
            (
                Error::InvalidInput {
                    message: "x".into(),
                },
                "net::ERR_INVALID_URL",
            ),
            (
                Error::Unsupported {
                    message: "x".into(),
                },
                "net::ERR_FAILED",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(fetch_error_text(&err), expected);
        }
    }

    // CdpState は AppState（Profile::open）が必要で非 unix では構築手段が無いため unix 限定
    // （protocol.rs の `mod dispatch` と同じ理由）。
    #[cfg(unix)]
    mod dispatch {
        use super::*;
        use crate::protocol::{DispatchOutcome, Dispatcher};
        use crate::server::CdpState;
        use fandhe_browser_core::AppState;
        use fandhe_browser_profile::Profile;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                Self(base.join(format!("fandhe-cdp-page-test-{}-{n}", std::process::id())))
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn state(dir: &TempDir) -> Arc<CdpState> {
            let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
            Arc::new(CdpState::new(Arc::new(AppState::with_disabled_renderer(
                profile,
            ))))
        }

        fn allowed_dispatcher() -> Dispatcher {
            let h = PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))
                .unwrap();
            Dispatcher::from_handlers(vec![("Page.navigate", Box::new(h))]).unwrap()
        }

        fn frames(o: DispatchOutcome) -> Vec<Value> {
            o.into_frames()
                .iter()
                .map(|f| serde_json::from_str(f).unwrap())
                .collect()
        }

        #[tokio::test]
        async fn cdp1_page_navigate_via_dispatcher_updates_app_state() {
            let dir = TempDir::new();
            let st = state(&dir);
            let port = serve("200 OK", "<p>via cdp</p>");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(
                f,
                vec![json!({"id": 1, "result": {"frameId": "main", "loaderId": "1"}})]
            );
            let latest = st.app_state().navigation().latest().unwrap();
            assert_eq!(latest.url(), url);
            assert_eq!(latest.html(), "<p>via cdp</p>");
        }

        #[tokio::test]
        async fn cdp1_page_navigate_unknown_session_is_rejected_without_state_change() {
            let dir = TempDir::new();
            let st = state(&dir);
            let port = serve("200 OK", "x");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": "NOSUCH", "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(
                f[0]["error"],
                json!({"code": -32602, "message": "invalid params"})
            );
            let nav = st.app_state().navigation();
            assert_eq!(nav.current_generation().get(), 0);
            assert!(nav.latest().is_none());
        }

        #[tokio::test]
        async fn cdp1_page_navigate_registered_session_succeeds() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let port = serve("200 OK", "ok");
            let url = format!("http://127.0.0.1:{port}/");
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": url}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f[0]["result"]["loaderId"], json!("1"));
            assert_eq!(st.app_state().navigation().latest().unwrap().html(), "ok");
            // 成功後はターゲット表の URL も遷移先へ更新される（`CDP-1`）。
            assert_eq!(st.registry().target(&tid).unwrap().url(), url);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_failure_keeps_target_url() {
            use crate::target::TargetKind;
            let dir = TempDir::new();
            let st = state(&dir);
            let tid = st
                .registry()
                .create_target(TargetKind::Page, "about:blank")
                .unwrap();
            let sid = st.registry().attach(&tid).unwrap();
            let req = json!({"id": 1, "method": "Page.navigate",
                "sessionId": sid.as_str(), "params": {"url": "file:///etc/passwd"}});
            let f = frames(allowed_dispatcher().dispatch(&st, &req.to_string()).await);
            assert_eq!(f[0]["result"]["errorText"], json!("net::ERR_ACCESS_DENIED"));
            assert_eq!(st.registry().target(&tid).unwrap().url(), "about:blank");
        }

        #[tokio::test]
        async fn cdp1_page_navigate_invalid_params_keep_generation() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = allowed_dispatcher();
            let too_long = format!("http://example.invalid/{}", "a".repeat(MAX_URL_LEN));
            let bodies = [
                json!({"id": 1, "method": "Page.navigate"}),
                json!({"id": 2, "method": "Page.navigate", "params": {"url": 5}}),
                json!({"id": 3, "method": "Page.navigate", "params": {"url": ""}}),
                json!({"id": 4, "method": "Page.navigate", "params": {"url": too_long}}),
            ];
            for (i, b) in bodies.iter().enumerate() {
                let f = frames(d.dispatch(&st, &b.to_string()).await);
                assert_eq!(
                    f,
                    vec![
                        json!({"id": i + 1, "error": {"code": -32602, "message": "invalid params"}})
                    ]
                );
            }
            assert_eq!(st.app_state().navigation().current_generation().get(), 0);
        }

        #[tokio::test]
        async fn cdp1_page_navigate_error_frame_does_not_echo_url() {
            let dir = TempDir::new();
            let st = state(&dir);
            let d = Dispatcher::builtin().unwrap();
            let req = json!({"id": 1, "method": "Page.navigate",
                "params": {"url": "http://127.0.0.1:9/path?token=dummy-secret"}});
            let raw = d
                .dispatch(&st, &req.to_string())
                .await
                .into_frames()
                .remove(0);
            assert!(!raw.contains("dummy-secret") && !raw.contains("127.0.0.1"));
        }

        #[tokio::test]
        async fn cdp1_builtin_dispatcher_registers_page_navigate() {
            let dir = TempDir::new();
            let st = state(&dir);
            let req = json!({"id": 1, "method": "Page.navigate",
                "params": {"url": "http://127.0.0.1:9/"}});
            let f = frames(
                Dispatcher::builtin()
                    .unwrap()
                    .dispatch(&st, &req.to_string())
                    .await,
            );
            assert_eq!(
                f,
                vec![json!({"id": 1, "result": {
                    "frameId": "main", "loaderId": "0", "errorText": "net::ERR_ACCESS_DENIED"}})]
            );
        }
    }
}

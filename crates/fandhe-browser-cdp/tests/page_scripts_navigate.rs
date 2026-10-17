//! `Page.navigate` がページ JS を実行し、その結果の DOM を `DOM.getDocument` で取得でき、
//! 完了後にイベントが決まった順序で送出されることの結合テスト（TASK-112・Issue #785・
//! ビヘイビア `JS-4`・`CDP-1`）。
//!
//! 実エンジン（boa）で `PageScriptOutcome::Executed` の成功経路を通す。boa は子プロセス版のため
//! `main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり `harness = false`。
//! `Profile::open` は unix のみ対応のため、本体は unix かつ macOS 以外（macOS の boa は
//! fail-closed で無効）に限る。それ以外では何も検証せず成功終了する（core の
//! `page_actor_boa.rs` と同じ作法。macOS 側の fail-closed は core 側で検証済み）。

use std::process::ExitCode;

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    cases::run();
    eprintln!("page_scripts_navigate: all cases passed");
    ExitCode::SUCCESS
}

#[cfg(all(unix, not(target_os = "macos")))]
mod cases {
    use std::future::Future;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::pin::Pin;
    use std::sync::Arc;

    use fandhe_browser_cdp::CdpState;
    use fandhe_browser_cdp::test_support::LoopbackDispatcher;
    use fandhe_browser_core::{AppState, Config, PageScriptExecutor, PageScriptHandle};
    use fandhe_browser_profile::Profile;
    use serde_json::{Value, json};

    /// 現スレッドの tokio ランタイムで future を駆動する実行器（cli の実行器と同じ形）。
    struct TokioExecutor;

    impl PageScriptExecutor for TokioExecutor {
        fn block_on<'a>(
            &self,
            future: Pin<Box<dyn Future<Output = ()> + 'a>>,
        ) -> std::io::Result<()> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(future);
            Ok(())
        }
    }

    const PAGE: &str = "<html><body><script>var p=document.createElement('p');\
         p.textContent='from-'+'script';document.body.appendChild(p)</script></body></html>";

    fn serve(body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        port
    }

    fn state(dir: &std::path::Path, scripts: bool) -> Arc<CdpState> {
        let profile = Arc::new(Profile::open(dir).expect("profile open"));
        let mut app = AppState::with_disabled_renderer(profile);
        if scripts {
            let cfg =
                Config::from_toml_str("[js]\nengine = \"boa\"\nexecute_page_scripts = true\n")
                    .expect("config");
            app = app.with_page_scripts(PageScriptHandle::new(cfg.js(), Arc::new(TokioExecutor)));
        }
        Arc::new(CdpState::new(Arc::new(app)))
    }

    /// 送出フレームを JSON として返す。
    fn frames(raw: Vec<String>) -> Vec<Value> {
        raw.iter()
            .map(|f| serde_json::from_str(f).expect("json frame"))
            .collect()
    }

    /// `Page.navigate` → `DOM.getDocument` を実行し、(遷移フレーム, getDocument 応答) を返す。
    async fn navigate_and_get_document(st: &Arc<CdpState>, url: &str) -> (Vec<Value>, Value) {
        let d = LoopbackDispatcher::new().expect("dispatcher");
        let nav = json!({"id": 1, "method": "Page.navigate", "params": {"url": url}});
        let nav_frames = frames(d.dispatch(st, &nav.to_string()).await);
        let doc = json!({"id": 2, "method": "DOM.getDocument", "params": {"depth": -1}});
        let mut doc_frames = frames(d.dispatch(st, &doc.to_string()).await);
        assert_eq!(doc_frames.len(), 1, "{doc_frames:?}");
        (nav_frames, doc_frames.remove(0))
    }

    pub fn run() {
        let base = fandhe_browser_profile::test_base_dir();
        let dir = base.join(format!("fandhe-cdp-page-scripts-{}", std::process::id()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let port = serve(PAGE);
            let url = format!("http://127.0.0.1:{port}/");

            // 実行有効: JS が追加した <p>from-script</p> が DOM.getDocument で取得できる。
            let st = state(&dir.join("on"), true);
            let (nav, doc) = navigate_and_get_document(&st, &url).await;
            // 応答 → frameNavigated → loadEventFired の順（`CDP-1`）。実行完了後に送出される。
            assert_eq!(nav.len(), 3, "{nav:?}");
            assert_eq!(
                nav[0],
                json!({"id": 1, "result": {"frameId": "main", "loaderId": "1"}})
            );
            assert_eq!(nav[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(nav[1]["params"]["frame"]["url"], json!(url));
            assert_eq!(nav[2]["method"], json!("Page.loadEventFired"));
            let text = doc.to_string();
            assert!(
                text.contains("from-script"),
                "JS 実行後の DOM を期待: {text}"
            );
            // 共有状態へ確定した HTML も実行後のもの。
            let latest = st.app_state().navigation().latest().expect("latest");
            assert!(
                latest.html().contains("<p>from-script</p>"),
                "{}",
                latest.html()
            );

            // 対照: 実行無効なら同じページでも JS の変更は現れず、イベント列は同じ。
            let st = state(&dir.join("off"), false);
            let (nav, doc) = navigate_and_get_document(&st, &url).await;
            assert_eq!(nav.len(), 3, "{nav:?}");
            assert_eq!(nav[1]["method"], json!("Page.frameNavigated"));
            assert_eq!(nav[2]["method"], json!("Page.loadEventFired"));
            assert!(!doc.to_string().contains("from-script"), "{doc}");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}

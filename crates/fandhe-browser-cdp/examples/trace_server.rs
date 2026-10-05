//! Playwright トレース収集用の最小 CDP サーバー（TASK-43（43.1）・#246、ビヘイビア `CDP-2`・MS-4）。
//!
//! `harness/playwright-trace/run.sh` が起動し、stdout の `listening 127.0.0.1:<port>` 行から
//! 接続先を得る。cli の固定ポート・既定プロファイルは使わず、一時プロファイルと空きポート
//! （`127.0.0.1:0`、loopback 限定。`SEC-4`）で立てるため、並列実行や既存環境と衝突しない。
//! CDP ハンドラは一切追加せず、現状の応答（未実装は `-32601`。`CDP-6`・`SEC-2`）をそのまま
//! トレースに残すための土台である。ライブラリ API は変更しない。

#[cfg(unix)]
fn main() {
    use std::io::Write;
    use std::sync::Arc;

    use fandhe_backend_core::server::Server;
    use fandhe_browser_cdp::{BrowserId, CdpState, endpoints};
    use fandhe_browser_core::AppState;
    use fandhe_browser_profile::Profile;

    // プロファイルは PID 付きの一時ディレクトリに作り、終了時に削除を試みる。
    let dir = std::env::temp_dir().join(format!("fandhe-cdp-trace-{}", std::process::id()));
    let profile = Arc::new(Profile::open(&dir).unwrap_or_else(|e| {
        eprintln!("failed to open profile: {e}");
        std::process::exit(1);
    }));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    let browser_id = BrowserId::parse("trace-1").unwrap_or_else(|e| {
        eprintln!("invalid browser id: {e:?}");
        std::process::exit(1);
    });
    let state = Arc::new(CdpState::with_browser_id(app, browser_id));
    let (router, ws_config) = endpoints(&state)
        .unwrap_or_else(|e| {
            eprintln!("failed to build endpoints: {e:?}");
            std::process::exit(1);
        })
        .into_parts();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("failed to build runtime: {e}");
            std::process::exit(1);
        });
    rt.block_on(async {
        let bound = Server::new()
            .handler(router)
            .websocket(ws_config)
            .bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| {
                eprintln!("failed to bind: {e:?}");
                std::process::exit(1);
            });
        let addr = bound.local_addr().unwrap_or_else(|e| {
            eprintln!("failed to get local addr: {e:?}");
            std::process::exit(1);
        });
        println!("listening {addr}");
        let _ = std::io::stdout().flush();
        let _ = bound.run().await;
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Profile::open` が非 unix 未対応のため、非 unix ではコンパイルのみ通して明示的に失敗する。
#[cfg(not(unix))]
fn main() {
    eprintln!("trace_server is supported on unix only");
    std::process::exit(1);
}

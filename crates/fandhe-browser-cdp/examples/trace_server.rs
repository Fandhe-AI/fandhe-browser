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

    // プロファイルは「自分が排他的に新規作成した」専用ディレクトリにのみ作り、作成を確認できた
    // ディレクトリだけを終了時に削除する（既存データを誤って再帰削除しない。PROF-1）。
    // 名前は PID と時刻（ナノ秒）で衝突を避け、`create_dir` は既存なら失敗するため、
    // 実行前から存在したパスは使わない（衝突時は別名で数回リトライする）。
    // Profile::open は祖先に symlink を含むパスを拒否する（macOS の /var -> /private/var 等）ため、
    // 信頼できる基点を canonicalize してから専用名を結合する。
    let base = std::env::temp_dir().canonicalize().unwrap_or_else(|e| {
        eprintln!("failed to canonicalize temp dir: {e}");
        std::process::exit(1);
    });
    let dir = {
        use std::os::unix::fs::DirBuilderExt;
        let mut created = None;
        for attempt in 0..16u32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let candidate = base.join(format!(
                "fandhe-cdp-trace-{}-{nanos}-{attempt}",
                std::process::id()
            ));
            // 非 recursive の mkdir（0700）。既存パスなら AlreadyExists で失敗し、使わない。
            match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => {
                    created = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    eprintln!("failed to create profile dir: {e}");
                    std::process::exit(1);
                }
            }
        }
        created.unwrap_or_else(|| {
            eprintln!("failed to create a unique profile dir");
            std::process::exit(1);
        })
    };
    let profile = Arc::new(Profile::open(&dir).unwrap_or_else(|e| {
        eprintln!("failed to open profile: {e}");
        // 自分が作成した空ディレクトリなので削除してよい。
        let _ = std::fs::remove_dir_all(&dir);
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

//! 計測ハーネス共通の「一時プロファイル上の AppState を本物の ai ルータで loopback 公開する」束
//! （TASK-97.1・Issue #385・`PLUG-5` / TASK-95.1・Issue #375・`PLUG-3`・`MS-9`）。
//!
//! `mcp_envelope/harness.rs`（エンベロープ測定）と `benches/plugin_overhead`（プラグインのアイドル RSS 増分）が
//! 共有する。mcp バイナリの接続（`mcp_client.rs`）は呼び出し側が担い、本モジュールはホスト側だけを持つ。
//! 一時ディレクトリはプロファイル境界（PROF）を守るため専用ディレクトリに限り、Drop で削除する。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_browser_ai::api::router;
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;

use crate::host::BenchHost;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（Drop で再帰削除）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // Profile::open は root 経路上の symlink を拒否する（macOS の /var -> /private/var 等）ため、
        // 既存の一時ディレクトリを事前に実体パスへ解決する（profile 側の検査は緩めない。PROF 境界）。
        let tmp = std::env::temp_dir();
        let base = std::fs::canonicalize(&tmp).unwrap_or(tmp);
        Self(base.join(format!(
            "fandhe-mcp-envelope-{}-{n}-{nanos}",
            std::process::id()
        )))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// CI 診断用にパスを含まない ProfileError の種別ラベルを返す。
fn profile_error_label(e: &fandhe_browser_profile::ProfileError) -> String {
    use fandhe_browser_profile::ProfileError;
    match e {
        ProfileError::Io(io) => format!("io:{:?}", io.kind()),
        ProfileError::InvalidLayout { reason, .. } => format!("invalid-layout:{reason}"),
        ProfileError::Unsupported { .. } => "unsupported-platform".to_string(),
        _ => "other".to_string(),
    }
}

/// ホスト・AppState・一時ディレクトリの束。フィールドの宣言順が Drop 順（host → app → dir）。
pub struct AppHost {
    host: BenchHost,
    // plugin_overhead ベンチ（TASK-95.1）は本項目を使わないが、mcp_envelope ベンチと共有するソースのため残す。
    #[allow(dead_code)]
    app: Arc<AppState>,
    _dir: TempDir,
}

impl AppHost {
    /// 一時プロファイルを開き、本物の ai ルータを `127.0.0.1:0` で公開する。
    pub fn start() -> Result<Self, String> {
        let dir = TempDir::new();
        let profile = Profile::open(&dir.0).map_err(|e| {
            format!(
                "failed to open temporary profile: {}",
                profile_error_label(&e)
            )
        })?;
        let app = Arc::new(AppState::with_disabled_renderer(Arc::new(profile)));
        let host = BenchHost::start(router(Arc::clone(&app)))
            .map_err(|_| "failed to start bench host".to_string())?;
        Ok(Self {
            host,
            app,
            _dir: dir,
        })
    }

    /// ホストの待ち受けアドレス（loopback）。
    pub fn addr(&self) -> std::net::SocketAddr {
        self.host.addr()
    }

    /// ホストが公開している AppState。
    // plugin_overhead ベンチ（TASK-95.1）は本項目を使わないが、mcp_envelope ベンチと共有するソースのため残す。
    #[allow(dead_code)]
    pub fn app(&self) -> &Arc<AppState> {
        &self.app
    }
}

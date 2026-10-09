//! MCP エンベロープ測定ハーネスの計測本体（TASK-97.1・Issue #385・`PLUG-5`・`MS-9`）。
//!
//! 一時プロファイル上の `AppState` を ai ルータ越しに `host.rs` で公開し、`mcp_client.rs` で
//! mcp バイナリを接続して、fixture ごとに「方式 B 本体（`GET /ai/snapshot` 直接）」と
//! 「MCP `tools/call snapshot`」の出力を比較する。呼び出し元は `mcp_envelope.rs` の main と
//! `mcp_envelope_e2e` テスト。
//!
//! ページは `navigate` ツールを使わず `begin_navigation` / `commit_navigation` で AppState へ直接
//! commit する（ホストに `POST /ai/navigate` が無く、取得経路は loopback を拒否するため。REPAIR-3:
//! `/ai/navigate` 実装後は MCP の navigate 経由へ切り替える）。ネットワークは使わない。
//!
//! 一致検証（受入基準の根拠）: MCP の text と直接応答本文を `serde_json::Value` として比較し、
//! 不一致は失敗にする。実測値は `docs/design/mcp-envelope-report.md`（#386）、判断は #387 が担う。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fandhe_browser_ai::api::router;
use fandhe_browser_core::{AppState, NavigationResult};
use fandhe_browser_profile::Profile;
use serde_json::Value;

use crate::host::{BenchHost, http_get};
use crate::mcp_client::McpSession;
use crate::stats::EnvelopeRow;
use crate::tokens::{TokenCounter, fixtures_dir};

/// 測定対象 fixture（PLUG-4 と PoC-15 の代表 5 サイト）。
pub const FIXTURES: [&str; 5] = [
    "example-minimal",
    "wikipedia-article",
    "hn-list",
    "login-form",
    "mdn-docs",
];

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

/// 1 fixture 分の計測結果。
pub struct FixtureResult {
    pub row: EnvelopeRow,
    /// 直接 GET のレイテンシ（ms）。
    pub direct_ms: Vec<f64>,
    /// MCP 経路のレイテンシ（ms）。
    pub mcp_ms: Vec<f64>,
}

/// ホスト・AppState・mcp セッションの束。フィールドの宣言順が Drop 順（mcp → host → app → dir）。
pub struct Harness {
    mcp: McpSession,
    host: BenchHost,
    app: Arc<AppState>,
    _dir: TempDir,
}

impl Harness {
    /// 一時プロファイル・ホスト・mcp（起動時に登録が 1 回発生）を立ち上げる。
    pub fn start(bin: &Path) -> Result<Self, String> {
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
        let mcp = McpSession::start(bin, host.addr())?;
        Ok(Self {
            mcp,
            host,
            app,
            _dir: dir,
        })
    }

    fn commit(&self, url: &str, html: &str) -> Result<(), String> {
        let nav = self.app.navigation();
        let g = nav
            .begin_navigation()
            .map_err(|_| "begin_navigation failed".to_string())?;
        nav.commit_navigation(g, NavigationResult::new(url, html))
            .map_err(|_| "commit_navigation failed".to_string())?;
        Ok(())
    }

    fn direct_body(&self) -> Result<Vec<u8>, String> {
        let r = http_get(self.host.addr(), "/ai/snapshot")?;
        if r.status != 200 {
            return Err(format!(
                "direct GET /ai/snapshot returned status {}",
                r.status
            ));
        }
        Ok(r.body)
    }

    /// `name` の fixture を commit し、4 系列のトークン計測と `iterations` 回のレイテンシ計測を行う。
    pub fn measure(
        &mut self,
        counter: &TokenCounter,
        name: &str,
        iterations: usize,
    ) -> Result<FixtureResult, String> {
        let path = fixtures_dir().join(format!("{name}.html"));
        let html = std::fs::read_to_string(&path)
            .map_err(|_| format!("fixture {name}.html is not readable"))?;
        self.commit(&format!("https://example.com/{name}"), &html)?;

        let direct = self.direct_body()?;
        let (line, _, text) = self.mcp.call_snapshot()?;
        let direct_json: Value = serde_json::from_slice(&direct)
            .map_err(|_| "direct response is not JSON".to_string())?;
        let mcp_json: Value =
            serde_json::from_str(&text).map_err(|_| "MCP text is not JSON".to_string())?;
        if direct_json != mcp_json {
            return Err(format!(
                "MCP snapshot text differs from direct response for {name}"
            ));
        }
        let direct_text = String::from_utf8_lossy(&direct);
        let row = EnvelopeRow {
            name: name.to_string(),
            raw_html_tokens: counter.count(&html),
            direct_tokens: counter.count(&direct_text),
            mcp_text_tokens: counter.count(&text),
            mcp_line_tokens: counter.count(&line),
            direct_bytes: direct.len(),
            mcp_line_bytes: line.len(),
        };

        // ウォームアップ 1 回の後、直接と MCP を交互に計測して反復ごとの対を作る。
        self.direct_body()?;
        self.mcp.call_snapshot()?;
        let mut direct_ms = Vec::with_capacity(iterations);
        let mut mcp_ms = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let t0 = std::time::Instant::now();
            self.direct_body()?;
            direct_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            let (_, ms, _) = self.mcp.call_snapshot()?;
            mcp_ms.push(ms);
        }
        Ok(FixtureResult {
            row,
            direct_ms,
            mcp_ms,
        })
    }
}

//! 既定（`[js] execute_page_scripts = false`）では snapshot と MCP 経由の削減率測定の出力が
//! 変わらないことの確認（TASK-113・Issue #787・`JS-4`/`PLUG-4`/`AISNAP-6`）。
//!
//! 役割: cdp の `navigate_gated` と同じデータの流れ（無効ハンドルでの実行要求 → HTML を確定 →
//! ai の `snapshot_body`）を、ai ⇔ cdp の依存禁止を守って core の公開 API だけで再現する。
//! 比較の基準は既存測定（`mcp_token_reduction.rs`）と同じ「HTML を直接 commit する経路」で、
//! 既存の固定値・レポートには触れない。エンジン不要のため既定ゲートで常に走る。
//! 有効時の検証は `page_scripts_snapshot_{v8,boa}.rs`。

use std::error::Error;
use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use fandhe_browser_ai::api::snapshot_body;
use fandhe_browser_core::{
    Config, NavigationResult, NavigationState, PageRunRequest, PageScriptExecutor,
    PageScriptHandle, PageScriptOutcome, PageScriptSkipReason,
};
use serde_json::Value;

// テスト間共有モジュールのため、取り込み側のテストバイナリで使わない項目は未使用警告になる。それを許容する。
#[allow(dead_code)]
#[path = "../benches/token_reduction/tokens.rs"]
mod tokens;

// テスト間共有モジュールのため、取り込み側のテストバイナリで使わない項目は未使用警告になる。それを許容する。
#[allow(dead_code)]
#[path = "support/page_scripts_fixtures.rs"]
mod fixtures;

use tokens::fixtures_dir;

type TestResult<T> = Result<T, Box<dyn Error>>;

/// PLUG-4 の代表 5 サイト（`mcp_token_reduction.rs` の `SITES` と同じ順）。
const SITES: [&str; 5] = [
    "example-minimal",
    "wikipedia-article",
    "hn-list",
    "login-form",
    "mdn-docs",
];

/// 現スレッドの tokio ランタイムで future を駆動する実行器（cli の実行器と同じ形）。
struct TokioExecutor;

impl PageScriptExecutor for TokioExecutor {
    fn block_on<'a>(&self, future: Pin<Box<dyn Future<Output = ()> + 'a>>) -> std::io::Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(future);
        Ok(())
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// 設定を何も書かずに作ったハンドル（既定は無効）。
fn default_handle() -> TestResult<PageScriptHandle> {
    let cfg = Config::from_toml_str("")?;
    Ok(PageScriptHandle::new(cfg.js(), Arc::new(TokioExecutor)))
}

fn site_url(name: &str) -> String {
    format!("http://127.0.0.1:8899/{name}.html")
}

/// 既存測定（`host_body`）と同じ経路: HTML を直接 commit して `snapshot_body` を得る。
fn direct_body(url: &str, html: &str) -> TestResult<String> {
    let nav = NavigationState::new();
    let g = nav
        .begin_navigation()
        .map_err(|e| format!("begin_navigation failed: {e:?}"))?;
    nav.commit_navigation(g, NavigationResult::new(url, html))
        .map_err(|e| format!("commit_navigation failed: {e:?}"))?;
    let body = snapshot_body(&nav).map_err(|e| format!("snapshot_body failed: {e:?}"))?;
    Ok(String::from_utf8(body)?)
}

/// `navigate_gated` と同じ流れ: 無効ハンドルへ実行を要求し、`Skipped(Disabled)` なら取得した
/// HTML のまま確定して `snapshot_body` を得る。
fn gated_body(handle: &PageScriptHandle, url: &str, html: &str) -> TestResult<String> {
    let nav = NavigationState::new();
    let g = nav
        .begin_navigation()
        .map_err(|e| format!("begin_navigation failed: {e:?}"))?;
    handle.begin_generation(nav.scope(), g);
    let request = PageRunRequest::new(Arc::from(html), url, nav.scope(), g);
    let outcome = rt().block_on(handle.run_page_scripts(request))?;
    assert!(
        matches!(
            outcome,
            PageScriptOutcome::Skipped(PageScriptSkipReason::Disabled)
        ),
        "default handle must skip with Disabled: {outcome:?}"
    );
    nav.commit_navigation(g, NavigationResult::new(url, html))
        .map_err(|e| format!("commit_navigation failed: {e:?}"))?;
    let body = snapshot_body(&nav).map_err(|e| format!("snapshot_body failed: {e:?}"))?;
    Ok(String::from_utf8(body)?)
}

/// mcp の `interpret` と同じ変換（解析 → コンパクト再直列化）。
fn reserialize(body: &str) -> TestResult<String> {
    let value: Value = serde_json::from_str(body)?;
    Ok(serde_json::to_string(&value)?)
}

/// `JS-4`: 既定の設定ではハンドルが無効で、実行要求は `Skipped(Disabled)` になり
/// ワーカーを起動しない。
#[test]
fn js4_default_config_keeps_page_scripts_disabled_without_spawning() -> TestResult<()> {
    let handle = default_handle()?;
    assert!(!handle.is_enabled());
    let nav = NavigationState::new();
    let g = nav
        .begin_navigation()
        .map_err(|e| format!("begin_navigation failed: {e:?}"))?;
    let request = PageRunRequest::new(
        Arc::from(fixtures::SPA_HTML),
        fixtures::SPA_URL,
        nav.scope(),
        g,
    );
    let outcome = rt().block_on(handle.run_page_scripts(request))?;
    assert!(
        matches!(
            outcome,
            PageScriptOutcome::Skipped(PageScriptSkipReason::Disabled)
        ),
        "{outcome:?}"
    );
    assert_eq!(handle.stats().worker_spawns(), 0);
    Ok(())
}

/// `JS-4`/`PLUG-4`: 5 サイトで、無効ハンドルを通した経路の応答本文が既存測定の経路
/// （HTML の直接 commit）とバイト一致し、`interpret` 相当の再直列化でも変わらない。
#[test]
fn js4_plug4_default_off_snapshot_bytes_equal_direct_commit() -> TestResult<()> {
    let handle = default_handle()?;
    for name in SITES {
        let path = fixtures_dir().join(format!("{name}.html"));
        let html = fs::read_to_string(&path)
            .map_err(|e| format!("failed to read fixture {}: {e}", path.display()))?;
        let url = site_url(name);
        let direct = direct_body(&url, &html)?;
        let gated = gated_body(&handle, &url, &html)?;
        assert_eq!(gated, direct, "{name}: body changed by the disabled handle");
        assert_eq!(
            reserialize(&gated)?,
            gated,
            "{name}: re-serialization differs"
        );
    }
    assert_eq!(handle.stats().worker_spawns(), 0);
    Ok(())
}

/// `JS-4`/`AISNAP-6`: 既定オフでは JS が描画する要素は snapshot に現れず、静的要素だけが現れる。
#[test]
fn js4_default_off_does_not_render_script_generated_elements() -> TestResult<()> {
    let handle = default_handle()?;
    let body = gated_body(&handle, fixtures::SPA_URL, fixtures::SPA_HTML)?;
    assert!(body.contains("Static action"), "{body}");
    assert!(body.contains("Home"), "{body}");
    for name in fixtures::RENDERED_NAMES {
        assert!(
            !body.contains(name),
            "{name} leaked into default-off: {body}"
        );
    }
    for marker in fixtures::FORBIDDEN_IN_AI_OUTPUT {
        assert!(!body.contains(marker), "{marker} found in {body}");
    }
    Ok(())
}

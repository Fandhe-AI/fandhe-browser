//! 呼び出しレイテンシの計測手順（TASK-95.3・Issue #377・`PLUG-3`・`MS-9`）。
//!
//! `plugin_overhead.rs` の main（と e2e テスト）から呼ばれる。一時プロファイル上のホスト（`app_host.rs`）へ
//! fixture ページを直接 commit し、mcp を接続して `tools/call snapshot` の往復時間を `--calls` 回採取する。
//! 計測窓は `McpSession` が返す ms（要求行をキューへ渡す直前から応答行の受信まで）で、プラグイン内の
//! `GET /ai/snapshot` とホストの簡約スナップショット生成を含む。応答の JSON 検証は窓の外。
//! ウォームアップ 1 回は標本に含めず、url が commit 済みページと一致することだけ検証する。
//!
//! PoC-15 との差: PoC-15 は `navigate` ツール経由でページを用意したが、ここはホストに `POST /ai/navigate`
//! が無いため直接 commit する。
//! 将来仕様（REPAIR-3）: `POST /ai/navigate` 実装後は `navigate` ツール経由へ、cli が引数を持ったら
//! 実バイナリのホストへ切り替える（TASK-47・`CLI-1`）。

use std::path::Path;

use fandhe_browser_core::NavigationResult;

use crate::app_host::AppHost;
use crate::call_latency_stats::CallLatencyMeasurement;
use crate::mcp_client::McpSession;
use crate::options::Options;

const PAGE_URL: &str = "https://example.com/example-minimal";

/// 計測を実行して標本（呼び出し順の ms）を返す。失敗した呼び出しは標本に数えず即時に Err。
pub fn measure(bin: &Path, opts: &Options) -> Result<CallLatencyMeasurement, String> {
    let host = AppHost::start()?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
        .join("example-minimal.html");
    let html = std::fs::read_to_string(&fixture)
        .map_err(|_| "failed to read call latency fixture".to_string())?;
    {
        let nav = host.app().navigation();
        let g = nav
            .begin_navigation()
            .map_err(|_| "begin_navigation failed".to_string())?;
        nav.commit_navigation(g, NavigationResult::new(PAGE_URL, &html))
            .map_err(|_| "commit_navigation failed".to_string())?;
    }
    let mut session = McpSession::start(bin, host.addr())?;
    let (_, _, text) = session
        .call_snapshot()
        .map_err(|e| format!("call latency warm-up failed: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| "warm-up snapshot is not valid JSON".to_string())?;
    if v.get("url").and_then(serde_json::Value::as_str) != Some(PAGE_URL) {
        return Err("warm-up snapshot does not describe the committed page".to_string());
    }
    let mut samples_ms = Vec::with_capacity(opts.calls);
    for i in 1..=opts.calls {
        let (_, ms, _) = session
            .call_snapshot()
            .map_err(|e| format!("call latency call {i} failed: {e}"))?;
        samples_ms.push(ms);
    }
    drop(session);
    drop(host);
    Ok(CallLatencyMeasurement { samples_ms })
}

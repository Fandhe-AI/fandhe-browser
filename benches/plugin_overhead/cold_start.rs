//! cold start（プラグイン起動から MCP ハンドシェイク完了まで）の計測手順
//! （TASK-95.2・Issue #376・`PLUG-3`・`MS-9`）。
//!
//! `plugin_overhead.rs` の main と `e2e_tests.rs` から呼ばれる。値の型・整形は `cold_start_stats.rs`。
//!
//! 計測窓: `McpSession::start`（`Command::spawn` → 起動時の自己申告 `POST /ai/plugins/register` →
//! rmcp stdio 初期化 → `initialize` 応答検証 → `notifications/initialized` 送信）の直前から返却直後まで。
//! ホスト起動（in-process の `AppHost`）・ツール呼び出し・終了処理は窓に含めない。
//! 各試行で新しいホストとプロセスを使う。同一ホストへの 2 回目以降の接続は 409
//! `duplicate_plugin_id` の警告継続経路になり、試行間で経路が揃わないため。
//!
//! PoC-15 の end-to-end（ホスト spawn 直後〜`navigate` 完了）より窓が狭く、値は直接比較できない。
//! 将来仕様（REPAIR-3）: cli が引数（ポート・プロファイル）を持ったら実バイナリのホスト起動を窓へ含める
//! （TASK-47・`CLI-1`）。ホストに `POST /ai/navigate` が実装されたら初回ツール呼び出しまで含める。
//! OS のページキャッシュ状態は制御しない（初回試行だけ遅く出得るため全試行値を出力する）。

use std::path::Path;
use std::time::{Duration, Instant};

use crate::app_host::AppHost;
use crate::cold_start_stats::{ColdStartMeasurement, duration_ms};
use crate::mcp_client::McpSession;
use crate::options::Options;

/// `opts.trials` 回、ハンドシェイク完了までの壁時計時間を採取する。
pub fn measure(bin: &Path, opts: &Options) -> Result<ColdStartMeasurement, String> {
    let interval = Duration::from_millis(opts.interval_ms);
    let mut handshake_ms = Vec::with_capacity(opts.trials);
    for i in 0..opts.trials {
        if i > 0 {
            std::thread::sleep(interval);
        }
        // ホスト起動は計測窓の外。
        let host = AppHost::start()?;
        let t0 = Instant::now();
        let session = McpSession::start(bin, host.addr())
            .map_err(|e| format!("cold start trial {i} failed: {e}"))?;
        let ms = duration_ms(t0.elapsed());
        eprintln!("cold start trial {i}: {ms:.3} ms");
        handshake_ms.push(ms);
        // 子プロセスとホストを確実に終了させてから次の試行へ。
        drop(session);
        drop(host);
    }
    Ok(ColdStartMeasurement { handshake_ms })
}

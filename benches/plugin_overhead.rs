//! MCP 参照プラグイン（`fandhe-browser-mcp`）の性能計測: アイドル RSS 増分
//! （TASK-95.1・Issue #375・`PLUG-3`・`MS-9`）。
//!
//! 実行: `make measure-plugin-overhead`（mcp の release ビルド後に
//! `cargo bench -p fandhe-browser-ai --bench plugin_overhead -- [--metric idle-rss|cold-start|call-latency|all] [--format tsv|jsonl] [--samples N] [--trials N] ...`）。
//! 詳細な手法・単位・PoC-15 との差は `benches/plugin_overhead/README.md`。
//!
//! 指標: `idle-rss`（既定）・`cold-start`・`call-latency`。
//! cold start（TASK-95.2・Issue #376）: `--metric cold-start [--trials N]` でプラグイン起動から MCP
//! ハンドシェイク完了までの壁時計時間を試行ごと（新しいホスト・新しいプロセス）に ms で採取する。
//! 呼び出しレイテンシ（TASK-95.3・Issue #377）: `--metric call-latency [--calls N]`。
//! `tools/call snapshot` の往復を N 回（既定 30）計測し p50・p95 を ms で出す。
//!
//! idle-rss の計測: 本物の ai ルータを in-process で loopback 公開した本体（ベンチ自身）の RSS と、mcp を接続して
//! アイドル（登録・initialize 済み・ツール未呼び出し）にした状態の RSS を `ps` で採取し、中央値の差を
//! MB（KiB ÷ 1024）で TSV として stdout に出す。診断は stderr（英語）。
//!
//! 終了コード: 計測できれば値に関係なく 0（`PLUG-3` の達成判定は #379 の人間担当で、本ベンチは判定しない）。
//! バイナリ不在・登録失敗・採取失敗・引数不正・非 unix は非 0（fail-closed）。
//!
//! 将来仕様（REPAIR-3）: cli が引数（ポート・プロファイル）を持ったら in-process ホストを実バイナリへ
//! 切り替える（TASK-47・`CLI-1`）。
//! TASK-10 形式のレポート化（TASK-95.4・Issue #378）: `--format jsonl` で指標ごとに 1 行の JSON Lines を
//! stdout へ出す（整形は `report.rs`）。`--metric all` で 3 指標を固定順に計測する。

#[cfg(unix)]
#[path = "../crates/fandhe-browser-ai/benches/mcp_envelope/app_host.rs"]
mod app_host;
#[cfg(unix)]
#[path = "plugin_overhead/call_latency.rs"]
mod call_latency;
#[cfg(unix)]
#[path = "plugin_overhead/call_latency_stats.rs"]
mod call_latency_stats;
#[cfg(unix)]
#[path = "plugin_overhead/cold_start.rs"]
mod cold_start;
#[cfg(unix)]
#[path = "plugin_overhead/cold_start_stats.rs"]
mod cold_start_stats;
#[cfg(unix)]
#[path = "../crates/fandhe-browser-ai/benches/mcp_envelope/host.rs"]
mod host;
#[cfg(unix)]
#[path = "plugin_overhead/idle_rss.rs"]
mod idle_rss;
#[cfg(unix)]
#[path = "../crates/fandhe-browser-ai/benches/mcp_envelope/jsonrpc.rs"]
mod jsonrpc;
#[cfg(unix)]
#[path = "../crates/fandhe-browser-ai/benches/mcp_envelope/mcp_client.rs"]
mod mcp_client;
#[cfg(unix)]
#[path = "plugin_overhead/options.rs"]
mod options;
#[cfg(unix)]
#[path = "plugin_overhead/report.rs"]
mod report;
#[cfg(unix)]
#[path = "plugin_overhead/rss.rs"]
mod rss;

#[cfg(unix)]
fn run() -> Result<(), String> {
    use options::{Format, Metric};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = options::Options::parse(&args)?;
    let env_bin = std::env::var(mcp_client::BIN_ENV).ok();
    let bin = mcp_client::resolve_bin(
        env_bin.as_deref(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
    )?;
    let t = &fandhe_browser_ai::plugin_api::PLUG3_TARGETS;
    // jsonl では全指標を計測し終えて整形に成功してから 1 回だけ出す（部分的なレポートを残さない）。
    let mut idle = None;
    let mut cold = None;
    let mut call = None;
    for metric in &opts.metrics {
        match metric {
            Metric::IdleRss => {
                let m = idle_rss::measure(&bin, &opts)?;
                if opts.format == Format::Tsv {
                    println!(
                        "{}",
                        rss::render_idle_rss(&m, t.idle_rss_increase_max_bytes())
                            .map_err(|e| e.to_string())?
                    );
                }
                idle = Some(m);
            }
            Metric::ColdStart => {
                let m = cold_start::measure(&bin, &opts)?;
                if opts.format == Format::Tsv {
                    println!(
                        "{}",
                        cold_start_stats::render_cold_start(&m, t.cold_start_max_ms())
                            .map_err(|e| e.to_string())?
                    );
                }
                cold = Some(m);
            }
            Metric::CallLatency => {
                let m = call_latency::measure(&bin, &opts)?;
                if opts.format == Format::Tsv {
                    println!(
                        "{}",
                        call_latency_stats::render_call_latency(
                            &m,
                            t.call_latency_p50_max_ms(),
                            t.call_latency_p95_max_ms()
                        )
                        .map_err(|e| e.to_string())?
                    );
                }
                call = Some(m);
            }
        }
    }
    if opts.format == Format::Jsonl {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before the unix epoch".to_string())?;
        let ts = u64::try_from(now.as_millis()).map_err(|_| "timestamp overflow".to_string())?;
        let targets = report::Targets {
            idle_rss_increase_max_bytes: t.idle_rss_increase_max_bytes(),
            cold_start_max_ms: t.cold_start_max_ms(),
            call_latency_p50_max_ms: t.call_latency_p50_max_ms(),
            call_latency_p95_max_ms: t.call_latency_p95_max_ms(),
        };
        let out = report::render_jsonl(idle.as_ref(), cold.as_ref(), call.as_ref(), &targets, ts)
            .map_err(|e| e.to_string())?;
        print!("{out}");
    }
    Ok(())
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("plugin_overhead: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 非 unix では `Profile::open` が `Unsupported` を返す（AppState を構築できない）ため計測不能。
#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("plugin_overhead: unsupported on this platform (profile requires unix)");
    std::process::ExitCode::FAILURE
}

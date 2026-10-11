//! MCP 参照プラグイン（`fandhe-browser-mcp`）の性能計測: アイドル RSS 増分
//! （TASK-95.1・Issue #375・`PLUG-3`・`MS-9`）。
//!
//! 実行: `make measure-plugin-overhead`（mcp の release ビルド後に
//! `cargo bench -p fandhe-browser-ai --bench plugin_overhead -- [--metric idle-rss] [--samples N] ...`）。
//! 詳細な手法・単位・PoC-15 との差は `benches/plugin_overhead/README.md`。
//!
//! 計測: 本物の ai ルータを in-process で loopback 公開した本体（ベンチ自身）の RSS と、mcp を接続して
//! アイドル（登録・initialize 済み・ツール未呼び出し）にした状態の RSS を `ps` で採取し、中央値の差を
//! MB（KiB ÷ 1024）で TSV として stdout に出す。診断は stderr（英語）。
//!
//! 終了コード: 計測できれば値に関係なく 0（`PLUG-3` の達成判定は #379 の人間担当で、本ベンチは判定しない）。
//! バイナリ不在・登録失敗・採取失敗・引数不正・非 unix は非 0（fail-closed）。
//!
//! 将来仕様（REPAIR-3）: cli が引数（ポート・プロファイル）を持ったら in-process ホストを実バイナリへ
//! 切り替える（TASK-47・`CLI-1`）。cold start（TASK-95.2）・レイテンシ（TASK-95.3）は `--metric` として
//! 追加し、TASK-10 形式のレポート化は TASK-95.4 が担う。

#[cfg(unix)]
#[path = "../crates/fandhe-browser-ai/benches/mcp_envelope/app_host.rs"]
mod app_host;
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
#[path = "plugin_overhead/rss.rs"]
mod rss;

#[cfg(unix)]
fn run() -> Result<(), String> {
    use options::Metric;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = options::Options::parse(&args)?;
    let env_bin = std::env::var(mcp_client::BIN_ENV).ok();
    let bin = mcp_client::resolve_bin(
        env_bin.as_deref(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
    )?;
    for metric in &opts.metrics {
        match metric {
            Metric::IdleRss => {
                let m = idle_rss::measure(&bin, &opts)?;
                let target =
                    fandhe_browser_ai::plugin_api::PLUG3_TARGETS.idle_rss_increase_max_bytes();
                println!(
                    "{}",
                    rss::render_idle_rss(&m, target).map_err(|e| e.to_string())?
                );
            }
        }
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

//! アイドル RSS 増分計測の e2e smoke（TASK-95.1・Issue #375・`PLUG-3`）。
//!
//! 実バイナリ（`FANDHE_BROWSER_MCP_BIN`）が必要なため `#[ignore]`。`make check-plugin-overhead` が
//! `--ignored` 付きで実行する。環境変数未設定は skip せず失敗にする。RSS の絶対値や 10MB との大小は
//! 環境依存のため検査しない（達成判定は #379）。非 unix は `Profile::open` が使えないため全体を除外する。

#![cfg(unix)]

#[allow(dead_code)]
#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/app_host.rs"]
mod app_host;
#[allow(dead_code)]
#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/host.rs"]
mod host;
#[path = "idle_rss.rs"]
mod idle_rss;
#[allow(dead_code)]
#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/jsonrpc.rs"]
mod jsonrpc;
#[allow(dead_code)]
#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/mcp_client.rs"]
mod mcp_client;
#[path = "options.rs"]
mod options;
#[path = "rss.rs"]
mod rss;

#[test]
#[ignore = "requires the fandhe-browser-mcp binary (FANDHE_BROWSER_MCP_BIN); run via make check-plugin-overhead"]
fn plug3_e2e_idle_rss_increment_is_measured_with_real_binary() {
    let env_bin = std::env::var(mcp_client::BIN_ENV).ok();
    let bin = mcp_client::resolve_bin(
        env_bin.as_deref(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect("mcp binary");
    let opts = options::Options::parse(
        &[
            "--samples",
            "3",
            "--settle-ms",
            "100",
            "--interval-ms",
            "20",
        ]
        .map(String::from),
    )
    .expect("options");
    let m = idle_rss::measure(&bin, &opts).expect("measure");
    assert_eq!(m.host_only_kib.len(), 3);
    assert_eq!(m.host_plus_plugin_kib.len(), 3);
    assert_eq!(m.plugin_kib.len(), 3);
    for (both, plugin) in m.host_plus_plugin_kib.iter().zip(&m.plugin_kib) {
        assert!(*plugin > 0, "plugin RSS must be positive");
        assert!(
            both >= plugin,
            "host+plugin {both} must include plugin {plugin}"
        );
    }
    assert!(m.increment_mb().is_some());
    let out = rss::render_idle_rss(&m, 10 * 1024 * 1024).expect("render");
    assert!(out.contains("idle_rss\t3\t"), "{out}");
}

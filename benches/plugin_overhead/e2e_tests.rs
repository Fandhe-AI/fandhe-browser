//! アイドル RSS 増分計測の e2e smoke（TASK-95.1・Issue #375・`PLUG-3`）。
//!
//! 実バイナリ（`FANDHE_BROWSER_MCP_BIN`）が必要なため `#[ignore]`。`make check-plugin-overhead` が
//! `--ignored` 付きで実行する。環境変数未設定は skip せず失敗にする。RSS の絶対値や 10MB との大小は
//! 環境依存のため検査しない（達成判定は #379）。非 unix は `Profile::open` が使えないため全体を除外する。

#![cfg(unix)]

#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/app_host.rs"]
mod app_host;
#[path = "call_latency.rs"]
mod call_latency;
#[path = "call_latency_stats.rs"]
mod call_latency_stats;
#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/host.rs"]
mod host;
#[path = "idle_rss.rs"]
mod idle_rss;
#[path = "../../crates/fandhe-browser-ai/benches/mcp_envelope/jsonrpc.rs"]
mod jsonrpc;
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

/// 初期化後にプラグインが終了した場合、ゾンビの RSS 0 を採取せず計測失敗になる（TASK-95.1・`PLUG-3`）。
#[test]
fn plug3_measure_fails_when_plugin_exits_after_initialize() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("fandhe-idle-rss-exit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let script = dir.join("fake-mcp.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nread line\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nsleep 0.2\nexit 0\n",
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let opts = options::Options::parse(
        &[
            "--samples",
            "2",
            "--settle-ms",
            "800",
            "--interval-ms",
            "20",
        ]
        .map(String::from),
    )
    .expect("options");
    let result = idle_rss::measure(&script, &opts);
    let _ = std::fs::remove_dir_all(&dir);
    let err = result.expect_err("measurement must fail when the plugin has exited");
    assert!(err.contains("plugin process exited"), "{err}");
}

/// 実バイナリで `tools/call snapshot` を N 回計測できる（TASK-95.3・`PLUG-3`）。目標値との大小は
/// debug ビルドで走るため検査しない（達成判定は #379）。
#[test]
#[ignore = "requires the fandhe-browser-mcp binary (FANDHE_BROWSER_MCP_BIN); run via make check-plugin-overhead"]
fn plug3_e2e_call_latency_is_measured_with_real_binary() {
    let env_bin = std::env::var(mcp_client::BIN_ENV).ok();
    let bin = mcp_client::resolve_bin(
        env_bin.as_deref(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect("mcp binary");
    let opts =
        options::Options::parse(&["--metric", "call-latency", "--calls", "5"].map(String::from))
            .expect("options");
    let m = call_latency::measure(&bin, &opts).expect("measure");
    assert_eq!(m.samples_ms.len(), 5);
    for v in &m.samples_ms {
        assert!(v.is_finite() && *v > 0.0 && *v < 30_000.0, "{v}");
    }
    assert!(m.p50_ms().expect("p50") <= m.p95_ms().expect("p95"));
    let out = call_latency_stats::render_call_latency(&m, 20, 100).expect("render");
    assert!(out.contains("call_latency\t5\t"), "{out}");
}

/// ツールが isError を返したら標本に数えず計測失敗になる（TASK-95.3・`PLUG-3`）。
#[test]
fn plug3_call_latency_fails_when_tool_returns_error() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("benches")
        .join("plugin_overhead")
        .join("fixtures")
        .join("fake-mcp-tool-error.sh");
    let opts =
        options::Options::parse(&["--metric", "call-latency", "--calls", "3"].map(String::from))
            .expect("options");
    let err = call_latency::measure(&script, &opts).expect_err("tool error must fail");
    assert!(err.contains("isError"), "{err}");
}

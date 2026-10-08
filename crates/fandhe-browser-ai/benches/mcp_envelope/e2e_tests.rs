//! MCP エンベロープ測定ハーネスの e2e smoke（TASK-97.1・Issue #385・`PLUG-5`）。
//!
//! 実バイナリ（`FANDHE_BROWSER_MCP_BIN`）が必要なため `#[ignore]`。`make check-mcp-envelope` が
//! `--ignored` 付きで実行する。環境変数未設定は skip せず失敗にする。
//! 非 unix では `Profile::open` が使えず AppState を構築できないため全体を `cfg(unix)` とする。

#![cfg(unix)]

#[allow(dead_code)]
#[path = "harness.rs"]
mod harness;
#[allow(dead_code)]
#[path = "host.rs"]
mod host;
#[allow(dead_code)]
#[path = "jsonrpc.rs"]
mod jsonrpc;
#[allow(dead_code)]
#[path = "mcp_client.rs"]
mod mcp_client;
#[allow(dead_code)]
#[path = "stats.rs"]
mod stats;
#[allow(dead_code)]
#[path = "../token_reduction/tokens.rs"]
mod tokens;

#[test]
#[ignore = "requires the fandhe-browser-mcp binary (FANDHE_BROWSER_MCP_BIN); run via make check-mcp-envelope"]
fn plug5_e2e_mcp_snapshot_matches_direct_and_envelope_is_positive() {
    let env_bin = std::env::var(mcp_client::BIN_ENV).ok();
    let bin = mcp_client::resolve_bin(
        env_bin.as_deref(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect("mcp binary");
    let counter = tokens::TokenCounter::new().expect("tokenizer");
    let mut h = harness::Harness::start(&bin).expect("harness start (register + initialize)");
    let r = h
        .measure(&counter, "example-minimal", 1)
        .expect("measure example-minimal");
    assert_eq!(r.row.name, "example-minimal");
    assert_eq!(r.row.raw_html_tokens, 103);
    // 一致検証は measure 内で済んでいる。MCP の text は方式 B 本体と同じトークン数になる。
    assert_eq!(r.row.mcp_text_tokens, r.row.direct_tokens);
    assert!(
        r.row.envelope_tokens() > 0,
        "envelope must add tokens: {}",
        r.row.envelope_tokens()
    );
    assert_eq!(r.direct_ms.len(), 1);
    assert_eq!(r.mcp_ms.len(), 1);
}

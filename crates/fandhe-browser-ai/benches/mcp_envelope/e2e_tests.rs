//! MCP エンベロープ測定ハーネスの e2e smoke（TASK-97.1・Issue #385・`PLUG-5`）。
//!
//! 実バイナリ（`FANDHE_BROWSER_MCP_BIN`）が必要なため `#[ignore]`。`make check-mcp-envelope` が
//! `--ignored` 付きで実行する。環境変数未設定は skip せず失敗にする。
//! 非 unix では `Profile::open` が使えず AppState を構築できないため全体を `cfg(unix)` とする。

#![cfg(unix)]

#[path = "harness.rs"]
mod harness;
#[path = "host.rs"]
mod host;
#[path = "jsonrpc.rs"]
mod jsonrpc;
#[path = "mcp_client.rs"]
mod mcp_client;
#[path = "report.rs"]
mod report;
#[path = "stats.rs"]
mod stats;
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
        .measure(&counter, harness::FIXTURES[0], 1)
        .expect("measure example-minimal");
    assert_eq!(r.row.name, "example-minimal");
    assert_eq!(harness::FIXTURES.len(), 5);
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
    // レポート整形は集計・TSV 関数を通す。1 反復でも行が出る。
    let report = report::render(&[r], 1).expect("render report");
    assert!(report.starts_with(stats::tsv_header()), "{report}");
    assert!(
        report.contains("# MCP envelope vs direct (PLUG-5)"),
        "{report}"
    );
    assert!(report.contains("pages\t1"), "{report}");
}

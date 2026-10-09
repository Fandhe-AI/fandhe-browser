//! MCP エンベロープ測定ハーネスの e2e smoke（TASK-97.1・Issue #385・`PLUG-5`）。
//!
//! 実バイナリ（`FANDHE_BROWSER_MCP_BIN`）が必要なため `#[ignore]`。`make check-mcp-envelope` が
//! `--ignored` 付きで実行する。環境変数未設定は skip せず失敗にする。
//! トークン・バイト列は固定値（`PINNED`）と、`docs/design/mcp-envelope-report.md` への逐語掲載を検査する
//! （TASK-97.2・#386）。レイテンシは環境依存のため検査しない。
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

/// 決定的な計測値: `[rawHtml, direct, mcpText, mcpLine, directBytes]`（id 桁数で変わる mcpLineBytes は除く）。
/// snapshot の形が変わったら `make measure-mcp-envelope MCP_ENVELOPE_ARGS="--iterations 100 --markdown"`
/// で再生成し、こことレポートを貼り直す。
const PINNED: [(&str, [usize; 5]); 5] = [
    ("example-minimal", [103, 193, 193, 255, 646]),
    ("wikipedia-article", [56482, 64318, 64318, 74555, 202138]),
    ("hn-list", [10220, 21248, 21248, 24490, 66877]),
    ("login-form", [284, 544, 544, 666, 1710]),
    ("mdn-docs", [19005, 31454, 31454, 35222, 96274]),
];

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
    assert_eq!(harness::FIXTURES.len(), 5);
    assert_eq!(PINNED.len(), harness::FIXTURES.len());
    let mut results = Vec::new();
    for (name, pinned) in PINNED {
        assert!(harness::FIXTURES.contains(&name), "unknown fixture {name}");
        let r = h.measure(&counter, name, 1).expect("measure fixture");
        let actual = [
            r.row.raw_html_tokens,
            r.row.direct_tokens,
            r.row.mcp_text_tokens,
            r.row.mcp_line_tokens,
            r.row.direct_bytes,
        ];
        assert_eq!(
            actual, pinned,
            "{name}: pinned values differ; if the snapshot shape changed, rerun `make measure-mcp-envelope MCP_ENVELOPE_ARGS=\"--iterations 100 --markdown\"` and update PINNED and the report"
        );
        // 一致検証は measure 内で済んでいる。MCP の text は方式 B 本体と同じトークン数になる。
        assert_eq!(r.row.mcp_text_tokens, r.row.direct_tokens);
        assert!(
            r.row.envelope_tokens() > 0,
            "envelope must add tokens: {}",
            r.row.envelope_tokens()
        );
        assert_eq!(r.direct_ms.len(), 1);
        assert_eq!(r.mcp_ms.len(), 1);
        results.push(r);
    }
    // レポート整形は集計・TSV 関数を通す。1 反復でも行が出る。
    let report = report::render(&results[..1], 1).expect("render report");
    assert!(report.starts_with(stats::tsv_header()), "{report}");
    assert!(
        report.contains("# MCP envelope vs direct (PLUG-5)"),
        "{report}"
    );
    assert!(report.contains("pages\t1"), "{report}");

    // コミット済みレポートのトークンブロックが現在の出力と逐語で一致する。
    let md = report::render_markdown(&results, 1).expect("render markdown");
    assert!(
        md.latency.starts_with("### Latency (iterations=1)"),
        "{}",
        md.latency
    );
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/design/mcp-envelope-report.md"
    ))
    .expect("read committed report")
    .replace("\r\n", "\n");
    assert!(
        doc.contains(&md.tokens),
        "committed report is out of date; regenerate with `make measure-mcp-envelope MCP_ENVELOPE_ARGS=\"--iterations 100 --markdown\"` and paste the tokens block:\n{}",
        md.tokens
    );
}

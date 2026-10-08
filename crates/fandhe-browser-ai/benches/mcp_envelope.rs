//! 方式 B 本体の MCP エンベロープ測定ハーネス（TASK-97.1・Issue #385・`PLUG-5`・`MS-9`）。
//!
//! 実行: `make measure-mcp-envelope`（mcp バイナリの release ビルド後に
//! `cargo bench -p fandhe-browser-ai --bench mcp_envelope -- [--iterations N]`）。
//!
//! 本物の ai ルータ（`GET /ai/snapshot`）と mcp バイナリ（`snapshot` ツール）を loopback で
//! 接続し、代表 5 サイトについて生 HTML・方式 B 本体・MCP text・MCP 応答行全体の
//! 4 系列のトークン数（`cl100k_base`）とバイト数、エンベロープ増分、直接 GET と MCP 経路の
//! レイテンシ（p50/p95/平均と増分）を TSV で stdout に出す。診断は stderr（英語）。
//!
//! 終了コード: 計測できれば値に関係なく 0（`PLUG-5` に目標値はなく、判断は #387）。
//! バイナリ不在・登録失敗・タイムアウト・一致検証失敗・`isError`・非 unix は非 0（fail-closed）。
//! 数値の確定とレポート化は #386（TASK-97.2）。
//!
//! 将来仕様（REPAIR-3）: ホストに `/ai/navigate` が実装されたら MCP の navigate 経由へ切り替える。
//! in-process の `dispatch`（TCP なし）系列の追加は #386 への申し送り。

#[cfg(unix)]
#[path = "mcp_envelope/harness.rs"]
mod harness;
#[cfg(unix)]
#[path = "mcp_envelope/host.rs"]
mod host;
#[cfg(unix)]
#[path = "mcp_envelope/jsonrpc.rs"]
mod jsonrpc;
#[cfg(unix)]
#[path = "mcp_envelope/mcp_client.rs"]
mod mcp_client;
#[cfg(unix)]
#[path = "mcp_envelope/stats.rs"]
mod stats;
#[cfg(unix)]
#[allow(
    dead_code,
    reason = "token_reduction 共有の tokens.rs の一部（raw HTML 計測等）をこの bench は使わない"
)]
#[path = "token_reduction/tokens.rs"]
mod tokens;

#[cfg(unix)]
fn parse_iterations(args: &[String]) -> Result<usize, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--iterations" {
            let v = it
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or("--iterations requires an integer value")?;
            if !(1..=1000).contains(&v) {
                return Err("--iterations must be in 1..=1000".to_string());
            }
            return Ok(v);
        }
    }
    Ok(30)
}

#[cfg(unix)]
fn run() -> Result<(), String> {
    use stats::{aggregate, increments, latency_row, summarize_latency, tsv_header, tsv_row};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let iterations = parse_iterations(&args)?;
    let env_bin = std::env::var(mcp_client::BIN_ENV).ok();
    let bin = mcp_client::resolve_bin(
        env_bin.as_deref(),
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
    )?;
    let counter = tokens::TokenCounter::new().map_err(|e| e.to_string())?;
    let mut h = harness::Harness::start(&bin)?;

    let mut results = Vec::new();
    for name in harness::FIXTURES {
        eprintln!("measuring {name} ({iterations} iterations)");
        results.push(h.measure(&counter, name, iterations)?);
    }

    println!("{}", tsv_header());
    for r in &results {
        println!("{}", tsv_row(&r.row));
    }
    println!();
    println!("fixture\tpath\tp50Ms\tp95Ms\tmeanMs");
    let (mut all_direct, mut all_mcp) = (Vec::new(), Vec::new());
    for r in &results {
        let inc = increments(&r.mcp_ms, &r.direct_ms);
        for (label, v) in [
            ("direct", &r.direct_ms),
            ("mcp", &r.mcp_ms),
            ("increment", &inc),
        ] {
            let s = summarize_latency(v).ok_or("no latency samples")?;
            println!("{}", latency_row(&format!("{}\t{label}", r.row.name), &s));
        }
        all_direct.extend_from_slice(&r.direct_ms);
        all_mcp.extend_from_slice(&r.mcp_ms);
    }
    let all_inc = increments(&all_mcp, &all_direct);
    for (label, v) in [
        ("direct", &all_direct),
        ("mcp", &all_mcp),
        ("increment", &all_inc),
    ] {
        let s = summarize_latency(v).ok_or("no latency samples")?;
        println!("{}", latency_row(&format!("ALL\t{label}"), &s));
    }

    let direct_pct: Vec<f64> = results
        .iter()
        .filter_map(|r| r.row.direct_reduction_pct())
        .collect();
    let mcp_pct: Vec<f64> = results
        .iter()
        .filter_map(|r| r.row.mcp_reduction_pct())
        .collect();
    let env_tokens: Vec<f64> = results
        .iter()
        .map(|r| r.row.envelope_tokens() as f64)
        .collect();
    println!();
    println!("# MCP envelope vs direct (PLUG-5)");
    println!("pages\t{}", results.len());
    println!("iterations\t{iterations}");
    for (label, v) in [
        ("DirectReductionPct", &direct_pct),
        ("McpReductionPct", &mcp_pct),
        ("EnvelopeTokens", &env_tokens),
    ] {
        let a = aggregate(v).ok_or("no aggregate")?;
        println!("mean{label}\t{:.1}", a.mean);
        println!("min{label}\t{:.1}", a.min);
        println!("max{label}\t{:.1}", a.max);
    }
    Ok(())
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mcp_envelope: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// 非 unix では `Profile::open` が `Unsupported` を返す（AppState を構築できない）ため測定不能。
#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("mcp_envelope: unsupported on this platform (profile requires unix)");
    std::process::ExitCode::FAILURE
}

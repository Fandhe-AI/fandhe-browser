//! 代表 5 サイトの MCP `snapshot` ツール出力のトークン削減率測定
//! （TASK-96.1・`PLUG-4`・Issue #381・`MS-9`）。
//!
//! 役割: MCP 参照プラグイン（`fandhe-browser-mcp`）の `snapshot` ツールが返すテキストと
//! 生 HTML を `cl100k_base` で数え、サイトごとの削減率と平均を算出する。
//! 84.0% 以上の判定は TASK-96.2（#382）、レポート出力は TASK-96.3（#383）の責務で、
//! ここは数値の算出だけを行い、しきい値に対する assert を持たない。
//!
//! 配置の理由: ルート manifest は仮想 manifest でルートの `tests/` は cargo に拾われず、
//! mcp crate は ai・core と `tiktoken-rs` に依存できない（`PLUG-1`・#92 の承認は ai の
//! dev-dependency に限る）ため、ai の結合テストとして置く。後続 Issue も本ファイルを編集する。
//!
//! 測定対象の同一性: mcp の `interpret` はホストの `GET /ai/snapshot` 応答本文を
//! `serde_json::Value` に解析して `to_string` で再直列化し、text コンテンツ 1 件として返す。
//! 本テストは ai の `snapshot_body`（応答本文の生成元）に同じ変換をかけてバイト一致を確かめ、
//! その文字列を数える。数える範囲は text コンテンツのみで JSON-RPC のフレームは含めない
//! （PoC-15 と同じ）。
//!
//! 注意: フィクスチャは合成で、PoC-15 の実ページ（87.8%）とは厳密に比較できない。
//! 固定値は snapshot の形（TASK-15/16/19・`AISNAP-6`）が変われば測り直しが要る。

use std::error::Error;
use std::fs;

use fandhe_browser_ai::api::snapshot_body;
use fandhe_browser_core::{NavigationResult, NavigationState};
use serde_json::Value;

// 共有モジュールを #[path] で取り込むため、本テストが使わない公開項目は dead_code になる。
#[allow(dead_code)]
#[path = "../benches/token_reduction/tokens.rs"]
mod tokens;

#[allow(dead_code)]
#[path = "../benches/token_reduction/snapshot_text.rs"]
mod snapshot_text;

#[allow(dead_code)]
#[path = "../benches/token_reduction/reduction.rs"]
mod reduction;

use reduction::{ReductionRow, reduction_pct, summarize};
use tokens::{TokenCounter, fixtures_dir};

type TestResult<T> = Result<T, Box<dyn Error>>;

/// 代表 5 サイト（`PLUG-4` の列挙順）。
const SITES: [&str; 5] = [
    "example-minimal",
    "wikipedia-article",
    "hn-list",
    "login-form",
    "mdn-docs",
];

/// PoC-15 と同じ形の URL。エンベロープの `url` として数トークンを占める。
fn site_url(name: &str) -> String {
    format!("http://127.0.0.1:8899/{name}.html")
}

fn read_fixture(name: &str) -> TestResult<String> {
    let path = fixtures_dir().join(format!("{name}.html"));
    fs::read_to_string(&path)
        .map_err(|e| format!("failed to read fixture {}: {e}", path.display()).into())
}

/// ホスト応答本文（`snapshot_body`）を文字列で得る。
fn host_body(name: &str, html: &str) -> TestResult<String> {
    let nav = NavigationState::new();
    let g = nav
        .begin_navigation()
        .map_err(|e| format!("failed to begin navigation for {name}: {e:?}"))?;
    nav.commit_navigation(g, NavigationResult::new(site_url(name), html))
        .map_err(|e| format!("failed to commit navigation for {name}: {e:?}"))?;
    let body =
        snapshot_body(&nav).map_err(|e| format!("snapshot_body failed for {name}: {e:?}"))?;
    String::from_utf8(body).map_err(|e| format!("non-utf8 body for {name}: {e}").into())
}

/// mcp の `interpret` と同じ変換（解析 → コンパクト再直列化）を行い、
/// エンベロープ形（url:文字列・tree:オブジェクト・truncated:真偽値）を検証して
/// (MCP 出力テキスト, truncated) を返す。
fn mcp_snapshot_text(name: &str, body: &str) -> TestResult<(String, bool)> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("host body of {name} is not JSON: {e}"))?;
    let truncated = match (
        value.get("url").map(Value::is_string),
        value.get("tree").map(Value::is_object),
        value.get("truncated").and_then(Value::as_bool),
    ) {
        (Some(true), Some(true), Some(t)) => t,
        _ => return Err(format!("unexpected snapshot envelope shape for {name}").into()),
    };
    let text = serde_json::to_string(&value)
        .map_err(|e| format!("failed to re-serialize body of {name}: {e}"))?;
    Ok((text, truncated))
}

/// 5 サイトの削減率を測る。
fn measure_mcp_reduction(counter: &TokenCounter) -> TestResult<Vec<ReductionRow>> {
    let mut rows = Vec::with_capacity(SITES.len());
    for name in SITES {
        let html = read_fixture(name)?;
        let body = host_body(name, &html)?;
        let (text, truncated) = mcp_snapshot_text(name, &body)?;
        let raw = counter.count(&html);
        let snap = counter.count(&text);
        let pct = reduction_pct(raw, snap)
            .ok_or_else(|| format!("raw HTML of {name} has zero tokens"))?;
        rows.push(ReductionRow {
            name: name.to_string(),
            bytes: html.len(),
            raw_html_tokens: raw,
            snapshot_tokens: snap,
            reduction_pct: pct,
            snapshot_truncated: truncated,
        });
    }
    Ok(rows)
}

/// `PLUG-4`: MCP 出力は `snapshot_body` のバイト列と一致する（再直列化で変化しない）。
#[test]
fn plug4_mcp_snapshot_text_matches_interpret_transform() -> TestResult<()> {
    for name in SITES {
        let html = read_fixture(name)?;
        let body = host_body(name, &html)?;
        let (text, _) = mcp_snapshot_text(name, &body)?;
        assert_eq!(text, body, "re-serialization changed the body of {name}");
    }
    Ok(())
}

/// `PLUG-4`: 5 サイトそれぞれで削減率が算出される。
#[test]
fn plug4_mcp_token_reduction_per_site() -> TestResult<()> {
    let counter = TokenCounter::new()?;
    let rows = measure_mcp_reduction(&counter)?;
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, SITES.to_vec());
    // 現行 snapshot_body の挙動を固定した実測値（site, 生 HTML トークン, snapshot トークン, truncated）。
    // 現状は JSON エンベロープが生 HTML より大きく削減率は負になる。判定は #382、
    // 出力形式の調整は範囲外（PLUG-4・PLUG-5 側の論点）。
    let pinned = [
        ("example-minimal", 103, 202, false),
        ("wikipedia-article", 56482, 64327, false),
        ("hn-list", 10220, 21257, true),
        ("login-form", 284, 553, false),
        ("mdn-docs", 19005, 31463, false),
    ];
    for (r, (name, raw, snap, truncated)) in rows.iter().zip(pinned) {
        assert_eq!(
            (
                r.name.as_str(),
                r.raw_html_tokens,
                r.snapshot_tokens,
                r.snapshot_truncated
            ),
            (name, raw, snap, truncated)
        );
    }
    for r in &rows {
        let expected =
            reduction_pct(r.raw_html_tokens, r.snapshot_tokens).ok_or("zero raw tokens")?;
        assert!(r.reduction_pct.is_finite(), "{}: not finite", r.name);
        assert!((r.reduction_pct - expected).abs() < 1e-9, "{}", r.name);
    }
    Ok(())
}

/// `PLUG-4`: 平均・最小・最大がページごとの値と整合する（84.0% の判定は #382）。
#[test]
fn plug4_mcp_token_reduction_summary() -> TestResult<()> {
    let counter = TokenCounter::new()?;
    let rows = measure_mcp_reduction(&counter)?;
    let s = summarize(&rows).ok_or("no rows")?;
    assert_eq!(s.pages, 5);
    let mean = rows.iter().map(|r| r.reduction_pct).sum::<f64>() / 5.0;
    assert!((s.mean_reduction_pct - mean).abs() < 1e-9);
    assert!(s.min_reduction_pct <= s.mean_reduction_pct);
    assert!(s.mean_reduction_pct <= s.max_reduction_pct);
    Ok(())
}

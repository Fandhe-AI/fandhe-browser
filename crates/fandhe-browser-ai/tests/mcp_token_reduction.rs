//! 代表 5 サイトの MCP `snapshot` ツール出力のトークン削減率測定
//! （TASK-96.1・`PLUG-4`・Issue #381・`MS-9`）。
//!
//! 役割: MCP 参照プラグイン（`fandhe-browser-mcp`）の `snapshot` ツールが返すテキストと
//! 生 HTML を `cl100k_base` で数え、サイトごとの削減率と平均を算出する。
//! 84.0% 以上の判定は本ファイルの `judge_plug4` / `assert_plug4` が担う（TASK-96.2・#382）。
//! 測定レポートは TASK-96.3（#383）で `render_report` が Markdown として stdout に出し、
//! `docs/design/mcp-token-reduction-report.md` へ転記する（再生成: `cargo test -p fandhe-browser-ai
//! --test mcp_token_reduction plug4_mcp_token_reduction_report_prints_markdown -- --nocapture`）。
//! コミット済みレポートとの同期は `plug4_committed_report_contains_rendered_block` が検査する。
//! 再利用が要る場合は判定を `benches/token_reduction/` へ移す。
//!
//! 実データの現状: 平均削減率は 84.0% に届いていない（約 -8.5%）。PLUG-4 の前提は PoC-5 の
//! フラット形式だが、現行ホストは JSON ツリーのエンベロープを返し生 HTML より大きいため
//! （PLUG-5・TASK-97・AISNAP-6 の論点）。実データの verdict は未達のまま固定して事実を記録し
//! （REPAIR-3）、合否ロジックは合成データで境界値まで検証する。
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
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

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

use reduction::{ReductionRow, ReductionSummary, reduction_pct, summarize};
use tokens::{TokenCounter, fixtures_dir};

type TestResult<T> = Result<T, Box<dyn Error>>;

/// `PLUG-4` の合格しきい値（%）。方式 B（`AISNAP-1`）の 87.0% から -3 ポイント以内
/// （PoC-15 成功基準 4）。平均削減率がこの値以上なら合格。
const PLUG4_THRESHOLD_PCT: f64 = 84.0;

/// しきい値判定の結果。真偽値にせず、差分を持たせてレポート出力（`render_report`・#383）で差分まで描画できる形にする
/// （REPAIR-4）。
#[derive(Debug, Clone, PartialEq)]
enum Plug4Verdict {
    /// 平均削減率がしきい値以上。
    Met { mean_pct: f64, threshold_pct: f64 },
    /// 平均削減率がしきい値未満（または非有限値）。`gap_pts` は不足ポイント数。
    Shortfall {
        mean_pct: f64,
        threshold_pct: f64,
        gap_pts: f64,
    },
}

/// 平均削減率をしきい値と比べる（境界は以上で合格）。NaN など非有限値は不合格に倒す
/// （fail-closed）。
fn judge_plug4(summary: &ReductionSummary, threshold_pct: f64) -> Plug4Verdict {
    let mean_pct = summary.mean_reduction_pct;
    if mean_pct.is_finite() && mean_pct >= threshold_pct {
        Plug4Verdict::Met {
            mean_pct,
            threshold_pct,
        }
    } else {
        Plug4Verdict::Shortfall {
            mean_pct,
            threshold_pct,
            gap_pts: threshold_pct - mean_pct,
        }
    }
}

/// `PLUG-4` のゲート。不合格なら `Err`（テスト失敗）、合格なら `Ok`。
fn assert_plug4(summary: &ReductionSummary) -> TestResult<()> {
    match judge_plug4(summary, PLUG4_THRESHOLD_PCT) {
        Plug4Verdict::Met { .. } => Ok(()),
        Plug4Verdict::Shortfall {
            mean_pct,
            threshold_pct,
            gap_pts,
        } => Err(format!(
            "PLUG-4 token reduction below threshold: mean {mean_pct:.3}% < {threshold_pct:.1}% (gap {gap_pts:.3} pts)"
        )
        .into()),
    }
}

/// 現行 `snapshot_body` の挙動を固定した実測値（site, 生 HTML トークン, snapshot トークン, truncated）。
/// 現状は JSON エンベロープが生 HTML より大きく削減率は負になる。出力形式の調整は範囲外
/// （PLUG-4・PLUG-5 側の論点）。
const PINNED: [(&str, usize, usize, bool); 5] = [
    ("example-minimal", 103, 92, false),
    ("wikipedia-article", 56482, 41283, false),
    ("hn-list", 10220, 16350, true),
    ("login-form", 284, 209, false),
    ("mdn-docs", 19005, 27817, false),
];

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

/// 測定結果を Markdown のレポート本体（サイト別・集計・PLUG-4 判定の 3 表）に描画する
/// （TASK-96.3・`PLUG-4`・#383）。
///
/// 呼び出し元は stdout 出力テストと、コミット済みレポートとの同期テスト。出力を
/// `docs/design/mcp-token-reduction-report.md` へ逐語で転記する。判定は `Plug4Verdict` を
/// `match` して描画し、未達は未達のまま記す（REPAIR-3）。bytes は `PINNED` に無いため載せない。
fn render_report(
    rows: &[ReductionRow],
    summary: &ReductionSummary,
    verdict: &Plug4Verdict,
) -> Result<String, std::fmt::Error> {
    let mut out = String::new();
    writeln!(out, "### Per site\n")?;
    writeln!(
        out,
        "| site | rawHtmlTokens | snapshotTokens | reductionPct | truncated |"
    )?;
    writeln!(
        out,
        "| ---- | ------------- | -------------- | ------------ | --------- |"
    )?;
    for r in rows {
        writeln!(
            out,
            "| {} | {} | {} | {:.1} | {} |",
            r.name, r.raw_html_tokens, r.snapshot_tokens, r.reduction_pct, r.snapshot_truncated
        )?;
    }
    writeln!(out, "\n### Summary\n")?;
    writeln!(
        out,
        "| pages | meanReductionPct | minReductionPct | maxReductionPct | medianSnapshotTokens |"
    )?;
    writeln!(
        out,
        "| ----- | ---------------- | --------------- | --------------- | -------------------- |"
    )?;
    writeln!(
        out,
        "| {} | {:.1} | {:.1} | {:.1} | {:.1} |",
        summary.pages,
        summary.mean_reduction_pct,
        summary.min_reduction_pct,
        summary.max_reduction_pct,
        summary.median_snapshot_tokens
    )?;
    writeln!(out, "\n### PLUG-4 verdict\n")?;
    writeln!(out, "| thresholdPct | meanPct | verdict | gapPts |")?;
    writeln!(out, "| ------------ | ------- | ------- | ------ |")?;
    match verdict {
        Plug4Verdict::Met {
            mean_pct,
            threshold_pct,
        } => writeln!(out, "| {threshold_pct:.1} | {mean_pct:.1} | Met | 0.0 |")?,
        Plug4Verdict::Shortfall {
            mean_pct,
            threshold_pct,
            gap_pts,
        } => writeln!(
            out,
            "| {threshold_pct:.1} | {mean_pct:.1} | Shortfall | {gap_pts:.1} |"
        )?,
    }
    Ok(out)
}

/// 実測から `render_report` の出力を作る（テスト共通）。
fn measured_report() -> TestResult<String> {
    let counter = TokenCounter::new()?;
    let rows = measure_mcp_reduction(&counter)?;
    let s = summarize(&rows).ok_or("no rows")?;
    let v = judge_plug4(&s, PLUG4_THRESHOLD_PCT);
    Ok(render_report(&rows, &s, &v)?)
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
    for (r, (name, raw, snap, truncated)) in rows.iter().zip(PINNED) {
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

/// `PLUG-4`: 平均・最小・最大がページごとの値と整合する（84.0% の判定は `judge_plug4`）。
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

/// テスト用に平均削減率だけが意味を持つ行を作る（整数トークン数からは境界がちょうど 84.0 にならない）。
fn synthetic_row(name: &str, pct: f64) -> ReductionRow {
    ReductionRow {
        name: name.to_string(),
        bytes: 0,
        raw_html_tokens: 0,
        snapshot_tokens: 0,
        reduction_pct: pct,
        snapshot_truncated: false,
    }
}

fn summary_of(pcts: &[f64]) -> TestResult<ReductionSummary> {
    let rows: Vec<ReductionRow> = pcts
        .iter()
        .enumerate()
        .map(|(i, p)| synthetic_row(&format!("site{i}"), *p))
        .collect();
    summarize(&rows).ok_or_else(|| "no rows".into())
}

/// `PLUG-4`: 平均がちょうど 84.0% なら合格（境界は以上）。
#[test]
fn plug4_threshold_met_at_exact_boundary() -> TestResult<()> {
    let s = summary_of(&[84.0; 5])?;
    assert_eq!(
        judge_plug4(&s, PLUG4_THRESHOLD_PCT),
        Plug4Verdict::Met {
            mean_pct: 84.0,
            threshold_pct: 84.0
        }
    );
    assert_plug4(&s)
}

/// `PLUG-4`: 平均が 84.0% を超えれば合格。
#[test]
fn plug4_threshold_met_above() -> TestResult<()> {
    let s = summary_of(&[87.8, 90.0, 85.0, 86.0, 88.0])?;
    match judge_plug4(&s, PLUG4_THRESHOLD_PCT) {
        Plug4Verdict::Met { mean_pct, .. } => assert!((mean_pct - 87.36).abs() < 1e-9),
        other => return Err(format!("expected Met, got {other:?}").into()),
    }
    assert_plug4(&s)
}

/// `PLUG-4`: 平均が 84.0% を下回れば不合格（テスト失敗に相当する `Err`）。
#[test]
fn plug4_threshold_shortfall_below() -> TestResult<()> {
    let s = summary_of(&[83.9; 5])?;
    match judge_plug4(&s, PLUG4_THRESHOLD_PCT) {
        Plug4Verdict::Shortfall {
            mean_pct,
            threshold_pct,
            gap_pts,
        } => {
            assert!((mean_pct - 83.9).abs() < 1e-9);
            assert_eq!(threshold_pct, 84.0);
            assert!((gap_pts - 0.1).abs() < 1e-9);
        }
        other => return Err(format!("expected Shortfall, got {other:?}").into()),
    }
    let err = match assert_plug4(&s) {
        Ok(()) => return Err("assert_plug4 must fail below threshold".into()),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("84.0"), "{err}");
    assert!(err.contains("83.9"), "{err}");
    Ok(())
}

/// `PLUG-4`: 1 サイトだけ大きく低く平均が割れる場合も不合格（最小値でなく平均で判定する）。
#[test]
fn plug4_threshold_shortfall_when_one_site_drags_mean() -> TestResult<()> {
    let s = summary_of(&[90.0, 90.0, 90.0, 90.0, 40.0])?;
    assert!(matches!(
        judge_plug4(&s, PLUG4_THRESHOLD_PCT),
        Plug4Verdict::Shortfall { .. }
    ));
    assert!(assert_plug4(&s).is_err());
    Ok(())
}

/// `PLUG-4`: 平均が NaN なら不合格に倒す（fail-closed）。
#[test]
fn plug4_threshold_nan_is_shortfall() -> TestResult<()> {
    let s = summary_of(&[90.0, f64::NAN, 90.0])?;
    assert!(matches!(
        judge_plug4(&s, PLUG4_THRESHOLD_PCT),
        Plug4Verdict::Shortfall { .. }
    ));
    assert!(assert_plug4(&s).is_err());
    Ok(())
}

/// `PLUG-4`: 実データの現状 verdict（84.0% 未達・平均約 -8.5%）を固定する。
///
/// これは PLUG-4 未達の事実の記録であり、達成を装うものではない（REPAIR-3）。snapshot の出力形式
/// が縮んで verdict が `Met` に変わったら、本テストを `assert_plug4(&summary)?` による本ゲートに
/// 置き換える（PLUG-5・TASK-97・AISNAP-6 側の対応後）。
#[test]
fn plug4_mcp_token_reduction_current_verdict_is_pinned_shortfall() -> TestResult<()> {
    let counter = TokenCounter::new()?;
    let rows = measure_mcp_reduction(&counter)?;
    let s = summarize(&rows).ok_or("no rows")?;
    let mut sum = 0.0;
    for (_, raw, snap, _) in PINNED {
        sum += reduction_pct(raw, snap).ok_or("zero raw tokens")?;
    }
    let expected_mean = sum / PINNED.len() as f64;
    match judge_plug4(&s, PLUG4_THRESHOLD_PCT) {
        Plug4Verdict::Shortfall {
            mean_pct,
            threshold_pct,
            gap_pts,
        } => {
            assert!((mean_pct - expected_mean).abs() < 1e-9);
            assert_eq!(threshold_pct, 84.0);
            assert!((gap_pts - (84.0 - expected_mean)).abs() < 1e-9);
        }
        other => {
            return Err(format!(
                "verdict changed to {other:?}; replace this test with assert_plug4(&summary)?"
            )
            .into());
        }
    }
    Ok(())
}

/// `PLUG-4`・TASK-96.3: 測定レポートを stdout に出す（`-- --nocapture` で表示し docs へ転記する）。
/// 5 サイトすべての削減率・平均・しきい値・判定が含まれることを具体値で確かめる。
#[test]
fn plug4_mcp_token_reduction_report_prints_markdown() -> TestResult<()> {
    let report = measured_report()?;
    println!("{report}");
    for needle in [
        "| example-minimal | 103 | 92 | 10.7 | false |",
        "| wikipedia-article | 56482 | 41283 | 26.9 | false |",
        "| hn-list | 10220 | 16350 | -60.0 | true |",
        "| login-form | 284 | 209 | 26.4 | false |",
        "| mdn-docs | 19005 | 27817 | -46.4 | false |",
        "| 5 | -8.5 | -60.0 | 26.9 |",
        "| 84.0 | -8.5 | Shortfall | 92.5 |",
    ] {
        assert!(report.contains(needle), "missing row: {needle}\n{report}");
    }
    Ok(())
}

/// `PLUG-4`: サイト別表が `PINNED` から導いた行と一致する（snapshot 形式が変われば落ちる）。
#[test]
fn plug4_report_rows_match_pinned() -> TestResult<()> {
    let report = measured_report()?;
    for (name, raw, snap, truncated) in PINNED {
        let pct = reduction_pct(raw, snap).ok_or("zero raw tokens")?;
        let line = format!("| {name} | {raw} | {snap} | {pct:.1} | {truncated} |");
        assert!(report.lines().any(|l| l == line), "missing line: {line}");
    }
    Ok(())
}

/// `PLUG-4`: 合成データの `Met` 側の描画。
#[test]
fn plug4_report_renders_met_verdict_for_synthetic_data() -> TestResult<()> {
    let s = summary_of(&[87.8, 90.0, 85.0, 86.0, 88.0])?;
    let v = judge_plug4(&s, PLUG4_THRESHOLD_PCT);
    let report = render_report(&[synthetic_row("a", 87.8)], &s, &v)?;
    assert!(report.contains("| 84.0 | 87.4 | Met | 0.0 |"), "{report}");
    assert!(report.contains("| a | 0 | 0 | 87.8 | false |"), "{report}");
    Ok(())
}

/// `PLUG-4`・TASK-96.3: コミット済みレポートに `render_report` の出力が逐語で含まれる。
/// 読み込み失敗は `Err`（fail-closed）。CRLF は LF に正規化して比較する。
#[test]
fn plug4_committed_report_contains_rendered_block() -> TestResult<()> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("design")
        .join("mcp-token-reduction-report.md");
    let committed = fs::read_to_string(&path)?.replace("\r\n", "\n");
    let report = measured_report()?;
    assert!(
        committed.contains(&report),
        "docs/design/mcp-token-reduction-report.md is out of sync; regenerate with --nocapture"
    );
    Ok(())
}

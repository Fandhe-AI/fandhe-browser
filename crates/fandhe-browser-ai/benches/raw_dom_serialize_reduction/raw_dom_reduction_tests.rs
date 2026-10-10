//! `raw_dom_reduction.rs` のユニットテスト（TASK-23.2・`AISNAP-15`・Issue #130）。
//!
//! 期待値はフィクスチャ・core パーサー・現行 snapshot 実装の挙動を固定したもの。
//! 85% 目標の達成判断は #131 の担当で、ここでは目標値に対する assert を持たない。

// 共有モジュールは bench 側の全 API を持ち、このテストが使わない項目が dead_code 警告に
// なる。共有コードの個別削除はできないので理由付きで抑止する。
#[allow(dead_code)]
#[path = "../token_reduction/tokens.rs"]
mod tokens;

#[allow(dead_code)]
#[path = "../token_reduction/snapshot_text.rs"]
mod snapshot_text;

#[allow(dead_code)]
#[path = "../token_reduction/reduction.rs"]
mod reduction;

#[path = "../token_reduction/raw_dom.rs"]
mod raw_dom;

#[allow(dead_code)]
#[path = "raw_dom_tokens.rs"]
mod raw_dom_tokens;

#[allow(dead_code)]
#[path = "raw_dom_reduction.rs"]
mod raw_dom_reduction;

use raw_dom_reduction::{
    RawDomReductionError, RawDomReductionRow, join_rows, measure_raw_dom_reduction, summarize,
};
use raw_dom_tokens::RawDomTokens;
use reduction::ReductionRow;
use std::path::Path;
use tokens::{TokenCounter, fixtures_dir};

fn raw(name: &str, dom: usize) -> RawDomTokens {
    RawDomTokens {
        name: name.to_string(),
        bytes: 10,
        raw_html_tokens: dom + 5,
        raw_dom_tokens: dom,
    }
}

fn snap(name: &str, tokens: usize) -> ReductionRow {
    ReductionRow {
        name: name.to_string(),
        bytes: 10,
        raw_html_tokens: 1000,
        snapshot_tokens: tokens,
        reduction_pct: 0.0,
        snapshot_truncated: false,
    }
}

fn row(pct: f64) -> RawDomReductionRow {
    RawDomReductionRow {
        name: "x.html".into(),
        bytes: 1,
        raw_html_tokens: 1,
        raw_dom_tokens: 1,
        snapshot_tokens: 1,
        reduction_vs_raw_dom_pct: pct,
        reduction_vs_raw_html_pct: 0.0,
        snapshot_truncated: false,
    }
}

#[test]
fn aisnap15_join_rows_computes_reduction_against_raw_dom() {
    let rows = join_rows(
        vec![raw("a.html", 1000), raw("b.html", 100)],
        vec![snap("b.html", 150), snap("a.html", 150)],
    )
    .expect("join");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "a.html");
    assert!((rows[0].reduction_vs_raw_dom_pct - 85.0).abs() < 1e-9);
    assert_eq!(rows[1].name, "b.html");
    assert!((rows[1].reduction_vs_raw_dom_pct - -50.0).abs() < 1e-9);
}

#[test]
fn aisnap15_join_rows_rejects_name_mismatch() {
    let e = join_rows(vec![raw("a.html", 10)], vec![snap("b.html", 5)]).unwrap_err();
    assert!(matches!(e, RawDomReductionError::FixtureMismatch { ref name } if name == "a.html"));
    let e = join_rows(
        vec![raw("a.html", 10)],
        vec![snap("a.html", 5), snap("extra.html", 5)],
    )
    .unwrap_err();
    assert!(
        matches!(e, RawDomReductionError::FixtureMismatch { ref name } if name == "extra.html")
    );
}

#[test]
fn aisnap15_join_rows_rejects_empty_raw_dom() {
    let e = join_rows(vec![raw("a.html", 0)], vec![snap("a.html", 5)]).unwrap_err();
    assert!(matches!(e, RawDomReductionError::EmptyRawDom { ref name } if name == "a.html"));
    assert_eq!(e.to_string(), "raw DOM of a.html has zero tokens");
}

#[test]
fn aisnap15_summarize_gives_mean_min_max() {
    let s = summarize(&[row(80.0), row(-20.0), row(30.0)]).expect("summary");
    assert_eq!(s.pages, 3);
    assert!((s.mean_reduction_pct - 30.0).abs() < 1e-9);
    assert!((s.min_reduction_pct - -20.0).abs() < 1e-9);
    assert!((s.max_reduction_pct - 80.0).abs() < 1e-9);
    assert!(summarize(&[]).is_none());
}

#[test]
fn aisnap15_missing_directory_is_an_error() {
    let c = TokenCounter::new().expect("tokenizer");
    let r = measure_raw_dom_reduction(&c, Path::new("/nonexistent/fixtures-dir"));
    assert!(r.is_err());
}

#[test]
fn aisnap15_real_fixtures_formula_and_order() {
    let c = TokenCounter::new().expect("tokenizer");
    let rows = measure_raw_dom_reduction(&c, &fixtures_dir()).expect("measure");
    assert_eq!(rows.len(), 17);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    for r in &rows {
        let expected = (1.0 - r.snapshot_tokens as f64 / r.raw_dom_tokens as f64) * 100.0;
        assert!(
            (r.reduction_vs_raw_dom_pct - expected).abs() < 1e-9,
            "{}",
            r.name
        );
    }
}

/// 実測の具体値（名前, 生 DOM, snapshot, 削減率）。bench 出力（小数 1 桁）と一致させる。
const EXPECTED: [(&str, usize, usize, f64); 3] = [
    ("example-minimal.html", 52, 43, 17.3),
    ("login-form.html", 233, 137, 41.2),
    ("wikipedia-article.html", 55_917, 26_350, 52.9),
];

#[test]
fn aisnap15_real_fixtures_pinned_values_and_summary() {
    let c = TokenCounter::new().expect("tokenizer");
    let rows = measure_raw_dom_reduction(&c, &fixtures_dir()).expect("measure");
    for (name, dom, snapshot, pct) in EXPECTED {
        let r = rows.iter().find(|r| r.name == name).expect(name);
        assert_eq!(r.raw_dom_tokens, dom, "{name}");
        assert_eq!(r.snapshot_tokens, snapshot, "{name}");
        assert!((r.reduction_vs_raw_dom_pct - pct).abs() < 0.05, "{name}");
    }
    let s = summarize(&rows).expect("summary");
    assert_eq!(s.pages, 17);
    assert!((s.mean_reduction_pct - 9.1).abs() < 0.05);
    assert!((s.min_reduction_pct - -144.5).abs() < 0.05);
    assert!((s.max_reduction_pct - 52.9).abs() < 0.05);
}

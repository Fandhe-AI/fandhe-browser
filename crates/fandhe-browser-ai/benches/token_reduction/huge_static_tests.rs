//! `huge_static.rs`（巨大静的ページ単体の削減率測定）のテスト
//! （TASK-14.5・`AISNAP-5`・Issue #96）。
//!
//! 実測値は現行 `build_snapshot` の挙動を固定したもので、TASK-15・16 等で
//! snapshot の形が変われば更新が要る。現行値は 85% 目標（`AISNAP-5`）未達で、
//! 達成判定は #97（人間担当）のため、目標値に対する assert は書かない。

// 共有モジュールを #[path] で取り込むため、本テストが使わない公開項目は dead_code になる。
#[allow(dead_code)]
#[path = "tokens.rs"]
mod tokens;

#[path = "snapshot_text.rs"]
mod snapshot_text;

#[allow(dead_code)]
#[path = "reduction.rs"]
mod reduction;

#[path = "raw_dom.rs"]
mod raw_dom;

#[allow(dead_code)]
#[path = "huge_static.rs"]
mod huge_static;

use huge_static::{HUGE_STATIC_FIXTURE, measure_huge_static};
use reduction::ReductionError;
use tokens::{TokenCountError, TokenCounter, fixtures_dir};

#[test]
fn huge_static_measured_with_concrete_values() {
    let c = TokenCounter::new().expect("tokenizer");
    let r = measure_huge_static(&c, &fixtures_dir()).expect("measure");
    assert_eq!(r.name, HUGE_STATIC_FIXTURE);
    assert_eq!(r.bytes, 237_689);
    assert_eq!(r.raw_html_tokens, 56_482);
    assert_eq!(r.snapshot_tokens, 29_340);
    assert_eq!(r.raw_dom_tokens, 55_917);
    assert!(!r.snapshot_truncated);
    assert!((r.reduction_vs_raw_dom_pct - 47.5).abs() < 0.05);
    assert!((r.reduction_vs_raw_html_pct - 48.1).abs() < 0.05);
    let expected = (1.0 - r.snapshot_tokens as f64 / r.raw_dom_tokens as f64) * 100.0;
    assert!((r.reduction_vs_raw_dom_pct - expected).abs() < 1e-9);
    assert!(r.raw_dom_tokens > 0 && r.raw_dom_tokens <= r.raw_html_tokens);
}

#[test]
fn missing_dir_is_io_error() {
    let c = TokenCounter::new().expect("tokenizer");
    let dir = fixtures_dir().join("no-such-dir");
    let err = measure_huge_static(&c, &dir).expect_err("must fail");
    assert!(matches!(
        err,
        ReductionError::Tokens(TokenCountError::Io { .. })
    ));
}

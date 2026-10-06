//! `raw_dom_tokens.rs` のユニットテスト（TASK-23.1・`AISNAP-15`・Issue #129）。
//!
//! 期待値はフィクスチャ・`raw_dom.rs`・core パーサーの現行挙動を固定したもの。
//! 85% 目標の達成判断は #131 の担当で、ここでは判定しない。

#[allow(dead_code)]
#[path = "../token_reduction/tokens.rs"]
mod tokens;

#[path = "../token_reduction/raw_dom.rs"]
mod raw_dom;

#[allow(dead_code)]
#[path = "raw_dom_tokens.rs"]
mod raw_dom_tokens;

use raw_dom_tokens::{RawDomTokensError, count_raw_dom_tokens, measure_raw_dom};
use std::path::Path;
use tokens::{TokenCountError, TokenCounter, fixtures_dir};

/// フィクスチャ 17 件の生 DOM トークン数（名前, トークン数）。
const EXPECTED: [(&str, usize); 17] = [
    ("checkboxes-form.html", 108),
    ("dashboard-table.html", 846),
    ("dropdown-form.html", 136),
    ("ec-product-list.html", 7947),
    ("example-minimal.html", 52),
    ("hn-list.html", 10172),
    ("inputs-form.html", 120),
    ("large-table.html", 25706),
    ("login-form.html", 233),
    ("mdn-docs.html", 18963),
    ("python-portal.html", 8554),
    ("quotes-list.html", 1676),
    ("reddit-list.html", 33051),
    ("ssr-next-prerendered.html", 2300),
    ("ssr-nuxt-hydrated-list.html", 2536),
    ("wiki-portal-nav.html", 4348),
    ("wikipedia-article.html", 55_917),
];

#[test]
fn aisnap15_script_and_style_are_removed_before_counting() {
    let c = TokenCounter::new().expect("tokenizer");
    let html = "<html><head><style>p{color:red}</style></head><body><p>hello</p><script>var x = 1;</script></body></html>";
    let n = count_raw_dom_tokens(&c, "inline", html).expect("count");
    assert_eq!(n, c.count("<body><p>hello</p></body>"));
    assert!(n < c.count(html));
}

#[test]
fn aisnap15_html_without_script_or_style_is_unchanged() {
    let c = TokenCounter::new().expect("tokenizer");
    let html = "<html><body><p>hello</p></body></html>";
    let n = count_raw_dom_tokens(&c, "inline", html).expect("count");
    assert_eq!(n, c.count("<body><p>hello</p></body>"));
}

#[test]
fn aisnap15_fixtures_have_concrete_raw_dom_tokens() {
    let c = TokenCounter::new().expect("tokenizer");
    let rows = measure_raw_dom(&c, &fixtures_dir()).expect("measure");
    let actual: Vec<(&str, usize)> = rows
        .iter()
        .map(|r| (r.name.as_str(), r.raw_dom_tokens))
        .collect();
    assert_eq!(actual, EXPECTED.to_vec());
}

#[test]
fn aisnap15_raw_dom_is_nonempty_and_not_larger_than_raw_html() {
    let c = TokenCounter::new().expect("tokenizer");
    let rows = measure_raw_dom(&c, &fixtures_dir()).expect("measure");
    for r in &rows {
        assert!(r.raw_dom_tokens > 0, "{} is empty", r.name);
        assert!(r.raw_dom_tokens <= r.raw_html_tokens, "{} grew", r.name);
    }
    let r = rows
        .iter()
        .find(|r| r.name == "example-minimal.html")
        .expect("row");
    assert_eq!(r.raw_html_tokens, 103);
    assert_eq!(r.bytes, 355);
}

#[test]
fn aisnap15_missing_directory_is_io_error() {
    let c = TokenCounter::new().expect("tokenizer");
    let err = measure_raw_dom(&c, Path::new("/nonexistent/fandhe-raw-dom-dir")).unwrap_err();
    assert!(matches!(
        err,
        RawDomTokensError::Tokens(TokenCountError::Io { .. })
    ));
}

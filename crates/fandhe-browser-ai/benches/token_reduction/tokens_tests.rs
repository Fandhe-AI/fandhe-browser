//! `tokens.rs` のユニットテスト（TASK-14.2・`AISNAP-1`・Issue #93）。

#[path = "tokens.rs"]
mod tokens;

use tokens::{TokenCounter, fixtures_dir, measure_raw_html};

/// フィクスチャ 17 件の期待値（名前, トークン数）。
const EXPECTED: [(&str, usize); 17] = [
    ("checkboxes-form.html", 160),
    ("dashboard-table.html", 897),
    ("dropdown-form.html", 188),
    ("ec-product-list.html", 8055),
    ("example-minimal.html", 103),
    ("hn-list.html", 10220),
    ("inputs-form.html", 171),
    ("large-table.html", 25738),
    ("login-form.html", 284),
    ("mdn-docs.html", 19005),
    ("python-portal.html", 8626),
    ("quotes-list.html", 1762),
    ("reddit-list.html", 33111),
    ("ssr-next-prerendered.html", 3650),
    ("ssr-nuxt-hydrated-list.html", 3560),
    ("wiki-portal-nav.html", 51272),
    ("wikipedia-article.html", 56482),
];

#[test]
fn counts_known_cl100k_string() {
    // OpenAI tiktoken ドキュメントの既知値: ID 列 [83, 1609, 5963, 374, 2294, 0]。
    let c = TokenCounter::new().expect("tokenizer");
    let bpe = tiktoken_rs::cl100k_base().expect("bpe");
    assert_eq!(
        bpe.encode_ordinary("tiktoken is great!"),
        vec![83, 1609, 5963, 374, 2294, 0]
    );
    assert_eq!(c.count("tiktoken is great!"), 6);
}

#[test]
fn empty_and_multibyte() {
    let c = TokenCounter::new().expect("tokenizer");
    assert_eq!(c.count(""), 0);
    assert_eq!(c.count("こんにちは世界"), 4);
}

#[test]
fn special_token_string_is_ordinary_text() {
    let c = TokenCounter::new().expect("tokenizer");
    assert_eq!(c.count("<|endoftext|>"), 7);
}

#[test]
fn fixtures_raw_html_tokens() {
    let c = TokenCounter::new().expect("tokenizer");
    let rows = measure_raw_html(&c, &fixtures_dir()).expect("measure");
    let actual: Vec<(&str, usize)> = rows.iter().map(|r| (r.name.as_str(), r.tokens)).collect();
    assert_eq!(actual, EXPECTED.to_vec());
}

#[test]
fn fixture_bytes_match_file_size() {
    let c = TokenCounter::new().expect("tokenizer");
    let rows = measure_raw_html(&c, &fixtures_dir()).expect("measure");
    let r = rows
        .iter()
        .find(|r| r.name == "example-minimal.html")
        .expect("row");
    assert_eq!(r.bytes, 355);
}

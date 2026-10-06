//! `raw_dom.rs`（生 DOM シリアライザ）のユニットテスト
//! （TASK-14.5・`AISNAP-5`・Issue #96）。
//!
//! 入力はリポ内の小さな HTML 文字列とフィクスチャのみのため、テスト内の
//! `expect` は許容する（`fixtures_inventory.rs` と同じ扱い）。

#[path = "raw_dom.rs"]
mod raw_dom;

use fandhe_browser_core::parse::{ParseOptions, parse_document};
use raw_dom::serialize_raw_dom;

fn ser(html: &str) -> String {
    let parsed = parse_document(html, &ParseOptions::default()).expect("parse");
    serialize_raw_dom(&parsed.document)
}

#[test]
fn serializes_body_only() {
    let out = ser("<html><head><title>T</title></head><body><p id=\"a\">hi</p></body></html>");
    assert_eq!(out, "<body><p id=\"a\">hi</p></body>");
}

#[test]
fn removes_noise_subtrees() {
    let out = ser(concat!(
        "<body><div>a<script>var x=1;</script><style>p{}</style>",
        "<noscript><p>ns</p></noscript><svg><path d=\"M0\"/></svg>",
        "<link rel=\"x\"><meta name=\"y\">b</div></body>"
    ));
    assert_eq!(out, "<body><div>ab</div></body>");
}

#[test]
fn void_elements_have_no_end_tag() {
    let out = ser("<body>a<br><img src=\"x.png\"><input type=\"text\">b</body>");
    assert_eq!(
        out,
        "<body>a<br><img src=\"x.png\"><input type=\"text\">b</body>"
    );
}

#[test]
fn escapes_text_and_attributes() {
    let out =
        ser("<body><p title=\"a&amp;b &quot;q&quot;\">1 &lt; 2 &amp; 3 &gt; 0&nbsp;x</p></body>");
    assert_eq!(
        out,
        "<body><p title=\"a&amp;b &quot;q&quot;\">1 &lt; 2 &amp; 3 &gt; 0&nbsp;x</p></body>"
    );
}

#[test]
fn keeps_comments_and_attribute_order() {
    let out = ser("<body><!-- c --><a href=\"/x\" class=\"k\" id=\"i\">t</a></body>");
    assert_eq!(
        out,
        "<body><!-- c --><a href=\"/x\" class=\"k\" id=\"i\">t</a></body>"
    );
}

#[test]
fn deep_nesting_does_not_overflow() {
    let depth = 2000;
    let html = format!(
        "<body>{}x{}</body>",
        "<div>".repeat(depth),
        "</div>".repeat(depth)
    );
    let out = ser(&html);
    assert!(out.starts_with("<body><div><div>"));
    assert_eq!(out.matches("<div>").count(), depth);
    assert_eq!(out.matches("</div>").count(), depth);
}

#[test]
fn wikipedia_fixture_has_no_removed_tags() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/fixtures/wikipedia-article.html");
    let html = std::fs::read_to_string(path).expect("fixture");
    let out = ser(&html);
    assert!(out.starts_with("<body"));
    for tag in ["<script", "<style", "<meta", "<link", "<svg", "<noscript"] {
        assert!(!out.contains(tag), "{tag} must be removed");
    }
}

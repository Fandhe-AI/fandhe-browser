//! `tokens.rs` のユニットテスト（TASK-14.2・`AISNAP-1`・Issue #93）。

#[path = "tokens.rs"]
mod tokens;

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use tokens::{
    InputDirError, InputSource, TokenCounter, fixtures_dir, measure_raw_html, resolve_input,
};

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

/// テスト専用の一時ディレクトリ（PID とテスト名で一意）。
fn temp_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fb-927-{}-{test}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn resolve_input_unset_is_builtin_fixtures() {
    let r = resolve_input(None).unwrap();
    assert_eq!(r, InputSource::Fixtures(fixtures_dir()));
    assert!(!r.is_external());
    assert_eq!(r.label(), "fixtures");
}

#[test]
fn resolve_input_empty_value_is_error() {
    let e = resolve_input(Some(OsString::new())).unwrap_err();
    assert!(matches!(e, InputDirError::Empty));
    assert_eq!(
        e.to_string(),
        "FANDHE_BROWSER_BENCH_HTML_DIR is set but empty"
    );
}

#[test]
fn resolve_input_missing_path_is_error_with_path() {
    let p = std::env::temp_dir().join(format!("fb-927-{}-missing", std::process::id()));
    let e = resolve_input(Some(p.clone().into_os_string())).unwrap_err();
    assert!(matches!(e, InputDirError::NotADirectory { .. }));
    assert!(e.to_string().contains(&p.display().to_string()));
}

#[test]
fn resolve_input_file_is_not_a_directory() {
    let dir = temp_dir("file");
    let f = dir.join("a.html");
    fs::write(&f, "<p>x</p>").unwrap();
    let e = resolve_input(Some(f.into_os_string())).unwrap_err();
    assert!(matches!(e, InputDirError::NotADirectory { .. }));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resolve_input_dir_without_html_is_error() {
    let empty = temp_dir("nohtml-empty");
    let e = resolve_input(Some(empty.clone().into_os_string())).unwrap_err();
    assert!(matches!(e, InputDirError::NoHtml { .. }));
    assert!(e.to_string().starts_with("no *.html files found in "));
    fs::remove_dir_all(&empty).unwrap();

    let other = temp_dir("nohtml-other");
    fs::write(other.join("a.txt"), "x").unwrap();
    fs::write(other.join("b.htm"), "x").unwrap();
    let e = resolve_input(Some(other.clone().into_os_string())).unwrap_err();
    assert!(matches!(e, InputDirError::NoHtml { .. }));
    fs::remove_dir_all(&other).unwrap();
}

#[test]
fn resolve_input_dir_with_html_is_external() {
    let dir = temp_dir("external");
    fs::write(dir.join("a.html"), "<p>x</p>").unwrap();
    let r = resolve_input(Some(dir.clone().into_os_string())).unwrap();
    assert_eq!(r, InputSource::External(dir.clone()));
    assert!(r.is_external());
    assert_eq!(r.label(), "external");
    assert_eq!(r.dir(), dir.as_path());
    fs::remove_dir_all(&dir).unwrap();
}

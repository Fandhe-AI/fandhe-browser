//! `benches/fixtures/` の 14 ページ相当フィクスチャの一覧・類型横断・パース可否を固定する
//! 結合テスト（TASK-14.1・`AISNAP-4`・Issue #91・`MS-2`）。
//!
//! 呼び出し文脈: 後続の測定（#93〜#96。生 HTML トークン数・削減率・情報保持・巨大静的
//! ページ）が入力にするフィクスチャ資産を、ファイル名集合・5 類型の件数・サイズ・
//! 情報保持チェックの必須要素で固定する。core の `parse_document` と ai の
//! `build_snapshot` が全件で成功することも確かめ、測定が走る前提を保証する。
//!
//! フィクスチャは自作の合成ページであり、実サイトのスナップショットではない
//! （詳細は `benches/fixtures/README.md`）。入力はリポ内資産のため `expect` を使う。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use fandhe_browser_ai::snapshot::build_snapshot;
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;

/// 期待するフィクスチャ（ファイル名, 類型）。類型は README の 5 区分に対応する。
const EXPECTED: [(&str, &str); 17] = [
    ("checkboxes-form.html", "form"),
    ("dashboard-table.html", "list-table"),
    ("dropdown-form.html", "form"),
    ("ec-product-list.html", "list-table"),
    ("example-minimal.html", "static"),
    ("hn-list.html", "list-table"),
    ("inputs-form.html", "form"),
    ("large-table.html", "list-table"),
    ("login-form.html", "form"),
    ("mdn-docs.html", "static"),
    ("python-portal.html", "static"),
    ("quotes-list.html", "list-table"),
    ("reddit-list.html", "list-table"),
    ("ssr-next-prerendered.html", "ssr-spa"),
    ("ssr-nuxt-hydrated-list.html", "ssr-spa"),
    ("wiki-portal-nav.html", "static"),
    ("wikipedia-article.html", "huge-static"),
];

/// 巨大静的ページのバイト数の下限・上限（`AISNAP-5`。リポ肥大を避け 1MB 以下）。
const HUGE_MIN_BYTES: usize = 200_000;
const HUGE_MAX_BYTES: usize = 1_000_000;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
}

fn read(name: &str) -> String {
    fs::read_to_string(fixtures_dir().join(name)).expect("フィクスチャは UTF-8 で読める")
}

/// セレクタに一致する要素数を数える。
fn count(html: &str, selector: &str) -> usize {
    let parsed = parse_document(html, &ParseOptions::default()).expect("パースは成功する");
    let doc = parsed.document;
    query_selector_all_str(&doc, doc.root(), selector)
        .expect("セレクタは評価できる")
        .len()
}

#[test]
fn fixture_file_set_matches_expected_list() {
    let mut actual: Vec<String> = fs::read_dir(fixtures_dir())
        .expect("fixtures ディレクトリが存在する")
        .map(|e| {
            e.expect("エントリを読める")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n.ends_with(".html"))
        .collect();
    actual.sort();
    let expected: Vec<String> = EXPECTED.iter().map(|(n, _)| (*n).to_string()).collect();
    assert_eq!(actual, expected);
    assert!(actual.len() >= 14);
}

#[test]
fn fixtures_cover_five_categories_with_expected_counts() {
    let mut by_cat: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, cat) in EXPECTED {
        *by_cat.entry(cat).or_default() += 1;
    }
    let got: Vec<(&str, usize)> = by_cat.into_iter().collect();
    assert_eq!(
        got,
        vec![
            ("form", 4),
            ("huge-static", 1),
            ("list-table", 6),
            ("ssr-spa", 2),
            ("static", 4),
        ]
    );
}

#[test]
fn every_fixture_is_lf_utf8_parses_and_builds_snapshot() {
    for (name, _) in EXPECTED {
        let html = read(name);
        assert!(!html.contains('\r'), "{name} に CR が含まれる");
        let parsed = parse_document(&html, &ParseOptions::default())
            .unwrap_or_else(|e| panic!("{name} のパースに失敗: {e:?}"));
        build_snapshot(&parsed.document)
            .unwrap_or_else(|e| panic!("{name} の snapshot 構築に失敗: {e:?}"));
    }
}

#[test]
fn huge_static_page_size_is_within_bounds() {
    let len = read("wikipedia-article.html").len();
    assert!(
        (HUGE_MIN_BYTES..=HUGE_MAX_BYTES).contains(&len),
        "wikipedia-article.html のサイズ {len} が範囲外"
    );
}

#[test]
fn large_table_has_2500_cells() {
    assert_eq!(count(&read("large-table.html"), "td"), 2500);
    assert_eq!(count(&read("large-table.html"), "th"), 50);
}

/// 情報保持チェック（`AISNAP-3`・#95）が依存する要素の存在を固定する。
#[test]
fn retention_check_elements_exist() {
    assert_eq!(count(&read("login-form.html"), "button[type=submit]"), 1);
    assert_eq!(count(&read("login-form.html"), "label"), 2);
    assert_eq!(count(&read("ec-product-list.html"), "p.price_color"), 20);
    assert_eq!(count(&read("inputs-form.html"), "input[type=number]"), 1);
    assert_eq!(count(&read("dashboard-table.html"), "table#table1"), 1);
    assert_eq!(
        count(&read("dashboard-table.html"), "table#table1 thead th"),
        6
    );
    assert_eq!(count(&read("dropdown-form.html"), "select#dropdown"), 1);
    assert_eq!(
        count(&read("dropdown-form.html"), "select#dropdown option"),
        4
    );
    assert_eq!(count(&read("hn-list.html"), ".athing"), 30);
    assert_eq!(count(&read("hn-list.html"), ".athing .titleline > a"), 30);
    assert_eq!(
        count(&read("checkboxes-form.html"), "input[type=checkbox]"),
        2
    );
    assert_eq!(count(&read("checkboxes-form.html"), "input[checked]"), 1);
}

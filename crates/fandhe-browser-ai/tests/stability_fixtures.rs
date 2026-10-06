//! `tests/fixtures/stability/` の「ページ軽微変化」フィクスチャ棚卸しテスト
//! （TASK-17.2・`AISNAP-10`・Issue #111・`MS-2`）。
//!
//! 呼び出し文脈: 参照破損率の測定（TASK-17。#112 が `cases.json` と各ケースの
//! `before.html` / `after.html` を読んで再特定判定にかける）が入力にする資産を、
//! ケース一覧・ファイル配置・LF/UTF-8・パース可否・セレクタの一意性・サイズで固定する。
//! 判定ロジック本体（破損率の算出）はここでは扱わない。
//!
//! フィクスチャは `benches/fixtures/` の合成ページを元にした自作資産で、実サイトの
//! スナップショットではない（詳細は `tests/fixtures/stability/README.md`）。
//! 入力はリポ内資産のため `expect` を使う。

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use fandhe_browser_ai::snapshot::build_snapshot;
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;

/// 期待するケース id（昇順）。追加・削除時は README の一覧表も更新する。
const EXPECTED_IDS: [&str; 19] = [
    "01-login-submit",
    "02-login-username",
    "03-dropdown-select",
    "04-checkbox-first",
    "05-number-input",
    "06-quote-text",
    "07-quote-tag-link",
    "08-hn-first-title",
    "09-hn-more-link",
    "10-ec-price",
    "11-ec-product-link",
    "12-table-header-cell",
    "13-table-data-cell",
    "14-python-download-link",
    "15-wiki-language-link",
    "16-login-password",
    "17-hn-second-title",
    "18-quotes-author-link",
    "19-quotes-login-link",
];

/// フィクスチャ合計サイズの上限（`AISNAP-5` と同じくリポ肥大を避け 1MB 以下）。
const MAX_TOTAL_BYTES: usize = 1_000_000;

fn stability_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("stability")
}

struct Case {
    id: String,
    selector: String,
}

fn load_cases() -> Vec<Case> {
    let raw = fs::read_to_string(stability_dir().join("cases.json")).expect("cases.json を読める");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("cases.json は JSON");
    value
        .as_array()
        .expect("cases.json は配列")
        .iter()
        .map(|c| Case {
            id: c["id"].as_str().expect("id は文字列").to_string(),
            selector: c["selector"]
                .as_str()
                .expect("selector は文字列")
                .to_string(),
        })
        .collect()
}

/// id は小文字英数字とハイフンのみ（パス区切り・`..`・大文字小文字衝突を排除する）。
fn assert_safe_id(id: &str) {
    assert!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "id {id:?} が安全文字集合に適合しない"
    );
}

fn read(id: &str, file: &str) -> String {
    assert_safe_id(id);
    fs::read_to_string(stability_dir().join(id).join(file))
        .unwrap_or_else(|e| panic!("{id}/{file} を UTF-8 で読めない: {e}"))
}

fn count(html: &str, selector: &str) -> usize {
    let parsed = parse_document(html, &ParseOptions::default()).expect("パースは成功する");
    let doc = parsed.document;
    query_selector_all_str(&doc, doc.root(), selector)
        .expect("セレクタは評価できる")
        .len()
}

#[test]
fn case_ids_match_expected_list_and_are_unique() {
    let ids: Vec<String> = load_cases().into_iter().map(|c| c.id).collect();
    let expected: Vec<String> = EXPECTED_IDS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(ids, expected);
    assert!(ids.len() >= 15);
    let unique: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
}

#[test]
fn fixture_directories_match_manifest() {
    let mut dirs: Vec<String> = fs::read_dir(stability_dir())
        .expect("stability ディレクトリが存在する")
        .map(|e| e.expect("エントリを読める"))
        .filter(|e| e.file_type().expect("種別を取得できる").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    let expected: Vec<String> = EXPECTED_IDS.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(dirs, expected);
}

#[test]
fn before_after_are_lf_utf8_distinct_and_after_is_longer() {
    for case in load_cases() {
        let before = read(&case.id, "before.html");
        let after = read(&case.id, "after.html");
        for (name, html) in [("before", &before), ("after", &after)] {
            assert!(!html.contains('\r'), "{}/{name} に CR が含まれる", case.id);
            assert!(html.ends_with('\n'), "{}/{name} の末尾改行がない", case.id);
        }
        assert_ne!(before, after, "{} の前後が同一", case.id);
        assert!(after.len() > before.len(), "{} は追加変化ではない", case.id);
    }
}

#[test]
fn before_after_parse_and_build_snapshot() {
    for case in load_cases() {
        for file in ["before.html", "after.html"] {
            let html = read(&case.id, file);
            let parsed = parse_document(&html, &ParseOptions::default())
                .unwrap_or_else(|e| panic!("{}/{file} のパースに失敗: {e:?}", case.id));
            build_snapshot(&parsed.document)
                .unwrap_or_else(|e| panic!("{}/{file} の snapshot 構築に失敗: {e:?}", case.id));
        }
    }
}

#[test]
fn selector_matches_exactly_one_element_before_and_after() {
    for case in load_cases() {
        for file in ["before.html", "after.html"] {
            let n = count(&read(&case.id, file), &case.selector);
            assert_eq!(
                n, 1,
                "{}/{file} のセレクタ {} の一致数",
                case.id, case.selector
            );
        }
    }
}

#[test]
fn total_fixture_size_is_within_bounds() {
    let total: usize = load_cases()
        .iter()
        .map(|c| read(&c.id, "before.html").len() + read(&c.id, "after.html").len())
        .sum();
    assert!(total <= MAX_TOTAL_BYTES, "合計サイズ {total} が上限超過");
}

/// ref を発行しない対象は `ref_expected: false` と理由を明記し、測定対象から除外する契約
/// （`AISNAP-10`・Issue #111）。現状の除外は表データセルの 13 のみ。
#[test]
fn ref_less_cases_declare_exclusion_contract() {
    let raw = fs::read_to_string(stability_dir().join("cases.json")).expect("cases.json を読める");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("cases.json は JSON");
    let excluded: Vec<&str> = value
        .as_array()
        .expect("cases.json は配列")
        .iter()
        .filter(|c| c["ref_expected"] == serde_json::Value::Bool(false))
        .map(|c| {
            assert!(
                c["ref_note"].as_str().is_some_and(|n| !n.is_empty()),
                "ref_expected=false のケースには ref_note が必要"
            );
            c["id"].as_str().expect("id は文字列")
        })
        .collect();
    assert_eq!(excluded, vec!["13-table-data-cell"]);
}

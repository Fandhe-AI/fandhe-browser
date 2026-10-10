//! `generate_reduced.rs` のテスト（TASK-21.2・`AISNAP-8`・Issue #120）。
//!
//! 25 タスク全件の対象ページで簡約表現が生成されること、生成物（`reduced/*.txt`・
//! `golden-refs.json`）が生成器の出力と一致すること、ref 解決の構造を固定する。
//! `AGENT_EVAL_WRITE=1` を付けて実行すると生成物を書き直す（未設定時は比較のみ）。
//! ref の解決率は測定対象（#121・#118）のためここでは assert しない（`REPAIR-3`）。

// 未使用の公開項目（retention_check の判定 API 等）は本 target では使わない。
#[allow(dead_code)]
#[path = "generate_reduced.rs"]
mod generate_reduced;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/raw_dom.rs"]
mod raw_dom;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/retention_check.rs"]
mod retention_check;
#[allow(dead_code)]
#[path = "../../benches/token_reduction/snapshot_text.rs"]
mod snapshot_text;
#[allow(dead_code)]
#[path = "tasks.rs"]
mod tasks;

use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;
use generate_reduced::{
    GenerateError, InputMode, generate_all, generate_page, generate_raw_dom_all,
    generate_raw_dom_page, golden_locators, render_golden_refs_json, resolve_golden, task_inputs,
    task_pages, write_raw_dom_inputs,
};
use std::path::PathBuf;
use tasks::{GOLDEN, TASKS};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    manifest().join("benches").join("fixtures")
}

fn eval_dir() -> PathBuf {
    manifest().join("harness").join("agent_eval")
}

#[test]
fn aisnap8_every_task_page_has_nonempty_reduced_text() {
    let mut seen = 0;
    for t in &TASKS {
        let p = generate_page(&fixtures(), t.page).expect("generate");
        assert!(!p.text.trim().is_empty(), "{}: empty text", t.id);
        assert!(
            p.text.starts_with("- document"),
            "{}: {:?}",
            t.id,
            p.text.lines().next()
        );
        assert!(p.text.ends_with('\n') && !p.text.ends_with("\n\n"));
        seen += 1;
    }
    assert_eq!(seen, 25);
}

#[test]
fn aisnap8_task_pages_are_unique_and_sorted() {
    let pages = task_pages();
    assert_eq!(pages.len(), 13);
    let mut sorted = pages.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(pages, sorted);
    for p in &pages {
        assert!(
            p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "page name {p:?} must be a plain file stem"
        );
    }
}

/// 生成物が正本と一致すること。`AGENT_EVAL_WRITE=1` なら書き直す。
fn check_or_write(rel: &str, expected: &str) {
    let path = eval_dir().join(rel);
    if std::env::var_os("AGENT_EVAL_WRITE").is_some() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create dir");
        }
        std::fs::write(&path, expected).expect("write generated file");
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel} must exist: {e}"));
    assert!(!actual.contains('\r'), "{rel} must use LF");
    assert_eq!(
        actual, expected,
        "{rel} is stale; rerun with AGENT_EVAL_WRITE=1"
    );
}

#[test]
fn aisnap8_reduced_files_match_generator() {
    let pages = generate_all(&fixtures()).expect("generate");
    assert_eq!(pages.len(), 13);
    for p in &pages {
        check_or_write(&format!("reduced/{}.txt", p.page), &p.text);
    }
    // 陳腐化したファイルが残っていないこと。
    let expected: std::collections::BTreeSet<String> =
        pages.iter().map(|p| format!("{}.txt", p.page)).collect();
    let actual: std::collections::BTreeSet<String> = std::fs::read_dir(eval_dir().join("reduced"))
        .expect("reduced dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn aisnap8_golden_refs_json_matches_generator() {
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    check_or_write("golden-refs.json", &render_golden_refs_json(&resolved));
}

#[test]
fn aisnap8_golden_refs_cover_all_tasks_in_order() {
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    assert_eq!(resolved.len(), 25);
    for (r, (t, (_, g))) in resolved.iter().zip(TASKS.iter().zip(GOLDEN.iter())) {
        assert_eq!(r.id, t.id);
        let locs = golden_locators(g);
        assert_eq!(r.locators.len(), locs.len(), "{}", t.id);
        for (rl, l) in r.locators.iter().zip(locs.iter()) {
            assert_eq!(rl.selector, l.selector);
            assert_eq!(rl.index, l.index);
        }
    }
}

#[test]
fn aisnap8_resolved_refs_appear_in_reduced_text() {
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    for (r, t) in resolved.iter().zip(TASKS.iter()) {
        let text = generate_page(&fixtures(), t.page).expect("generate").text;
        for l in &r.locators {
            for rf in &l.refs {
                assert!(text.contains(rf.as_str()), "{}: ref {rf} not in text", t.id);
            }
        }
    }
}

#[test]
fn aisnap8_reduced_text_has_no_golden_leak() {
    for p in generate_all(&fixtures()).expect("generate") {
        assert!(!p.text.contains("\"selector\""), "{}", p.page);
        assert!(!p.text.contains("\"refs\""), "{}", p.page);
        assert!(
            p.text.lines().all(|l| l == l.trim_end()),
            "{}: trailing whitespace",
            p.page
        );
    }
}

#[test]
fn aisnap8_click01_resolves_to_login_button_ref() {
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    let c = resolved
        .iter()
        .find(|r| r.id == "click-01")
        .expect("click-01");
    assert_eq!(c.locators.len(), 1);
    assert_eq!(c.locators[0].refs, vec!["e7e730a2753f66c98".to_owned()]);
    let text = generate_page(&fixtures(), "login-form")
        .expect("generate")
        .text;
    assert!(
        text.lines()
            .any(|l| l.contains("button") && l.contains("[e7e730a2753f66c98]"))
    );
}

/// 圧縮された表・一覧の行内リンク（`AISNAP-13`）も scope が異なるだけで解決できること。
#[test]
fn aisnap8_compressed_row_controls_resolve_to_refs() {
    let resolved = resolve_golden(&fixtures()).expect("resolve");
    for (id, expected) in [
        ("click-04", "ed42c6b3dec4e715c-2"),
        ("nav-02", "eaa09f8e5a0ba62fc"),
        ("nav-05", "eecb7a375060b0fc8"),
    ] {
        let t = resolved.iter().find(|r| r.id == id).expect(id);
        assert_eq!(t.locators[0].refs, vec![expected.to_owned()], "{id}");
    }
}

#[test]
fn aisnap8_truncated_pages_are_those_with_snapshot_cap() {
    // 上限で打ち切られるページは 2 件。測定結果の解釈に必要な事実の固定。
    let truncated: Vec<String> = generate_all(&fixtures())
        .expect("generate")
        .into_iter()
        .filter(|p| p.truncated)
        .map(|p| p.page)
        .collect();
    assert_eq!(
        truncated,
        vec!["hn-list".to_owned(), "large-table".to_owned()]
    );
}

/// golden の値・選択肢が簡約表現から回答可能であること（PR #706 指摘・`AISNAP-8`・`AISNAP-3`）。
/// 価格・引用本文・option のラベルと value は DOM から補う（`generate_reduced::value_annotations`）。
#[test]
fn aisnap8_reduced_text_keeps_values_needed_by_tasks() {
    let text = |page: &str| generate_page(&fixtures(), page).expect("generate").text;
    let ec = text("ec-product-list");
    assert!(ec.contains("\"\u{a3}13.17\""), "extract-02 price missing");
    let quotes = text("quotes-list");
    assert!(
        quotes.contains(
            "\"\u{201c}River stone beta garden prism record valley vector record theta delta system.\u{201d}\""
        ),
        "extract-05 quote missing"
    );
    let dd = text("dropdown-form");
    assert!(
        dd.lines()
            .any(|l| l.contains("\"Option 2\"") && l.contains("value=\"2\"")),
        "click-02 option missing: {dd}"
    );
    assert!(
        dd.lines()
            .any(|l| l.contains("\"Option 1\"") && l.contains("value=\"1\"")),
        "form-02 option missing: {dd}"
    );
}
// ---- TASK-22.1（`AISNAP-9`・Issue #124）: 生 DOM 直渡し方式の入力 ----

#[test]
fn aisnap9_raw_dom_input_for_every_task() {
    let inputs = task_inputs(&fixtures(), InputMode::RawDom).expect("inputs");
    assert_eq!(inputs.len(), 25);
    for (i, t) in inputs.iter().zip(TASKS.iter()) {
        assert_eq!(i.id, t.id);
        assert_eq!(i.page, t.page);
        assert_eq!(i.prompt, t.prompt);
        assert_eq!(i.mode, InputMode::RawDom);
        assert!(
            i.input.starts_with("<body"),
            "{}: not a body outerHTML",
            i.id
        );
    }
    let reduced = task_inputs(&fixtures(), InputMode::Reduced).expect("reduced");
    assert_eq!(reduced.len(), 25);
    assert!(reduced.iter().all(|i| i.input.starts_with("- document")));
}

#[test]
fn aisnap9_raw_dom_pages_cover_task_pages() {
    let pages = generate_raw_dom_all(&fixtures()).expect("raw");
    let names: Vec<&str> = pages.iter().map(|p| p.page.as_str()).collect();
    assert_eq!(names.len(), 13);
    assert_eq!(names, task_pages());
}

#[test]
fn aisnap9_raw_dom_excludes_removed_tags() {
    let mut original_has_script_or_style = false;
    for page in task_pages() {
        let raw = generate_raw_dom_page(&fixtures(), page).expect("raw");
        let lower = raw.html.to_ascii_lowercase();
        for tag in ["<script", "<style", "<noscript", "<svg", "<link", "<meta"] {
            assert!(!lower.contains(tag), "{page}: {tag} must be removed");
        }
        let src = std::fs::read_to_string(fixtures().join(format!("{page}.html"))).expect("read");
        let src = src.to_ascii_lowercase();
        if src.contains("<script") || src.contains("<style") {
            original_has_script_or_style = true;
        }
    }
    assert!(
        original_has_script_or_style,
        "removal must actually be exercised"
    );
}

#[test]
fn aisnap9_raw_dom_is_deterministic_and_lf() {
    let a = generate_raw_dom_all(&fixtures()).expect("a");
    let b = generate_raw_dom_all(&fixtures()).expect("b");
    assert_eq!(a, b);
    for p in &a {
        assert!(!p.html.contains('\r'), "{}: must be LF only", p.page);
    }
}

#[test]
fn aisnap9_raw_dom_matches_serializer() {
    for page in task_pages() {
        let src = std::fs::read_to_string(fixtures().join(format!("{page}.html"))).expect("read");
        let doc = parse_document(&src, &ParseOptions::default())
            .expect("parse")
            .document;
        let expected = raw_dom::serialize_raw_dom(&doc);
        let actual = generate_raw_dom_page(&fixtures(), page).expect("raw").html;
        assert_eq!(actual, expected, "{page}");
    }
}

fn normalized_text(doc: &Document, id: NodeId) -> String {
    doc.text_content(id)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn attr_list(doc: &Document, id: NodeId) -> Vec<(String, String)> {
    doc.attributes(id)
        .iter()
        .map(|a| (a.name.local.to_string(), a.value.to_string()))
        .collect()
}

/// 回答者は生 DOM 文字列からロケータを答え、golden は原本 DOM で定義される。
/// 両者で同じ `{selector, index}` が同じ要素を指すこと（生 DOM 方式の回答が採点可能であることの裏付け）。
#[test]
fn aisnap9_golden_locators_survive_raw_dom_roundtrip() {
    let mut checked = 0;
    for t in &TASKS {
        let src =
            std::fs::read_to_string(fixtures().join(format!("{}.html", t.page))).expect("read");
        let orig = parse_document(&src, &ParseOptions::default())
            .expect("parse")
            .document;
        let raw = generate_raw_dom_page(&fixtures(), t.page)
            .expect("raw")
            .html;
        let re = parse_document(&raw, &ParseOptions::default())
            .expect("reparse")
            .document;
        let g = GOLDEN
            .iter()
            .find(|(id, _)| *id == t.id)
            .map(|(_, g)| g)
            .expect("golden");
        for l in golden_locators(g) {
            let o = query_selector_all_str(&orig, orig.root(), l.selector).expect("orig query");
            let r = query_selector_all_str(&re, re.root(), l.selector).expect("re query");
            let on = *o.get(l.index).expect("orig locator in range");
            let rn = *r
                .get(l.index)
                .unwrap_or_else(|| panic!("{}: raw dom lacks {} [{}]", t.id, l.selector, l.index));
            assert_eq!(orig.local_name(on), re.local_name(rn), "{}", t.id);
            assert_eq!(attr_list(&orig, on), attr_list(&re, rn), "{}", t.id);
            assert_eq!(
                normalized_text(&orig, on),
                normalized_text(&re, rn),
                "{}",
                t.id
            );
            checked += 1;
        }
    }
    assert!(checked >= 25, "checked {checked}");
}

/// 生 DOM は性質上 golden の値（価格等）を本文に含むが、それは設計どおりで漏えいではない。
/// ここでは golden / golden-refs のメタデータ（JSON キー）が入力へ混入しないことだけを固定する。
#[test]
fn aisnap9_raw_dom_input_has_no_golden_metadata() {
    for i in task_inputs(&fixtures(), InputMode::RawDom).expect("inputs") {
        assert!(!i.input.contains("\"refs\""), "{}", i.id);
        assert!(!i.input.contains("\"selector\""), "{}", i.id);
    }
}

#[test]
fn aisnap9_write_raw_dom_inputs_writes_under_out_dir() {
    let out = std::env::temp_dir().join(format!("fandhe-aisnap9-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let paths = write_raw_dom_inputs(&fixtures(), &out).expect("write");
    let pages = generate_raw_dom_all(&fixtures()).expect("raw");
    assert_eq!(paths.len(), 13);
    for (path, p) in paths.iter().zip(pages.iter()) {
        assert_eq!(path.parent(), Some(out.as_path()));
        let expected_name = format!("{}.html", p.page);
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(expected_name.as_str())
        );
        assert_eq!(std::fs::read_to_string(path).expect("read"), p.html);
    }
    assert_eq!(std::fs::read_dir(&out).expect("dir").count(), 13);
    std::fs::remove_dir_all(&out).expect("cleanup");
}

#[test]
fn aisnap9_write_failure_reports_cannot_write_not_read() {
    // out_dir の親が通常ファイルだと create_dir_all が失敗する（書き込み系エラー）。
    let base = std::env::temp_dir().join(format!("fandhe-aisnap9-wf-{}", std::process::id()));
    std::fs::write(&base, b"x").expect("setup");
    let out = base.join("sub");
    let err = write_raw_dom_inputs(&fixtures(), &out).expect_err("must fail");
    assert!(matches!(err, GenerateError::Write { .. }), "{err}");
    let msg = err.to_string();
    assert!(msg.starts_with("cannot write "), "{msg}");
    assert!(!msg.contains("cannot read"), "{msg}");
    std::fs::remove_file(&base).expect("cleanup");
}

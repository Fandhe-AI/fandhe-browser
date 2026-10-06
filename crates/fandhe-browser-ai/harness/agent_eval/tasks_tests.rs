//! `tasks.rs` のテスト（TASK-21.1・`AISNAP-8`・Issue #119）。
//!
//! 25 件の件数・種別配分・golden の形・fixture 上でのロケータ解決・生成 JSON との一致を固定する。
//! `AGENT_EVAL_WRITE=1` を付けて実行すると、`tasks.json`・`golden-answers.json` を
//! `render_*_json()` の出力で書き直す（未設定時は必ず比較し、書き込まない）。

#[path = "tasks.rs"]
mod tasks;

use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tasks::{
    Action, Category, GOLDEN, Golden, Locator, TASKS, render_golden_json, render_tasks_json,
};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_html(page: &str) -> String {
    let path = manifest()
        .join("benches")
        .join("fixtures")
        .join(format!("{page}.html"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("fixture {} must be readable: {e}", path.display()))
}

fn parse(page: &str) -> Document {
    parse_document(&fixture_html(page), &ParseOptions::default())
        .expect("fixture must parse")
        .document
}

/// ロケータを解決する。件数が index 以下なら panic。
fn resolve(doc: &Document, l: &Locator) -> NodeId {
    let found = query_selector_all_str(doc, doc.root(), l.selector)
        .unwrap_or_else(|e| panic!("selector {:?} must be supported: {e}", l.selector));
    assert!(
        found.len() > l.index,
        "selector {:?} matched {} elements, index {} out of range",
        l.selector,
        found.len(),
        l.index
    );
    found[l.index]
}

fn norm_text(doc: &Document, id: NodeId) -> String {
    doc.text_content(id)
        .expect("text_content")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn golden_of(id: &str) -> &'static Golden {
    &GOLDEN.iter().find(|(gid, _)| *gid == id).expect("golden").1
}

#[test]
fn aisnap8_task_count_and_category_split() {
    assert_eq!(TASKS.len(), 25);
    assert_eq!(GOLDEN.len(), 25);
    let count = |c: Category| TASKS.iter().filter(|t| t.category == c).count();
    assert_eq!(count(Category::Click), 7);
    assert_eq!(count(Category::Extract), 6);
    assert_eq!(count(Category::Form), 6);
    assert_eq!(count(Category::Nav), 6);
}

#[test]
fn aisnap8_ids_are_unique_and_match_golden_in_order() {
    let ids: BTreeSet<&str> = TASKS.iter().map(|t| t.id).collect();
    assert_eq!(ids.len(), 25);
    for (t, (gid, _)) in TASKS.iter().zip(GOLDEN.iter()) {
        assert_eq!(t.id, *gid);
        assert!(
            t.id.starts_with(t.category.as_str()),
            "id {} must start with its category",
            t.id
        );
        assert!(!t.prompt.is_empty());
    }
}

#[test]
fn aisnap8_golden_shape_matches_category() {
    for t in &TASKS {
        let g = golden_of(t.id);
        let ok = match (t.category, g) {
            (Category::Click | Category::Nav, Golden::Ref { any_of }) => !any_of.is_empty(),
            (Category::Extract, Golden::Value { value, .. }) => !value.is_empty(),
            (Category::Form, Golden::Steps(steps)) => !steps.is_empty(),
            _ => false,
        };
        assert!(ok, "golden shape mismatch for {}", t.id);
    }
}

#[test]
fn aisnap8_step_values_follow_action() {
    for (id, g) in &GOLDEN {
        if let Golden::Steps(steps) = g {
            for s in *steps {
                match s.action {
                    Action::Fill | Action::Select => {
                        assert!(s.value.is_some(), "{id}: fill/select needs value")
                    }
                    Action::Click => assert!(s.value.is_none(), "{id}: click has no value"),
                }
            }
        }
    }
}

/// click・nav の期待: (id, 要素名, 属性名, 属性値)。解決した全ロケータで同じ要素を指すこと。
const REF_EXPECT: [(&str, &str, &str, &str); 13] = [
    ("click-01", "button", "type", "submit"),
    ("click-02", "option", "value", "2"),
    ("click-03", "input", "type", "checkbox"),
    ("click-04", "a", "href", "#edit"),
    ("click-05", "a", "href", "https://example.com/story/2"),
    ("click-06", "button", "type", "submit"),
    ("click-07", "a", "href", "/login"),
    ("nav-01", "a", "href", "//l3.example.org/"),
    ("nav-02", "a", "href", "/docs/ref/group1/item3"),
    ("nav-03", "a", "href", "/downloads/"),
    ("nav-04", "a", "href", "login?goto=news"),
    ("nav-05", "a", "href", "/page/2/"),
    ("nav-06", "a", "href", "https://example.com/"),
];

#[test]
fn aisnap8_ref_locators_resolve_to_intended_elements() {
    let mut seen = 0;
    for t in &TASKS {
        let Golden::Ref { any_of } = golden_of(t.id) else {
            continue;
        };
        let (_, tag, attr, val) = REF_EXPECT
            .iter()
            .find(|(id, ..)| *id == t.id)
            .unwrap_or_else(|| panic!("REF_EXPECT missing {}", t.id));
        let doc = parse(t.page);
        for l in *any_of {
            let n = resolve(&doc, l);
            assert_eq!(doc.local_name(n), Some(*tag), "{}", t.id);
            assert_eq!(doc.attribute(n, attr), Some(*val), "{}", t.id);
        }
        seen += 1;
    }
    assert_eq!(seen, REF_EXPECT.len());
}

#[test]
fn aisnap8_ref_targets_sit_in_the_intended_context() {
    // click-04: 2 行目（Gamma）の edit。
    let doc = parse("dashboard-table");
    let Golden::Ref { any_of } = golden_of("click-04") else {
        panic!()
    };
    let a = resolve(&doc, &any_of[0]);
    let tr = doc
        .ancestors(a)
        .find(|n| doc.local_name(*n) == Some("tr"))
        .expect("tr");
    assert!(norm_text(&doc, tr).starts_with("Gamma Zeta user2@example.com"));

    // click-06: 1 番目の商品。
    let doc = parse("ec-product-list");
    let Golden::Ref { any_of } = golden_of("click-06") else {
        panic!()
    };
    let b = resolve(&doc, &any_of[0]);
    let article = doc
        .ancestors(b)
        .find(|n| doc.local_name(*n) == Some("article"))
        .expect("article");
    assert!(norm_text(&doc, article).contains("Record thread valley kappa"));
}

#[test]
fn aisnap8_value_goldens_match_resolved_elements() {
    let mut seen = 0;
    for t in &TASKS {
        let Golden::Value {
            value,
            source,
            attr,
        } = golden_of(t.id)
        else {
            continue;
        };
        let doc = parse(t.page);
        let n = resolve(&doc, source);
        let actual = match attr {
            Some(a) => doc.attribute(n, a).expect("attribute").to_owned(),
            None => norm_text(&doc, n),
        };
        assert_eq!(&actual, value, "{}", t.id);
        seen += 1;
    }
    assert_eq!(seen, 6);
}

#[test]
fn aisnap8_step_targets_resolve_with_expected_tags() {
    for t in &TASKS {
        let Golden::Steps(steps) = golden_of(t.id) else {
            continue;
        };
        let doc = parse(t.page);
        for s in *steps {
            let n = resolve(&doc, &s.target);
            let tag = doc.local_name(n).expect("tag");
            match s.action {
                Action::Fill => assert_eq!(tag, "input", "{}", t.id),
                Action::Select => assert_eq!(tag, "select", "{}", t.id),
                Action::Click => assert!(["button", "input"].contains(&tag), "{}", t.id),
            }
        }
    }
    // form-02: 選択値 1 に対応する option が実在する。
    let doc = parse("dropdown-form");
    let opt = resolve(
        &doc,
        &Locator {
            selector: "select#dropdown option[value=\"1\"]",
            index: 0,
        },
    );
    assert_eq!(norm_text(&doc, opt), "Option 1");
}

#[test]
fn aisnap8_form04_second_checkbox_is_initially_checked() {
    let doc = parse("checkboxes-form");
    let l = |index| Locator {
        selector: "form#checkboxes input[type=checkbox]",
        index,
    };
    assert_eq!(doc.attribute(resolve(&doc, &l(0)), "checked"), None);
    assert!(doc.attribute(resolve(&doc, &l(1)), "checked").is_some());
}

#[test]
fn aisnap8_every_page_has_a_fixture() {
    for t in &TASKS {
        assert!(
            manifest()
                .join("benches")
                .join("fixtures")
                .join(format!("{}.html", t.page))
                .is_file(),
            "{}: fixture {} missing",
            t.id,
            t.page
        );
    }
}

/// 生成物 JSON が正本と一致すること。`AGENT_EVAL_WRITE=1` なら書き直す。
fn check_or_write(name: &str, expected: &str) {
    let path: PathBuf = manifest().join("harness").join("agent_eval").join(name);
    if std::env::var_os("AGENT_EVAL_WRITE").is_some() {
        std::fs::write(&path, expected).expect("write generated json");
    }
    let actual = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", Path::new(name).display()));
    assert!(!actual.contains('\r'), "{name} must use LF");
    assert_eq!(
        actual, expected,
        "{name} is stale; rerun with AGENT_EVAL_WRITE=1"
    );
}

#[test]
fn aisnap8_generated_json_matches_source() {
    check_or_write("tasks.json", &render_tasks_json());
    check_or_write("golden-answers.json", &render_golden_json());
}

#[test]
fn aisnap8_tasks_json_does_not_leak_golden_values() {
    let tasks_json = render_tasks_json();
    assert!(!tasks_json.contains("user3@example.com"));
    assert!(!tasks_json.contains("\"selector\""));
}

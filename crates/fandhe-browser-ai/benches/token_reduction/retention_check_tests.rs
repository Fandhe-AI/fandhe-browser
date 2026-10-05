//! `retention_check.rs` のユニットテスト（TASK-14.4・`AISNAP-3`・Issue #95）。
//!
//! 実フィクスチャに対する判定結果（ref・件数）を具体値で固定する。ref は FNV-1a ベースで
//! 決定的なため、リテラルが変われば `AISNAP-10`（ref の安定性）の回帰としても検出される。

#[path = "retention_check.rs"]
mod retention_check;

use fandhe_browser_ai::snapshot::{
    DataLeafKind, HeaderCell, Node, RowControl, TableRow, TableSummary,
};
use retention_check::{
    NotIdentifiedReason, RetentionCheckError, TASKS, TSV_HEADER, TaskCheck, Verdict, flatten,
    format_row, identified_count, judge, run_retention_checks,
};
use std::path::Path;

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
}

fn ident(r: &str) -> Verdict {
    Verdict::Identified {
        r#ref: r.to_owned(),
    }
}

fn checks() -> Vec<TaskCheck> {
    run_retention_checks(&fixtures()).expect("run")
}

/// (id, dom_matches, snapshot_matches, ref, data_leaf, matched_name, truncated)
type Expected = (
    &'static str,
    usize,
    usize,
    &'static str,
    Option<DataLeafKind>,
    &'static str,
    bool,
);

const EXPECTED: [Expected; 7] = [
    (
        "login-button",
        1,
        1,
        "e7e730a2753f66c98",
        None,
        "Login",
        false,
    ),
    (
        "price-first",
        20,
        1,
        "eebd41136ac2ff69e-6",
        Some(DataLeafKind::PriceClass),
        "",
        false,
    ),
    (
        "number-input",
        1,
        1,
        "e07e25f719b2299fb",
        None,
        "Number",
        false,
    ),
    (
        "table-header-first",
        6,
        1,
        "e091124527e8c0f5e",
        Some(DataLeafKind::TableCell),
        "Last Name",
        false,
    ),
    (
        "dropdown-select",
        1,
        1,
        "eb9b697ed4f125035",
        None,
        "",
        false,
    ),
    (
        "top-story-link",
        30,
        1,
        "ebd9595753298b235",
        None,
        "Compass pattern beta anchor lantern sigma ribbon",
        true,
    ),
    ("checkbox-first", 2, 1, "e91f35187869ba882", None, "", false),
];

#[test]
fn aisnap_3_seven_tasks_in_table_order() {
    let c = checks();
    let ids: Vec<&str> = c.iter().map(|t| t.id).collect();
    assert_eq!(
        ids,
        vec![
            "login-button",
            "price-first",
            "number-input",
            "table-header-first",
            "dropdown-select",
            "top-story-link",
            "checkbox-first"
        ]
    );
}

#[test]
fn aisnap_3_per_task_concrete_values() {
    let c = checks();
    assert_eq!(c.len(), EXPECTED.len());
    for (t, e) in c.iter().zip(EXPECTED.iter()) {
        assert_eq!(t.id, e.0);
        assert_eq!(t.dom_matches, e.1, "{} dom_matches", e.0);
        assert_eq!(t.snapshot_matches, e.2, "{} snapshot_matches", e.0);
        assert_eq!(t.verdict, ident(e.3), "{} verdict", e.0);
        assert_eq!(t.data_leaf, e.4, "{} data_leaf", e.0);
        assert_eq!(t.matched_name, e.5, "{} matched_name", e.0);
        assert_eq!(t.snapshot_truncated, e.6, "{} truncated", e.0);
    }
}

/// 達成基準の契約（6/7 以上）。最終判断は人間担当の #97 が行うため、ここでは閾値だけを検証する。
#[test]
fn aisnap_3_identified_count_meets_threshold() {
    let c = checks();
    assert!(
        identified_count(&c) >= 6,
        "identified {}",
        identified_count(&c)
    );
}

/// 現状値（7/7）の回帰固定。閾値契約（上のテスト）とは独立で、1 件でも判別不能になった場合は
/// 閾値内でも意図せぬ変化として検出する。許容範囲の変更時はこのテストを更新する。
#[test]
fn aisnap_3_current_baseline_identifies_all_seven() {
    assert_eq!(identified_count(&checks()), 7);
}

/// 同一性検証: 同じ role・name の別要素が ref を持っていても、DOM 対象要素から
/// 再計算した ref ダイジェストと一致しなければ Identified にしない。
#[test]
fn aisnap_3_same_signature_other_element_is_not_identified() {
    let root = Node::new("document", "").with_children(vec![
        Node::new("combobox", "").with_ref("e0000000000000001"),
    ]);
    let task = TASKS
        .iter()
        .find(|t| t.id == "dropdown-select")
        .expect("task");
    let c = judge(task, 1, None, &[(0x2, 1)], &flatten(&root), false);
    assert_eq!(
        c.verdict,
        Verdict::NotIdentified {
            reason: NotIdentifiedReason::NoMatchingNode
        }
    );
    let ok = judge(task, 1, None, &[(0x1, 1)], &flatten(&root), false);
    assert_eq!(ok.verdict, ident("e0000000000000001"));
}

/// 同一性検証: ダイジェストが同じでも出現番号が異なる兄弟（`-2`）は対象（出現番号 1）と
/// 区別し、Identified にしない（先頭要素が消えて兄弟だけが残るケース）。
#[test]
fn aisnap_3_same_digest_different_occurrence_is_not_identified() {
    let task = TASKS
        .iter()
        .find(|t| t.id == "dropdown-select")
        .expect("task");
    let sibling = Node::new("document", "").with_children(vec![
        Node::new("combobox", "").with_ref("e0000000000000001-2"),
    ]);
    let c = judge(task, 1, None, &[(0x1, 1)], &flatten(&sibling), false);
    assert_eq!(
        c.verdict,
        Verdict::NotIdentified {
            reason: NotIdentifiedReason::NoMatchingNode
        }
    );
    let ok = judge(task, 1, None, &[(0x1, 2)], &flatten(&sibling), false);
    assert_eq!(ok.verdict, ident("e0000000000000001-2"));
}

#[test]
fn missing_fixture_dir_is_io_error() {
    let err = run_retention_checks(Path::new("/nonexistent-retention-dir")).unwrap_err();
    assert!(matches!(err, RetentionCheckError::Io { .. }));
    assert!(err.to_string().starts_with("failed to read "));
}

#[test]
fn flatten_includes_header_and_controls_in_pre_order() {
    let table = TableSummary::new(
        vec![HeaderCell::new("columnheader", "H", "e-h").with_data_leaf(DataLeafKind::TableCell)],
        vec![
            TableRow::new("row", false).with_controls(vec![RowControl::new("button", "Go", "e-c")]),
        ],
        0,
    );
    let root = Node::new("document", "").with_children(vec![
        Node::new("table", "").with_ref("e-t").with_table(table),
        Node::new("link", "L").with_ref("e-l"),
    ]);
    let roles: Vec<String> = flatten(&root).into_iter().map(|e| e.role).collect();
    assert_eq!(
        roles,
        vec!["document", "table", "columnheader", "button", "link"]
    );
}

#[test]
fn match_without_ref_is_not_identified() {
    let root = Node::new("document", "").with_children(vec![Node::new("combobox", "")]);
    let task = TASKS
        .iter()
        .find(|t| t.id == "dropdown-select")
        .expect("task");
    let c = judge(task, 1, None, &[], &flatten(&root), false);
    assert_eq!(
        c.verdict,
        Verdict::NotIdentified {
            reason: NotIdentifiedReason::MatchWithoutRef
        }
    );
    assert_eq!(c.snapshot_matches, 1);
}

#[test]
fn no_match_and_missing_dom_reasons() {
    let root = Node::new("document", "");
    let task = TASKS
        .iter()
        .find(|t| t.id == "dropdown-select")
        .expect("task");
    let none = judge(task, 1, None, &[], &flatten(&root), false);
    assert_eq!(
        none.verdict,
        Verdict::NotIdentified {
            reason: NotIdentifiedReason::NoMatchingNode
        }
    );
    let missing = judge(task, 0, None, &[], &flatten(&root), false);
    assert_eq!(
        missing.verdict,
        Verdict::NotIdentified {
            reason: NotIdentifiedReason::TargetMissingInDom
        }
    );
}

#[test]
fn aisnap_3_tsv_rows_have_header_column_count_and_concrete_text() {
    let c = checks();
    let cols = TSV_HEADER.split('\t').count();
    assert_eq!(cols, 11);
    for t in &c {
        assert_eq!(format_row(t).split('\t').count(), cols, "{}", t.id);
    }
    assert_eq!(
        c.first().map(format_row).as_deref(),
        Some(
            "login-button\tlogin-form.html\tbutton[type=submit]\tyes\t1\t1\te7e730a2753f66c98\t-\tLogin\tfalse\t-"
        )
    );
    assert_eq!(
        c.get(1).map(format_row).as_deref(),
        Some(
            "price-first\tec-product-list.html\tp.price_color\tyes\t20\t1\teebd41136ac2ff69e-6\tPriceClass\t-\tfalse\t-"
        )
    );
}

#[test]
fn not_identified_row_shows_reason() {
    let root = Node::new("document", "");
    let task = TASKS
        .iter()
        .find(|t| t.id == "dropdown-select")
        .expect("task");
    let c = judge(task, 1, None, &[], &flatten(&root), false);
    assert_eq!(
        format_row(&c),
        "dropdown-select\tdropdown-form.html\tselect#dropdown\tno\t1\t0\t-\t-\t-\tfalse\tno-matching-node"
    );
}

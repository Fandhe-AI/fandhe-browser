//! `snapshot_text.rs`・`reduction.rs` のユニットテスト
//! （TASK-14.3・`AISNAP-1`・Issue #94）。
//!
//! 実測値（フィクスチャの snapshot トークン数）は `build_snapshot` の現行挙動を
//! 固定したもので、TASK-15・16 等で snapshot の形が変われば更新が要る。
//! 85% 目標の達成可否は #97 の担当で、ここでは判定しない。

#[path = "tokens.rs"]
mod tokens;

#[path = "snapshot_text.rs"]
mod snapshot_text;

#[path = "reduction.rs"]
mod reduction;

use fandhe_browser_ai::snapshot::{
    CheckedState, DataLeafKind, FoldedRow, HeaderCell, Node, RowControl, Snapshot, State, TableRow,
    TableSummary,
};
use reduction::{ReductionError, ReductionRow, measure_reduction, reduction_pct, summarize};
use snapshot_text::render_snapshot;
use tokens::{TokenCounter, fixtures_dir};

fn sample_tree() -> Node {
    let controls = vec![
        RowControl::new("checkbox", "", "e3")
            .with_state(State::default().with_checked(Some(CheckedState::Unchecked))),
        RowControl::new("button", "Del", "e4").with_state(State::default().with_disabled(true)),
    ];
    let table = TableSummary::new(
        vec![HeaderCell::new("columnheader", "Name", "e1")],
        vec![
            TableRow::new("Alice", false)
                .with_controls(controls)
                .with_controls_truncated(true),
            TableRow::new("Bob", true),
        ],
        3,
    );
    Node::new("document", "Title").with_children(vec![
        Node::new("button", "")
            .with_ref("e5")
            .with_state(State::default().with_disabled(true)),
        Node::new("checkbox", "Agree")
            .with_ref("e6")
            .with_state(State::default().with_checked(Some(CheckedState::Checked))),
        Node::new("table", "T").with_ref("e2").with_table(table),
        Node::new("list", "").with_children(vec![Node::new("text", "a\nb")]),
    ])
}

#[test]
fn render_covers_every_output_field() {
    let mut tree = sample_tree();
    tree.folded_rows = vec![FoldedRow::new(1, "folded", true)];
    let text = render_snapshot(&Snapshot::new(tree).with_truncated(true));
    let expected = [
        "- document \"Title\"",
        "  - button [e5] (disabled)",
        "  - checkbox \"Agree\" [e6] (checked)",
        "  - table \"T\" [e2]",
        "    header: columnheader \"Name\" [e1]",
        "    Alice {checkbox=e3 (unchecked), button \"Del\"=e4 (disabled)} …(controls)",
        "    Bob …(text)",
        "    … +3 rows",
        "  - list",
        "    - text \"a b\"",
        // データ行の child を持たないので畳んだ行は末尾にまとめて出る。
        "  folded …(text)",
        "… truncated",
    ]
    .join("\n");
    assert_eq!(text, expected);
    assert_eq!(TokenCounter::new().expect("tokenizer").count(&text), 105);
}

#[test]
fn render_omits_data_leaf_marker() {
    let plain = Snapshot::new(Node::new("cell", "x").with_ref("e1"));
    let marked = Snapshot::new(
        Node::new("cell", "x")
            .with_ref("e1")
            .with_data_leaf(DataLeafKind::TableCell),
    );
    assert_eq!(render_snapshot(&plain), "- cell \"x\" [e1]");
    assert_eq!(render_snapshot(&marked), render_snapshot(&plain));
}

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "actual={actual} expected={expected}"
    );
}

#[test]
fn reduction_pct_formula() {
    approx(reduction_pct(1000, 130).expect("some"), 87.0);
    approx(reduction_pct(100, 150).expect("some"), -50.0);
    assert_eq!(reduction_pct(0, 0), None);
}

fn row(pct: f64, snap: usize) -> ReductionRow {
    ReductionRow {
        name: "x.html".to_string(),
        bytes: 1,
        raw_html_tokens: 1,
        snapshot_tokens: snap,
        reduction_pct: pct,
        snapshot_truncated: false,
    }
}

#[test]
fn summarize_odd_even_and_empty() {
    let s = summarize(&[row(80.0, 40), row(100.0, 10), row(90.0, 20)]).expect("some");
    assert_eq!(s.pages, 3);
    approx(s.mean_reduction_pct, 90.0);
    approx(s.median_snapshot_tokens, 20.0);
    approx(s.min_reduction_pct, 80.0);
    approx(s.max_reduction_pct, 100.0);
    let e = summarize(&[row(80.0, 10), row(90.0, 20), row(70.0, 30), row(60.0, 40)]).expect("some");
    approx(e.median_snapshot_tokens, 25.0);
    approx(e.mean_reduction_pct, 75.0);
    assert_eq!(summarize(&[]), None);
}

/// フィクスチャ 17 件の期待値（名前, 生 HTML トークン数）。`tokens_tests.rs` と同じ値。
const EXPECTED_RAW: [(&str, usize); 17] = [
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
fn fixtures_measured_with_concrete_values() {
    let counter = TokenCounter::new().expect("tokenizer");
    let rows = measure_reduction(&counter, &fixtures_dir()).expect("measure");
    assert_eq!(rows.len(), 17);
    for (r, (name, raw)) in rows.iter().zip(EXPECTED_RAW) {
        assert_eq!(r.name, name);
        assert_eq!(r.raw_html_tokens, raw, "{name}");
        assert!(r.snapshot_tokens > 0, "{name}");
        approx(
            r.reduction_pct,
            (1.0 - r.snapshot_tokens as f64 / raw as f64) * 100.0,
        );
    }
    let by = |n: &str| rows.iter().find(|r| r.name == n).expect("row");
    assert_eq!(by("example-minimal.html").snapshot_tokens, 43);
    assert_eq!(by("login-form.html").snapshot_tokens, 137);
    assert_eq!(by("wiki-portal-nav.html").snapshot_tokens, 2625);
    assert!(by("large-table.html").snapshot_truncated);
    assert!(!by("login-form.html").snapshot_truncated);

    let s = summarize(&rows).expect("summary");
    assert_eq!(s.pages, 17);
    // 実測値（小数 1 桁）。ref の付与を操作・参照の対象ノードに限る変更（TASK-23.4・#827）後も
    // 85% 目標（AISNAP-1）には届かない。判定は #97。
    assert!((s.mean_reduction_pct - 25.0).abs() < 0.05, "{s:?}");
    assert!((s.min_reduction_pct - -144.2).abs() < 0.05, "{s:?}");
    assert!((s.max_reduction_pct - 94.9).abs() < 0.05, "{s:?}");
    approx(s.median_snapshot_tokens, 2625.0);
}

#[test]
fn missing_directory_is_read_error() {
    let counter = TokenCounter::new().expect("tokenizer");
    let dir = fixtures_dir().join("does-not-exist");
    match measure_reduction(&counter, &dir) {
        Err(ReductionError::Tokens(_)) => {}
        other => panic!("unexpected: {other:?}"),
    }
}

/// AISNAP-12・AISNAP-1: ヘッダ行と index 0 の畳んだ行がある表で、畳んだ行はヘッダの後・
/// 先頭データ行の前に出る（ヘッダを数えず、データ行だけで位置を進める）。
#[test]
fn render_folded_row_follows_header_row_in_document_order() {
    let header =
        Node::new("row", "n").with_children(vec![Node::new("columnheader", "n").with_ref("e2")]);
    let data1 = Node::new("row", "").with_children(vec![Node::new("link", "Next").with_ref("e3")]);
    let mut table = Node::new("table", "T")
        .with_ref("e1")
        .with_children(vec![header, data1]);
    table.folded_rows = vec![
        FoldedRow::new(0, "row0", false),
        FoldedRow::new(2, "row2", false),
    ];
    let text = render_snapshot(&Snapshot::new(table));
    let expected = [
        "- table \"T\" [e1]",
        "  - row \"n\"",
        "    - columnheader \"n\" [e2]",
        "  row0",
        "  - row",
        "    - link \"Next\" [e3]",
        "  row2",
    ]
    .join("\n");
    assert_eq!(text, expected);
}

fn data_row(name: &str, r: &str) -> Node {
    Node::new("row", "").with_children(vec![Node::new("link", name).with_ref(r)])
}

/// AISNAP-12: 先頭の複数行が畳まれ、最初の展開行が index 2 のとき、畳んだ行 2 件が
/// 展開行より前に出る（畳んだ行も位置カウンタへ反映する）。
#[test]
fn render_leading_folded_rows_precede_first_expanded_row() {
    let mut list = Node::new("list", "").with_children(vec![
        Node::new("listitem", "").with_children(vec![Node::new("link", "L2").with_ref("e2")]),
    ]);
    list.folded_rows = vec![
        FoldedRow::new(0, "item0", false),
        FoldedRow::new(1, "item1", false),
        FoldedRow::new(3, "item3", false),
    ];
    let text = render_snapshot(&Snapshot::new(list));
    let expected = [
        "- list",
        "  item0",
        "  item1",
        "  - listitem",
        "    - link \"L2\" [e2]",
        "  item3",
    ]
    .join("\n");
    assert_eq!(text, expected);
}

/// AISNAP-12: 実際の表の形（行は `rowgroup` 配下）でも、畳んだ行は thead の後の tbody 内で
/// 展開行と通し位置で交互に出る（index 0・1・3 が畳まれ、2・4 が展開）。
#[test]
fn render_folded_rows_interleave_inside_rowgroups() {
    let thead = Node::new("rowgroup", "").with_children(vec![
        Node::new("row", "").with_children(vec![Node::new("columnheader", "n").with_ref("e1")]),
    ]);
    let tbody =
        Node::new("rowgroup", "").with_children(vec![data_row("R2", "e2"), data_row("R4", "e3")]);
    let mut table = Node::new("table", "T")
        .with_ref("e0")
        .with_children(vec![thead, tbody]);
    table.folded_rows = vec![
        FoldedRow::new(0, "row0", false),
        FoldedRow::new(1, "row1", false),
        FoldedRow::new(3, "row3", false),
        FoldedRow::new(5, "row5", false),
    ];
    let text = render_snapshot(&Snapshot::new(table));
    let expected = [
        "- table \"T\" [e0]",
        "  - rowgroup",
        "    - row",
        "      - columnheader \"n\" [e1]",
        "  - rowgroup",
        "    row0",
        "    row1",
        "    - row",
        "      - link \"R2\" [e2]",
        "    row3",
        "    - row",
        "      - link \"R4\" [e3]",
        "    row5",
    ]
    .join("\n");
    assert_eq!(text, expected);
}

/// AISNAP-12: 全データ行が畳まれた表では、畳んだ行は thead の後ろの最後の rowgroup 内に出る。
#[test]
fn render_all_rows_folded_after_header_group() {
    let thead = Node::new("rowgroup", "").with_children(vec![
        Node::new("row", "").with_children(vec![Node::new("columnheader", "n").with_ref("e1")]),
    ]);
    let tbody = Node::new("rowgroup", "");
    let mut table = Node::new("table", "T")
        .with_ref("e0")
        .with_children(vec![thead, tbody]);
    table.folded_rows = vec![
        FoldedRow::new(0, "row0", false),
        FoldedRow::new(1, "row1", false),
    ];
    let text = render_snapshot(&Snapshot::new(table));
    let expected = [
        "- table \"T\" [e0]",
        "  - rowgroup",
        "    - row",
        "      - columnheader \"n\" [e1]",
        "  - rowgroup",
        "    row0",
        "    row1",
    ]
    .join("\n");
    assert_eq!(text, expected);
}

/// AISNAP-1: name 中の `"` と `\` はエスケープされ、値の境界が曖昧にならない。
#[test]
fn render_escapes_quotes_in_names() {
    let n = Node::new("button", "Say \"hi\" \\o/").with_ref("e1");
    let text = render_snapshot(&Snapshot::new(n));
    assert_eq!(text, "- button \"Say \\\"hi\\\" \\\\o/\" [e1]");
}

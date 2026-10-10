//! PoC-4 追加検証で失敗したタスクが、改善後の設計で解消していることを crate 外の公開 API
//! から固定する回帰テスト（`AISNAP-13`・TASK-18・`MS-2`・Issue #114 配下）。
//!
//! extract-04 節（TASK-18.1・Issue #115）: 大規模テーブル 1 行目・15 列目の値抽出。
//! PoC では「テーブル 1 つあたりのデータセル上限（60 件）」で対象セルが簡約表現から
//! 欠落して失敗した。
//!
//! 2 層構成の理由: 実フィクスチャ `benches/fixtures/large-table.html` は 50 列で
//! `MAX_COLUMNS`（20）を超えるため圧縮せず展開経路（`row` -> `cell`）を通る（層 A）。
//! 圧縮 `TableSummary::rows` 経路は 20 列以下の合成表で確かめる（層 B）。
//!
//! extract-05 節（TASK-18.2・Issue #116）: `quotes-list` の 1 件目の引用文本文の抽出。
//! PoC では `span.text`（価格・表セル以外の地の文）がデータ葉判定の対象外で、簡約表現に
//! 引用文が現れず失敗した。TASK-15（`AISNAP-11`）で `DataLeafKind::ProseClass` /
//! `DataLeafKind::Quote` が加わり解消した。
//!
//! 契約: Snapshot は地の文葉の本文を `Node::name` に持たない（`span` の role は `generic` で
//! 内容命名の対象外。値は ref で別途取得する前提・`AISNAP-3`）。本節は「引用要素が ref 付きの
//! データ葉として公開され、文書順で対応する DOM 要素から本文が読める」ことを固定する。
//! 文書順対応はフィクスチャ制約（`span.text` より前に除外対象要素が無い）に依存する。
//!
//! nav-04 節（TASK-18.3・Issue #117）: 1 セル内に複数リンクが並ぶ上部ナビゲーションで、
//! 対象リンク（PoC は `past`、評価ハーネスは `login`）を選べること。PoC では表の行圧縮が
//! セル内の操作要素を先頭 1 件しか拾わず、2 件目以降のリンクに ref が付かず失敗した。
//! Issue #632 の `TableRow::controls`（行あたり `MAX_ROW_CONTROLS`）で全件を文書順に保持して解消した。
//!
//! 2 層構成の理由: 実フィクスチャ `hn-list.html` は入れ子 table のレイアウト表で展開経路
//! （`row` -> `cell` -> `link`）を通る（層 A）。PoC のバグがあった圧縮経路（`TableRow::controls`）は
//! `dashboard-table.html` と nav-04 形状の合成表で確かめる（層 B）。ハーネスの nav-04（`login`）と
//! PoC の nav-04（複数リンクセル内の `past`）は対象が別のため、両方を固定する。
//!
//! 呼び出し文脈: core の `parse_document` -> ai の `build_snapshot`。フィクスチャは
//! 自作の合成ページで外部通信をしない。入力がリポ内資産のため `expect` を使う。

use std::path::PathBuf;

use fandhe_browser_ai::compress_table::{
    IrregularReason, MAX_COLUMNS, MAX_TABLE_ROWS, TableDetection, detect_regular_structure,
};
use fandhe_browser_ai::data_leaf::classify_data_leaf;
use fandhe_browser_ai::snapshot::{
    DataLeafKind, MAX_ROW_CONTROLS, Node, Snapshot, TableRow, TableSummary, build_snapshot,
};
use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::{query_selector_all_str, query_selector_str};

/// `AISNAP-10` の ref 安定性に基づく固定値（`harness/agent_eval/golden-refs.json` の extract-04）。
const EXTRACT_04_REF: &str = "e7b0a026b4af3d2a1";

/// `AISNAP-10` の ref 安定性に基づく固定値（`golden-refs.json` の extract-05。`span.text` 0 番目）。
const EXTRACT_05_REF: &str = "e784578bcfc4c2ad1";

/// 1 件目の引用文本文（`golden-answers.json` の extract-05。フィクスチャは `&ldquo;`/`&rdquo;`）。
const EXTRACT_05_QUOTE: &str =
    "\u{201c}River stone beta garden prism record valley vector record theta delta system.\u{201d}";

/// `golden-refs.json` の nav-04（`span.pagetop > a[href="login?goto=news"]`。`login` リンク）。
const NAV_04_LOGIN_REF: &str = "e83c7716163c6b424";

/// `reduced/hn-list.txt` の `link "past"`（PoC の nav-04 の対象。同一セルに 8 リンクが並ぶ）。
const NAV_04_PAST_REF: &str = "ebb42928eda4ec7d6";

/// 上部ナビの 1 セルに並ぶリンク名（文書順）。
const NAV_04_CELL_LINKS: [&str; 8] = [
    "News Board",
    "new",
    "past",
    "comments",
    "ask",
    "show",
    "jobs",
    "submit",
];

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
        .join(name)
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name)).expect("フィクスチャは読み込める")
}

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("フィクスチャのパースは成功する")
        .document
}

fn snap(html: &str) -> Snapshot {
    build_snapshot(&parse(html)).expect("フィクスチャの構築は成功する")
}

/// 先行順（明示スタックの反復）で全ノードを列挙する。
fn all_nodes(root: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        out.push(node);
        stack.extend(node.children.iter().rev());
    }
    out
}

/// 合成グリッド表。ヘッダは `Column {c}`、セルは `r{r}c{c}-{c}`（40 文字未満）。
fn grid_html(cols: usize, rows: usize) -> String {
    let head: String = (0..cols).map(|c| format!("<th>Column {c}</th>")).collect();
    let body: String = (0..rows)
        .map(|r| {
            let cells: String = (0..cols)
                .map(|c| format!("<td>r{r}c{c}-{c}</td>"))
                .collect();
            format!("<tr>{cells}</tr>")
        })
        .collect();
    format!(
        "<!DOCTYPE html><html><head><title>t</title></head><body>\
         <table><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table></body></html>"
    )
}

/// 表ノード（ちょうど 1 つ）を返す。
fn only_table(s: &Snapshot) -> &Node {
    let tables: Vec<&Node> = all_nodes(&s.tree)
        .into_iter()
        .filter(|n| n.role == "table")
        .collect();
    assert_eq!(tables.len(), 1, "表はちょうど 1 つ");
    tables.first().copied().expect("直前で件数を確認済み")
}

/// 先頭データ行（最初のセルが `r0c0-0` の `row`）を返す。row の name は空白有無が
/// 入力 HTML で変わるため、子セルで特定する。
fn first_data_row(table: &Node) -> &Node {
    let rows: Vec<&Node> = all_nodes(table)
        .into_iter()
        .filter(|n| n.role == "row" && n.children.first().is_some_and(|c| c.name == "r0c0-0"))
        .collect();
    assert_eq!(rows.len(), 1, "1 行目はちょうど 1 つ");
    rows.first().copied().expect("直前で件数を確認済み")
}

/// `AISNAP-13`・TASK-18.1・Issue #115: Snapshot が打ち切られても 1 行目 15 列目が残る。
#[test]
fn aisnap_13_extract_04_large_table_back_column_cell_survives_truncation() {
    let s = snap(&read_fixture("large-table.html"));
    assert!(s.truncated, "上限打ち切りが起きている前提");
    let table = only_table(&s);
    assert!(table.table.is_none(), "50 列は展開経路");
    let row = first_data_row(table);
    let cell = row.children.get(14).expect("15 列目が残る");
    assert_eq!(cell.role, "cell");
    assert_eq!(cell.name, "r0c14-14");
    assert_eq!(cell.r#ref.as_deref(), Some(EXTRACT_04_REF));
    assert_eq!(cell.data_leaf, Some(DataLeafKind::TableCell));
    // 隣接セルとの取り違え防止。
    assert_eq!(
        row.children.get(13).map(|n| n.name.as_str()),
        Some("r0c13-13")
    );
    assert_eq!(
        row.children.get(15).map(|n| n.name.as_str()),
        Some("r0c15-15")
    );
    let hits = all_nodes(&s.tree)
        .into_iter()
        .filter(|n| n.name == "r0c14-14")
        .count();
    assert_eq!(hits, 1, "値は Snapshot 内で一意に抽出できる");
}

/// `AISNAP-13`・TASK-18.1・Issue #115: ヘッダ 15 番目が列位置と見出しを対応づける。
#[test]
fn aisnap_13_extract_04_column_header_identifies_back_column() {
    let s = snap(&read_fixture("large-table.html"));
    let table = only_table(&s);
    let header: Vec<&Node> = all_nodes(table)
        .into_iter()
        .filter(|n| n.role == "row" && n.name.starts_with("Column 0 "))
        .collect();
    assert_eq!(header.len(), 1, "ヘッダ行はちょうど 1 つ");
    let h = header
        .first()
        .and_then(|r| r.children.get(14))
        .expect("ヘッダ 15 番目が残る");
    assert_eq!(h.role, "columnheader");
    assert_eq!(h.name, "Column 14");
    assert!(h.r#ref.is_some());
}

/// `AISNAP-13`・TASK-18.1・Issue #115: 層 A が展開経路である理由（列数上限）の対照。
#[test]
fn aisnap_13_extract_04_large_table_is_expanded_because_of_column_cap() {
    let doc = parse(&read_fixture("large-table.html"));
    let ids = query_selector_str(&doc, doc.root(), "table").expect("セレクタは有効");
    let id = ids.expect("表が 1 つ以上ある");
    match detect_regular_structure(&doc, id) {
        TableDetection::Irregular(IrregularReason::TooManyColumns { count, max }) => {
            assert_eq!((count, max), (50, MAX_COLUMNS));
        }
        other => panic!("TooManyColumns を期待: {other:?}"),
    }
}

/// `AISNAP-13`・TASK-18.1・Issue #115: 圧縮 rows 経路でも後方列の値が全保持行に残る。
#[test]
fn aisnap_13_extract_04_compressed_rows_keep_back_column_value() {
    let s = snap(&grid_html(MAX_COLUMNS, 25));
    let summary = only_table(&s).table.as_ref().expect("20 列は圧縮経路");
    assert_eq!(summary.header.len(), MAX_COLUMNS);
    assert_eq!(
        summary.header.get(14).map(|h| h.name.as_str()),
        Some("Column 14")
    );
    assert_eq!(summary.rows.len(), MAX_TABLE_ROWS);
    assert_eq!(summary.truncated_rows, 5);
    let first = summary.rows.first().expect("1 行目");
    assert!(!first.truncated);
    let cells: Vec<&str> = first.text.split(" | ").collect();
    assert_eq!(cells.len(), MAX_COLUMNS);
    assert_eq!(cells.get(14).copied(), Some("r0c14-14"));
    assert_eq!(cells.get(19).copied(), Some("r0c19-19"));
    // 20 行 x 20 列 = 400 セル（PoC のセル上限 60 件を超える）で 15 列目が保持される。
    let got: Vec<String> = summary
        .rows
        .iter()
        .map(|r| r.text.split(" | ").nth(14).unwrap_or_default().to_string())
        .collect();
    let want: Vec<String> = (0..MAX_TABLE_ROWS).map(|i| format!("r{i}c14-14")).collect();
    assert_eq!(got, want);
}

/// `AISNAP-13`・TASK-18.1・Issue #115: 列数上限の境界両側で後方列が抽出できる。
#[test]
fn aisnap_13_extract_04_column_cap_boundary_switches_path() {
    let s = snap(&grid_html(MAX_COLUMNS + 1, 25));
    let table = only_table(&s);
    assert!(table.table.is_none(), "21 列は展開経路");
    let row = first_data_row(table);
    let cell = row.children.get(14).expect("15 列目が残る");
    assert_eq!(
        (cell.role.as_str(), cell.name.as_str()),
        ("cell", "r0c14-14")
    );
}

/// 先行順で `data_leaf == Some(kind)` のノードを集める。
fn leaves_of(s: &Snapshot, kind: DataLeafKind) -> Vec<&Node> {
    all_nodes(&s.tree)
        .into_iter()
        .filter(|n| n.data_leaf == Some(kind))
        .collect()
}

/// DOM 要素の本文（空白を 1 個に正規化）。
fn normalized_text(doc: &Document, id: NodeId) -> String {
    let raw = doc.text_content(id).expect("要素の本文は取得できる");
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `AISNAP-13`・TASK-18.2・Issue #116: 引用文の要素が ref 付き地の文葉として公開される。
/// 本文は `name` に載らない現行契約（`AISNAP-3`）のため、name は検査しない。
#[test]
fn aisnap_13_extract_05_quote_body_node_is_exposed_with_ref() {
    let s = snap(&read_fixture("quotes-list.html"));
    assert!(!s.truncated, "quotes-list は打ち切られない");
    let leaves = leaves_of(&s, DataLeafKind::ProseClass);
    assert_eq!(leaves.len(), 10, "引用文は 10 件");
    let first = leaves.first().expect("先頭がある");
    assert_eq!(first.role, "generic");
    assert_eq!(first.r#ref.as_deref(), Some(EXTRACT_05_REF));
    assert!(first.table.is_none());
    let mut refs: Vec<&str> = leaves
        .iter()
        .map(|n| n.r#ref.as_deref().expect("全葉に ref がある"))
        .collect();
    refs.sort_unstable();
    refs.dedup();
    assert_eq!(refs.len(), 10, "ref は重複しない");
}

/// `AISNAP-13`・TASK-18.2・Issue #116: ref 付き葉と文書順で対応する DOM 要素から 1 件目の
/// 本文が一意に読める。
#[test]
fn aisnap_13_extract_05_quote_body_text_is_read_from_matching_dom_element() {
    let html = read_fixture("quotes-list.html");
    let doc = parse(&html);
    let ids = query_selector_all_str(&doc, doc.root(), "span.text").expect("セレクタは有効");
    assert_eq!(ids.len(), 10, "DOM 側も 10 件（Snapshot 側と一致）");
    let first = *ids.first().expect("1 件目");
    assert_eq!(
        classify_data_leaf(&doc, first),
        Some(DataLeafKind::ProseClass)
    );
    assert_eq!(normalized_text(&doc, first), EXTRACT_05_QUOTE);
    let hits = ids
        .iter()
        .filter(|id| normalized_text(&doc, **id) == EXTRACT_05_QUOTE)
        .count();
    assert_eq!(hits, 1, "本文は一意");
    let s = build_snapshot(&doc).expect("構築は成功する");
    assert_eq!(leaves_of(&s, DataLeafKind::ProseClass).len(), ids.len());
}

/// `AISNAP-13`・TASK-18.2・Issue #116: 同一シグネチャの兄弟（著者 span・tags）と取り違えない。
#[test]
fn aisnap_13_extract_05_body_is_not_confused_with_author_or_tags() {
    let s = snap(&read_fixture("quotes-list.html"));
    let parent = all_nodes(&s.tree)
        .into_iter()
        .find(|n| {
            n.children
                .iter()
                .any(|c| c.r#ref.as_deref() == Some(EXTRACT_05_REF))
        })
        .expect("対象の親がある");
    let pos = parent
        .children
        .iter()
        .position(|c| c.r#ref.as_deref() == Some(EXTRACT_05_REF))
        .expect("対象の位置がある");
    let target = parent.children.get(pos).expect("対象");
    // 名前なしの generic（著者 span・tags div）は折り畳まれ、その子（著者リンク・タグリンク）が
    // 親へ繰り上がる（TASK-23.3）。本文以外の兄弟は本文と取り違えられる ref・分類を持たない。
    // 次の引用本文（ProseClass 葉）までを同じ引用ブロックの兄弟として扱う。
    let others: Vec<&Node> = parent
        .children
        .iter()
        .skip(pos + 1)
        .take_while(|c| c.data_leaf != Some(DataLeafKind::ProseClass))
        .collect();
    assert!(!others.is_empty());
    for o in &others {
        assert_ne!(o.data_leaf, Some(DataLeafKind::ProseClass));
        assert_ne!(o.r#ref, target.r#ref);
    }
    let link_names: Vec<String> = others
        .iter()
        .flat_map(|o| all_nodes(o))
        .filter(|c| c.role == "link")
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(link_names, vec!["(about)", "alpha", "beta", "gamma"]);
}

/// `AISNAP-13`・TASK-18.2・Issue #116: 実際の引用要素（`blockquote`・`q`）も `Quote` 葉として
/// 公開され、対応する DOM 要素から本文が読める。
#[test]
fn aisnap_13_extract_05_quote_elements_are_exposed_as_quote_leaves() {
    let html = "<!DOCTYPE html><html><head><title>t</title></head><body>\
        <blockquote><p>Block quote body.</p></blockquote>\
        <p>Lead <q>Inline quote body.</q></p></body></html>";
    let doc = parse(html);
    let s = build_snapshot(&doc).expect("構築は成功する");
    let leaves = leaves_of(&s, DataLeafKind::Quote);
    assert_eq!(leaves.len(), 2, "blockquote と q の 2 件");
    assert!(leaves.iter().all(|n| n.r#ref.is_some()));
    let bodies: Vec<String> = ["blockquote", "q"]
        .iter()
        .map(|sel| {
            let ids = query_selector_all_str(&doc, doc.root(), sel).expect("セレクタは有効");
            assert_eq!(ids.len(), 1);
            let id = *ids.first().expect("1 件");
            assert_eq!(classify_data_leaf(&doc, id), Some(DataLeafKind::Quote));
            normalized_text(&doc, id)
        })
        .collect();
    assert_eq!(bodies, vec!["Block quote body.", "Inline quote body."]);
}

/// 先行順で `role == "link"` の子孫を集める。
fn links_under(n: &Node) -> Vec<&Node> {
    all_nodes(n)
        .into_iter()
        .filter(|c| c.role == "link")
        .collect()
}

/// `past` リンクを子孫に持つ最も内側の `cell`（複数リンクを抱えるセル）を返す。
fn nav_cell(s: &Snapshot) -> &Node {
    let cells: Vec<&Node> = all_nodes(&s.tree)
        .into_iter()
        .filter(|n| {
            n.role == "cell"
                && links_under(n).iter().any(|l| l.name == "past")
                && !n
                    .children
                    .iter()
                    .any(|c| all_nodes(c).iter().any(|d| d.role == "cell"))
        })
        .collect();
    assert_eq!(cells.len(), 1, "past を含む最内セルはちょうど 1 つ");
    cells.first().copied().expect("直前で件数を確認済み")
}

/// `AISNAP-13`・TASK-18.3・Issue #117: 同一セルの 8 リンクがすべて ref 付きで文書順に残る。
#[test]
fn aisnap_13_nav_04_links_sharing_one_cell_each_keep_a_ref() {
    let s = snap(&read_fixture("hn-list.html"));
    assert!(s.truncated, "上限打ち切りが起きている前提");
    let cell = nav_cell(&s);
    let links = links_under(cell);
    let names: Vec<&str> = links.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, NAV_04_CELL_LINKS);
    let mut refs: Vec<&str> = links
        .iter()
        .map(|l| l.r#ref.as_deref().expect("全リンクに ref がある"))
        .collect();
    refs.sort_unstable();
    refs.dedup();
    assert_eq!(refs.len(), NAV_04_CELL_LINKS.len(), "ref は重複しない");
    let past = links.get(2).expect("3 件目");
    assert_eq!(past.name, "past");
    assert_eq!(past.r#ref.as_deref(), Some(NAV_04_PAST_REF));
    let hits = all_nodes(&s.tree)
        .into_iter()
        .filter(|n| n.role == "link" && n.name == "past")
        .count();
    assert_eq!(hits, 1, "past は Snapshot 内で一意に特定できる");
}

/// `AISNAP-13`・TASK-18.3・Issue #117: ハーネスの nav-04（`login`）が隣接セルと取り違えず残る。
#[test]
fn aisnap_13_nav_04_golden_login_link_is_kept_with_ref() {
    let doc = parse(&read_fixture("hn-list.html"));
    let ids = query_selector_all_str(
        &doc,
        doc.root(),
        "span.pagetop > a[href=\"login?goto=news\"]",
    )
    .expect("セレクタは有効");
    assert_eq!(ids.len(), 1, "golden のロケータは 1 要素に対応する");
    let s = build_snapshot(&doc).expect("構築は成功する");
    let hits: Vec<&Node> = all_nodes(&s.tree)
        .into_iter()
        .filter(|n| n.r#ref.as_deref() == Some(NAV_04_LOGIN_REF))
        .collect();
    assert_eq!(hits.len(), 1, "login の ref はちょうど 1 ノード");
    let login = hits.first().copied().expect("直前で件数を確認済み");
    assert_eq!(
        (login.role.as_str(), login.name.as_str()),
        ("link", "login")
    );
    let cell_refs: Vec<&str> = links_under(nav_cell(&s))
        .iter()
        .filter_map(|l| l.r#ref.as_deref())
        .collect();
    assert!(
        !cell_refs.contains(&NAV_04_LOGIN_REF),
        "別セルのリンクである"
    );
}

/// `AISNAP-13`・TASK-18.3・Issue #117: 層 A が展開経路である理由（入れ子 table）の対照。
#[test]
fn aisnap_13_nav_04_hn_list_is_expanded_because_of_nested_tables() {
    let doc = parse(&read_fixture("hn-list.html"));
    let ids = query_selector_all_str(&doc, doc.root(), "table").expect("セレクタは有効");
    let outer = *ids.first().expect("表がある");
    assert_eq!(
        detect_regular_structure(&doc, outer),
        TableDetection::Irregular(IrregularReason::NestedTable)
    );
    let s = build_snapshot(&doc).expect("構築は成功する");
    assert!(
        links_under(nav_cell(&s)).len() == NAV_04_CELL_LINKS.len()
            && all_nodes(&s.tree)
                .iter()
                .filter(|n| n.role == "table")
                .any(|t| t.table.is_none()),
        "ナビを含む表は展開される"
    );
}

/// nav-04 形状（1 セルに 7 リンク + 別セルに login）の圧縮対象の合成表。入れ子なし・title なし。
fn nav_row_html() -> String {
    "<!DOCTYPE html><html><head><title>t</title></head><body><table>\
     <thead><tr><th>Site</th><th>Menu</th><th>Account</th></tr></thead><tbody>\
     <tr><td>News Board</td>\
     <td><a href=\"/new\">new</a> | <a href=\"/past\">past</a> | <a href=\"/comments\">comments</a> | \
     <a href=\"/ask\">ask</a> | <a href=\"/show\">show</a> | <a href=\"/jobs\">jobs</a> | \
     <a href=\"/submit\">submit</a></td>\
     <td><a href=\"/login?goto=news\">login</a></td></tr>\
     <tr><td>plain</td><td>row</td><td>two</td></tr>\
     </tbody></table></body></html>"
        .to_string()
}

fn control_pairs(row: &TableRow) -> Vec<(&str, &str)> {
    row.controls
        .iter()
        .map(|c| (c.role.as_str(), c.name.as_str()))
        .collect()
}

/// `AISNAP-13`・TASK-18.3・Issue #117: 圧縮行でも複数リンクセルの全リンクが ref 付きで残る。
#[test]
fn aisnap_13_nav_04_compressed_row_keeps_every_link_of_a_multi_link_cell() {
    let s = snap(&nav_row_html());
    let summary = only_table(&s).table.as_ref().expect("圧縮経路");
    let first = summary.rows.first().expect("1 行目");
    let want: Vec<(&str, &str)> = [
        "new", "past", "comments", "ask", "show", "jobs", "submit", "login",
    ]
    .iter()
    .map(|n| ("link", *n))
    .collect();
    assert_eq!(control_pairs(first), want);
    assert!(MAX_ROW_CONTROLS >= want.len(), "上限内の構成");
    assert!(!first.controls_truncated);
    let mut refs: Vec<&str> = first.controls.iter().map(|c| c.r#ref.as_str()).collect();
    assert!(refs.iter().all(|r| r.starts_with('e') && r.len() > 1));
    refs.sort_unstable();
    refs.dedup();
    assert_eq!(refs.len(), want.len(), "ref は重複しない");
    let past = first.controls.iter().filter(|c| c.name == "past").count();
    assert_eq!(past, 1);
    // 行文字列はセル内 40 文字で切り詰められるが、操作要素は切り詰めに依存せず全件残る。
    assert!(first.truncated, "メニューセルの本文は切り詰められる");
    assert!(first.text.starts_with("News Board | new | past | comments"));
    assert!(first.text.ends_with(" | login"));
}

/// `AISNAP-13`・TASK-18.3・Issue #117: 実フィクスチャの圧縮表で同名の操作リンクが行ごとに別 ref。
#[test]
fn aisnap_13_nav_04_dashboard_action_links_keep_distinct_refs() {
    let s = snap(&read_fixture("dashboard-table.html"));
    let tables: Vec<&TableSummary> = all_nodes(&s.tree)
        .into_iter()
        .filter_map(|n| n.table.as_ref())
        .collect();
    assert_eq!(tables.len(), 2, "圧縮された表は 2 つ");
    let mut refs: Vec<&str> = Vec::new();
    for t in &tables {
        assert_eq!(t.rows.len(), 4);
        for row in &t.rows {
            assert_eq!(
                control_pairs(row),
                vec![("link", "edit"), ("link", "delete")]
            );
            assert!(!row.controls_truncated);
            refs.extend(row.controls.iter().map(|c| c.r#ref.as_str()));
        }
    }
    let first = tables.first().and_then(|t| t.rows.first()).expect("先頭行");
    assert_eq!(
        first.controls.first().map(|c| c.r#ref.as_str()),
        Some("ed42c6b3dec4e715c")
    );
    assert_eq!(
        first.controls.get(1).map(|c| c.r#ref.as_str()),
        Some("e5cfbb88de0597dda")
    );
    refs.sort_unstable();
    refs.dedup();
    assert_eq!(refs.len(), 16, "全 16 リンクの ref は一意");
}

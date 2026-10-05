//! キャップ境界付近の要素が、ページの軽微な変化（バナー追加・行の挿入・行の追加）の
//! 前後で簡約表現から消えないことを確かめる結合テスト
//! （`AISNAP-12`・TASK-16.5・Issue #108・`MS-2`）。
//!
//! 背景: PoC-4 では行数上限（20 行）付近の要素がバナー追加などで押し出され、参照が
//! 壊れた。TASK-16.4 で固定の `take(20)` を予算モデル（`retention`）へ置換したため、
//! 本ファイルはその回帰を crate 外の公開 API から検出する。
//!
//! 2 層構成の理由: (B) `compress_rows` の公開 API 経路で境界をまたぐ配置を検証し、
//! 対照アサーション（優先候補なしの `select_retained` ならその行は消える）で、
//! テストが実際に境界をまたいでいることを示す。(A) Snapshot 経路は変化前後の 2 回の
//! `build_snapshot` で存在と ref の有無を確かめる。リンク・ボタンだけの表・一覧は圧縮され
//! （Issue #632）、保持行の `TableRow::controls` に ref 付きで残る。`caption` を足した変種は
//! 圧縮を拒否する展開経路（操作要素を持つ行は保持選択に関係なく常に残る）を引き続き検証する。
//!
//! 範囲外: 2 回の Snapshot 間での ref の一致（`AISNAP-10`・TASK-17）。ここでは確かめない。
//! フィクスチャはすべて静的なダミー値で、外部通信をしない。

use fandhe_browser_ai::compress_table::{
    MAX_TABLE_ROWS, TABLE_HEAD_KEEP, compress_rows, detect_regular_structure,
};
use fandhe_browser_ai::retention::{RetentionPolicy, select_retained};
use fandhe_browser_ai::snapshot::{
    Node, RowControl, Snapshot, TableRow, TableSummary, build_snapshot,
};
use fandhe_browser_core::dom::Document;
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_str;

/// キャップ内の最終枠の index（優先候補なしでも残る最後の位置）。
const BOUNDARY: usize = MAX_TABLE_ROWS - 1;
/// 前提: 先頭確保は境界より手前（値が見直されたらコンパイルで検出する）。
const _: () = assert!(TABLE_HEAD_KEEP < BOUNDARY);
/// フィクスチャの行数。
const TOTAL: usize = MAX_TABLE_ROWS * 2;

const NEXT: &str = "<a href=\"/next\" rel=\"next\">Next</a>";
const SEND: &str = "<button type=\"submit\">Send</button>";

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("フィクスチャのパースは成功する")
        .document
}

fn snap(html: &str) -> Snapshot {
    build_snapshot(&parse(html)).expect("フィクスチャの構築は成功する")
}

fn page(body: &str) -> String {
    format!("<!DOCTYPE html><html><head><title>t</title></head><body>{body}</body></html>")
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

/// 各行のセル内容（`row{i}`）を `total` 件作る。
fn base_rows(total: usize) -> Vec<String> {
    (0..total).map(|i| format!("row{i}")).collect()
}

fn set_row(rows: &mut [String], index: usize, content: &str) {
    *rows.get_mut(index).expect("index は行数の範囲内") = content.to_string();
}

fn list_html(rows: &[String]) -> String {
    let items: String = rows.iter().map(|r| format!("<li>{r}</li>")).collect();
    format!("<ul>{items}</ul>")
}

fn table_html(rows: &[String]) -> String {
    let body: String = rows
        .iter()
        .map(|r| format!("<tr><td>{r}</td></tr>"))
        .collect();
    format!("<table><thead><tr><th>n</th></tr></thead><tbody>{body}</tbody></table>")
}

/// `compress_rows` の保持行テキスト（文書順）と省略行数。
fn kept(html: &str, selector: &str) -> (Vec<String>, usize) {
    let doc = parse(html);
    let id = query_selector_str(&doc, doc.root(), selector)
        .expect("セレクタは有効")
        .expect("対象要素が存在する");
    let detection = detect_regular_structure(&doc, id);
    let structure = detection.as_regular().expect("規則的な構造である");
    let r = compress_rows(&doc, structure);
    (
        r.rows.into_iter().map(|x| x.text).collect(),
        r.truncated_rows,
    )
}

fn has(texts: &[String], want: &str) -> bool {
    texts.iter().any(|t| t == want)
}

/// 対照: 優先候補なしの固定キャップ相当では `index` が保持されない。
fn assert_fixed_cap_would_drop(len: usize, index: usize) {
    let policy = RetentionPolicy::new(MAX_TABLE_ROWS, TABLE_HEAD_KEEP);
    let r = select_retained(len, &policy);
    assert!(
        r.kept.iter().all(|k| k.index != index),
        "固定キャップなら index {index} は消えるシナリオである"
    );
}

/// AISNAP-12・TASK-16.5・Issue #108: 先頭への行挿入で境界の内側から外側へ押し出された
/// ページネーション行が、変化後も保持される。
#[test]
fn aisnap_12_pagination_survives_row_inserted_at_top() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, NEXT);
    let (before, trunc_before) = kept(&list_html(&rows), "ul");
    assert_eq!(before.len(), MAX_TABLE_ROWS);
    assert_eq!(trunc_before, TOTAL - MAX_TABLE_ROWS);
    assert!(has(&before, "Next"));

    rows.insert(0, "お知らせ".to_string());
    let (after, trunc_after) = kept(&list_html(&rows), "ul");
    assert_eq!(after.len(), MAX_TABLE_ROWS);
    assert_eq!(trunc_after, TOTAL + 1 - MAX_TABLE_ROWS);
    assert!(has(&after, "Next"));
    assert_fixed_cap_would_drop(TOTAL + 1, MAX_TABLE_ROWS);
}

/// AISNAP-12・TASK-16.5・Issue #108: 送信ボタン行でも同様に、境界外へ押し出されても残る。
#[test]
fn aisnap_12_submit_button_survives_row_inserted_at_top() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, SEND);
    let wrap = |rows: &[String]| format!("<form>{}</form>", table_html(rows));
    let (before, trunc_before) = kept(&page(&wrap(&rows)), "table");
    assert_eq!(before.len(), MAX_TABLE_ROWS);
    assert_eq!(trunc_before, TOTAL - MAX_TABLE_ROWS);
    assert!(has(&before, "Send"));

    rows.insert(0, "お知らせ".to_string());
    let (after, trunc_after) = kept(&page(&wrap(&rows)), "table");
    assert_eq!(after.len(), MAX_TABLE_ROWS);
    assert_eq!(trunc_after, TOTAL + 1 - MAX_TABLE_ROWS);
    assert!(has(&after, "Send"));
    assert_fixed_cap_would_drop(TOTAL + 1, MAX_TABLE_ROWS);
}

/// AISNAP-12・TASK-16.5・Issue #108: 末尾に通常行を足しても、ページネーション行・
/// 送信ボタン行・先頭行が残り、省略行数は追加した行数だけ正確に増える。
#[test]
fn aisnap_12_priority_rows_survive_rows_appended_at_tail() {
    let added = 3;
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, NEXT);
    set_row(&mut rows, MAX_TABLE_ROWS, SEND);
    let wrap = |rows: &[String]| format!("<form>{}</form>", list_html(rows));
    let (before, trunc_before) = kept(&wrap(&rows), "ul");
    for want in ["row0", "Next", "Send"] {
        assert!(has(&before, want), "{want} は変化前に残る");
    }
    assert_eq!(before.len(), MAX_TABLE_ROWS);
    assert_eq!(trunc_before, TOTAL - MAX_TABLE_ROWS);

    rows.extend((0..added).map(|i| format!("tail{i}")));
    let (after, trunc_after) = kept(&wrap(&rows), "ul");
    for want in ["row0", "Next", "Send"] {
        assert!(has(&after, want), "{want} は変化後にも残る");
    }
    assert_eq!(after.len(), MAX_TABLE_ROWS);
    assert_eq!(trunc_after, trunc_before + added);
}

/// AISNAP-12・TASK-16.5・Issue #108: 先頭へ非表示行を挿入しても index 空間に入らず、
/// 保持される行と省略行数は変化前と完全に一致する。
#[test]
fn aisnap_12_hidden_row_inserted_at_top_does_not_shift_selection() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, NEXT);
    let html = list_html(&rows);
    let before = kept(&html, "ul");
    let html_hidden = html.replacen("<ul>", "<ul><li hidden>お知らせ</li>", 1);
    assert_ne!(html, html_hidden);
    let after = kept(&html_hidden, "ul");
    assert_eq!(before, after);
    assert!(has(&after.0, "Next"));
}

/// 展開された本文行数（ヘッダ行を除く）と畳まれた行数の合計。
fn table_totals(s: &Snapshot) -> usize {
    let nodes = all_nodes(&s.tree);
    let table = nodes
        .iter()
        .find(|n| n.role == "table")
        .expect("表ノードがある");
    let expanded = nodes.iter().filter(|n| n.role == "row").count();
    expanded.saturating_sub(1) + table.folded_rows.len()
}

fn folded_has(s: &Snapshot, needle: &str) -> bool {
    all_nodes(&s.tree)
        .iter()
        .filter(|n| n.role == "table")
        .any(|t| t.folded_rows.iter().any(|r| r.text.contains(needle)))
}

fn nodes_named<'a>(s: &'a Snapshot, role: &str, name: &str) -> Vec<&'a Node> {
    all_nodes(&s.tree)
        .into_iter()
        .filter(|n| n.role == role && n.name == name)
        .collect()
}

/// 圧縮された表・一覧の行内操作要素から `role`・`name` が一致するものを集める。
fn controls_named<'a>(s: &'a Snapshot, role: &str, name: &str) -> Vec<&'a RowControl> {
    all_nodes(&s.tree)
        .into_iter()
        .filter_map(|n| n.table.as_ref())
        .flat_map(|t| t.rows.iter())
        .flat_map(|r| r.controls.iter())
        .filter(|c| c.role == role && c.name == name)
        .collect()
}

/// `caption` を足して圧縮を拒否し、展開経路（優先保持 + 畳み込み）へ固定した表。
fn captioned(table: &str) -> String {
    table.replacen("<table>", "<table><caption>c</caption>", 1)
}

/// AISNAP-12・AISNAP-2・AISNAP-13・TASK-16.5・Issue #108・#632: 圧縮経路で、バナーと先頭行の
/// 挿入の前後に Snapshot を取り直しても、キャップ外のページネーションリンクを含む行が
/// 圧縮行として残り、その control が ref 付きで 1 件だけ存在し、行数の合計が欠けない。
#[test]
fn aisnap_12_snapshot_compressed_table_keeps_pagination_control_across_banner_and_row_insertion() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, NEXT);
    let before = snap(&page(&table_html(&rows)));

    rows.insert(0, "告知".to_string());
    let after = snap(&page(&format!("<div>お知らせ</div>{}", table_html(&rows))));

    for (s, total) in [(&before, TOTAL), (&after, TOTAL + 1)] {
        let controls = controls_named(s, "link", "Next");
        assert_eq!(controls.len(), 1);
        assert!(controls.iter().all(|c| c.r#ref.starts_with('e')));
        let t = summary(s);
        assert_eq!(t.rows.len(), MAX_TABLE_ROWS);
        assert_eq!(t.rows.len() + t.truncated_rows, total);
        assert!(t.rows.iter().any(|r| r.text == "Next"));
    }
}

/// AISNAP-12・AISNAP-2・Issue #632: 表外へのバナー挿入の前後でも、送信ボタンは圧縮行の
/// control として ref 付きで 1 件だけ残り、その ref は変わらない（`AISNAP-10`）。
#[test]
fn aisnap_12_snapshot_compressed_table_keeps_submit_control_across_banner_insertion() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, "<button type=\"submit\">send</button>");
    let form = format!("<form>{}</form>", table_html(&rows));
    let before = snap(&page(&form));
    let after = snap(&page(&format!("<div>お知らせ</div>{form}")));
    for s in [&before, &after] {
        assert_eq!(controls_named(s, "button", "send").len(), 1);
        let t = summary(s);
        assert_eq!(t.rows.len(), MAX_TABLE_ROWS);
        assert_eq!(t.rows.len() + t.truncated_rows, TOTAL);
    }
    let refs = |s: &Snapshot| {
        controls_named(s, "button", "send")
            .iter()
            .map(|c| c.r#ref.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(refs(&before), refs(&after));
}

/// AISNAP-12・TASK-16.5・Issue #108: `caption` で圧縮を拒否した展開経路でも、バナーと
/// 先頭行の挿入の前後でページネーションリンクは ref 付きで 1 つだけ残り、畳まれない。
#[test]
fn aisnap_12_snapshot_keeps_pagination_link_across_banner_and_row_insertion() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, NEXT);
    let before = snap(&page(&captioned(&table_html(&rows))));

    rows.insert(0, "告知".to_string());
    let after = snap(&page(&format!(
        "<div>お知らせ</div>{}",
        captioned(&table_html(&rows))
    )));

    for (s, total) in [(&before, TOTAL), (&after, TOTAL + 1)] {
        let links = nodes_named(s, "link", "Next");
        assert_eq!(links.len(), 1);
        assert!(links.iter().all(|l| l.r#ref.is_some()));
        assert!(!folded_has(s, "Next"));
        assert_eq!(table_totals(s), total);
    }
}

/// AISNAP-12・TASK-16.5・Issue #108: `caption` で圧縮を拒否した展開経路でも、表外への
/// バナー挿入の前後で送信ボタンは ref 付きで残る。
#[test]
fn aisnap_12_snapshot_keeps_submit_button_across_banner_insertion() {
    let mut rows = base_rows(TOTAL);
    set_row(&mut rows, BOUNDARY, "<button type=\"submit\">send</button>");
    let form = format!("<form>{}</form>", captioned(&table_html(&rows)));
    let before = snap(&page(&form));
    let after = snap(&page(&format!("<div>お知らせ</div>{form}")));
    for s in [&before, &after] {
        let buttons = nodes_named(s, "button", "send");
        assert_eq!(buttons.len(), 1);
        assert!(buttons.iter().all(|b| b.r#ref.is_some()));
        assert!(!folded_has(s, "send"));
        assert_eq!(table_totals(s), TOTAL);
    }
}

fn summary(s: &Snapshot) -> &TableSummary {
    all_nodes(&s.tree)
        .into_iter()
        .find_map(|n| n.table.as_ref())
        .expect("圧縮された表がある")
}

/// AISNAP-12・TASK-16.5・Issue #108: 操作要素のない圧縮可能な表では、バナー挿入で
/// 要約全体が不変、末尾への行追加でも先頭行が残り省略行数だけが増える。
#[test]
fn aisnap_12_snapshot_compressed_table_keeps_head_row_across_minor_change() {
    let added = 3;
    let mut rows = base_rows(TOTAL);
    let html = table_html(&rows);
    let before = snap(&page(&html));
    let bannered = snap(&page(&format!("<div>お知らせ</div>{html}")));
    assert_eq!(summary(&before), summary(&bannered));
    assert_eq!(summary(&before).truncated_rows, TOTAL - MAX_TABLE_ROWS);

    rows.extend((0..added).map(|i| format!("tail{i}")));
    let appended = snap(&page(&table_html(&rows)));
    for s in [&before, &bannered, &appended] {
        let t = summary(s);
        assert_eq!(t.rows.len(), MAX_TABLE_ROWS);
        assert_eq!(t.rows.first(), Some(&TableRow::new("row0", false)));
    }
    assert_eq!(
        summary(&appended).truncated_rows,
        TOTAL - MAX_TABLE_ROWS + added
    );
}

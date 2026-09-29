//! 表・一覧の規則的な行列構造の検出ロジック（`AISNAP-2`・`TASK-12.1`・`MS-2`・
//! Issue #79）。
//!
//! 表・一覧類型のページでは、単純なアクセシビリティツリー変換が生 HTML より
//! トークンを増やす逆転現象が起きる。これを避けるため「ヘッダ行のみ個別 ref、
//! データ行は圧縮 1 行表現」へ変換する戦略（PoC-4 `reduce.mjs` の
//! `renderCompactTable` が土台）の第一段として、本モジュールは
//! `table`・`ul`・`ol` 要素が規則的な行列構造かを **判定するだけ**の層を担う。
//!
//! 呼び出し文脈: 現時点では呼び出し元は無い。`snapshot` のツリー構築への統合
//! （TASK-12.5・Issue #83）が table/list ごとに [`detect_regular_structure`] を
//! 呼び、TASK-12.2（#80）・12.3（#81）・12.4（#82）が結果の
//! [`RegularStructure::header_cells`]・[`RegularStructure::body_rows`] を再走査
//! なしで使う想定である。ヘッダの個別 ref 付与・行の圧縮表現・`truncated_rows`
//! 注記・統合は未実装（実装済みを装わない。REPAIR-3）。
//!
//! # 判定規則（table）
//!
//! 1. role が `table` であること（`role="presentation"` 等のレイアウト用は不規則）
//! 2. 子孫に別の `table` を含まない（入れ子はレイアウト用とみなす）
//! 3. 行は直下の `tr` と `thead`/`tbody`/`tfoot` 直下の `tr`。セル 0 個の行は除外
//! 4. `rowspan` が 2 以上または 0 のセルを含まない
//! 5. `thead` の非空行は 1 行以下
//! 6. 全行の実効列数（`colspan` の合計）が一致し、[`MAX_COLUMNS`] 以下
//!
//! `list` は `li` のみを直下に持ち、`li` 内に `ul`/`ol`/`table` を含まないものを
//! 規則的とする。
//!
//! # スコープ外
//!
//! ARIA `role="table"`/`"grid"`/`"list"` を付けた非ネイティブ要素・`dl`・
//! `rowspan` を含む表のグリッド展開・多段ヘッダは対象外（不規則扱いまたは
//! 未検出）。HTML 要素名判定・非負整数解析は `snapshot` 配下にも private 実装が
//! あるが、共通化は統合時（TASK-12.5）に検討する（波及範囲を本モジュールに
//! 閉じるため複製している）。

use crate::snapshot::compute_role;
use fandhe_browser_core::dom::{Document, NodeId};

/// HTML 名前空間の URI。`core` 側の定義が `pub(crate)` のためローカルに持つ。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// 規則的とみなす実効列数の上限（PoC-4 `reduce.mjs` と同値）。
pub const MAX_COLUMNS: usize = 20;

/// `colspan` のクランプ上限（HTML Standard の 1000）。
const MAX_COLSPAN: usize = 1000;

/// 検出対象の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StructureKind {
    /// `table` 要素。
    Table,
    /// `ul`/`ol` 要素。
    List,
}

/// 規則的と判定された構造。後続タスク（#80〜#83）が利用する。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegularStructure {
    /// 構造の種類。
    pub kind: StructureKind,
    /// `table`/`ul`/`ol` 要素自身。
    pub container: NodeId,
    /// ヘッダ行の `tr`（無ければ `None`。List は常に `None`）。
    pub header_row: Option<NodeId>,
    /// ヘッダ行のセル（文書順）。
    pub header_cells: Vec<NodeId>,
    /// データ行の `tr`/`li`（文書順。空行・ヘッダ・`tfoot` 行は除く）。
    pub body_rows: Vec<NodeId>,
    /// `tfoot` 配下の非空 `tr`（圧縮表現上の扱いは後続タスクで決める）。
    pub footer_rows: Vec<NodeId>,
    /// `colspan` を考慮した実効列数（List は 1）。
    pub column_count: usize,
    /// セル 0 個のため判定から除外した `tr` の数。
    pub skipped_empty_rows: usize,
}

/// 不規則と判定した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IrregularReason {
    /// `table`/`ul`/`ol` 以外、非要素、または範囲外の ID。
    NotTableOrList,
    /// role がデータ用ではない（`presentation`/`none` 等）。
    NotDataRole {
        /// 算出された role トークン。
        role: String,
    },
    /// 非空の行が 0 個。
    NoRows,
    /// 子孫に `table` を含む（レイアウト用テーブル）。
    NestedTable,
    /// `li` 配下に `ul`/`ol` を含む（階層メニュー等）。
    NestedList,
    /// List 直下に `li` 以外の要素がある。
    UnexpectedChild,
    /// `thead` に非空行が 2 行以上ある。
    MultipleHeaderRows {
        /// `thead` の非空行数。
        count: usize,
    },
    /// `rowspan` が 2 以上または 0 のセルがある。
    RowSpan,
    /// 実効列数が [`MAX_COLUMNS`] を超える。
    TooManyColumns {
        /// 実効列数。
        count: usize,
        /// 上限。
        max: usize,
    },
    /// 行ごとの実効列数が一致しない。
    InconsistentColumnCount {
        /// 最初の非空行の実効列数。
        expected: usize,
        /// 食い違った行の実効列数。
        found: usize,
        /// 食い違った最初の行。
        row: NodeId,
    },
}

/// [`detect_regular_structure`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableDetection {
    /// 規則的。
    Regular(RegularStructure),
    /// 不規則（理由付き）。
    Irregular(IrregularReason),
}

impl TableDetection {
    /// 規則的なら `true`。
    pub fn is_regular(&self) -> bool {
        matches!(self, Self::Regular(_))
    }

    /// 規則的なら構造への参照を返す。
    pub fn as_regular(&self) -> Option<&RegularStructure> {
        match self {
            Self::Regular(s) => Some(s),
            Self::Irregular(_) => None,
        }
    }
}

/// HTML 名前空間の要素で、local name が `name` と（ASCII 大文字小文字非区別で）
/// 一致するか。
fn is_html(doc: &Document, id: NodeId, name: &str) -> bool {
    doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && doc
            .local_name(id)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
}

/// HTML の非負整数の解析規則（先頭の空白を飛ばし数字列を読む）。
/// オーバーフローは `usize::MAX` へ飽和する。数字で始まらなければ `None`。
fn parse_non_negative(value: &str) -> Option<usize> {
    let trimmed = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let mut acc: usize = 0;
    let mut seen = false;
    for c in trimmed.chars() {
        let Some(d) = c.to_digit(10) else { break };
        seen = true;
        acc = acc
            .checked_mul(10)
            .and_then(|v| v.checked_add(d as usize))
            .unwrap_or(usize::MAX);
    }
    seen.then_some(acc)
}

/// `colspan` の実効値（解釈不能・0 は 1、1000 超は 1000）。
fn colspan_of(doc: &Document, id: NodeId) -> usize {
    match doc.attribute(id, "colspan").and_then(parse_non_negative) {
        None | Some(0) => 1,
        Some(n) => n.min(MAX_COLSPAN),
    }
}

/// `rowspan` が行単位圧縮と相性の悪い値（2 以上または 0）か。
fn has_unsupported_rowspan(doc: &Document, id: NodeId) -> bool {
    !matches!(
        doc.attribute(id, "rowspan").and_then(parse_non_negative),
        None | Some(1)
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Head,
    Body,
    Foot,
}

struct Row {
    id: NodeId,
    group: Group,
    cells: Vec<NodeId>,
    columns: usize,
}

/// `tr` のセル（直下の `td`/`th`）と実効列数を集める。`rowspan` 違反は `Err`。
fn collect_row(doc: &Document, id: NodeId, group: Group) -> Result<Row, IrregularReason> {
    let mut cells = Vec::new();
    let mut columns: usize = 0;
    for child in doc.children(id) {
        if !(is_html(doc, child, "td") || is_html(doc, child, "th")) {
            continue;
        }
        if has_unsupported_rowspan(doc, child) {
            return Err(IrregularReason::RowSpan);
        }
        columns = columns.saturating_add(colspan_of(doc, child));
        cells.push(child);
    }
    Ok(Row {
        id,
        group,
        cells,
        columns,
    })
}

/// 行を集める。戻り値は（非空行, 除外した空行数）。行は文書順。
/// 深さは table 直下と行グループ直下の 2 段に固定する（再帰しない）。
fn collect_rows(doc: &Document, table: NodeId) -> Result<(Vec<Row>, usize), IrregularReason> {
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    let mut add = |row: Row| {
        if row.cells.is_empty() {
            skipped += 1;
        } else {
            rows.push(row);
        }
    };
    for child in doc.children(table) {
        if is_html(doc, child, "tr") {
            add(collect_row(doc, child, Group::Body)?);
            continue;
        }
        let group = if is_html(doc, child, "thead") {
            Group::Head
        } else if is_html(doc, child, "tbody") {
            Group::Body
        } else if is_html(doc, child, "tfoot") {
            Group::Foot
        } else {
            continue;
        };
        for tr in doc.children(child).filter(|&t| is_html(doc, t, "tr")) {
            add(collect_row(doc, tr, group)?);
        }
    }
    Ok((rows, skipped))
}

fn detect_table(doc: &Document, table: NodeId) -> TableDetection {
    use IrregularReason as R;
    let irregular = TableDetection::Irregular;

    if let Some(role) = compute_role(doc, table)
        && role.as_str() != "table"
    {
        return irregular(R::NotDataRole {
            role: role.as_str().to_string(),
        });
    }
    if doc.descendants(table).any(|d| is_html(doc, d, "table")) {
        return irregular(R::NestedTable);
    }

    let (rows, skipped_empty_rows) = match collect_rows(doc, table) {
        Ok(v) => v,
        Err(e) => return irregular(e),
    };
    let Some(first) = rows.first() else {
        return irregular(R::NoRows);
    };
    let head_count = rows.iter().filter(|r| r.group == Group::Head).count();
    if head_count >= 2 {
        return irregular(R::MultipleHeaderRows { count: head_count });
    }

    let expected = first.columns;
    if expected > MAX_COLUMNS {
        return irregular(R::TooManyColumns {
            count: expected,
            max: MAX_COLUMNS,
        });
    }
    if let Some(bad) = rows.iter().find(|r| r.columns != expected) {
        return irregular(R::InconsistentColumnCount {
            expected,
            found: bad.columns,
            row: bad.id,
        });
    }

    // ヘッダ行: thead の 1 行、無ければ全セルが th の先頭行。
    let header_idx = if head_count == 1 {
        rows.iter().position(|r| r.group == Group::Head)
    } else {
        (first.group != Group::Foot && first.cells.iter().all(|&c| is_html(doc, c, "th")))
            .then_some(0)
    };

    let mut header_row = None;
    let mut header_cells = Vec::new();
    let mut body_rows = Vec::new();
    let mut footer_rows = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if Some(i) == header_idx {
            header_row = Some(row.id);
            header_cells = row.cells.clone();
        } else if row.group == Group::Foot {
            footer_rows.push(row.id);
        } else {
            body_rows.push(row.id);
        }
    }

    TableDetection::Regular(RegularStructure {
        kind: StructureKind::Table,
        container: table,
        header_row,
        header_cells,
        body_rows,
        footer_rows,
        column_count: expected,
        skipped_empty_rows,
    })
}

fn detect_list(doc: &Document, list: NodeId) -> TableDetection {
    use IrregularReason as R;
    let irregular = TableDetection::Irregular;

    if let Some(role) = compute_role(doc, list)
        && role.as_str() != "list"
    {
        return irregular(R::NotDataRole {
            role: role.as_str().to_string(),
        });
    }
    let mut items = Vec::new();
    for child in doc.children(list) {
        if !doc.is_element(child) {
            continue;
        }
        if is_html(doc, child, "li") {
            items.push(child);
        } else if !(is_html(doc, child, "script") || is_html(doc, child, "template")) {
            return irregular(R::UnexpectedChild);
        }
    }
    for &li in &items {
        for d in doc.descendants(li) {
            if is_html(doc, d, "table") {
                return irregular(R::NestedTable);
            }
            if is_html(doc, d, "ul") || is_html(doc, d, "ol") {
                return irregular(R::NestedList);
            }
        }
    }
    if items.is_empty() {
        return irregular(R::NoRows);
    }
    TableDetection::Regular(RegularStructure {
        kind: StructureKind::List,
        container: list,
        header_row: None,
        header_cells: Vec::new(),
        body_rows: items,
        footer_rows: Vec::new(),
        column_count: 1,
        skipped_empty_rows: 0,
    })
}

/// `id` の要素（`table`/`ul`/`ol`）が規則的な行列構造かを判定する
/// （`AISNAP-2`・`TASK-12.1`）。
///
/// 範囲外の ID・非要素・対象外要素は panic せず
/// [`IrregularReason::NotTableOrList`] を返す。
pub fn detect_regular_structure(doc: &Document, id: NodeId) -> TableDetection {
    if is_html(doc, id, "table") {
        detect_table(doc, id)
    } else if is_html(doc, id, "ul") || is_html(doc, id, "ol") {
        detect_list(doc, id)
    } else {
        TableDetection::Irregular(IrregularReason::NotTableOrList)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fandhe_browser_core::parse::{ParseOptions, parse_document};
    use fandhe_browser_core::query::query_selector_str;

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn select(doc: &Document, sel: &str) -> NodeId {
        query_selector_str(doc, doc.root(), sel)
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる")
    }

    fn detect(html: &str, sel: &str) -> TableDetection {
        let doc = parse(html);
        detect_regular_structure(&doc, select(&doc, sel))
    }

    fn reason(html: &str, sel: &str) -> IrregularReason {
        match detect(html, sel) {
            TableDetection::Irregular(r) => r,
            other => panic!("不規則を期待したが {other:?}"),
        }
    }

    fn regular(html: &str, sel: &str) -> RegularStructure {
        match detect(html, sel) {
            TableDetection::Regular(s) => s,
            other => panic!("規則的を期待したが {other:?}"),
        }
    }

    /// AISNAP-2（TASK-12.1・Issue #79）: thead/tbody を持つ規則的な表（受入基準）。
    #[test]
    fn aisnap_2_regular_thead_tbody() {
        let doc = parse(
            "<table><thead><tr id=h><th>a<th>b<th>c</thead><tbody>\
             <tr><td>1<td>2<td>3<tr><td>4<td>5<td>6</tbody></table>",
        );
        let det = detect_regular_structure(&doc, select(&doc, "table"));
        assert!(det.is_regular());
        let s = det.as_regular().expect("規則的");
        assert_eq!(s.kind, StructureKind::Table);
        assert_eq!(s.column_count, 3);
        assert_eq!(s.header_cells.len(), 3);
        assert_eq!(s.body_rows.len(), 2);
        assert_eq!(s.header_row, Some(select(&doc, "#h")));
    }

    /// AISNAP-2（TASK-12.1・Issue #79）: 列数が食い違う表は不規則（受入基準）。
    #[test]
    fn aisnap_2_irregular_column_mismatch() {
        let doc = parse(
            "<table><thead><tr><th>a<th>b<th>c</thead><tbody>\
             <tr><td>1<td>2<td>3<tr id=bad><td>4<td>5</tbody></table>",
        );
        let det = detect_regular_structure(&doc, select(&doc, "table"));
        assert!(!det.is_regular());
        assert_eq!(
            det,
            TableDetection::Irregular(IrregularReason::InconsistentColumnCount {
                expected: 3,
                found: 2,
                row: select(&doc, "#bad"),
            })
        );
    }

    /// AISNAP-2: thead なしの裸の tr（html5ever が tbody を補う）。
    #[test]
    fn aisnap_2_bare_rows() {
        let s = regular(
            "<table><tr><td>1<td>2<tr><td>3<td>4<tr><td>5<td>6</table>",
            "table",
        );
        assert_eq!(s.header_row, None);
        assert_eq!(s.body_rows.len(), 3);
    }

    /// AISNAP-2: thead なしで先頭行が全 th ならヘッダ。
    #[test]
    fn aisnap_2_first_row_th_is_header() {
        let s = regular("<table><tr><th>a<th>b<tr><td>1<td>2</table>", "table");
        assert!(s.header_row.is_some());
        assert_eq!(s.header_cells.len(), 2);
        assert_eq!(s.body_rows.len(), 1);
    }

    /// AISNAP-2: 入れ子の表はレイアウト用として不規則。
    #[test]
    fn aisnap_2_nested_table() {
        let r = reason("<table id=o><tr><td><table><tr><td>x</table></table>", "#o");
        assert_eq!(r, IrregularReason::NestedTable);
    }

    /// AISNAP-2: 空の表・セル 0 個の行のみは NoRows。
    #[test]
    fn aisnap_2_no_rows() {
        assert_eq!(reason("<table></table>", "table"), IrregularReason::NoRows);
        assert_eq!(
            reason("<table><tr></tr><tr></tr></table>", "table"),
            IrregularReason::NoRows
        );
    }

    /// AISNAP-2: Hacker News 相当（3 セル行・colspan=2+1 セル行・空 spacer 行）。
    #[test]
    fn aisnap_2_hn_like() {
        let s = regular(
            "<table><tr><td>1<td>up<td>title</tr><tr><td colspan=2>x<td>sub</tr><tr class=s></tr>\
             <tr><td>2<td>up<td>title</tr><tr><td colspan=2>y<td>sub</tr><tr class=s></tr></table>",
            "table",
        );
        assert_eq!(s.column_count, 3);
        assert_eq!(s.skipped_empty_rows, 2);
        assert_eq!(s.body_rows.len(), 4);
    }

    /// AISNAP-2: rowspan が 2 以上または 0 は RowSpan。
    #[test]
    fn aisnap_2_rowspan() {
        for v in ["2", "0"] {
            let html = format!("<table><tr><td rowspan={v}>a<td>b</table>");
            assert_eq!(reason(&html, "table"), IrregularReason::RowSpan);
        }
    }

    /// AISNAP-2: colspan の異常値は HTML 規則で解釈される。
    #[test]
    fn aisnap_2_colspan_edge_values() {
        for v in ["abc", "0"] {
            let html = format!("<table><tr><td colspan={v}>a</table>");
            assert_eq!(regular(&html, "table").column_count, 1);
        }
        assert_eq!(
            reason("<table><tr><td colspan=99999>a</table>", "table"),
            IrregularReason::TooManyColumns {
                count: 1000,
                max: MAX_COLUMNS
            }
        );
    }

    /// AISNAP-2: 21 列は TooManyColumns、20 列は規則的。
    #[test]
    fn aisnap_2_column_limit() {
        let row = |n: usize| format!("<table><tr>{}</table>", "<td>x".repeat(n));
        assert_eq!(
            reason(&row(21), "table"),
            IrregularReason::TooManyColumns { count: 21, max: 20 }
        );
        assert_eq!(regular(&row(20), "table").column_count, 20);
    }

    /// AISNAP-2: thead の 2 行は MultipleHeaderRows。
    #[test]
    fn aisnap_2_multiple_header_rows() {
        assert_eq!(
            reason(
                "<table><thead><tr><th>a<tr><th>b</thead><tbody><tr><td>1</table>",
                "table"
            ),
            IrregularReason::MultipleHeaderRows { count: 2 }
        );
    }

    /// AISNAP-2: レイアウト用 role の table は NotDataRole。
    #[test]
    fn aisnap_2_presentation_role() {
        assert_eq!(
            reason("<table role=presentation><tr><td>a</table>", "table"),
            IrregularReason::NotDataRole {
                role: "presentation".to_string()
            }
        );
    }

    /// AISNAP-2: tfoot は body_rows に含まれず footer_rows へ入る。
    #[test]
    fn aisnap_2_tfoot() {
        let s = regular(
            "<table><thead><tr><th>a<th>b</thead><tbody><tr><td>1<td>2</tbody>\
             <tfoot><tr><td>s<td>t</tfoot></table>",
            "table",
        );
        assert_eq!(s.footer_rows.len(), 1);
        assert_eq!(s.body_rows.len(), 1);
    }

    /// AISNAP-2: ul/ol は規則的な一覧。
    #[test]
    fn aisnap_2_lists() {
        for tag in ["ul", "ol"] {
            let html = format!("<{tag}><li>a<li>b<li>c</{tag}>");
            let s = regular(&html, tag);
            assert_eq!(s.kind, StructureKind::List);
            assert_eq!(s.column_count, 1);
            assert_eq!(s.body_rows.len(), 3);
            assert_eq!(s.header_row, None);
        }
    }

    /// AISNAP-2: li 内の入れ子一覧は NestedList。
    #[test]
    fn aisnap_2_nested_list() {
        assert_eq!(
            reason("<ul id=o><li>a<ul><li>b</ul></ul>", "#o"),
            IrregularReason::NestedList
        );
    }

    /// AISNAP-2: 対象外要素・非要素でも panic しない。
    #[test]
    fn aisnap_2_not_table_or_list() {
        assert_eq!(
            reason("<div>x</div>", "div"),
            IrregularReason::NotTableOrList
        );
        let doc = parse("<div>x</div>");
        let text = doc
            .first_child(select(&doc, "div"))
            .expect("テキストノードがある");
        assert_eq!(
            detect_regular_structure(&doc, text),
            TableDetection::Irregular(IrregularReason::NotTableOrList)
        );
        assert_eq!(
            detect_regular_structure(&doc, doc.root()),
            TableDetection::Irregular(IrregularReason::NotTableOrList)
        );
    }
}

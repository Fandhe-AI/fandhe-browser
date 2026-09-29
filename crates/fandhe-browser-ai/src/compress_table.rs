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
//! なしで使う想定である。データ行の圧縮 1 行表現は実装済み
//! （[`compress_rows`]・TASK-12.3・Issue #81。統合前のため呼び出し元なし）。
//! ヘッダの個別 ref 付与・`truncated_rows` 注記・統合は未実装
//! （実装済みを装わない。REPAIR-3）。
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
use crate::snapshot::name::{SKIPPED_SUBTREES, is_hidden_element};
use fandhe_browser_core::dom::{Document, NodeData, NodeId};

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

// ---- データ行の圧縮 1 行表現（TASK-12.3・Issue #81） ----

/// 圧縮表現に含めるデータ行の上限（`AISNAP-2` の「先頭 20 行」・PoC-4 の
/// `MAX_TABLE_ROWS`）。
pub const MAX_TABLE_ROWS: usize = 20;

/// 1 セルあたりの文字数上限（`char` 単位。PoC-4 の `slice(0, 40)`）。
/// 超えた場合は末尾に `…` を付ける。
pub const MAX_CELL_TEXT_CHARS: usize = 40;

/// セルの区切り（spec の想定スキーマ `"セル1 | セル2"`）。
const CELL_SEPARATOR: &str = " | ";

/// 1 セルの走査で訪問するノード数の上限（巨大・敵対的なセルでの CPU 消費対策）。
const MAX_CELL_SCAN_STEPS: usize = 1024;

/// 1 セルの走査で読む文字数（空白を含む）の上限。空白のみの巨大な単一テキスト
/// ノードはノード数・出力文字数の上限のどちらにも達しないため別枠で打ち切る。
const MAX_CELL_SCAN_CHARS: usize = 4096;

/// 圧縮した 1 データ行（`AISNAP-2`・`TASK-12.3`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompressedRow {
    /// 元の `tr`/`li` 要素（行 ref 付与側が再走査なしで使えるよう保持する）。
    pub row: NodeId,
    /// セル文字列を `" | "` で連結した 1 行表現。
    pub text: String,
    /// いずれかのセルが上限（文字数・走査ステップ）で切り詰められたか。
    pub truncated: bool,
}

/// [`compress_rows`] の結果。
///
/// `truncated_rows`（20 行を超えた件数）は TASK-12.4（Issue #82）がここへ
/// フィールドを足す想定で、`#[non_exhaustive]` により非破壊で拡張できる。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CompressedRows {
    /// 先頭から最大 [`MAX_TABLE_ROWS`] 行（文書順）。
    pub rows: Vec<CompressedRow>,
}

/// 規則的と判定済みの構造から、データ行を先頭 [`MAX_TABLE_ROWS`] 行まで
/// 1 行 1 文字列へ圧縮する（`AISNAP-2`・`TASK-12.3`・Issue #81）。
///
/// 呼び出し文脈: 現時点では呼び出し元は無い。TASK-12.5（Issue #83）の
/// `build_snapshot` 統合が、[`detect_regular_structure`] が `Regular` を返した
/// 表・一覧に対して呼ぶ想定。
///
/// - Table は行 `tr` 直下の `td`/`th` を文書順に `" | "` で連結する。空セルは
///   空文字列にして列位置を保つ。`colspan` 分の空セル埋めはしない（セル数基準）
/// - List は `li` 自身を 1 セルとする
/// - セル文字列は `hidden`/`aria-hidden="true"` 配下と `script`/`style`/
///   `noscript`/`template` を除き、HTML 空白と U+00A0 を 1 つの半角スペースへ
///   畳んで trim し、[`MAX_CELL_TEXT_CHARS`] 文字で切る。走査は
///   [`MAX_CELL_SCAN_STEPS`] ノード・[`MAX_CELL_SCAN_CHARS`] 文字で打ち切る。CSS による非表示はスタイル未評価
///   のため対象外
/// - ヘッダ行・`tfoot` 行は含めない（`footer_rows` の扱いは統合時に決める。保留）
/// - 20 行を超えた件数はここでは返さない（TASK-12.4・Issue #82）
/// - セル内に `" | "` が含まれると区切りが曖昧になる（エスケープの要否は
///   出力形式が決まる統合時に判断する既知の制約）
pub fn compress_rows(doc: &Document, structure: &RegularStructure) -> CompressedRows {
    let mut rows = Vec::with_capacity(structure.body_rows.len().min(MAX_TABLE_ROWS));
    for &row in structure.body_rows.iter().take(MAX_TABLE_ROWS) {
        let cells: Vec<NodeId> = match structure.kind {
            StructureKind::Table => doc
                .children(row)
                .filter(|&c| is_html(doc, c, "td") || is_html(doc, c, "th"))
                .take(MAX_COLUMNS)
                .collect(),
            // List と将来の種別は行要素自身を 1 セルとする。
            _ => vec![row],
        };
        let mut text = String::new();
        let mut truncated = false;
        for (i, cell) in cells.into_iter().enumerate() {
            if i > 0 {
                text.push_str(CELL_SEPARATOR);
            }
            let (cell_text, cut) = cell_text(doc, cell);
            text.push_str(&cell_text);
            truncated |= cut;
        }
        rows.push(CompressedRow {
            row,
            text,
            truncated,
        });
    }
    CompressedRows { rows }
}

/// HTML の空白（ASCII 空白）または U+00A0 か。
fn is_cell_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ' | '\u{A0}')
}

/// セルの表示テキストを有界に取り出す（戻り値は（文字列, 切り詰めたか））。
/// 明示スタックの DFS で再帰せず、`text_content` のような全文連結もしない。
fn cell_text(doc: &Document, cell: NodeId) -> (String, bool) {
    let mut out = String::new();
    let mut count = 0usize;
    let mut pending_space = false;
    let mut truncated = false;
    if doc.is_element(cell) && is_hidden_element(doc, cell) {
        return (out, false);
    }
    let mut stack = vec![cell];
    let mut steps = 0usize;
    let mut scanned = 0usize;
    'walk: while let Some(id) = stack.pop() {
        steps += 1;
        if steps > MAX_CELL_SCAN_STEPS {
            truncated = true;
            break;
        }
        match doc.node_data(id) {
            Some(NodeData::Text { contents }) => {
                for c in contents.chars() {
                    scanned += 1;
                    if scanned > MAX_CELL_SCAN_CHARS {
                        truncated = true;
                        break 'walk;
                    }
                    if is_cell_space(c) {
                        pending_space = !out.is_empty();
                        continue;
                    }
                    let needed = 1 + usize::from(pending_space);
                    if count + needed > MAX_CELL_TEXT_CHARS {
                        out.push('…');
                        truncated = true;
                        break 'walk;
                    }
                    if pending_space {
                        out.push(' ');
                        pending_space = false;
                    }
                    out.push(c);
                    count += needed;
                }
            }
            Some(NodeData::Element { .. }) => {
                if id != cell
                    && (SKIPPED_SUBTREES.iter().any(|n| is_html(doc, id, n))
                        || is_hidden_element(doc, id))
                {
                    continue;
                }
                let mut kids: Vec<NodeId> =
                    doc.children(id).take(MAX_CELL_SCAN_STEPS + 1).collect();
                if kids.len() > MAX_CELL_SCAN_STEPS {
                    kids.truncate(MAX_CELL_SCAN_STEPS);
                    truncated = true;
                }
                stack.extend(kids.into_iter().rev());
            }
            _ => {}
        }
    }
    (out, truncated)
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

    // ---- TASK-12.3（Issue #81）行圧縮 ----

    fn rows_of(html: &str, sel: &str) -> CompressedRows {
        let doc = parse(html);
        let s = match detect_regular_structure(&doc, select(&doc, sel)) {
            TableDetection::Regular(s) => s,
            other => panic!("規則的を期待したが {other:?}"),
        };
        compress_rows(&doc, &s)
    }

    fn texts(r: &CompressedRows) -> Vec<&str> {
        r.rows.iter().map(|x| x.text.as_str()).collect()
    }

    fn table_html(n: usize, thead: bool) -> String {
        let mut h = String::from("<table>");
        if thead {
            h.push_str("<thead><tr><th>a<th>b</thead><tbody>");
        }
        for i in 1..=n {
            h.push_str(&format!("<tr><td>r{i}a<td>r{i}b"));
        }
        h.push_str("</table>");
        h
    }

    /// AISNAP-2（TASK-12.3・Issue #81）: 21 行以上でも先頭 20 行のみ（受入基準）。
    #[test]
    fn aisnap_2_compress_rows_limits_to_20() {
        let doc = parse(&table_html(21, true));
        let s = match detect_regular_structure(&doc, select(&doc, "table")) {
            TableDetection::Regular(s) => s,
            other => panic!("{other:?}"),
        };
        let r = compress_rows(&doc, &s);
        assert_eq!(r.rows.len(), 20);
        assert_eq!(r.rows[0].text, "r1a | r1b");
        assert_eq!(r.rows[19].text, "r20a | r20b");
        assert_eq!(r.rows[5].row, s.body_rows[5]);
    }

    /// AISNAP-2（TASK-12.3）: thead なし 100 行・境界（20 行・3 行・ヘッダのみ）。
    #[test]
    fn aisnap_2_compress_rows_boundaries() {
        let r = rows_of(&table_html(100, false), "table");
        assert_eq!(r.rows.len(), 20);
        assert_eq!(r.rows[19].text, "r20a | r20b");
        assert_eq!(rows_of(&table_html(20, true), "table").rows.len(), 20);
        let r3 = rows_of(&table_html(3, true), "table");
        assert_eq!(texts(&r3), ["r1a | r1b", "r2a | r2b", "r3a | r3b"]);
        let h = rows_of("<table><tr><th>a<th>b</table>", "table");
        assert!(h.rows.is_empty());
    }

    /// AISNAP-2（TASK-12.3）: ヘッダ行・tfoot 行は含めない。
    #[test]
    fn aisnap_2_compress_rows_excludes_header_and_footer() {
        let r = rows_of(
            "<table><thead><tr><th>H1<th>H2</thead><tbody><tr><td>x<td>y</tbody>\
             <tfoot><tr><td>F1<td>F2</tfoot></table>",
            "table",
        );
        assert_eq!(texts(&r), ["x | y"]);
    }

    /// AISNAP-2（TASK-12.3）: ul/ol は li 1 つが 1 行。
    #[test]
    fn aisnap_2_compress_rows_list() {
        let mut ul = String::from("<ul>");
        for i in 1..=25 {
            ul.push_str(&format!("<li>item{i}"));
        }
        ul.push_str("</ul>");
        let r = rows_of(&ul, "ul");
        assert_eq!(r.rows.len(), 20);
        assert_eq!(r.rows[0].text, "item1");
        assert_eq!(r.rows[19].text, "item20");
        let ol = rows_of("<ol><li>a<li>b</ol>", "ol");
        assert_eq!(texts(&ol), ["a", "b"]);
    }

    /// AISNAP-2（TASK-12.3）: 空白の畳み込み・空セル・colspan 行。
    #[test]
    fn aisnap_2_compress_rows_whitespace_and_empty_cells() {
        let r = rows_of(
            "<table><tr><td>  foo <b>bar</b>\n baz </td><td>&nbsp;</td></tr></table>",
            "table",
        );
        assert_eq!(texts(&r), ["foo bar baz | "]);
        let e = rows_of("<table><tr><td>a<td><td>c</tr></table>", "table");
        assert_eq!(texts(&e), ["a |  | c"]);
        let c = rows_of(
            "<table><tr><td colspan=2>x<td>sub</tr><tr><td>p<td>q<td>r</table>",
            "table",
        );
        // 列数は colspan 込みで 3 のため規則的。セル数基準で 2 セルになる。
        assert_eq!(texts(&c), ["x | sub", "p | q | r"]);
    }

    /// AISNAP-2（TASK-12.3）: セル文字列の 40 文字切り詰め（多バイト含む）。
    #[test]
    fn aisnap_2_compress_rows_truncates_cell() {
        let long = "a".repeat(41);
        let exact = "b".repeat(40);
        let ja = "あ".repeat(45);
        let r = rows_of(
            &format!("<table><tr><td>{long}<tr><td>{exact}<tr><td>{ja}</table>"),
            "table",
        );
        assert_eq!(r.rows[0].text, format!("{}…", "a".repeat(40)));
        assert!(r.rows[0].truncated);
        assert_eq!(r.rows[1].text, exact);
        assert!(!r.rows[1].truncated);
        assert_eq!(r.rows[2].text, format!("{}…", "あ".repeat(40)));
        assert!(r.rows[2].truncated);
    }

    /// AISNAP-2（TASK-12.3）: hidden・script 等の除外。
    #[test]
    fn aisnap_2_compress_rows_skips_hidden_and_script() {
        let r = rows_of(
            "<table><tr><td>a<script>x</script><span hidden>h</span>\
             <span aria-hidden=true>z</span>b</td><td hidden>q</table>",
            "table",
        );
        assert_eq!(texts(&r), ["ab | "]);
    }

    /// AISNAP-2（TASK-12.3）: 走査ステップ上限で打ち切り panic しない。
    #[test]
    fn aisnap_2_compress_rows_scan_step_limit() {
        let spans = "<span></span>".repeat(2000);
        let r = rows_of(&format!("<table><tr><td>{spans}tail</table>"), "table");
        assert_eq!(r.rows[0].text, "");
        assert!(r.rows[0].truncated);
    }

    /// AISNAP-2（TASK-12.3）: 空白のみの巨大テキストノードも文字数上限で打ち切る。
    #[test]
    fn aisnap_2_compress_rows_scan_char_limit_whitespace() {
        let ws = " ".repeat(MAX_CELL_SCAN_CHARS * 4);
        let r = rows_of(&format!("<table><tr><td>{ws}tail</table>"), "table");
        assert_eq!(r.rows[0].text, "");
        assert!(r.rows[0].truncated);
    }
}

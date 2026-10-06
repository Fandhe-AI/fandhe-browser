//! `core::dom::Document` を走査して方式 B の `Snapshot`/`Node` ツリーを構築する
//! （`AISNAP-1`・`AISNAP-10`・TASK-11.7・Issue #76・`MS-2`）。
//!
//! 役割: 算出部品（[`super::role::compute_role`]・
//! [`super::name::compute_name_with_index`]・[`super::state::compute_state`]・
//! [`super::element_ref::RefAllocator`]・[`crate::data_leaf::classify_data_leaf`]）を
//! 1 回の DOM 走査へ統合する。
//!
//! 呼び出し文脈: 将来 `cli` 層の配線を経由して `cdp` の `/ai/snapshot`
//! （TASK-19・`AISNAP-6`/`AISNAP-7`）から呼ばれる。`ai` は `cdp` に依存しない。
//!
//! # 暫定仕様（実装済みを装わない。REPAIR-3）
//!
//! - 要素のみを `Node` にする。テキスト・コメント等は `Node` にせず、テキストは
//!   name 算出側が取り込む。
//! - 次の要素はサブツリーごと省略し、子孫の繰り上げはしない（DOM の親子関係と
//!   ネストを一致させるため）: `head`・`script`・`style`・`noscript`・`template`・
//!   `hidden` 属性または `aria-hidden="true"` の要素・`input[type=hidden]`。
//! - データ葉（表セル・価格クラス要素。`AISNAP-3`・TASK-13.3・Issue #88）は
//!   `Node::data_leaf`（圧縮表では `HeaderCell::data_leaf`）へ印を付けるだけで、role・ref・剪定・打ち切りには使わない。
//! - 規則的な `table`・`ul`・`ol`（[`crate::compress_table::detect_regular_structure`]
//!   が `Regular`。`AISNAP-2`・TASK-12.5・Issue #83）は子孫を展開せず、ヘッダ
//!   （個別 ref）・圧縮行・超過行数を持つ 1 ノード（[`Node::table`]）へ置き換える。
//!   圧縮では `Snapshot::truncated` を立てない（行の省略は `truncated_rows`、セルの
//!   切り詰めは `TableRow::truncated` で通知）。ヘッダ name の打ち切りのみ立てる。
//!   本文行（`tr`/`li`）内の `a[href]`・`button` は圧縮を拒否せず、圧縮行ごとに
//!   `TableRow::controls` へ ref・state 付きで保持する（セル内に複数あっても行あたり
//!   [`MAX_ROW_CONTROLS`]・表全体 [`MAX_TABLE_CONTROLS`] を上限に全件を文書順で保持し、
//!   超過は `TableRow::controls_truncated` で通知する。行は末尾まで走査し、優先保持
//!   （ページネーション・送信ボタン。`AISNAP-12`）は表全体で先に枠を確保する
//!   （優先要素だけで行・表の上限を超える表、および保持上限で省略される行に操作要素が
//!   ある表は圧縮せず展開する）。`AISNAP-2`・`AISNAP-13`・Issue #632）。
//!   ただし `tfoot` 行がある構造、またはそれ以外の操作要素（入力欄・`select`・`textarea`・
//!   `role`/`tabindex`/`onclick` 付き、ヘッダ行内のリンク等）を含む構造は圧縮せず
//!   通常どおり展開する（ref・state・フッターの可視情報を失わないため）。`img` の `alt`・`aria-label` 等テキスト以外に由来する
//!   accessible name を持つ子孫、または `caption` を含む構造も同様に展開する。`title` は
//!   展開時に name の出所になる要素（`generic`・`listitem` 等）の場合だけ展開を維持する。
//!   圧縮できない規則的構造で表示対象行が `MAX_TABLE_ROWS` 超のときは、優先保持
//!   （`AISNAP-12`・TASK-16.4。ページネーション・送信ボタン）で選ばれた行と ref・state 等を
//!   持つ行を展開のまま残し、それ以外の通常行は `Node::folded_rows` へ文字列で畳む（内容は省略しない）。
//!   検出は table/list ごとに子孫を走査するが、走査量はコンテナごと
//!   （`MAX_COMPRESS_SCAN_NODES`）と 1 回の構築全体（`MAX_TOTAL_COMPRESS_SCAN_NODES`）
//!   で有界とし、超過したコンテナは圧縮せず展開する。
//!   既知の制約（REPAIR-3）: ヘッダ行（`th`）に操作要素がある表（ソートリンク付き等）は
//!   圧縮せず展開する。表全体の上限は文書順に消費し、行をまたぐ優先保持はしない
//!   （暫定。#84・#108 で見直す）。リンクのテキストはセル文字列と control の name の双方に現れる。
//! - generic の折り畳み・`none`/`presentation` の除去・空ノードの剪定は行わない
//!   （トークン削減は後続タスク。`AISNAP-1`・`AISNAP-2`）。
//! - ルート以外の全要素に ref を振る（対話可能な要素への絞り込みは後続タスク）。
//! - 深さが [`MAX_TREE_DEPTH`] を超えるサブツリーは省略し `Snapshot::truncated` を立てる。
//! - いずれかの name が打ち切られた場合（文字数上限・子孫走査の上限・構築全体で共有する
//!   走査予算）も `Snapshot::truncated` を立てる。

use std::collections::HashSet;
use std::fmt;

use fandhe_browser_core::dom::{Children, Document, NodeId};

use crate::compress_table::{
    TableDetection, assign_header_refs, compress_row_list, compress_rows, detect_regular_structure,
    rows_to_fold,
};
use crate::data_leaf::{DataLeafKind, classify_data_leaf};
use crate::retention::priority_candidates;

use super::element_ref::{ElementRef, ElementSignature, RefAllocator, RefError};
use super::name::{
    MAX_TOTAL_CONTENT_STEPS, NameIndex, NameSource, SKIPPED_SUBTREES, compute_name_with_index,
    is_hidden_element, normalized_input_type,
};
use super::role::compute_role;
use super::state::{compute_state, is_html_element_named};
use super::{FoldedRow, HeaderCell, Node, RowControl, Snapshot, TableRow, TableSummary};

/// 構築するツリーの最大深さ（ルートを 0 とする）。
///
/// `Node` の derive した `Drop`/`PartialEq`/`Debug`/`Clone` は再帰するため、
/// 深さを有界にしてスタックオーバーフローを防ぐ（既定 2 MiB スレッドの debug
/// ビルドでも収まる保守的な値。実サイトの DOM はこれより十分浅い）。
pub const MAX_TREE_DEPTH: usize = 256;

/// ツリー構築の失敗（将来の拡張に備え `non_exhaustive`。REPAIR-4）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// ref の発行に失敗した。
    Ref(RefError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Ref(e) => write!(f, "failed to allocate element ref: {e}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<RefError> for SnapshotError {
    fn from(e: RefError) -> Self {
        SnapshotError::Ref(e)
    }
}

/// サブツリーごと `Snapshot` から省略する要素かを返す。
fn is_excluded(doc: &Document, id: NodeId) -> bool {
    is_html_element_named(doc, id, "head")
        || SKIPPED_SUBTREES
            .iter()
            .any(|name| is_html_element_named(doc, id, name))
        || is_hidden_element(doc, id)
        || (is_html_element_named(doc, id, "input") && normalized_input_type(doc, id) == "hidden")
}

/// 圧縮判定 1 コンテナあたりに走査してよい子孫ノード数の上限（`AISNAP-2`・TASK-12.5）。
///
/// 規則構造の検出（`detect_regular_structure`）と展開維持の判定は、外部 HTML の
/// 子孫を全走査する。圧縮行の 20 件上限より前に走るため、走査量を先に有界化する。
/// 超過したコンテナは圧縮せず通常の展開を維持する（従来の Snapshot と同じ結果）。
const MAX_COMPRESS_SCAN_NODES: usize = 20_000;

/// 1 回の `build_snapshot` で圧縮判定に使える走査量の総予算（ノード数）。
///
/// 入れ子の表・一覧が多数あっても、判定の総コストがコンテナ数に比例して
/// 膨らまないようにする。使い切った後の表・一覧は圧縮せず展開する。
const MAX_TOTAL_COMPRESS_SCAN_NODES: usize = 200_000;

/// `container` の子孫数が `limit` 以下かを反復走査で確かめ、その数を返す（超過は `None`）。
///
/// 積むノード数も `limit` 以内に抑える（子は残り予算 + 1 個までしか取らない）。
fn bounded_descendant_count(doc: &Document, container: NodeId, limit: usize) -> Option<usize> {
    let mut count = 0usize;
    let mut stack: Vec<NodeId> = doc.children(container).take(limit + 1).collect();
    while let Some(id) = stack.pop() {
        count += 1;
        if count > limit {
            return None;
        }
        let room = limit.saturating_sub(count + stack.len());
        stack.extend(doc.children(id).take(room + 1));
    }
    Some(count)
}

/// `container` の子孫要素がすべて相対深さ `max_rel` 以内（子を 1 とする）かを返す。
///
/// 圧縮は子孫を展開しないため、展開経路なら [`MAX_TREE_DEPTH`] で省略され
/// `truncated` が立つ子孫が行文字列へ現れてしまう。深さ超過を含む場合は圧縮せず
/// 通常の展開経路へ進める（深さ制限と省略通知の契約を維持。`AISNAP-2`）。
/// 呼び出し前の [`bounded_descendant_count`] で走査量は有界化済みで、反復で走査する。
fn subtree_fits_depth(doc: &Document, container: NodeId, max_rel: usize) -> bool {
    let mut stack: Vec<(NodeId, usize)> = doc.children(container).map(|c| (c, 1)).collect();
    while let Some((id, rel)) = stack.pop() {
        if !doc.is_element(id) {
            continue;
        }
        if rel > max_rel {
            return false;
        }
        stack.extend(doc.children(id).map(|c| (c, rel + 1)));
    }
    true
}

/// 規則的構造を圧縮してよいかを返す（`AISNAP-2`・TASK-12.5）。
///
/// 圧縮は子孫の展開（ref・state・accessible name の付与）を省略するため、
/// 次の場合は情報が失われる。このときは圧縮せず通常の展開を維持する
/// （ref・state を持つ行は展開を維持する。表示対象行が多い場合は優先保持
/// （AISNAP-12・TASK-16.4）の対象外で失われる内容のない通常行だけを 1 行文字列へ畳み、
/// 内容は省略しない）。
/// - `tfoot` 行がある（圧縮表現は本文行のみで、合計額などフッターの可視情報が消える）。
/// - 表・一覧の本文行の外（ヘッダ行等）に操作要素がある、または本文行の中に `a[href]`・
///   `button` 以外の操作要素（入力欄・`select`・`tabindex`/`onclick`/`role` 付き等）がある
///   （ref と state が消える）。本文行内の `a[href]`・`button` は拒否せず、
///   [`collect_row_controls`] が `TableRow::controls` へ保持する（Issue #632）。
/// - 表に `caption` がある（圧縮表現は caption を保持せず、表題が Snapshot から消える）。
/// - 見出し・入れ子の表 / 一覧・ランドマーク等の意味的な子孫がある（heading 等のノードと ref が消える）。
/// - 圧縮行はテキストノードのみを取り込むため、テキスト以外に由来する accessible name
///   （`img` の `alt`・`aria-label` 等）を持つ子孫がある。`title` は、展開時にその要素の
///   name の出所になる場合（`generic`・`listitem` 等、または子孫テキストが空の `td` 等）だけが
///   対象で、子孫テキストが name になる要素の `title` は拒否の理由にしない
///   （[`title_becomes_name`]。`AISNAP-2`・TASK-12・Issue #631）。
///   既知の制約: `span[title]` のような `generic` の `title` は展開で name に現れるため
///   展開を維持する。圧縮行へ `title` を表現する対応は後続候補（未実装）。
/// - 表セル以外のデータ葉（価格クラス要素等。`AISNAP-3`）の子孫がある（分類が消える）。
fn can_compress(
    doc: &Document,
    container: NodeId,
    structure: &crate::compress_table::RegularStructure,
    index: &NameIndex<'_>,
) -> bool {
    if !structure.footer_rows.is_empty() {
        return false;
    }
    // 本文行（`tr`/`li`）の内側の `a[href]`・`button` は行内操作要素として保持できる
    // （`TableRow::controls`。`AISNAP-2`・`AISNAP-13`・Issue #632）。ヘッダ行などの外側は
    // 格納先がなく黙って消えるため、従来どおり圧縮を拒否する。
    let body_rows: HashSet<NodeId> = structure.body_rows.iter().copied().collect();
    !subtree_has_lossy(doc, index, container, false, Some(&body_rows))
}

/// `root`（`include_root` なら自身を含む）配下に、1 行文字列へ畳むと失われる要素
/// （操作要素・意味的ノード・テキスト以外由来の name・非セルのデータ葉・`caption`）があるか。
///
/// 畳む行の判定（`AISNAP-12`・TASK-16.4。`controls_allowed_in` は `None`）と
/// [`can_compress`]（本文行の集合を渡す）で共有する。
/// `controls_allowed_in` が `Some` のとき、その集合の行の内側にある [`is_row_control`] な
/// 要素は操作要素であることを拒否の理由にしない（他の拒否条件は適用する。Issue #632）。
/// 走査量は呼び出し前の [`bounded_descendant_count`] で有界化済みの部分木に限る。
fn subtree_has_lossy(
    doc: &Document,
    index: &NameIndex<'_>,
    root: NodeId,
    include_root: bool,
    controls_allowed_in: Option<&HashSet<NodeId>>,
) -> bool {
    // 要素は (ノード, 許可された行の内側か) の組で積む。
    let mut stack: Vec<(NodeId, bool)> = if include_root {
        vec![(root, false)]
    } else {
        doc.children(root).map(|c| (c, false)).collect()
    };
    while let Some((id, in_row)) = stack.pop() {
        if !doc.is_element(id) || is_excluded(doc, id) {
            continue;
        }
        let interactive_blocks =
            is_interactive_element(doc, id) && !(in_row && is_row_control(doc, id));
        if is_html_element_named(doc, id, "caption")
            || interactive_blocks
            || is_semantic_structure_element(doc, id)
            || has_non_text_name_source(doc, id)
            || title_becomes_name(doc, index, id)
            || has_non_cell_data_leaf(doc, id)
        {
            return true;
        }
        let child_in_row = in_row || controls_allowed_in.is_some_and(|rows| rows.contains(&id));
        stack.extend(doc.children(id).map(|c| (c, child_in_row)));
    }
    false
}

/// 表セル以外のデータ葉（価格クラス要素等）か（`AISNAP-3`）。
///
/// 圧縮行はセルのテキストしか保持せず、子孫要素の `Node::data_leaf` 分類が消える。
/// `td`/`th`（`TableCell`）は圧縮の単位そのものなので除外する。`ProseClass`（地の文。
/// `AISNAP-11`・TASK-15.2）は直下テキストが圧縮行の文字列にそのまま保持され値が失われないため、
/// 除外して一覧全体の圧縮を妨げない（`span.text` を持つ通常の一覧の削減効果を維持する）。
/// それ以外の分類（`PriceClass` と将来追加される種別）は保守的に展開を維持する。
fn has_non_cell_data_leaf(doc: &Document, id: NodeId) -> bool {
    classify_data_leaf(doc, id)
        .is_some_and(|k| !matches!(k, DataLeafKind::TableCell | DataLeafKind::ProseClass))
}

/// 圧縮行の文字列で表せない意味的構造（見出し・入れ子の表 / 一覧・ランドマーク等）か。
///
/// 表・一覧の構造そのもの（`tr` / `td` / `th` / `li` 等）は圧縮の対象なので含めない。
/// これらが子孫にあると、展開時に出る heading 等のノードと ref が圧縮で消える。
fn is_semantic_structure_element(doc: &Document, id: NodeId) -> bool {
    [
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "table",
        "ul",
        "ol",
        "dl",
        "menu",
        "form",
        "fieldset",
        "figure",
        "nav",
        "header",
        "footer",
        "main",
        "aside",
        "section",
        "article",
        "dialog",
        "blockquote",
        "pre",
        "hr",
        "img",
        "progress",
        "meter",
        "output",
        "iframe",
        "object",
        "embed",
        "canvas",
        "svg",
        "math",
    ]
    .iter()
    .any(|n| is_html_element_named(doc, id, n))
}

/// テキストノード以外（属性）に由来する accessible name を持ち得る要素か。
fn has_non_text_name_source(doc: &Document, id: NodeId) -> bool {
    if is_html_element_named(doc, id, "img")
        && doc
            .attribute(id, "alt")
            .is_some_and(|v| !v.trim().is_empty())
    {
        return true;
    }
    ["aria-label", "aria-labelledby"]
        .iter()
        .any(|a| doc.attribute(id, a).is_some_and(|v| !v.trim().is_empty()))
}

/// 空でない `title` が、展開時にこの要素の accessible name になる（圧縮すると消える）か。
///
/// `title` を持つ要素に限り [`compute_name_with_index`] で name の出所を確認する
/// （`AISNAP-2`・`AISNAP-1`・TASK-12・Issue #631）。出所が `title` なら true。
/// 子孫テキストの超過（`truncated`）では `title` へフォールバックしない（name from content が
/// 打ち切られた場合は空の名前＋`truncated` で、`title` は name に現れない）ため、`truncated` は
/// 判定に使わず `source` だけで判断する（content 由来の `truncated` を title 由来と誤認しない）。
/// 判定は name 算出用の共有予算とは別枠の予算で行い（[`NameIndex::with_isolated_content_budget`]）、
/// 判定対象外の要素の name・`truncated` を変えない。対象は `title` 付き要素に限り有界。
/// 判定予算が枯渇して出所を確定できない場合は保守的に true（展開を維持）を返す。
/// 畳む行の判定（`AISNAP-12`）と [`can_compress`] の双方から呼ばれる。
fn title_becomes_name(doc: &Document, index: &NameIndex<'_>, id: NodeId) -> bool {
    if !doc
        .attribute(id, "title")
        .is_some_and(|v| !v.trim().is_empty())
    {
        return false;
    }
    // 判定は別枠の予算で行い、name 算出用の共有予算を消費しない。
    let name = index.with_isolated_content_budget(|| compute_name_with_index(doc, index, id));
    if name.source == NameSource::Title {
        return true;
    }
    // 判定予算の枯渇で名前の出所を確定できない場合は保守的に「title が name になる」
    // 扱いとし、展開を維持する（圧縮で accessible name を失わない）。
    name.source == NameSource::None && name.truncated && index.isolated_budget_exhausted()
}

/// 操作可能（フォーカス・クリック・入力の対象になり得る）要素か。
fn is_interactive_element(doc: &Document, id: NodeId) -> bool {
    if [
        "button", "input", "select", "textarea", "summary", "details",
    ]
    .iter()
    .any(|n| is_html_element_named(doc, id, n))
    {
        return true;
    }
    if ["a", "area"]
        .iter()
        .any(|n| is_html_element_named(doc, id, n))
        && doc.attribute(id, "href").is_some()
    {
        return true;
    }
    // `controls` 付きの音声・動画は再生 UI を持つ操作要素として扱う。
    if ["audio", "video"]
        .iter()
        .any(|n| is_html_element_named(doc, id, n))
        && doc.attribute(id, "controls").is_some()
    {
        return true;
    }
    ["tabindex", "contenteditable", "onclick", "role"]
        .iter()
        .any(|a| doc.attribute(id, a).is_some())
}

/// 圧縮行の中で個別の `ref` を付けて保持できる操作要素（`a[href]`・`button`）か。
///
/// [`is_interactive_element`] とは別の述語で、圧縮の拒否理由の判定を緩めるためだけに使う
/// （`AISNAP-2`・`AISNAP-13`・Issue #632）。`tabindex`・`contenteditable`・`onclick`・`role`
/// 付きの要素は、役割の上書きやスクリプト由来の挙動を `RowControl` で表せないため含めない。
fn is_row_control(doc: &Document, id: NodeId) -> bool {
    let is_link = is_html_element_named(doc, id, "a") && doc.attribute(id, "href").is_some();
    let is_button = is_html_element_named(doc, id, "button");
    (is_link || is_button)
        && !["tabindex", "contenteditable", "onclick", "role"]
            .iter()
            .any(|a| doc.attribute(id, a).is_some())
}

/// 圧縮行 1 行あたりに保持する操作要素の最大件数（`AISNAP-13`）。
///
/// 超過時は優先保持（`AISNAP-12`。ページネーション・送信ボタン）を先に残し、
/// `TableRow::controls_truncated` で通知する。暫定値で、#84 の測定で調整する。
pub const MAX_ROW_CONTROLS: usize = 8;

/// 1 つの圧縮表・一覧が保持する操作要素の合計上限（`AISNAP-13`）。
///
/// 優先保持の要素は表全体で先に枠を確保し、残りを文書順に配る（`apply_table_budget`）。
/// 優先要素だけで超える場合は圧縮せず展開する。暫定値（#84・#108 で見直す）。
pub const MAX_TABLE_CONTROLS: usize = 120;

/// 圧縮行 1 行ぶんの操作要素の採用計画（ref 発行前。`AISNAP-13`・`AISNAP-12`）。
struct RowControlPlan {
    /// 行上限（[`MAX_ROW_CONTROLS`]）適用後の `(要素, 優先保持か)`（文書順）。
    kept: Vec<(NodeId, bool)>,
    /// 行上限・表上限のいずれかで落とした要素があるか。
    truncated: bool,
}

/// 圧縮行 `row` の子孫から操作要素を文書順で全件集め、行上限を適用した計画を返す。
///
/// 走査は行末まで続ける（64 件目より後の `rel="next"`・送信ボタンも優先検出へ渡すため）。
/// 確保量は呼び出し元が `MAX_COMPRESS_SCAN_NODES` で有界化済みの子孫数以下。
///
/// 行内の優先要素（`AISNAP-12`）だけで [`MAX_ROW_CONTROLS`] を超える場合は優先要素を
/// 落とさずに表現できないため `None`（呼び出し元は圧縮を拒否して展開経路へ進む）。
fn plan_row_controls(doc: &Document, row: NodeId) -> Option<RowControlPlan> {
    // 先行順（文書順）の反復走査。子は逆順に積む。
    let mut candidates: Vec<NodeId> = Vec::new();
    let mut stack: Vec<NodeId> = doc.children(row).collect();
    stack.reverse();
    while let Some(id) = stack.pop() {
        if !doc.is_element(id) || is_excluded(doc, id) {
            continue;
        }
        if is_row_control(doc, id) {
            candidates.push(id);
        }
        let before = stack.len();
        stack.extend(doc.children(id));
        if let Some(added) = stack.get_mut(before..) {
            added.reverse();
        }
    }
    if candidates.is_empty() {
        return Some(RowControlPlan {
            kept: Vec::new(),
            truncated: false,
        });
    }
    let priority = priority_candidates(doc, &candidates);
    if priority.len() > MAX_ROW_CONTROLS {
        return None;
    }
    let mut is_priority = vec![false; candidates.len()];
    for p in &priority {
        if let Some(flag) = is_priority.get_mut(p.index) {
            *flag = true;
        }
    }
    if candidates.len() <= MAX_ROW_CONTROLS {
        let kept = candidates.iter().copied().zip(is_priority).collect();
        return Some(RowControlPlan {
            kept,
            truncated: false,
        });
    }
    // 優先要素（件数は MAX_ROW_CONTROLS 以下を確認済み）を先に全件確保し、
    // 残り枠を非優先要素へ文書順に配る（先頭枠の予約で優先要素を押し出さない）。
    let mut room = MAX_ROW_CONTROLS - priority.len();
    let kept: Vec<(NodeId, bool)> = candidates
        .iter()
        .copied()
        .zip(is_priority)
        .filter(|(_, pri)| {
            if *pri {
                true
            } else if room > 0 {
                room -= 1;
                true
            } else {
                false
            }
        })
        .collect();
    Some(RowControlPlan {
        kept,
        truncated: true,
    })
}

/// 保持上限で省略される本文行に操作要素（`a[href]`・`button`）が含まれるか。
///
/// 省略行の操作要素には ref を発行できず再特定できなくなるため、含まれる場合は呼び出し元が
/// 圧縮を拒否して展開経路へ進む（`AISNAP-2`・`AISNAP-13`・Issue #632）。
fn omitted_rows_have_controls(
    doc: &Document,
    structure: &crate::compress_table::RegularStructure,
    compressed: &crate::compress_table::CompressedRows,
) -> bool {
    if compressed.truncated_rows == 0 {
        return false;
    }
    let kept: HashSet<NodeId> = compressed.rows.iter().map(|r| r.row).collect();
    // compress_rows が省略し得るのは可視行のみ（非表示 tbody 配下の行は対象外）。
    crate::compress_table::visible_body_rows(doc, structure)
        .iter()
        .filter(|r| !kept.contains(r) && doc.is_element(**r) && !is_excluded(doc, **r))
        .any(|&r| plan_row_controls(doc, r).is_none_or(|p| !p.kept.is_empty()))
}

/// 表全体の上限（[`MAX_TABLE_CONTROLS`]）を行計画へ適用する（`AISNAP-12`・`AISNAP-13`）。
///
/// 優先保持（ページネーション・送信ボタン）の要素は表全体で先に枠を確保し、残り枠を
/// 非優先要素へ文書順に配る。優先要素だけで上限を超える場合は `None`（圧縮を拒否し、
/// 呼び出し元は展開経路へ進む）。
fn apply_table_budget(plans: &mut [RowControlPlan]) -> Option<()> {
    let priority_total: usize = plans
        .iter()
        .map(|p| p.kept.iter().filter(|(_, pri)| *pri).count())
        .sum();
    if priority_total > MAX_TABLE_CONTROLS {
        return None;
    }
    let mut room = MAX_TABLE_CONTROLS - priority_total;
    for plan in plans.iter_mut() {
        let before = plan.kept.len();
        plan.kept.retain(|(_, pri)| {
            if *pri {
                true
            } else if room > 0 {
                room -= 1;
                true
            } else {
                false
            }
        });
        if plan.kept.len() < before {
            plan.truncated = true;
        }
    }
    Some(())
}

/// 計画済みの操作要素へ ref を発行し [`RowControl`] 列にする（`AISNAP-13`・`AISNAP-10`）。
///
/// `build_snapshot` の圧縮経路から呼ばれる。ref は展開時と同じ規則
/// （role・name・`discriminator`・コンテナ scope）で発行する。
fn build_row_controls(
    doc: &Document,
    index: &NameIndex<'_>,
    refs: &mut RefAllocator,
    container_ref: ElementRef,
    plan: &RowControlPlan,
    name_truncated: &mut bool,
) -> Result<(Vec<RowControl>, bool), SnapshotError> {
    let candidates: Vec<NodeId> = plan.kept.iter().map(|(id, _)| *id).collect();
    let truncated = plan.truncated;
    let mut controls = Vec::with_capacity(candidates.len());
    for id in candidates {
        let role = compute_role(doc, id)
            .map(|r| r.as_str().to_string())
            .unwrap_or_else(|| {
                if is_html_element_named(doc, id, "a") {
                    "link".to_string()
                } else {
                    "button".to_string()
                }
            });
        let accessible = compute_name_with_index(doc, index, id);
        *name_truncated |= accessible.truncated;
        let mut sig = ElementSignature::new(&role, &accessible.text).with_scope(container_ref);
        if let Some(d) = discriminator(doc, id) {
            sig = sig.with_discriminator(d);
        }
        let elem_ref = refs.allocate_signature(&sig)?;
        controls.push(
            RowControl::new(role, accessible.text, elem_ref.to_ref_string())
                .with_state(compute_state(doc, id)),
        );
    }
    Ok((controls, truncated))
}

/// ref の識別属性。`id` → フォーム部品の `name` → `a`/`area` の `href` の順に、
/// 最初に空でないものを返す。値はダイジェストにのみ使われ ref 文字列には入らない。
fn discriminator(doc: &Document, id: NodeId) -> Option<&str> {
    if let Some(v) = doc.attribute(id, "id").filter(|s| !s.is_empty()) {
        return Some(v);
    }
    if ["input", "select", "textarea", "button"]
        .iter()
        .any(|n| is_html_element_named(doc, id, n))
        && let Some(v) = doc.attribute(id, "name").filter(|s| !s.is_empty())
    {
        return Some(v);
    }
    if ["a", "area"]
        .iter()
        .any(|n| is_html_element_named(doc, id, n))
    {
        return doc.attribute(id, "href").filter(|s| !s.is_empty());
    }
    None
}

/// 走査中の 1 要素分の状態（明示スタックの要素）。
struct Frame<'a> {
    node: Node,
    children: Children<'a>,
    elem_ref: Option<ElementRef>,
    depth: usize,
}

/// `doc` から `Snapshot` を構築する（`AISNAP-1`・`AISNAP-10`）。
///
/// 走査は明示スタックの反復（再帰なし）で、ref は先行順の文書順に発行する。
/// 深さ [`MAX_TREE_DEPTH`] を超えるサブツリーは省略され `truncated` が立つ。
///
/// # エラー
///
/// ref の発行に失敗した場合 [`SnapshotError::Ref`]。
pub fn build_snapshot(doc: &Document) -> Result<Snapshot, SnapshotError> {
    let index = NameIndex::build(doc).with_content_budget(MAX_TOTAL_CONTENT_STEPS);
    let mut refs = RefAllocator::new();
    let mut truncated = false;
    let mut compress_budget = MAX_TOTAL_COMPRESS_SCAN_NODES;
    // 展開する表・一覧で `Node::folded_rows` へ畳んだ行。主ループは子ノード化しない。
    let mut folded: HashSet<NodeId> = HashSet::new();

    let root_id = doc.root();
    let root_role = compute_role(doc, root_id)
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|| "document".to_string());
    let root_name = compute_name_with_index(doc, &index, root_id);
    truncated |= root_name.truncated;
    let root_name = root_name.text;
    let mut stack: Vec<Frame<'_>> = vec![Frame {
        node: Node::new(root_role, root_name),
        children: doc.children(root_id),
        elem_ref: None,
        depth: 0,
    }];

    loop {
        let Some(top) = stack.last_mut() else {
            // 到達しない（ルートは最後まで残る）が、panic せず空ツリーを返す。
            return Ok(Snapshot::new(Node::new("document", "")).with_truncated(truncated));
        };
        let Some(child) = top.children.next() else {
            let Some(done) = stack.pop() else {
                continue;
            };
            match stack.last_mut() {
                Some(parent) => parent.node.push_child(done.node),
                None => return Ok(Snapshot::new(done.node).with_truncated(truncated)),
            }
            continue;
        };
        if !doc.is_element(child) || is_excluded(doc, child) || folded.contains(&child) {
            continue;
        }
        let (parent_depth, parent_ref) = (top.depth, top.elem_ref);
        let depth = parent_depth + 1;
        if depth > MAX_TREE_DEPTH {
            truncated = true;
            continue;
        }
        let Some(role) = compute_role(doc, child) else {
            continue;
        };
        let role = role.as_str();
        let accessible = compute_name_with_index(doc, &index, child);
        // 名前の打ち切り（文字数・走査量の上限）も Snapshot 全体へ通知する。
        truncated |= accessible.truncated;
        let name = accessible.text;
        let mut sig = ElementSignature::new(role, &name);
        if let Some(d) = discriminator(doc, child) {
            sig = sig.with_discriminator(d);
        }
        if let Some(scope) = parent_ref {
            sig = sig.with_scope(scope);
        }
        let elem_ref = refs.allocate_signature(&sig)?;
        let mut node = Node::new(role, name)
            .with_ref(elem_ref.to_ref_string())
            .with_state(compute_state(doc, child));
        node.data_leaf = classify_data_leaf(doc, child);
        // 規則的な表・一覧は子孫を展開せず 1 ノードへ圧縮する（AISNAP-2・TASK-12.5）。
        let compressible_kind = ["table", "ul", "ol"]
            .iter()
            .any(|n| is_html_element_named(doc, child, n));
        // 走査量の予算内に収まるコンテナだけを圧縮判定へ進める（超過時は展開を維持）。
        // 超過した走査も予算を消費し、巨大な入れ子の反復で総コストが膨らまないようにする。
        let within_budget = compressible_kind && {
            let limit = MAX_COMPRESS_SCAN_NODES.min(compress_budget);
            let scanned = bounded_descendant_count(doc, child, limit);
            compress_budget = compress_budget.saturating_sub(scanned.unwrap_or(limit));
            scanned.is_some() && subtree_fits_depth(doc, child, MAX_TREE_DEPTH - depth)
        };
        let regular = if within_budget {
            match detect_regular_structure(doc, child) {
                TableDetection::Regular(structure) => Some(structure),
                _ => None,
            }
        } else {
            None
        };
        // 圧縮できない（操作要素等を含む）規則的構造は展開を維持する。ref・state を持つ行、
        // 優先保持（AISNAP-12・TASK-16.4）で選ばれた行は展開のまま残し、それ以外で
        // 失われる内容のない通常行は件数を省略せず 1 行文字列へ畳む（`Node::folded_rows`）。
        // 行の選択はページネーション・送信ボタンの検出（`priority_candidates`）を含む。
        // 行内操作要素の採用計画で表全体の優先枠を確保できない場合は圧縮を拒否する。
        let (compressible, expanded_structure) = match regular {
            Some(s) if can_compress(doc, child, &s, &index) => {
                let compressed = compress_rows(doc, &s);
                let plans: Option<Vec<RowControlPlan>> = compressed
                    .rows
                    .iter()
                    .map(|r| plan_row_controls(doc, r.row))
                    .collect();
                let planned = plans.filter(|_| !omitted_rows_have_controls(doc, &s, &compressed));
                match planned {
                    Some(mut plans) => match apply_table_budget(&mut plans) {
                        Some(()) => (Some((s, compressed, plans)), None),
                        None => (None, Some(s)),
                    },
                    None => (None, Some(s)),
                }
            }
            other => (None, other),
        };
        if let Some(structure) = expanded_structure {
            let fold_rows = rows_to_fold(doc, &structure, |row| {
                subtree_has_lossy(doc, &index, row, true, None)
            });
            let ids: Vec<NodeId> = fold_rows.iter().map(|&(_, id)| id).collect();
            node.folded_rows = compress_row_list(doc, structure.kind, &ids)
                .into_iter()
                .zip(fold_rows.iter())
                .map(|(r, &(index, _))| FoldedRow::new(index, r.text, r.truncated))
                .collect();
            folded.extend(ids);
        }
        if let Some((structure, compressed, plans)) = compressible {
            let headers = assign_header_refs(doc, &index, &structure, elem_ref, &mut refs)?;
            truncated |= headers.iter().any(|h| h.name_truncated);
            // 保持された行の操作要素を文書順に収集する（省略行に操作要素がある表は上で圧縮を拒否済み）。
            let mut rows = Vec::with_capacity(compressed.rows.len());
            for (r, plan) in compressed.rows.into_iter().zip(plans.iter()) {
                let (controls, controls_truncated) =
                    build_row_controls(doc, &index, &mut refs, elem_ref, plan, &mut truncated)?;
                rows.push(
                    TableRow::new(r.text, r.truncated)
                        .with_controls(controls)
                        .with_controls_truncated(controls_truncated),
                );
            }
            node.table = Some(TableSummary::new(
                headers
                    .into_iter()
                    .map(|h| {
                        // 展開時の Node と同じ規則で元セルを分類する（値の決め打ちはしない）。
                        let cell = HeaderCell::new(h.role, h.name, h.elem_ref.to_ref_string());
                        match classify_data_leaf(doc, h.cell) {
                            Some(kind) => cell.with_data_leaf(kind),
                            None => cell,
                        }
                    })
                    .collect(),
                rows,
                compressed.truncated_rows,
            ));
            if let Some(parent) = stack.last_mut() {
                parent.node.push_child(node);
            }
            continue;
        }
        stack.push(Frame {
            node,
            children: doc.children(child),
            elem_ref: Some(elem_ref),
            depth,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_ROW_CONTROLS, MAX_TABLE_CONTROLS};
    use super::{MAX_TREE_DEPTH, build_snapshot};
    use crate::data_leaf::DataLeafKind;
    use crate::snapshot::{CheckedState, Node, Snapshot};
    use fandhe_browser_core::parse::{ParseOptions, parse_document};

    fn snap(html: &str) -> Snapshot {
        let parsed =
            parse_document(html, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        build_snapshot(&parsed.document).expect("構築は成功する")
    }

    fn child(node: &Node, i: usize) -> &Node {
        node.children.get(i).expect("子ノードが存在する")
    }

    /// 全ノードを先行順で集める（反復。テスト内でも再帰しない）。
    fn all_nodes(root: &Node) -> Vec<&Node> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            out.push(n);
            stack.extend(n.children.iter().rev());
        }
        out
    }

    fn max_depth(root: &Node) -> usize {
        let mut best = 0;
        let mut stack = vec![(root, 0usize)];
        while let Some((n, d)) = stack.pop() {
            best = best.max(d);
            stack.extend(n.children.iter().map(|c| (c, d + 1)));
        }
        best
    }

    const PAGE: &str = "<html><head><title>Example Domain</title></head><body><div>\
        <h1>Example Domain</h1><p><a href=\"https://iana.org\">More information...</a></p>\
        </div></body></html>";

    /// AISNAP-1: 受入基準。ネストが DOM の親子関係と一致する。
    #[test]
    fn aisnap_1_build_snapshot_nesting_matches_dom() {
        let s = snap(PAGE);
        assert_eq!(s.tree.role, "document");
        assert_eq!(s.tree.name, "Example Domain");
        assert_eq!(s.tree.r#ref, None);
        assert!(!s.truncated);
        assert_eq!(s.tree.children.len(), 1);
        let html = child(&s.tree, 0);
        assert_eq!(html.role, "generic");
        // head は省略され body のみ。
        assert_eq!(html.children.len(), 1);
        let body = child(html, 0);
        assert_eq!(body.role, "generic");
        let div = child(body, 0);
        assert_eq!(div.role, "generic");
        assert_eq!(div.children.len(), 2);
        let h1 = child(div, 0);
        assert_eq!(
            (h1.role.as_str(), h1.name.as_str()),
            ("heading", "Example Domain")
        );
        let p = child(div, 1);
        assert_eq!(p.role, "generic");
        assert_eq!(p.children.len(), 1);
        let a = child(p, 0);
        assert_eq!(
            (a.role.as_str(), a.name.as_str()),
            ("link", "More information...")
        );
        assert!(a.children.is_empty());
    }

    /// AISNAP-1: 描画されない要素はサブツリーごと省略され、兄弟の順序は保たれる。
    #[test]
    fn aisnap_1_build_snapshot_excludes_non_rendered_subtrees() {
        let s = snap(
            "<body><button>A</button><script>x</script><style>p{}</style>\
             <template><b>t</b></template><noscript><i>n</i></noscript>\
             <div hidden><button>H</button></div><div aria-hidden=\"true\"><button>AH</button></div>\
             <input type=\"hidden\" name=\"csrf\" value=\"secret\"><button>B</button></body>",
        );
        let body = child(child(&s.tree, 0), 0);
        let names: Vec<&str> = body.children.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
        let all = all_nodes(&s.tree);
        assert!(all.iter().all(|n| n.name != "H" && n.name != "AH"));
    }

    /// AISNAP-1: テキスト・コメントは Node にならない。
    #[test]
    fn aisnap_1_build_snapshot_ignores_text_and_comments() {
        let s = snap("<body><p>hello<!-- c --> world</p></body>");
        let p = child(child(child(&s.tree, 0), 0), 0);
        assert_eq!(p.children.len(), 0);
    }

    /// AISNAP-1: state の配線。
    #[test]
    fn aisnap_1_build_snapshot_wires_state() {
        let s = snap("<body><input type=\"checkbox\" checked><button disabled>x</button></body>");
        let body = child(child(&s.tree, 0), 0);
        let cb = child(body, 0);
        assert_eq!(cb.role, "checkbox");
        assert_eq!(cb.state.checked, Some(CheckedState::Checked));
        let btn = child(body, 1);
        assert!(btn.state.disabled);
    }

    /// AISNAP-1: name の配線とルート name の既定値。
    #[test]
    fn aisnap_1_build_snapshot_wires_name() {
        let s = snap("<body><button>送信</button></body>");
        assert_eq!(s.tree.name, "");
        assert_eq!(child(child(child(&s.tree, 0), 0), 0).name, "送信");
    }

    /// AISNAP-3: 表セル・価格クラス要素にだけ data_leaf が付く（TASK-13.3・Issue #88）。
    #[test]
    fn aisnap_3_build_snapshot_reflects_data_leaf() {
        let s = snap(
            "<body><table><tr><th>名前</th><td rowspan=\"2\">80</td><td><a href=\"/x\">詳細</a></td></tr></table>\
             <div class=\"price\"><span class=\"price\">1</span></div><span>plain</span></body>",
        );
        assert_eq!(s.tree.data_leaf, None);
        let kinds: Vec<(&str, &str, Option<DataLeafKind>)> = all_nodes(&s.tree)
            .into_iter()
            .map(|n| (n.role.as_str(), n.name.as_str(), n.data_leaf))
            .collect();
        let tc = Some(DataLeafKind::TableCell);
        assert!(kinds.contains(&("columnheader", "名前", tc)), "{kinds:?}");
        assert!(kinds.contains(&("cell", "80", tc)), "{kinds:?}");
        assert!(kinds.contains(&("cell", "詳細", tc)), "{kinds:?}");
        assert!(kinds.contains(&("link", "詳細", None)), "{kinds:?}");
        assert!(
            kinds.contains(&("generic", "", Some(DataLeafKind::PriceClass))),
            "{kinds:?}"
        );
        assert!(kinds.contains(&("generic", "", None)), "{kinds:?}");
        let price_leaves = kinds
            .iter()
            .filter(|k| k.2 == Some(DataLeafKind::PriceClass))
            .count();
        assert_eq!(price_leaves, 1);
        // 外側の price div は子要素を持つのでデータ葉ではない。
        let outer = all_nodes(&s.tree)
            .into_iter()
            .find(|n| {
                n.role == "generic"
                    && n.children.len() == 1
                    && n.children.iter().all(|c| c.data_leaf.is_some())
            })
            .expect("外側の div がある");
        assert_eq!(outer.data_leaf, None);
    }

    /// AISNAP-3/AISNAP-10: data_leaf の付与は ref を変えない（TASK-13.3・Issue #88）。
    #[test]
    fn aisnap_3_data_leaf_does_not_change_refs() {
        let with_class = snap("<body><span class=\"price\">1</span></body>");
        let without = snap("<body><span class=\"plain\">1</span></body>");
        let span = |s: &Snapshot| child(child(child(&s.tree, 0), 0), 0).clone();
        assert_eq!(span(&with_class).r#ref, span(&without).r#ref);
        assert_eq!(span(&with_class).data_leaf, Some(DataLeafKind::PriceClass));
        assert_eq!(span(&without).data_leaf, None);
    }

    fn is_ref_shape(r: &str) -> bool {
        let Some(rest) = r.strip_prefix('e') else {
            return false;
        };
        let hex: String = rest.chars().take(16).collect();
        hex.len() == 16
            && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
            && rest
                .chars()
                .skip(16)
                .all(|c| matches!(c, '0'..='9' | 'a'..='z' | '-'))
    }

    /// AISNAP-10: ref の形式・一意性・決定性。
    #[test]
    fn aisnap_10_refs_are_well_formed_unique_and_deterministic() {
        let s1 = snap(PAGE);
        let s2 = snap(PAGE);
        assert_eq!(s1, s2);
        let refs: Vec<&str> = all_nodes(&s1.tree)
            .into_iter()
            .skip(1)
            .map(|n| n.r#ref.as_deref().expect("非ルートは ref を持つ"))
            .collect();
        assert_eq!(refs.len(), 6);
        assert!(refs.iter().all(|r| is_ref_shape(r)));
        let mut sorted = refs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), refs.len());
    }

    /// AISNAP-10: 同名ボタンは別 ref。id 付きは前方への挿入で ref を保つ。
    #[test]
    fn aisnap_10_same_name_elements_get_distinct_and_stable_refs() {
        let s = snap("<body><div><button>OK</button><button>OK</button></div></body>");
        let div = child(child(child(&s.tree, 0), 0), 0);
        assert_ne!(child(div, 0).r#ref, child(div, 1).r#ref);

        let before = snap("<body><div><button id=\"b\">OK</button></div></body>");
        let after = snap(
            "<body><div><button id=\"a\">OK</button><button id=\"b\">OK</button></div></body>",
        );
        let b_before = child(child(child(child(&before.tree, 0), 0), 0), 0);
        let b_after = child(child(child(child(&after.tree, 0), 0), 0), 1);
        assert_eq!(b_before.r#ref, b_after.r#ref);
    }

    /// AISNAP-10: 無関係な兄弟の挿入で heading・link の ref が変わらない。
    #[test]
    fn aisnap_10_refs_stable_under_unrelated_insertion() {
        let with_notice = PAGE.replace("<body>", "<body><div role=\"alert\">Notice</div>");
        let a = snap(PAGE);
        let b = snap(&with_notice);
        let pick = |s: &Snapshot, role: &str| {
            all_nodes(&s.tree)
                .into_iter()
                .find(|n| n.role == role)
                .and_then(|n| n.r#ref.clone())
        };
        assert!(pick(&a, "heading").is_some());
        assert_eq!(pick(&a, "heading"), pick(&b, "heading"));
        assert_eq!(pick(&a, "link"), pick(&b, "link"));
    }

    /// AISNAP-1: 深い入れ子でも上限で打ち切り、スタックオーバーフローしない。
    #[test]
    fn aisnap_1_build_snapshot_caps_depth_without_stack_overflow() {
        let depth = 10_000;
        let html = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
        let options = ParseOptions::default();
        let parsed = parse_document(&html, &options).expect("パースは成功する");
        let s = build_snapshot(&parsed.document).expect("構築は成功する");
        assert!(s.truncated);
        assert!(max_depth(&s.tree) <= MAX_TREE_DEPTH);
        let s2 = s.clone();
        assert_eq!(s, s2);
        drop(s2);
        assert!(!snap(PAGE).truncated);
    }

    /// AISNAP-1: name の打ち切りが Snapshot::truncated に伝わる。
    #[test]
    fn aisnap_1_build_snapshot_propagates_name_truncation() {
        let long = "あ".repeat(500);
        let s = snap(&format!("<body><button>{long}</button></body>"));
        assert!(s.truncated);
        let btn = child(child(child(&s.tree, 0), 0), 0);
        assert!(btn.name.chars().count() <= 120);
    }

    /// AISNAP-1: 入れ子の heading でも構築全体の走査予算で有界に完了し、
    /// 予算超過は truncated で通知される。
    #[test]
    fn aisnap_1_build_snapshot_shared_name_budget_bounded() {
        let depth = 200;
        let html = format!(
            "<body>{}x{}</body>",
            "<div role=\"heading\">".repeat(depth),
            "</div>".repeat(depth)
        );
        let s = snap(&html);
        assert!(!s.truncated);
        // 入れ子 heading の塊を多数並べて共有予算を使い切らせる。
        let group = format!(
            "{}{}{}",
            "<div role=\"heading\">".repeat(100),
            "<i></i>".repeat(800),
            "</div>".repeat(100)
        );
        let one = snap(&format!("<body>{group}</body>"));
        assert!(!one.truncated);
        let s = snap(&format!("<body>{}</body>", group.repeat(60)));
        assert!(s.truncated);
    }

    fn has_table_summary(s: &Snapshot) -> bool {
        all_nodes(&s.tree).iter().any(|n| n.table.is_some())
    }

    /// AISNAP-2: 一覧の圧縮が有効な基準ケース（テキストのみの li）。
    #[test]
    fn aisnap_2_plain_list_is_compressed() {
        let s = snap("<body><ul><li>alpha</li><li>beta</li></ul></body>");
        assert!(has_table_summary(&s));
    }

    /// AISNAP-2: img の alt（accessible name）は行文字列へ入らないため圧縮せず展開する。
    #[test]
    fn aisnap_2_list_with_img_alt_is_expanded() {
        let s = snap(
            "<body><ul><li><img src=\"a.png\" alt=\"Apple logo\"></li><li>beta</li></ul></body>",
        );
        assert!(!has_table_summary(&s));
        assert!(all_nodes(&s.tree).iter().any(|n| n.name == "Apple logo"));
    }

    /// AISNAP-2: aria-label を持つ子孫を含む表は圧縮せず展開する。
    #[test]
    fn aisnap_2_table_with_aria_label_is_expanded() {
        let s = snap(
            "<body><table><tr><th>A</th></tr><tr><td><span aria-label=\"Total\">5</span></td></tr></table></body>",
        );
        assert!(!has_table_summary(&s));
    }

    /// AISNAP-2: caption を持つ表は圧縮せず展開し、従来の展開結果を維持する（圧縮表現は caption を保持しないため）。
    /// caption の accessible name 化は name.rs の担当範囲で本 Issue の対象外。
    #[test]
    fn aisnap_2_table_with_caption_is_expanded() {
        let s = snap("<body><table><caption>売上</caption><tr><td>100</td></tr></table></body>");
        assert!(!has_table_summary(&s));
    }

    /// AISNAP-2: controls 付きの audio / video を含む一覧は操作要素があるため圧縮せず展開する。
    #[test]
    fn aisnap_2_list_with_media_controls_is_expanded() {
        for tag in ["audio", "video"] {
            let s = snap(&format!(
                "<body><ul><li><{tag} controls src=\"a.mp4\"></{tag}></li><li>beta</li></ul></body>"
            ));
            assert!(!has_table_summary(&s), "{tag} with controls must expand");
        }
    }

    /// AISNAP-2: 見出し・入れ子の表 / 一覧を含む構造は圧縮せず展開し、heading ノードを保持する。
    #[test]
    fn aisnap_2_structure_with_semantic_descendant_is_expanded() {
        let s = snap("<body><ul><li><h2>見出し</h2>本文</li><li>beta</li></ul></body>");
        assert!(!has_table_summary(&s));
        assert!(all_nodes(&s.tree).iter().any(|n| n.role == "heading"));
        // 入れ子の表・一覧では外側のコンテナが圧縮されない（内側は単独で判定される）。
        for html in [
            "<body><ul><li><ul><li>x</li></ul></li><li>y</li></ul></body>",
            "<body><table><tr><td><table><tr><td>x</td></tr></table></td></tr><tr><td>y</td></tr></table></body>",
        ] {
            let s = snap(html);
            let outer = all_nodes(&s.tree)
                .into_iter()
                .find(|n| n.role == "list" || n.role == "table")
                .expect("outer container");
            assert!(outer.table.is_none(), "{html}");
        }
    }

    /// AISNAP-3 / AISNAP-2: 価格クラスのデータ葉を子孫に持つ表・一覧は圧縮せず展開し、
    /// `DataLeafKind::PriceClass` を保持する（圧縮で分類が消える後退の回帰テスト）。
    #[test]
    fn aisnap_3_price_class_descendant_is_not_lost_by_compression() {
        for html in [
            "<body><ul><li><span class=\"price\">100</span></li><li>beta</li></ul></body>",
            "<body><ul><li class=\"item-amount\">100</li><li>beta</li></ul></body>",
            "<body><table><tr><th>A</th></tr><tr><td><b class=\"Currency\">JPY</b></td></tr></table></body>",
        ] {
            let s = snap(html);
            assert!(!has_table_summary(&s), "{html}");
            assert!(
                all_nodes(&s.tree)
                    .iter()
                    .any(|n| n.data_leaf == Some(DataLeafKind::PriceClass)),
                "{html}"
            );
        }
    }

    /// AISNAP-11 / AISNAP-2: 地の文クラス（`span.text` 等）の子孫は圧縮を拒否せず、
    /// テキストは圧縮行に保持される（TASK-15.2・Issue #100）。
    #[test]
    fn aisnap_11_prose_class_descendant_does_not_block_compression() {
        let s = snap(
            "<body><ul><li><span class=\"text\">alpha quote</span></li><li><span class=\"text\">beta quote</span></li></ul></body>",
        );
        assert!(has_table_summary(&s));
        let table = all_nodes(&s.tree)
            .into_iter()
            .find_map(|n| n.table.as_ref())
            .expect("圧縮される");
        let text = format!("{table:?}");
        assert!(text.contains("alpha quote"));
        assert!(text.contains("beta quote"));
    }

    /// AISNAP-2（Issue #631）: 子孫テキストが name になる td/tr の title は展開でも name に
    /// 出ないため、title だけを理由に圧縮を拒否しない。
    #[test]
    fn aisnap_2_table_with_titled_cell_text_is_compressed() {
        let s = snap(
            "<body><table><tr><th>A</th></tr><tr title=\"r\"><td title=\"tip\">80</td></tr></table></body>",
        );
        assert!(has_table_summary(&s));
        let table = all_nodes(&s.tree)
            .into_iter()
            .find_map(|n| n.table.as_ref())
            .expect("圧縮される");
        let text = format!("{table:?}");
        assert!(text.contains("80"));
        assert!(!text.contains("tip"));
    }

    /// AISNAP-2（Issue #631）: 子孫テキストが name 文字数上限を超える title 付き td でも、
    /// name は content 由来（title は name に出ない）なので圧縮を拒否しない。
    #[test]
    fn aisnap_2_titled_cell_with_overlong_text_is_still_compressed() {
        let long = "x".repeat(400);
        let html = format!(
            "<body><table><tr><th>A</th></tr><tr><td title=\"tip\">{long}</td></tr></table></body>"
        );
        let s = snap(&html);
        assert!(has_table_summary(&s));
    }

    /// AISNAP-2（Issue #631）: 判定予算が枯渇して name の出所を確定できない場合は
    /// 保守的に「title が name になる」扱い（展開維持）にし、名前を失わない。
    #[test]
    fn aisnap_2_exhausted_judge_budget_keeps_expansion() {
        use super::title_becomes_name;
        use crate::snapshot::name::NameIndex;

        let html = r#"<table><tr><td id="c" title="tip">x</td></tr></table>"#;
        let parsed =
            parse_document(html, &ParseOptions::default()).expect("テスト入力は必ず成功する");
        let doc = parsed.document;
        let id = fandhe_browser_core::query::query_selector_str(&doc, doc.root(), "td#c")
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる");
        // 予算が十分なら name は子孫テキスト由来で title は出ない（false）。
        let ample = NameIndex::build(&doc).with_content_budget(1024);
        assert!(!title_becomes_name(&doc, &ample, id));
        // 予算 0 では出所を確定できないため保守的に true。
        let zero = NameIndex::build(&doc).with_content_budget(0);
        assert!(title_becomes_name(&doc, &zero, id));
    }

    /// AISNAP-2（Issue #631）: generic（span）の title は展開で name になるため展開を維持する。
    #[test]
    fn aisnap_2_titled_generic_span_keeps_expansion() {
        let s = snap(
            "<body><table><tr><th>A</th></tr><tr><td><span title=\"2026-09-30T12:00\">2 hours ago</span></td></tr></table></body>",
        );
        assert!(!has_table_summary(&s));
        assert!(
            all_nodes(&s.tree)
                .iter()
                .any(|n| n.role == "generic" && n.name == "2026-09-30T12:00")
        );
    }

    /// AISNAP-2（Issue #631）: listitem の title は name になるため展開を維持する。
    #[test]
    fn aisnap_2_titled_list_item_keeps_expansion() {
        let s = snap("<body><ul><li title=\"x\">text</li><li>beta</li></ul></body>");
        assert!(!has_table_summary(&s));
        assert!(
            all_nodes(&s.tree)
                .iter()
                .any(|n| n.role == "listitem" && n.name == "x")
        );
    }

    /// AISNAP-2（Issue #631）: 子孫テキストのない td の title は name になるため展開を維持する。
    #[test]
    fn aisnap_2_titled_empty_cell_keeps_expansion() {
        let s =
            snap("<body><table><tr><th>A</th></tr><tr><td title=\"tip\"></td></tr></table></body>");
        assert!(!has_table_summary(&s));
        assert!(
            all_nodes(&s.tree)
                .iter()
                .any(|n| n.role == "cell" && n.name == "tip")
        );
    }

    /// AISNAP-2: aria-labelledby を持つ子孫を含む表は圧縮せず展開する。
    #[test]
    fn aisnap_2_table_with_aria_labelledby_is_expanded() {
        let s = snap(
            "<body><span id=\"l\">Total</span><table><tr><th>A</th></tr><tr><td><span aria-labelledby=\"l\">5</span></td></tr></table></body>",
        );
        assert!(!has_table_summary(&s));
    }

    /// AISNAP-3: 表セル（TableCell）は圧縮の単位なので、従来どおり表は圧縮される。
    #[test]
    fn aisnap_3_plain_table_cells_still_compress() {
        let s = snap("<body><table><tr><th>A</th></tr><tr><td>1</td></tr></table></body>");
        assert!(has_table_summary(&s));
    }

    /// AISNAP-2 / AISNAP-3（Issue #625）: 圧縮表のヘッダセルは `classify_data_leaf` 由来の
    /// `TableCell` を持ち、展開した場合の columnheader `Node::data_leaf` と一致する。
    #[test]
    fn aisnap_2_compressed_header_cells_carry_table_cell_data_leaf() {
        let cases = [
            "<body><table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table></body>",
            "<body><table><thead><tr><td>A</td><td>B</td></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table></body>",
        ];
        for html in cases {
            let s = snap(html);
            let table = all_nodes(&s.tree)
                .into_iter()
                .find_map(|n| n.table.as_ref())
                .expect("圧縮される");
            assert_eq!(table.header.len(), 2, "{html}");
            for h in &table.header {
                assert_eq!(h.data_leaf, Some(DataLeafKind::TableCell), "{html}");
            }
        }
        // 同じ th を展開した場合（rowspan で不規則化）の分類と一致する。
        let expanded = snap(
            "<body><table><thead><tr><th>A</th></tr></thead><tbody><tr><td rowspan=\"2\">1</td></tr></tbody></table></body>",
        );
        assert!(!has_table_summary(&expanded));
        let th = all_nodes(&expanded.tree)
            .into_iter()
            .find(|n| n.role == "columnheader")
            .expect("columnheader がある");
        assert_eq!(th.data_leaf, Some(DataLeafKind::TableCell));
    }

    /// AISNAP-2 / AISNAP-1: 子孫が `MAX_TREE_DEPTH` を超える一覧は圧縮せず展開し、
    /// 従来どおり深さ超過を省略して `truncated` を立てる（圧縮で省略通知が消えない）。
    #[test]
    fn aisnap_2_list_with_descendant_beyond_max_depth_is_not_compressed() {
        // ul は深さ MAX_TREE_DEPTH - 1、li は MAX_TREE_DEPTH、span は MAX_TREE_DEPTH + 1。
        let wrap = MAX_TREE_DEPTH - 4;
        let html = format!(
            "<body>{}<ul><li><span>x</span></li><li>y</li></ul>{}</body>",
            "<div>".repeat(wrap),
            "</div>".repeat(wrap)
        );
        let s = snap(&html);
        assert!(s.truncated);
        assert!(!has_table_summary(&s));
        // 深さに収まる同形の一覧は従来どおり圧縮され、truncated も立たない。
        let html = format!(
            "<body>{}<ul><li><span>x</span></li><li>y</li></ul>{}</body>",
            "<div>".repeat(wrap - 1),
            "</div>".repeat(wrap - 1)
        );
        let s = snap(&html);
        assert!(!s.truncated);
        assert!(has_table_summary(&s));
    }

    /// AISNAP-2: 圧縮判定の走査量が上限を超える巨大な一覧は圧縮せず展開する（有界）。
    #[test]
    fn aisnap_2_oversized_list_skips_compression() {
        let items = "<li>x</li>".repeat(super::MAX_COMPRESS_SCAN_NODES + 10);
        let s = snap(&format!("<body><ul>{items}</ul></body>"));
        assert!(!has_table_summary(&s));
    }

    /// 展開経路のテスト用。`caption`（圧縮の拒否条件）を付けて圧縮を避け、リンクだけでは
    /// 展開されなくなった（Issue #632）後も展開・畳み込みの経路を検証できるようにする。
    fn table_with_rows(total: usize, special: &[(usize, &str)]) -> String {
        let mut html =
            String::from("<table><caption>c</caption><thead><tr><th>n</th></tr></thead><tbody>");
        for i in 0..total {
            match special.iter().find(|(idx, _)| *idx == i) {
                Some((_, cell)) => html.push_str(&format!("<tr><td>{cell}</td></tr>")),
                None => html.push_str(&format!("<tr><td>row{i}</td></tr>")),
            }
        }
        html.push_str("</tbody></table>");
        html
    }

    /// AISNAP-12・Issue #631: title 付き td の行は畳まれ、title が name になる span の行は
    /// 展開のまま残る。
    #[test]
    fn aisnap_12_titled_cell_row_is_folded_but_titled_span_row_is_kept() {
        let s = snap(&table_with_rows(
            40,
            &[
                (30, "<a href=\"/next\">Next</a>"),
                (35, "<span title=\"KEEPTITLE\">kept</span>"),
                (36, "plain"),
            ],
        ));
        let nodes = all_nodes(&s.tree);
        assert!(nodes.iter().any(|n| n.name == "KEEPTITLE"));
        let table = nodes.iter().find(|n| n.role == "table").expect("table");
        assert!(!table.folded_rows.iter().any(|r| r.index == 35));
        assert!(table.folded_rows.iter().any(|r| r.index == 36));
    }

    /// AISNAP-12・TASK-16.4: 操作要素を含み全体を圧縮できない表でも、キャップ外の
    /// ページネーションリンクは ref 付きで展開して残し（実 Snapshot 経路）、
    /// 通常行は件数を省略せず文字列へ畳む。
    #[test]
    fn aisnap_12_expanded_table_keeps_pagination_link_and_folds_plain_rows() {
        let s = snap(&table_with_rows(40, &[(30, "<a href=\"/next\">Next</a>")]));
        let nodes = all_nodes(&s.tree);
        let links: Vec<_> = nodes.iter().filter(|n| n.role == "link").collect();
        assert_eq!(links.len(), 1);
        assert!(links[0].r#ref.is_some());
        let table = nodes
            .iter()
            .find(|n| n.role == "table")
            .expect("table node");
        // 展開された行（ヘッダ行 + 優先保持 20 行）と畳まれた通常行の合計が全行。
        let expanded_rows = nodes.iter().filter(|n| n.role == "row").count();
        assert_eq!(expanded_rows, 1 + 20);
        assert_eq!(table.folded_rows.len(), 40 - 20);
        // 畳まれた行の内容は失われない。
        for i in 0..40 {
            if i == 30 {
                continue;
            }
            let text = format!("row{i}");
            let in_folded = table.folded_rows.iter().any(|r| r.text == text);
            let in_tree = nodes.iter().any(|n| n.name.contains(&text));
            assert!(in_folded || in_tree, "{text} must survive");
        }
        assert!(!s.truncated);
    }

    /// AISNAP-12・AISNAP-2: 途中の操作行だけを展開した 20 行超の表で、畳んだ行と展開行を
    /// index で文書順に統合でき、畳んだ行は ref を持たず、操作行の ref は残る。
    #[test]
    fn aisnap_12_folded_rows_merge_in_document_order() {
        let s = snap(&table_with_rows(40, &[(30, "<a href=\"/next\">Next</a>")]));
        let nodes = all_nodes(&s.tree);
        let table = nodes.iter().find(|n| n.role == "table").expect("table");
        // 展開された本文行（ヘッダ行を除く）は畳まれていない index を昇順で占める。
        let expanded_body: Vec<&Node> = nodes
            .iter()
            .copied()
            .filter(|n| n.role == "row" && n.name != "n")
            .collect();
        let folded_idx: Vec<usize> = table.folded_rows.iter().map(|r| r.index).collect();
        let free: Vec<usize> = (0..40).filter(|i| !folded_idx.contains(i)).collect();
        assert_eq!(free.len(), expanded_body.len());
        assert!(free.contains(&30));
        let mut merged: Vec<(usize, String)> = table
            .folded_rows
            .iter()
            .map(|r| (r.index, r.text.clone()))
            .collect();
        for (i, row) in free.iter().zip(&expanded_body) {
            let text = all_nodes(row)
                .iter()
                .map(|n| n.name.as_str())
                .find(|n| !n.is_empty())
                .unwrap_or("")
                .to_string();
            merged.push((*i, text));
        }
        merged.sort_by_key(|(i, _)| *i);
        // 文書順に統合すると行 0..40 が欠けずに並ぶ（操作行は index 30）。
        for (i, text) in &merged {
            if *i == 30 {
                assert_eq!(text, "Next");
            } else {
                assert_eq!(text, &format!("row{i}"));
            }
        }
        assert_eq!(merged.len(), 40);
        // 畳んだ行は ref を持たない（型に ref が無い）。操作行の ref は残る。
        let link = nodes.iter().find(|n| n.role == "link").expect("link");
        assert!(link.r#ref.is_some());
    }

    /// AISNAP-12: 見出し等の意味的な子孫を持つ行は件数上限を超えても畳まず展開する。
    #[test]
    fn aisnap_12_semantic_row_beyond_cap_is_expanded_not_folded() {
        let s = snap(&table_with_rows(
            40,
            &[(35, "<h2>KEEPHEAD</h2>"), (36, "<a href=\"/x\">x</a>")],
        ));
        let nodes = all_nodes(&s.tree);
        assert!(nodes.iter().any(|n| n.role == "heading"));
        let table = nodes.iter().find(|n| n.role == "table").expect("table");
        assert!(
            table
                .folded_rows
                .iter()
                .all(|r| !r.text.contains("KEEPHEAD"))
        );
    }

    /// AISNAP-12: 表示対象行が上限以下なら畳まず従来どおり全行を展開する。
    #[test]
    fn aisnap_12_small_expanded_table_folds_nothing() {
        let s = snap(&table_with_rows(10, &[(3, "<a href=\"/n\">n</a>")]));
        let nodes = all_nodes(&s.tree);
        assert_eq!(nodes.iter().filter(|n| n.role == "row").count(), 11);
        assert!(nodes.iter().all(|n| n.folded_rows.is_empty()));
    }

    /// AISNAP-12・AISNAP-13・Issue #632: 1 行に 64 件超のリンクがあっても、後ろにある
    /// `rel="next"` リンクは行上限の中で優先保持される。
    #[test]
    fn aisnap_12_row_controls_keep_next_link_beyond_scan_cap() {
        let mut cell = String::new();
        for i in 0..70 {
            cell.push_str(&format!("<a href=\"/p{i}\">L{i}</a>"));
        }
        cell.push_str("<a href=\"/next\" rel=\"next\">Next</a>");
        let s = snap(&format!(
            "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>{cell}</td></tr></tbody></table>"
        ));
        let nodes = all_nodes(&s.tree);
        let table = nodes.iter().find_map(|n| n.table.as_ref()).expect("table");
        let row = table.rows.first().expect("row");
        assert_eq!(row.controls.len(), MAX_ROW_CONTROLS);
        assert!(row.controls_truncated);
        assert!(row.controls.iter().any(|c| c.name == "Next"));
    }

    /// AISNAP-12・AISNAP-13・Issue #632: 先行行が表全体の枠を使い切っても、後続行の
    /// `rel="next"` リンクは優先枠で保持される。
    #[test]
    fn aisnap_12_table_budget_reserves_priority_for_later_rows() {
        let mut html = String::from("<table><thead><tr><th>h</th></tr></thead><tbody>");
        for r in 0..19 {
            html.push_str("<tr><td>");
            for i in 0..MAX_ROW_CONTROLS {
                html.push_str(&format!("<a href=\"/r{r}c{i}\">R{r}C{i}</a>"));
            }
            html.push_str("</td></tr>");
        }
        html.push_str("<tr><td><a href=\"/next\" rel=\"next\">Next</a></td></tr>");
        html.push_str("</tbody></table>");
        let s = snap(&html);
        let nodes = all_nodes(&s.tree);
        let table = nodes.iter().find_map(|n| n.table.as_ref()).expect("table");
        let last = table.rows.last().expect("last row");
        assert_eq!(last.controls.len(), 1);
        assert_eq!(last.controls[0].name, "Next");
        let total: usize = table.rows.iter().map(|r| r.controls.len()).sum();
        assert_eq!(total, MAX_TABLE_CONTROLS);
        assert!(table.rows.iter().any(|r| r.controls_truncated));
    }

    /// AISNAP-12・Issue #632: 1 行の優先要素が行上限を超える表は、優先要素を落とさないよう
    /// 圧縮せず展開する。
    #[test]
    fn aisnap_12_row_priority_over_row_cap_rejects_compression() {
        let mut cell = String::new();
        for i in 0..=MAX_ROW_CONTROLS {
            cell.push_str(&format!("<a href=\"/n{i}\" rel=\"next\">Next{i}</a>"));
        }
        let s = snap(&format!(
            "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>{cell}</td></tr></tbody></table>"
        ));
        let nodes = all_nodes(&s.tree);
        assert!(nodes.iter().all(|n| n.table.is_none()));
        for i in 0..=MAX_ROW_CONTROLS {
            let name = format!("Next{i}");
            assert!(nodes.iter().any(|n| n.name == name && n.r#ref.is_some()));
        }
    }

    /// AISNAP-13・Issue #632: 保持上限で省略される行に操作要素がある表は圧縮せず展開し、
    /// 21 行目以降の操作要素にも ref を発行する。
    #[test]
    fn aisnap_13_omitted_rows_with_controls_reject_compression() {
        let mut html = String::from("<table><thead><tr><th>h</th></tr></thead><tbody>");
        for r in 0..25 {
            html.push_str(&format!("<tr><td><a href=\"/r{r}\">Row{r}</a></td></tr>"));
        }
        html.push_str("</tbody></table>");
        let s = snap(&html);
        let nodes = all_nodes(&s.tree);
        assert!(nodes.iter().all(|n| n.table.is_none()));
        assert!(nodes.iter().any(|n| n.name == "Row24" && n.r#ref.is_some()));
    }

    /// AISNAP-12・Issue #632: 先頭の通常リンクが枠を予約せず、優先リンク 8 件が全件残る。
    #[test]
    fn aisnap_12_row_priority_not_displaced_by_leading_plain_link() {
        let mut cell = String::from("<a href=\"/plain\">Plain</a>");
        for i in 0..MAX_ROW_CONTROLS {
            cell.push_str(&format!("<a href=\"/n{i}\" rel=\"next\">Next{i}</a>"));
        }
        let s = snap(&format!(
            "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>{cell}</td></tr></tbody></table>"
        ));
        let nodes = all_nodes(&s.tree);
        let table = nodes.iter().find_map(|n| n.table.as_ref()).expect("table");
        let row = table.rows.first().expect("row");
        assert_eq!(row.controls.len(), MAX_ROW_CONTROLS);
        for i in 0..MAX_ROW_CONTROLS {
            let name = format!("Next{i}");
            assert!(row.controls.iter().any(|c| c.name == name));
        }
        assert!(row.controls.iter().all(|c| c.name != "Plain"));
        assert!(row.controls_truncated);
    }

    /// AISNAP-13・Issue #632: 非表示 tbody 内の操作要素は省略行として数えず、可視行が
    /// 操作要素を持たなければ圧縮する。
    #[test]
    fn aisnap_13_hidden_rows_with_controls_do_not_block_compression() {
        let mut html = String::from("<table><thead><tr><th>h</th></tr></thead><tbody>");
        for r in 0..25 {
            html.push_str(&format!("<tr><td>Row{r}</td></tr>"));
        }
        html.push_str(
            "</tbody><tbody hidden><tr><td><a href=\"/x\">Hidden</a></td></tr></tbody></table>",
        );
        let s = snap(&html);
        let nodes = all_nodes(&s.tree);
        assert!(nodes.iter().any(|n| n.table.is_some()));
    }
}

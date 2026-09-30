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
//!   `Node::data_leaf` へ印を付けるだけで、role・ref・剪定・打ち切りには使わない。
//! - 規則的な `table`・`ul`・`ol`（[`crate::compress_table::detect_regular_structure`]
//!   が `Regular`。`AISNAP-2`・TASK-12.5・Issue #83）は子孫を展開せず、ヘッダ
//!   （個別 ref）・圧縮行・超過行数を持つ 1 ノード（[`Node::table`]）へ置き換える。
//!   圧縮では `Snapshot::truncated` を立てない（行の省略は `truncated_rows`、セルの
//!   切り詰めは `TableRow::truncated` で通知）。ヘッダ name の打ち切りのみ立てる。
//!   ただし `tfoot` 行がある構造、または操作要素（リンク・ボタン・入力欄・`role`/
//!   `tabindex` 付き等）を含む構造は圧縮せず通常どおり展開する（ref・state・フッター
//!   の可視情報を失わないため）。`img` の `alt`・`aria-label` 等テキスト以外に由来する
//!   accessible name を持つ子孫、または `caption` を含む構造も同様に展開する。
//!   検出は table/list ごとに子孫を走査するが、走査量はコンテナごと
//!   （`MAX_COMPRESS_SCAN_NODES`）と 1 回の構築全体（`MAX_TOTAL_COMPRESS_SCAN_NODES`）
//!   で有界とし、超過したコンテナは圧縮せず展開する。
//! - generic の折り畳み・`none`/`presentation` の除去・空ノードの剪定は行わない
//!   （トークン削減は後続タスク。`AISNAP-1`・`AISNAP-2`）。
//! - ルート以外の全要素に ref を振る（対話可能な要素への絞り込みは後続タスク）。
//! - 深さが [`MAX_TREE_DEPTH`] を超えるサブツリーは省略し `Snapshot::truncated` を立てる。
//! - いずれかの name が打ち切られた場合（文字数上限・子孫走査の上限・構築全体で共有する
//!   走査予算）も `Snapshot::truncated` を立てる。

use std::fmt;

use fandhe_browser_core::dom::{Children, Document, NodeId};

use crate::compress_table::{
    TableDetection, assign_header_refs, compress_rows, detect_regular_structure,
};
use crate::data_leaf::classify_data_leaf;

use super::element_ref::{ElementRef, ElementSignature, RefAllocator, RefError};
use super::name::{
    MAX_TOTAL_CONTENT_STEPS, NameIndex, SKIPPED_SUBTREES, compute_name_with_index,
    is_hidden_element, normalized_input_type,
};
use super::role::compute_role;
use super::state::{compute_state, is_html_element_named};
use super::{HeaderCell, Node, Snapshot, TableRow, TableSummary};

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

/// 規則的構造を圧縮してよいかを返す（`AISNAP-2`・TASK-12.5）。
///
/// 圧縮は子孫の展開（ref・state・accessible name の付与）を省略するため、
/// 次の場合は情報が失われる。このときは圧縮せず通常の展開を維持する
/// （従来の Snapshot と同じ結果）。
/// - `tfoot` 行がある（圧縮表現は本文行のみで、合計額などフッターの可視情報が消える）。
/// - 表・一覧の中に操作要素（リンク・ボタン・入力欄等）がある（ref と state が消える）。
/// - 表に `caption` がある（圧縮表現は caption を保持せず、表題が Snapshot から消える）。
/// - 見出し・入れ子の表 / 一覧・ランドマーク等の意味的な子孫がある（heading 等のノードと ref が消える）。
/// - 圧縮行はテキストノードのみを取り込むため、テキスト以外に由来する accessible name
///   （`img` の `alt`・`aria-label` 等）を持つ子孫がある。
fn can_compress(
    doc: &Document,
    container: NodeId,
    structure: &crate::compress_table::RegularStructure,
) -> bool {
    structure.footer_rows.is_empty() && !has_lossy_descendant(doc, container)
}

/// `container` の子孫（描画対象のみ）に、圧縮すると失われる要素があるかを反復走査で返す。
///
/// 走査量は呼び出し前の [`bounded_descendant_count`] で有界化済みである。
fn has_lossy_descendant(doc: &Document, container: NodeId) -> bool {
    let mut stack: Vec<NodeId> = doc.children(container).collect();
    while let Some(id) = stack.pop() {
        if !doc.is_element(id) || is_excluded(doc, id) {
            continue;
        }
        if is_html_element_named(doc, id, "caption")
            || is_interactive_element(doc, id)
            || is_semantic_structure_element(doc, id)
            || has_non_text_name_source(doc, id)
        {
            return true;
        }
        stack.extend(doc.children(id));
    }
    false
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
    ["aria-label", "aria-labelledby", "title"]
        .iter()
        .any(|a| doc.attribute(id, a).is_some_and(|v| !v.trim().is_empty()))
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
        if !doc.is_element(child) || is_excluded(doc, child) {
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
            scanned.is_some()
        };
        if within_budget
            && let TableDetection::Regular(structure) = detect_regular_structure(doc, child)
            && can_compress(doc, child, &structure)
        {
            let headers = assign_header_refs(doc, &index, &structure, elem_ref, &mut refs)?;
            let compressed = compress_rows(doc, &structure);
            truncated |= headers.iter().any(|h| h.name_truncated);
            node.table = Some(TableSummary::new(
                headers
                    .into_iter()
                    .map(|h| HeaderCell::new(h.role, h.name, h.elem_ref.to_ref_string()))
                    .collect(),
                compressed
                    .rows
                    .into_iter()
                    .map(|r| TableRow::new(r.text, r.truncated))
                    .collect(),
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

    /// AISNAP-2: 圧縮判定の走査量が上限を超える巨大な一覧は圧縮せず展開する（有界）。
    #[test]
    fn aisnap_2_oversized_list_skips_compression() {
        let items = "<li>x</li>".repeat(super::MAX_COMPRESS_SCAN_NODES + 10);
        let s = snap(&format!("<body><ul>{items}</ul></body>"));
        assert!(!has_table_summary(&s));
    }
}

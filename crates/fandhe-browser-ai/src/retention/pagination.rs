//! ページネーションリンクの DOM 検出（`AISNAP-12`・`TASK-16.2`・`MS-2`・Issue #105）。
//!
//! 役割: 親モジュール [`super`]（選択層）は index しか扱わないため、本モジュールが
//! 一覧・表の各項目（`li`・`tr` 等）のサブツリーから「次へ・前へ・ページ番号・
//! もっと見る」リンクを検出し、優先候補（[`PriorityCandidate`]）へ変換する。
//! TASK-16.4（Issue #107）で `compress_table::compress_rows` が
//! `pagination_candidates(doc, &structure.body_rows)` の結果を
//! [`select_retained_with_priority`](super::select_retained_with_priority) へ渡す
//! 想定で、現時点の呼び出し元はテストのみ（統合前）。
//!
//! # 判定規則（強い根拠から順）
//!
//! 対象は HTML 名前空間の `<a>` で、空でない `href` を持ち非表示でないもの。
//!
//! 1. `rel` トークン: `next` / `prev` / `previous`
//! 2. ラベルの完全一致（`aria-label` を優先、無ければ短い表示テキスト。
//!    空白正規化・ASCII 小文字化後の完全一致で、部分一致はしない）
//! 3. `class` トークン: `next` / `prev` / `previous` / `morelink`（Hacker News）
//! 4. 数字だけのラベル（1〜4 桁）は、`aria-current="page"` または
//!    `nav`・`role="navigation"`・`pagination`/`pager`/`paging` 系 class の
//!    祖先が近くにある場合に限り [`PaginationKind::PageNumber`]
//!
//! 語表はヒューリスティックで、誤検出はキャップ内の 1 枠を消費するだけ、
//! 未検出は従来の文書順 `take` へ劣化するだけ（安全側）。語表の拡充は将来課題。
//!
//! # 上限（security.md「不安全な設計」）
//!
//! ネットワーク由来の HTML を走査するため、ラベル取得・項目サブツリー走査・
//! 祖先探索はすべて定数上限で打ち切り、明示スタックで再帰しない。

use super::{PriorityCandidate, RetentionReason};
use crate::snapshot::name::{SKIPPED_SUBTREES, is_hidden_element};
use fandhe_browser_core::dom::{Document, NodeData, NodeId};

/// HTML 名前空間の URI（`core` 側の定義が `pub(crate)` のためローカルに持つ）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// ラベル取得で訪問するノード数の上限（訪問済み + 未訪問スタックの総量）。
const MAX_LABEL_SCAN_STEPS: usize = 64;
/// ラベル（空白正規化後）の最大文字数。超える長文はキーワードとみなさない。
const MAX_LABEL_CHARS: usize = 32;
/// ラベル取得で走査するテキストの総文字数の上限。
const MAX_LABEL_SCAN_CHARS: usize = 256;
/// 項目サブツリー走査で訪問するノード数の上限（訪問済み + 未訪問スタックの総量）。
const MAX_ITEM_SCAN_STEPS: usize = 256;
/// ページ番号の文脈ガードで辿る祖先の最大深さ。
const MAX_PAGINATION_ANCESTOR_DEPTH: usize = 8;
/// ページ番号とみなす数字ラベルの最大桁数。
const MAX_PAGE_NUMBER_DIGITS: usize = 4;

/// ページネーションリンクの種別（`AISNAP-12`・`TASK-16.2`）。
///
/// 真偽値でなく拡張可能な型で根拠を返す（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PaginationKind {
    /// 次のページ。
    Next,
    /// 前のページ。
    Previous,
    /// 最初のページ。
    First,
    /// 最後のページ。
    Last,
    /// 「もっと見る」系（追加読み込み）。
    More,
    /// ページ番号リンク。
    PageNumber,
}

// ラベル語表（出典: PoC-4 発見事項 4・Hacker News fixture `hn-list.html` の
// `morelink`、および一般的なページャの表記）。比較前に ASCII 小文字化する。
const NEXT_LABELS: &[&str] = &[
    "next",
    "next page",
    "next ›",
    "next »",
    "›",
    "»",
    "→",
    "次へ",
    "次のページ",
    "次ページ",
];
const PREVIOUS_LABELS: &[&str] = &[
    "prev",
    "previous",
    "previous page",
    "‹ prev",
    "« previous",
    "‹",
    "«",
    "←",
    "前へ",
    "前のページ",
    "前ページ",
];
const FIRST_LABELS: &[&str] = &["first", "first page", "« first", "最初", "最初へ", "先頭へ"];
const LAST_LABELS: &[&str] = &["last", "last page", "last »", "最後", "最後へ"];
const MORE_LABELS: &[&str] = &["more", "load more", "show more", "もっと見る", "さらに表示"];

fn is_html_named(doc: &Document, id: NodeId, name: &str) -> bool {
    doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && doc
            .local_name(id)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
}

fn is_ascii_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ' | '\u{A0}')
}

/// 空白区切りトークンに `wanted` が（ASCII 大文字小文字非区別で）完全一致するか。
fn token_matches(value: &str, wanted: &str) -> bool {
    value
        .split(is_ascii_space)
        .any(|t| !t.is_empty() && t.eq_ignore_ascii_case(wanted))
}

/// 空白を 1 つに畳み ASCII 小文字化した文字列を返す。
fn normalize_label(raw: &str) -> String {
    let mut out = String::new();
    for word in raw.split(is_ascii_space).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word.to_ascii_lowercase());
    }
    out
}

/// `a` 要素の短い表示テキストを有界に取り出す。上限超過・空なら `None`。
///
/// `compute_name` は呼ぶたびに `NameIndex` を再構築し文書サイズの二乗になるため
/// 使わず、明示スタックの DFS で必要最小限だけ読む。
fn short_label(doc: &Document, id: NodeId) -> Option<String> {
    let mut raw = String::new();
    let mut stack = vec![id];
    let mut steps = 0usize;
    let mut scanned = 0usize;
    while let Some(cur) = stack.pop() {
        steps += 1;
        if steps > MAX_LABEL_SCAN_STEPS {
            return None;
        }
        match doc.node_data(cur) {
            Some(NodeData::Text { contents }) => {
                for c in contents.chars() {
                    scanned += 1;
                    if scanned > MAX_LABEL_SCAN_CHARS {
                        return None;
                    }
                    raw.push(c);
                }
                if raw.chars().filter(|c| !is_ascii_space(*c)).count() > MAX_LABEL_CHARS {
                    return None;
                }
            }
            Some(NodeData::Element { .. }) => {
                if cur != id
                    && (SKIPPED_SUBTREES.iter().any(|n| is_html_named(doc, cur, n))
                        || is_hidden_element(doc, cur))
                {
                    continue;
                }
                let budget = MAX_LABEL_SCAN_STEPS.saturating_sub(steps + stack.len());
                let mut kids: Vec<NodeId> = doc.children(cur).take(budget + 1).collect();
                if kids.len() > budget {
                    return None;
                }
                kids.reverse();
                stack.extend(kids);
            }
            _ => {}
        }
    }
    let label = normalize_label(&raw);
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        None
    } else {
        Some(label)
    }
}

fn kind_from_label(label: &str) -> Option<PaginationKind> {
    let table: [(&[&str], PaginationKind); 5] = [
        (NEXT_LABELS, PaginationKind::Next),
        (PREVIOUS_LABELS, PaginationKind::Previous),
        (FIRST_LABELS, PaginationKind::First),
        (LAST_LABELS, PaginationKind::Last),
        (MORE_LABELS, PaginationKind::More),
    ];
    table
        .iter()
        .find(|(words, _)| words.contains(&label))
        .map(|(_, kind)| *kind)
}

fn is_page_number_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_PAGE_NUMBER_DIGITS
        && label.bytes().all(|b| b.is_ascii_digit())
}

/// ページ番号の文脈ガード（数字だけのリンクの誤検出を防ぐ）。
fn has_pagination_context(doc: &Document, id: NodeId) -> bool {
    if doc
        .attribute(id, "aria-current")
        .is_some_and(|v| v.trim_matches(is_ascii_space).eq_ignore_ascii_case("page"))
    {
        return true;
    }
    doc.ancestors(id)
        .take(MAX_PAGINATION_ANCESTOR_DEPTH)
        .any(|a| {
            if !doc.is_element(a) {
                return false;
            }
            if is_html_named(doc, a, "nav") {
                return true;
            }
            if doc
                .attribute(a, "role")
                .is_some_and(|r| token_matches(r, "navigation"))
            {
                return true;
            }
            doc.attribute(a, "class").is_some_and(|c| {
                c.split(is_ascii_space).any(|t| {
                    let t = t.to_ascii_lowercase();
                    t.contains("pagination") || t.contains("pager") || t.contains("paging")
                })
            })
        })
}

/// 単一要素がページネーションリンクか判定する（`AISNAP-12`・`TASK-16.2`）。
///
/// HTML 名前空間の `<a>`（空でない `href`・非表示でない）だけが対象で、範囲外の
/// `NodeId`・非要素・SVG 内の `<a>` は `None`。判定順はモジュール doc を参照。
pub fn classify_pagination_link(doc: &Document, id: NodeId) -> Option<PaginationKind> {
    if !is_html_named(doc, id, "a") {
        return None;
    }
    if doc.attribute(id, "href").is_none_or(|h| h.is_empty()) || is_hidden_element(doc, id) {
        return None;
    }

    if let Some(rel) = doc.attribute(id, "rel") {
        if token_matches(rel, "next") {
            return Some(PaginationKind::Next);
        }
        if token_matches(rel, "prev") || token_matches(rel, "previous") {
            return Some(PaginationKind::Previous);
        }
    }

    let label = doc
        .attribute(id, "aria-label")
        .map(normalize_label)
        .filter(|l| !l.is_empty())
        .or_else(|| short_label(doc, id));

    if let Some(kind) = label.as_deref().and_then(kind_from_label) {
        return Some(kind);
    }

    if let Some(class) = doc.attribute(id, "class") {
        if token_matches(class, "next") {
            return Some(PaginationKind::Next);
        }
        if token_matches(class, "prev") || token_matches(class, "previous") {
            return Some(PaginationKind::Previous);
        }
        if token_matches(class, "morelink") {
            return Some(PaginationKind::More);
        }
    }

    if label.as_deref().is_some_and(is_page_number_label) && has_pagination_context(doc, id) {
        return Some(PaginationKind::PageNumber);
    }
    None
}

/// 項目（`li`・`tr` 等）のサブツリーからページネーションリンクを探す
/// （`AISNAP-12`・`TASK-16.2`）。
///
/// 項目自身も対象に含む。非表示サブツリーは読み飛ばし、訪問済み + 未訪問の総量を
/// 内部上限（256 ノード）以内に保つ。上限に達したら「見つからなかった」として
/// `None`。複数ある場合は文書順で最初の種別を返す。
pub fn find_pagination_link(doc: &Document, item: NodeId) -> Option<PaginationKind> {
    let mut stack = vec![item];
    let mut steps = 0usize;
    while let Some(id) = stack.pop() {
        steps += 1;
        if steps > MAX_ITEM_SCAN_STEPS {
            return None;
        }
        if !doc.is_element(id) {
            continue;
        }
        if is_hidden_element(doc, id)
            || (id != item && SKIPPED_SUBTREES.iter().any(|n| is_html_named(doc, id, n)))
        {
            continue;
        }
        if let Some(kind) = classify_pagination_link(doc, id) {
            return Some(kind);
        }
        let budget = MAX_ITEM_SCAN_STEPS.saturating_sub(steps + stack.len());
        let mut kids: Vec<NodeId> = doc.children(id).take(budget + 1).collect();
        if kids.len() > budget {
            return None;
        }
        kids.reverse();
        stack.extend(kids);
    }
    None
}

/// 並びの各項目にページネーションリンク検出を適用し、該当項目を優先候補として返す
/// （`AISNAP-12`・`TASK-16.2`・Issue #105）。
///
/// 戻り値は index 昇順で、理由は [`RetentionReason::Pagination`]。確保量は
/// `items.len()` 以下、総走査量は `items.len()` × 256 ノードで上限される。
pub fn pagination_candidates(doc: &Document, items: &[NodeId]) -> Vec<PriorityCandidate> {
    items
        .iter()
        .enumerate()
        .filter(|&(_, &item)| find_pagination_link(doc, item).is_some())
        .map(|(index, _)| PriorityCandidate::new(index, RetentionReason::Pagination))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress_table::detect_regular_structure;
    use crate::retention::{RetentionPolicy, select_retained, select_retained_with_priority};
    use fandhe_browser_core::parse::{ParseOptions, parse_document};
    use fandhe_browser_core::query::query_selector_str;

    fn parse(html: &str) -> Document {
        parse_document(html, &ParseOptions::default())
            .expect("テスト入力は必ず成功する")
            .document
    }

    fn select(doc: &Document, selector: &str) -> NodeId {
        query_selector_str(doc, doc.root(), selector)
            .expect("セレクタは解釈できる")
            .expect("対象要素が見つかる")
    }

    fn classify(html: &str) -> Option<PaginationKind> {
        let doc = parse(html);
        classify_pagination_link(&doc, select(&doc, "a"))
    }

    /// AISNAP-12（TASK-16.2）: rel トークンで次・前を判定する。
    #[test]
    fn aisnap_12_rel_tokens() {
        assert_eq!(
            classify(r#"<a href="?p=2" rel="next">x</a>"#),
            Some(PaginationKind::Next)
        );
        assert_eq!(
            classify(r#"<a href="?p=1" rel="prev">x</a>"#),
            Some(PaginationKind::Previous)
        );
        assert_eq!(
            classify(r#"<a href="?p=1" rel="Previous">x</a>"#),
            Some(PaginationKind::Previous)
        );
        assert_eq!(
            classify(r#"<a href="?p=2" rel="nofollow next">x</a>"#),
            Some(PaginationKind::Next)
        );
    }

    /// AISNAP-12（TASK-16.2）: ラベル語で判定し aria-label が本文より優先される。
    #[test]
    fn aisnap_12_label_words() {
        assert_eq!(
            classify(r#"<a href="/p2">次へ</a>"#),
            Some(PaginationKind::Next)
        );
        assert_eq!(
            classify(r#"<a href="/p1">前へ</a>"#),
            Some(PaginationKind::Previous)
        );
        assert_eq!(
            classify(r#"<a href="/p2"> Next </a>"#),
            Some(PaginationKind::Next)
        );
        assert_eq!(
            classify(r#"<a href="/p2"><span>»</span></a>"#),
            Some(PaginationKind::Next)
        );
        assert_eq!(
            classify(r#"<a href="/p2" aria-label="Next page">text</a>"#),
            Some(PaginationKind::Next)
        );
        assert_eq!(
            classify(r#"<a href="/p9" aria-label="Last page">9</a>"#),
            Some(PaginationKind::Last)
        );
        assert_eq!(
            classify(r#"<a href="/more">もっと見る</a>"#),
            Some(PaginationKind::More)
        );
    }

    /// AISNAP-12（TASK-16.2）: HN の morelink（rel なし）を More と判定する。
    #[test]
    fn aisnap_12_morelink_class() {
        assert_eq!(
            classify(r#"<a href="?p=2" class="morelink">Read on</a>"#),
            Some(PaginationKind::More)
        );
    }

    /// AISNAP-12（TASK-16.2）: 数字リンクは文脈ガードがある場合だけ PageNumber。
    #[test]
    fn aisnap_12_page_number_requires_context() {
        assert_eq!(
            classify(r#"<nav><a href="?p=3">3</a></nav>"#),
            Some(PaginationKind::PageNumber)
        );
        assert_eq!(
            classify(r#"<div class="Pagination-list"><a href="?p=3">3</a></div>"#),
            Some(PaginationKind::PageNumber)
        );
        assert_eq!(
            classify(r#"<a href="?p=3" aria-current="page">3</a>"#),
            Some(PaginationKind::PageNumber)
        );
        assert_eq!(classify(r#"<a href="/item">42</a>"#), None);
        assert_eq!(classify(r#"<nav><a href="/item">12345</a></nav>"#), None);
    }

    /// AISNAP-12（TASK-16.2）: 対象外の要素・状態は None。
    #[test]
    fn aisnap_12_non_targets() {
        assert_eq!(classify(r#"<span>次へ</span><a>x</a>"#), None);
        assert_eq!(classify(r#"<a rel="next">次へ</a>"#), None);
        assert_eq!(classify(r#"<a href="" rel="next">次へ</a>"#), None);
        assert_eq!(classify(r#"<a href="/x" rel="next" hidden>次へ</a>"#), None);
        assert_eq!(
            classify(r#"<a href="/x" rel="next" aria-hidden="true">次へ</a>"#),
            None
        );
        let doc = parse(r#"<svg><a href="/x" rel="next">次へ</a></svg>"#);
        let svg_a = doc
            .descendants(doc.root())
            .find(|&n| doc.local_name(n) == Some("a"))
            .expect("svg 内の a が存在する");
        assert_eq!(classify_pagination_link(&doc, svg_a), None);
        assert_eq!(classify_pagination_link(&doc, doc.root()), None);
    }

    /// AISNAP-12（TASK-16.2）: 部分一致・長文は検出しない。
    #[test]
    fn aisnap_12_no_partial_match() {
        assert_eq!(
            classify(r#"<a href="/x">Next article about paging</a>"#),
            None
        );
        assert_eq!(
            classify(r#"<a href="/x">More details on this topic and more</a>"#),
            None
        );
    }

    /// AISNAP-12（TASK-16.2）: 大量の子・深いネストでも有界に終了する。
    #[test]
    fn aisnap_12_scan_is_bounded() {
        let mut html = String::from("<ul><li><a href=\"/x\">");
        for _ in 0..500 {
            html.push_str("<b>x</b>");
        }
        html.push_str("</a></li></ul>");
        let doc = parse(&html);
        assert_eq!(classify_pagination_link(&doc, select(&doc, "a")), None);

        let mut html = String::from("<ul><li>");
        for _ in 0..2000 {
            html.push_str("<span>");
        }
        html.push_str(r#"<a href="?p=2" rel="next">次へ</a>"#);
        let doc = parse(&html);
        // 上限内で終了すること（見つかるか否かは問わない）。
        let _ = find_pagination_link(&doc, select(&doc, "li"));

        let mut html = String::from("<ul><li>");
        for _ in 0..1000 {
            html.push_str("<i></i>");
        }
        html.push_str(r#"<a href="?p=2" rel="next">次へ</a></li></ul>"#);
        let doc = parse(&html);
        assert_eq!(find_pagination_link(&doc, select(&doc, "li")), None);
    }

    fn list_fixture(with_next: bool) -> String {
        let mut html = String::from("<ul>");
        for i in 0..29 {
            html.push_str(&format!("<li>item{i}</li>"));
        }
        if with_next {
            html.push_str(r#"<li><a href="?p=2" rel="next">次へ</a></li>"#);
        } else {
            html.push_str("<li>item29</li>");
        }
        html.push_str("</ul>");
        html
    }

    /// AISNAP-12（TASK-16.2・Issue #105 受入）: キャップを超える並びでも
    /// ページネーションリンクを含む項目が保持される。
    #[test]
    fn aisnap_12_pagination_item_retained_beyond_cap() {
        let doc = parse(&list_fixture(true));
        let ul = select(&doc, "ul");
        let detection = detect_regular_structure(&doc, ul);
        let structure = detection.as_regular().expect("規則的な一覧");
        assert_eq!(structure.body_rows.len(), 30);

        let candidates = pagination_candidates(&doc, &structure.body_rows);
        assert_eq!(
            candidates,
            vec![PriorityCandidate::new(29, RetentionReason::Pagination)]
        );

        let policy = RetentionPolicy::new(20, 5);
        let r = select_retained_with_priority(30, &policy, &candidates);
        let reasons: Vec<(usize, RetentionReason)> =
            r.kept.iter().map(|i| (i.index, i.reason)).collect();
        assert_eq!(r.kept.len(), 20);
        assert_eq!(r.omitted, 10);
        assert_eq!(reasons.first(), Some(&(0, RetentionReason::Head)));
        assert_eq!(reasons.last(), Some(&(29, RetentionReason::Pagination)));

        let last_li = *structure.body_rows.last().expect("30 行ある");
        let rows = r.apply(&structure.body_rows);
        assert!(rows.iter().any(|&&n| n == last_li));

        // 対比: 候補なしでは末尾項目は切り捨てられる。
        let plain = select_retained(30, &policy);
        assert!(
            !plain
                .apply(&structure.body_rows)
                .iter()
                .any(|&&n| n == last_li)
        );
    }

    /// AISNAP-12（TASK-16.2）: HN 風の表で末尾の morelink 行が保持される。
    #[test]
    fn aisnap_12_hn_style_table_morelink_row_retained() {
        let mut html = String::from("<table>");
        for i in 0..30 {
            html.push_str(&format!("<tr><td>{i}</td><td>story{i}</td></tr>"));
        }
        html.push_str(
            r#"<tr><td></td><td><a href="?p=2" class="morelink" rel="next">More</a></td></tr>"#,
        );
        html.push_str("</table>");
        let doc = parse(&html);
        let table = select(&doc, "table");
        let detection = detect_regular_structure(&doc, table);
        let structure = detection.as_regular().expect("規則的な表");
        let n = structure.body_rows.len();
        assert_eq!(n, 31);
        let candidates = pagination_candidates(&doc, &structure.body_rows);
        assert_eq!(
            candidates,
            vec![PriorityCandidate::new(30, RetentionReason::Pagination)]
        );
        let r = select_retained_with_priority(n, &RetentionPolicy::new(20, 5), &candidates);
        let last_row = *structure.body_rows.last().expect("行がある");
        assert!(
            r.apply(&structure.body_rows)
                .iter()
                .any(|&&x| x == last_row)
        );
    }

    /// AISNAP-12（TASK-16.2）: 先頭へ行が挿入され index がずれても保持される。
    #[test]
    fn aisnap_12_pagination_survives_row_insertion() {
        let html = list_fixture(true).replace("<ul>", "<ul><li>banner</li>");
        let doc = parse(&html);
        let ul = select(&doc, "ul");
        let detection = detect_regular_structure(&doc, ul);
        let structure = detection.as_regular().expect("規則的な一覧");
        let candidates = pagination_candidates(&doc, &structure.body_rows);
        let r = select_retained_with_priority(
            structure.body_rows.len(),
            &RetentionPolicy::new(20, 5),
            &candidates,
        );
        let last_li = *structure.body_rows.last().expect("行がある");
        assert!(r.apply(&structure.body_rows).iter().any(|&&n| n == last_li));
    }

    /// AISNAP-12（TASK-16.2）: リンクのない一覧では候補が空。
    #[test]
    fn aisnap_12_no_candidates_without_pagination() {
        let doc = parse(&list_fixture(false));
        let ul = select(&doc, "ul");
        let detection = detect_regular_structure(&doc, ul);
        let structure = detection.as_regular().expect("規則的な一覧");
        assert!(pagination_candidates(&doc, &structure.body_rows).is_empty());
    }
}

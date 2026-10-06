//! 非インタラクティブなデータ値（表セル・価格等）を検出する `isDataLeaf`
//! ヒューリスティック（`AISNAP-3`・`TASK-13`・`TASK-13.1`・`TASK-13.2`・`MS-2`・Issue #86・#87）。
//!
//! 役割: 簡約表現をインタラクティブ要素・見出し・ランドマークだけに絞ると
//! 表セルの値が抜け落ちるため（PoC-4）、情報を保持すべき「データ葉」かどうかを
//! 要素単位で判定する。PoC-4 の `reduce.mjs` の `isDataLeaf` と同じ規則で、
//! `td`/`th` は子要素の有無を問わず無条件でデータ葉とする。もう一方の分岐として、
//! `class` 属性値に `price`・`amount`・`currency` を ASCII 大文字小文字非区別の
//! 部分一致で含み、かつ子要素を持たない末端要素もデータ葉とする
//! （子要素条件は価格クラス名分岐にのみ掛かる）。`cost` は PoC-4 の規則に無く
//! 測定ベースライン（TASK-14）が変わるため含めない（拡充は TASK-15・Issue #100 で検討）。
//!
//! 引用文（TASK-15.1・Issue #99・`AISNAP-11`）: HTML 名前空間の `blockquote`/`q` 要素は
//! 子要素の有無を問わず [`DataLeafKind::Quote`] とする（通常形 `<blockquote><p>…</p></blockquote>`
//! を取りこぼさないため、価格クラスの子要素条件は課さない）。判定順は末尾に追加し、
//! 既存入力の分類結果は変えない（TASK-14 の測定ベースライン維持）。内側の `p`・`a` 等は対象外。
//!
//! # スタブについて（REPAIR-3）
//!
//! `snapshot::build::build_snapshot` から呼ばれ、結果は `Node::data_leaf`（圧縮表のヘッダは `HeaderCell::data_leaf`。TASK-12.5）に入る
//! （TASK-13.3・Issue #88。簡約・剪定への利用は後続）。判定規則として実装済みなのは `td`/`th` と
//! 価格クラス名パターン（TASK-13.2・Issue #87）と `blockquote`/`q`（TASK-15.1・Issue #99）で、
//! 地の文は TASK-15.2（Issue #100。`AISNAP-11`）、`span.text` 等のクラスベースの引用は未対応
//! （Issue #100・TASK-18）。
//!
//! HTML 名前空間定数を本モジュールに持つ理由: `core` 側の定数は `pub(crate)`、
//! `snapshot::state` のヘルパーは `pub(super)` で、いずれもここから使えないため。

use fandhe_browser_core::dom::{Document, NodeId};

/// HTML 名前空間の URI（上記の理由でローカルに定義する）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// データ葉と判定した根拠（`AISNAP-3`）。
///
/// `#[non_exhaustive]` により、将来の判定根拠（TASK-15 の拡充等）の追加が非破壊になる
/// （REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataLeafKind {
    /// HTML の `td`/`th` 要素（表セル）。
    TableCell,
    /// HTML 要素のうち `class` 属性に price/amount/currency を含み、
    /// 要素の子を持たないもの（TASK-13.2・Issue #87）。
    PriceClass,
    /// HTML の `blockquote`/`q` 要素（引用文の容器。TASK-15.1・Issue #99・`AISNAP-11`）。
    Quote,
}

/// 価格系クラス名の部分一致パターン（PoC-4 `reduce.mjs` 由来。すべて小文字・非空）。
/// `cost` は spec の規則に無いため含めない（拡充は TASK-15・Issue #100）。
const PRICE_CLASS_PATTERNS: [&str; 3] = ["price", "amount", "currency"];

/// HTML 名前空間の要素かを返す。価格分岐も `td`/`th` と同じく HTML 要素に限定する
/// （PoC が `<svg>` サブツリーを丸ごとスキップしていたこととの一貫性）。
fn is_html_element(doc: &Document, id: NodeId) -> bool {
    doc.is_element(id) && doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
}

/// `haystack` が `needle` を ASCII 大文字小文字非区別の部分一致で含むかを返す。
/// アロケーションしない。`needle` は空でない定数を渡す前提（空は `false`）。
fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    !needle.is_empty()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// 要素の子に要素が 1 つでもあるかを返す（テキストノードは数えない）。
fn has_element_child(doc: &Document, id: NodeId) -> bool {
    doc.children(id).any(|c| doc.is_element(c))
}

/// HTML 名前空間の要素で、local name が `name` と ASCII 大文字小文字非区別で
/// 一致するかを返す。
fn is_html_element_named(doc: &Document, id: NodeId, name: &str) -> bool {
    is_html_element(doc, id)
        && doc
            .local_name(id)
            .is_some_and(|local| local.eq_ignore_ascii_case(name))
}

/// 要素 `id` がデータ葉ならその根拠を返す（`AISNAP-3`・TASK-13.1・TASK-13.2）。
///
/// 判定順: `td`/`th` は子要素の有無を問わず [`DataLeafKind::TableCell`]。
/// 次に HTML 要素で `class` 属性全体に価格系パターンを部分一致で含み、かつ
/// 子要素を持たなければ [`DataLeafKind::PriceClass`]。
/// 最後に `blockquote`/`q` を [`DataLeafKind::Quote`] とする（TASK-15.1・Issue #99）。
/// 非要素・ドキュメントルート・対象外の要素・範囲外の `NodeId` は `None`
/// （panic しない）。`snapshot::build::build_snapshot` が要素ごとに呼ぶ（TASK-13.3・Issue #88）。
pub fn classify_data_leaf(doc: &Document, id: NodeId) -> Option<DataLeafKind> {
    if is_html_element_named(doc, id, "td") || is_html_element_named(doc, id, "th") {
        Some(DataLeafKind::TableCell)
    } else if is_html_element(doc, id)
        && doc.attribute(id, "class").is_some_and(|class| {
            PRICE_CLASS_PATTERNS
                .iter()
                .any(|p| contains_ascii_case_insensitive(class, p))
        })
        && !has_element_child(doc, id)
    {
        Some(DataLeafKind::PriceClass)
    } else if is_html_element_named(doc, id, "blockquote") || is_html_element_named(doc, id, "q") {
        Some(DataLeafKind::Quote)
    } else {
        None
    }
}

/// 要素 `id` がデータ葉かどうかの真偽だけを返す（[`classify_data_leaf`] の簡易版）。
pub fn is_data_leaf(doc: &Document, id: NodeId) -> bool {
    classify_data_leaf(doc, id).is_some()
}

#[cfg(test)]
mod tests {
    use super::{DataLeafKind, classify_data_leaf, is_data_leaf};
    use fandhe_browser_core::dom::{Document, NodeId};
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

    const TABLE: &str = r#"<table><caption>c</caption><thead><tr><th>列</th></tr></thead>
<tbody><tr><th scope="row">行</th><td>1</td><td><a href="/x">l</a></td></tr></tbody></table>"#;

    /// AISNAP-3（TASK-13.1・Issue #86）: td/th は表セルとして真。
    #[test]
    fn aisnap_3_td_th_are_table_cells() {
        let doc = parse(TABLE);
        for sel in ["thead th", "th[scope=row]", "td"] {
            let id = select(&doc, sel);
            assert_eq!(
                classify_data_leaf(&doc, id),
                Some(DataLeafKind::TableCell),
                "{sel}"
            );
            assert!(is_data_leaf(&doc, id), "{sel}");
        }
    }

    /// AISNAP-3（TASK-13.1・Issue #86）: 子要素を持つ td も無条件で真、
    /// その子の `a` は偽。
    #[test]
    fn aisnap_3_td_with_children_is_data_leaf() {
        let doc = parse(TABLE);
        let a = select(&doc, "td a");
        let td = doc.parent(a).expect("a は td の子");
        assert_eq!(classify_data_leaf(&doc, td), Some(DataLeafKind::TableCell));
        assert_eq!(classify_data_leaf(&doc, a), None);
    }

    /// AISNAP-3（TASK-13.1・Issue #86）: td/th 以外の要素は偽。
    #[test]
    fn aisnap_3_other_elements_are_not_data_leaf() {
        let doc = parse(&format!("<div><p><span>s</span></p></div>{TABLE}"));
        for sel in [
            "div", "p", "span", "table", "tr", "thead", "tbody", "caption",
        ] {
            let id = select(&doc, sel);
            assert_eq!(classify_data_leaf(&doc, id), None, "{sel}");
            assert!(!is_data_leaf(&doc, id), "{sel}");
        }
    }

    /// AISNAP-3（TASK-13.1・Issue #86）: テキストノードとルートは偽。
    #[test]
    fn aisnap_3_text_and_root_are_not_data_leaf() {
        let doc = parse(TABLE);
        let td = select(&doc, "td");
        let text = doc.first_child(td).expect("td はテキストを持つ");
        assert!(!doc.is_element(text));
        assert_eq!(classify_data_leaf(&doc, text), None);
        assert_eq!(classify_data_leaf(&doc, doc.root()), None);
    }

    fn class_kind(class: &str) -> Option<DataLeafKind> {
        let doc = parse(&format!(r#"<div><span class="{class}">1</span></div>"#));
        classify_data_leaf(&doc, select(&doc, "span"))
    }

    /// AISNAP-3（TASK-13.2・Issue #87）: 3 語で真。
    #[test]
    fn aisnap_3_price_amount_currency_classes() {
        for c in ["price", "amount", "currency"] {
            let doc = parse(&format!(r#"<p><span class="{c}">1</span></p>"#));
            let id = select(&doc, "span");
            assert_eq!(
                classify_data_leaf(&doc, id),
                Some(DataLeafKind::PriceClass),
                "{c}"
            );
            assert!(is_data_leaf(&doc, id), "{c}");
        }
    }

    /// AISNAP-3（TASK-13.2・Issue #87）: 大文字小文字・複合名・複数トークン。
    #[test]
    fn aisnap_3_price_class_partial_and_case_insensitive() {
        for c in [
            "PRICE",
            "Amount",
            "product-price",
            "priceTag",
            "foo bar-currency baz",
        ] {
            assert_eq!(class_kind(c), Some(DataLeafKind::PriceClass), "{c}");
        }
    }

    /// AISNAP-3（TASK-13.2・Issue #87）: 負例。
    #[test]
    fn aisnap_3_non_price_classes_are_not_data_leaf() {
        assert_eq!(class_kind("pricing"), None);
        assert_eq!(class_kind("title"), None);
        let doc = parse("<p><span>1</span></p>");
        assert_eq!(classify_data_leaf(&doc, select(&doc, "span")), None);
    }

    /// AISNAP-3（TASK-13.2・Issue #87）: 子要素を持つ要素は偽、テキストのみは真。
    #[test]
    fn aisnap_3_price_class_requires_no_element_children() {
        let doc = parse(r#"<div class="price"><span class="price">1</span></div>"#);
        assert_eq!(classify_data_leaf(&doc, select(&doc, "div")), None);
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "span")),
            Some(DataLeafKind::PriceClass)
        );
        let doc = parse(r#"<div class="price">text<!-- c --></div>"#);
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "div")),
            Some(DataLeafKind::PriceClass)
        );
    }

    /// AISNAP-3（TASK-13.2・Issue #87）: td/th が優先、非 HTML 名前空間は偽。
    #[test]
    fn aisnap_3_price_class_precedence_and_namespace() {
        let doc = parse(r#"<table><tr><td class="price">1</td></tr></table>"#);
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "td")),
            Some(DataLeafKind::TableCell)
        );
        let doc = parse(r#"<svg><text class="price">1</text></svg>"#);
        assert_eq!(classify_data_leaf(&doc, select(&doc, "text")), None);
    }

    /// AISNAP-11（TASK-15.1・Issue #99）: blockquote/q は引用文として真（受入基準）。
    #[test]
    fn aisnap_11_blockquote_and_q_are_quote() {
        let doc = parse("<blockquote><p>引用</p></blockquote><p><q>引用</q></p>");
        for sel in ["blockquote", "q"] {
            let id = select(&doc, sel);
            assert_eq!(
                classify_data_leaf(&doc, id),
                Some(DataLeafKind::Quote),
                "{sel}"
            );
            assert!(is_data_leaf(&doc, id), "{sel}");
        }
    }

    /// AISNAP-11（TASK-15.1・Issue #99）: 子要素の有無を問わず容器のみが対象。
    #[test]
    fn aisnap_11_quote_ignores_element_children() {
        let doc = parse(
            "<blockquote id=a>text</blockquote><blockquote id=b><p>x <a href=\"/y\">l</a></p></blockquote><q id=c><em>e</em></q>",
        );
        for sel in ["#a", "#b", "#c"] {
            assert_eq!(
                classify_data_leaf(&doc, select(&doc, sel)),
                Some(DataLeafKind::Quote),
                "{sel}"
            );
        }
        for sel in ["p", "a", "em"] {
            assert_eq!(classify_data_leaf(&doc, select(&doc, sel)), None, "{sel}");
        }
    }

    /// AISNAP-11（TASK-15.1・Issue #99）: 判定順（既存分類を保持）と名前空間。
    #[test]
    fn aisnap_11_quote_precedence_and_namespace() {
        let doc = parse("<table><tr><td><q>x</q></td></tr></table>");
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "q")),
            Some(DataLeafKind::Quote)
        );
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "td")),
            Some(DataLeafKind::TableCell)
        );
        let doc = parse(r#"<blockquote class="price">text</blockquote>"#);
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "blockquote")),
            Some(DataLeafKind::PriceClass)
        );
        let doc = parse(r#"<blockquote class="price"><p>x</p></blockquote>"#);
        assert_eq!(
            classify_data_leaf(&doc, select(&doc, "blockquote")),
            Some(DataLeafKind::Quote)
        );
        let doc = parse("<svg><q>x</q></svg>");
        assert_eq!(classify_data_leaf(&doc, select(&doc, "q")), None);
        let doc = parse("<div><cite>c</cite></div>");
        assert_eq!(classify_data_leaf(&doc, select(&doc, "cite")), None);
    }
}

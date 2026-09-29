//! 非インタラクティブなデータ値（表セル・価格等）を検出する `isDataLeaf`
//! ヒューリスティック（`AISNAP-3`・`TASK-13`・`TASK-13.1`・`MS-2`・Issue #86）。
//!
//! 役割: 簡約表現をインタラクティブ要素・見出し・ランドマークだけに絞ると
//! 表セルの値が抜け落ちるため（PoC-4）、情報を保持すべき「データ葉」かどうかを
//! 要素単位で判定する。PoC-4 の `reduce.mjs` の `isDataLeaf` と同じ規則で、
//! `td`/`th` は子要素の有無を問わず無条件でデータ葉とする
//! （子要素条件は価格クラス名分岐にのみ掛かる）。
//!
//! # スタブについて（REPAIR-3）
//!
//! 現時点では呼び出し元がない。TASK-13.3（Issue #88）が
//! `snapshot` のノード構築へ組み込む予定である。実装済みなのは `td`/`th` のみで、
//! 価格クラス名パターンは TASK-13.2（Issue #87）、地の文・引用文への拡充は
//! TASK-15（Issue #99・#100。`AISNAP-11`）で実装する。
//!
//! HTML 名前空間定数を本モジュールに持つ理由: `core` 側の定数は `pub(crate)`、
//! `snapshot::state` のヘルパーは `pub(super)` で、いずれもここから使えないため。

use fandhe_browser_core::dom::{Document, NodeId};

/// HTML 名前空間の URI（上記の理由でローカルに定義する）。
const HTML_NAMESPACE_URI: &str = "http://www.w3.org/1999/xhtml";

/// データ葉と判定した根拠（`AISNAP-3`）。
///
/// `#[non_exhaustive]` により、TASK-13.2 の価格パターン等の追加が非破壊になる
/// （REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DataLeafKind {
    /// HTML の `td`/`th` 要素（表セル）。
    TableCell,
}

/// HTML 名前空間の要素で、local name が `name` と ASCII 大文字小文字非区別で
/// 一致するかを返す。
fn is_html_element_named(doc: &Document, id: NodeId, name: &str) -> bool {
    doc.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && doc
            .local_name(id)
            .is_some_and(|local| local.eq_ignore_ascii_case(name))
}

/// 要素 `id` がデータ葉ならその根拠を返す（`AISNAP-3`・TASK-13.1）。
///
/// `td`/`th` は子要素の有無を問わず [`DataLeafKind::TableCell`] とする。
/// 非要素・ドキュメントルート・対象外の要素・範囲外の `NodeId` は `None`
/// （panic しない）。TASK-13.3 の snapshot 構築から呼ばれる予定。
pub fn classify_data_leaf(doc: &Document, id: NodeId) -> Option<DataLeafKind> {
    if is_html_element_named(doc, id, "td") || is_html_element_named(doc, id, "th") {
        Some(DataLeafKind::TableCell)
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
}

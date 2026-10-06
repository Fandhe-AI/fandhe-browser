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
//! 測定ベースライン（TASK-14）が変わるため含めない。
//!
//! 地の文の拡充（TASK-15.2・Issue #100・`AISNAP-11`）として、`class` トークンが
//! `text`・`description`・`note` に完全一致する末端の非インタラクティブ要素も
//! [`DataLeafKind::ProseClass`] とする。トークンはフィクスチャ実測（`quotes-list.html` の
//! `span.text` のみが該当し、表・一覧の子孫に当たらない）に基づく。素の `p` 等のタグ規則や
//! 部分一致は表・一覧内に広く当たり、圧縮判定（`build.rs`）を通じて TASK-14 の測定値を変えるため採らない。
//!
//! # スタブについて（REPAIR-3）
//!
//! `snapshot::build::build_snapshot` から呼ばれ、結果は `Node::data_leaf`（圧縮表のヘッダは `HeaderCell::data_leaf`。TASK-12.5）に入る
//! （TASK-13.3・Issue #88。簡約・剪定への利用は後続）。判定規則として実装済みなのは `td`/`th` と
//! 価格クラス名パターン（TASK-13.2・Issue #87）、地の文クラス（TASK-15.2・Issue #100）。
//! 引用文（Issue #99）は別タスク。地の文の既知の制約: クラスを持たない地の文・上記 3 語以外の
//! クラス名・インライン子要素を含む地の文は未検出（汎用テキストブロック検出は未実装。`AISNAP-11`）。
//!
//! HTML 名前空間定数を本モジュールに持つ理由: `core` 側の定数は `pub(crate)`、
//! `snapshot::state` のヘルパーは `pub(super)` で、いずれもここから使えないため。

use fandhe_browser_core::dom::{Document, NodeData, NodeId};

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
    /// `class` トークンが `text`/`description`/`note` に完全一致し、子要素を持たず、
    /// 直下に非空白テキストを持つ非インタラクティブな地の文要素（TASK-15.2・Issue #100）。
    ProseClass,
}

/// 地の文クラスのトークン（ASCII 大文字小文字非区別の完全一致。`AISNAP-11`）。
const PROSE_CLASS_TOKENS: [&str; 3] = ["text", "description", "note"];

/// `class` のいずれかのトークンが地の文クラスに完全一致するか。
fn has_prose_class_token(doc: &Document, id: NodeId) -> bool {
    doc.class_names(id)
        .any(|t| PROSE_CLASS_TOKENS.iter().any(|p| t.eq_ignore_ascii_case(p)))
}

/// 直下の Text ノードに非空白文字が 1 つでもあるか。
fn has_direct_non_whitespace_text(doc: &Document, id: NodeId) -> bool {
    doc.children(id).any(|c| {
        matches!(doc.node_data(c), Some(NodeData::Text { contents }) if contents.chars().any(|ch| !ch.is_whitespace()))
    })
}

/// 操作対象になりうる要素・属性を持つか（地の文から除外する）。
fn is_interactive_for_prose(doc: &Document, id: NodeId) -> bool {
    let tag_interactive = doc.local_name(id).is_some_and(|n| {
        [
            "button", "input", "select", "textarea", "label", "summary", "option",
        ]
        .iter()
        .any(|t| n.eq_ignore_ascii_case(t))
            || ((n.eq_ignore_ascii_case("a") || n.eq_ignore_ascii_case("area"))
                && doc.attribute(id, "href").is_some())
    });
    tag_interactive
        || ["role", "tabindex", "onclick", "contenteditable"]
            .iter()
            .any(|a| doc.attribute(id, a).is_some())
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
/// 続いて地の文クラス（TASK-15.2）を満たせば [`DataLeafKind::ProseClass`]。
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
    } else if is_html_element(doc, id)
        && has_prose_class_token(doc, id)
        && !has_element_child(doc, id)
        && has_direct_non_whitespace_text(doc, id)
        && !is_interactive_for_prose(doc, id)
    {
        Some(DataLeafKind::ProseClass)
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

    fn prose_kind(html: &str) -> Option<DataLeafKind> {
        let doc = parse(html);
        classify_data_leaf(&doc, select(&doc, "#t"))
    }

    /// AISNAP-11（TASK-15.2・Issue #100）: 地の文クラスは真（大文字・複数トークン含む）。
    #[test]
    fn aisnap_11_prose_classes_are_data_leaf() {
        for c in ["text", "description", "note", "TEXT", "foo note bar"] {
            let doc = parse(&format!(
                r#"<div class="quote"><span id="t" class="{c}">本文</span></div>"#
            ));
            let id = select(&doc, "#t");
            assert_eq!(
                classify_data_leaf(&doc, id),
                Some(DataLeafKind::ProseClass),
                "{c}"
            );
            assert!(is_data_leaf(&doc, id), "{c}");
        }
    }

    /// AISNAP-11: 部分一致は偽。
    #[test]
    fn aisnap_11_prose_class_requires_exact_token() {
        for c in ["text-muted", "selftext", "toc-text", "subtext", "notes"] {
            let h = format!(r#"<p><span id="t" class="{c}">x</span></p>"#);
            assert_eq!(prose_kind(&h), None, "{c}");
        }
    }

    /// AISNAP-11: 子要素・空・空白のみは偽、コメント併存は真。
    #[test]
    fn aisnap_11_prose_class_requires_leaf_with_text() {
        assert_eq!(
            prose_kind(r#"<p><span id="t" class="text">a <em>b</em></span></p>"#),
            None
        );
        assert_eq!(
            prose_kind(r#"<p><span id="t" class="text"></span></p>"#),
            None
        );
        assert_eq!(
            prose_kind(r#"<p><span id="t" class="text">  </span></p>"#),
            None
        );
        assert_eq!(
            prose_kind(r#"<p><span id="t" class="text">x<!-- c --></span></p>"#),
            Some(DataLeafKind::ProseClass)
        );
    }

    /// AISNAP-11: インタラクティブ要素・属性は偽。href なしの a は真。
    #[test]
    fn aisnap_11_prose_class_excludes_interactive() {
        for h in [
            r#"<a id="t" class="text" href="/x">x</a>"#,
            r#"<button id="t" class="note">x</button>"#,
            r#"<label id="t" class="text">x</label>"#,
            r#"<span id="t" class="text" role="button">x</span>"#,
            r#"<span id="t" class="text" tabindex="0">x</span>"#,
        ] {
            assert_eq!(prose_kind(h), None, "{h}");
        }
        assert_eq!(
            prose_kind(r#"<a id="t" class="text">x</a>"#),
            Some(DataLeafKind::ProseClass)
        );
    }

    /// AISNAP-11: 判定順（TableCell > PriceClass > ProseClass）と名前空間。
    #[test]
    fn aisnap_11_prose_class_precedence_and_namespace() {
        assert_eq!(
            prose_kind(r#"<table><tr><td id="t" class="text">1</td></tr></table>"#),
            Some(DataLeafKind::TableCell)
        );
        assert_eq!(
            prose_kind(r#"<p><span id="t" class="price text">1</span></p>"#),
            Some(DataLeafKind::PriceClass)
        );
        assert_eq!(
            prose_kind(r#"<svg><text id="t" class="text">1</text></svg>"#),
            None
        );
    }
}

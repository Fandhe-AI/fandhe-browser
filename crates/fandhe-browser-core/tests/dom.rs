//! `fandhe-browser-core` の DOM 走査 API（`dom` モジュール・TASK-24.5・#39・
//! ビヘイビア `CORE-1`）を crate 外から検証する結合テスト。
//!
//! `parse` モジュールが構築した `Document` に対し、crate 外から走査 API・
//! アクセサ・`text_content` を呼べること、および `Document` が `Send + Sync`
//! であることを確認する。代表的な HTML 断片による網羅的な構造検証は
//! `tests/parse_dom.rs`（TASK-24.6・#40）を参照。ここでは crate 境界を
//! 越えて呼べることの最小限の確認に留める。

use fandhe_browser_core::{Document, NodeId, ParseOptions, parse_document};

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

fn find_by_local_name(doc: &Document, root: NodeId, local_name: &str) -> NodeId {
    doc.descendants(root)
        .find(|&id| doc.local_name(id) == Some(local_name))
        .unwrap_or_else(|| panic!("要素 {local_name} が見つからない"))
}

/// CORE-1: crate 外から `children`/`descendants`/`attribute`/`text_content`
/// を呼び、期待どおりの具体値が得られる。
#[test]
fn core_1_traversal_api_is_usable_from_outside_crate() {
    let doc = parse(r#"<div id="root"><p>hello <b>world</b></p><a href="/x">link</a></div>"#);
    let div = find_by_local_name(&doc, doc.root(), "div");

    assert_eq!(doc.attribute(div, "id"), Some("root"));

    let children: Vec<&str> = doc
        .children(div)
        .filter_map(|id| doc.local_name(id))
        .collect();
    assert_eq!(children, vec!["p", "a"]);

    let a = find_by_local_name(&doc, div, "a");
    assert_eq!(doc.attribute(a, "href"), Some("/x"));

    let p = find_by_local_name(&doc, div, "p");
    assert_eq!(doc.text_content(p).as_deref(), Some("hello world"));

    let descendant_names: Vec<&str> = doc
        .descendants(div)
        .filter_map(|id| doc.local_name(id))
        .collect();
    assert_eq!(descendant_names, vec!["p", "b", "a"]);
}

/// CORE-1: `Document`・`NodeId` は crate 外から見ても `Send + Sync` を満たす
/// （TASK-24.5 の受入基準）。
#[test]
fn core_1_document_is_send_sync_from_outside_crate() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<Document>();
    assert_bounds::<NodeId>();
}

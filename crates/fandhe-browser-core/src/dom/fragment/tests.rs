//! `Document::set_inner_html` のユニットテスト（TASK-108・Issue #778・`JS-5` / `JS-6`）。

use super::*;
use crate::dom::DomLimits;
use crate::parse::{ParseOptions, parse_document};
use crate::query::query_selector_str;

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

fn sel(doc: &Document, s: &str) -> NodeId {
    query_selector_str(doc, doc.root(), s)
        .expect("selector ok")
        .expect("要素が存在するはず")
}

fn html(doc: &Document) -> String {
    doc.serialize_html().expect("serialize").into_html()
}

fn dom_err(r: Result<InnerHtmlOutcome>) -> DomError {
    match r {
        Err(Error::Dom(e)) => e,
        other => panic!("expected Error::Dom, got {other:?}"),
    }
}

type Snap = Vec<(Option<NodeId>, Vec<NodeId>)>;

fn snapshot(doc: &Document) -> Snap {
    (0..doc.node_count())
        .map(|i| {
            let id = NodeId::new(i);
            (doc.parent(id), doc.children(id).collect())
        })
        .collect()
}

/// JS-5: 子が置換され、コメントと兄弟が保たれる。
#[test]
fn js_5_set_inner_html_replaces_children() {
    let mut doc = parse("<html><head></head><body><div id=a>x</div></body></html>");
    let a = sel(&doc, "#a");
    let out = doc
        .set_inner_html(a, "<p id=\"x\">a</p><!--c--><b>b</b>")
        .expect("ok");
    assert_eq!(out.inserted_nodes, 5);
    assert_eq!(out.removed_children, 1);
    assert_eq!(
        html(&doc),
        "<html><head></head><body><div id=\"a\"><p id=\"x\">a</p><!--c--><b>b</b></div></body></html>"
    );
    let x = sel(&doc, "#x");
    assert_eq!(doc.parent(x), Some(a));
}

/// JS-5: `<tbody>` を文脈にすると `<tr>` がそのまま入る。
#[test]
fn js_5_set_inner_html_uses_context_element() {
    let mut doc = parse("<html><head></head><body><table><tbody></tbody></table></body></html>");
    let tb = sel(&doc, "tbody");
    doc.set_inner_html(tb, "<tr><td>1</td></tr>").expect("ok");
    assert_eq!(
        html(&doc),
        "<html><head></head><body><table><tbody><tr><td>1</td></tr></tbody></table></body></html>"
    );
}

/// JS-5: SVG の名前空間が保たれる。
#[test]
fn js_5_set_inner_html_keeps_svg_namespace() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    let d = sel(&doc, "div");
    doc.set_inner_html(d, "<svg><circle r=\"1\"></circle></svg>")
        .expect("ok");
    let svg = doc.first_child(d).expect("svg");
    assert_eq!(doc.namespace_url(svg), Some("http://www.w3.org/2000/svg"));
}

/// JS-5: 空文字は子を全消去する。
#[test]
fn js_5_set_inner_html_empty_clears_children() {
    let mut doc = parse("<html><head></head><body><div><i></i>t</div></body></html>");
    let d = sel(&doc, "div");
    let out = doc.set_inner_html(d, "").expect("ok");
    assert_eq!(out.inserted_nodes, 0);
    assert_eq!(out.removed_children, 2);
    assert_eq!(
        html(&doc),
        "<html><head></head><body><div></div></body></html>"
    );
}

/// JS-5: `<template>` の contents も取り込む。
#[test]
fn js_5_set_inner_html_copies_template_contents() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    let d = sel(&doc, "div");
    doc.set_inner_html(d, "<template><i>t</i></template>")
        .expect("ok");
    let t = doc.first_child(d).expect("template");
    let tc = doc.template_contents(t).expect("contents");
    assert_eq!(doc.text_content(tc), Some("t".to_owned()));
    assert_eq!(
        html(&doc),
        "<html><head></head><body><div><template><i>t</i></template></div></body></html>"
    );
}

/// JS-5: `<template>` の innerHTML 設定は template contents を置換し、空文字でクリアする。
#[test]
fn js_5_set_inner_html_on_template_replaces_contents() {
    let mut doc = parse("<html><head></head><body><template><i>old</i></template></body></html>");
    let t = sel(&doc, "template");
    doc.set_inner_html(t, "<b>new</b>").expect("ok");
    assert_eq!(doc.first_child(t), None);
    assert_eq!(
        html(&doc),
        "<html><head></head><body><template><b>new</b></template></body></html>"
    );
    doc.set_inner_html(t, "").expect("ok");
    assert_eq!(
        html(&doc),
        "<html><head></head><body><template></template></body></html>"
    );
}

/// JS-6: ノード数上限超過は Err で木が不変。
#[test]
fn js_6_set_inner_html_node_limit_leaves_tree_unchanged() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    let limit = doc.node_count() + 3;
    doc.set_limits(DomLimits::default().with_max_nodes(limit));
    let d = sel(&doc, "div");
    let before = snapshot(&doc);
    let e = dom_err(doc.set_inner_html(d, "<i></i><i></i><i></i><i></i>"));
    assert!(matches!(e, DomError::NodeLimitExceeded { limit: l } if l == limit));
    assert_eq!(snapshot(&doc), before);
    // 上限ちょうどは成功する。
    doc.set_inner_html(d, "<i></i><i></i><i></i>").expect("ok");
}

/// JS-6: 残り容量 0 では空でない入力は Err。
#[test]
fn js_6_set_inner_html_no_capacity_left() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    doc.set_limits(DomLimits::default().with_max_nodes(doc.node_count()));
    let d = sel(&doc, "div");
    let e = dom_err(doc.set_inner_html(d, "x"));
    assert!(matches!(e, DomError::NodeLimitExceeded { .. }));
}

/// JS-6: 総バイト上限超過は Err で木が不変。
#[test]
fn js_6_set_inner_html_total_bytes_limit() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    let d = sel(&doc, "div");
    let used = doc.retained_bytes;
    doc.set_limits(DomLimits::default().with_max_total_bytes(used + 3));
    let before = snapshot(&doc);
    let e = dom_err(doc.set_inner_html(d, "abcdef"));
    assert!(matches!(e, DomError::TotalBytesExceeded { .. }));
    assert_eq!(snapshot(&doc), before);
}

/// JS-6: 入力長がテキスト上限を超えると Err。
#[test]
fn js_6_set_inner_html_input_text_limit() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    let d = sel(&doc, "div");
    doc.set_limits(DomLimits::default().with_max_text_bytes(4));
    let e = dom_err(doc.set_inner_html(d, "abcde"));
    assert!(matches!(e, DomError::TextTooLarge { len: 5, limit: 4 }));
}

/// JS-5: Text ノードは対象外。
#[test]
fn js_5_set_inner_html_rejects_text_node() {
    let mut doc = parse("<html><head></head><body><div>t</div></body></html>");
    let d = sel(&doc, "div");
    let t = doc.first_child(d).expect("text");
    let e = dom_err(doc.set_inner_html(t, "<b></b>"));
    assert!(matches!(e, DomError::InvalidNodeKind { .. }));
}

/// JS-6: 深いネストでも panic せずに取り込める。
#[test]
fn js_6_set_inner_html_deep_nesting_does_not_overflow() {
    let mut doc = parse("<html><head></head><body><div></div></body></html>");
    let d = sel(&doc, "div");
    let deep = "<span>".repeat(5000);
    let r = doc.set_inner_html(d, &deep);
    assert!(r.is_ok(), "{r:?}");
    assert!(doc.node_count() > 5000);
}

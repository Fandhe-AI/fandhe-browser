//! `Document` の変更 API（`dom::mutation`）を crate 外から検証する結合テスト
//! （TASK-107・Issue #772・ビヘイビア `JS-5` / `JS-6`）。
//!
//! 後続の `__dom.op` ブリッジ（TASK-108）は公開 API のみで変更・照会するため、
//! パース後の変更が query / text_content に反映されることと、上限拒否時に DOM が
//! 変化しないことを公開型（`Document`・`DomLimits`・`DomError`）だけで確認する。

use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{
    Document, DomError, DomLimits, Error, NodeId, ParseOptions, parse_document, query_selector,
    query_selector_all,
};

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

fn first(doc: &Document, selector: &str) -> Option<NodeId> {
    let list = parse_selector_list(selector).expect("セレクタは解析できるはず");
    query_selector(doc, doc.root(), &list).expect("キャッシュ上限に達しない")
}

fn count(doc: &Document, selector: &str) -> usize {
    let list = parse_selector_list(selector).expect("セレクタは解析できるはず");
    query_selector_all(doc, doc.root(), &list)
        .expect("キャッシュ上限に達しない")
        .len()
}

fn dom_err<T: std::fmt::Debug>(r: Result<T, Error>) -> DomError {
    match r {
        Err(Error::Dom(e)) => e,
        other => panic!("expected Error::Dom, got {other:?}"),
    }
}

/// 上限 0 を一時的に設定して現在の保持バイト数を読み取る（公開 API のみで観測する）。
fn retained_bytes(doc: &mut Document) -> usize {
    let saved = doc.limits();
    doc.set_limits(saved.with_max_total_bytes(0));
    let retained = match dom_err(doc.create_text_node("a")) {
        DomError::TotalBytesExceeded { retained, .. } => retained,
        other => panic!("unexpected {other:?}"),
    };
    doc.set_limits(saved);
    retained
}

/// JS-5: パース後の追加・属性設定・テキスト置換が query / text_content に反映される。
#[test]
fn js_5_mutations_after_parse_are_visible_to_query_and_text_content() {
    let mut doc = parse("<body><ul id=list><li>one</li></ul></body>");
    let list = first(&doc, "#list").expect("ul が存在する");

    let li = doc.create_element("LI").expect("create");
    doc.set_attribute(li, "class", "added").expect("attr");
    let text = doc.create_text_node("two").expect("text");
    doc.append_child(li, text).expect("append text");
    doc.append_child(list, li).expect("append li");

    assert_eq!(count(&doc, "#list > li"), 2);
    let added = first(&doc, "li.added").expect("追加した li が見つかる");
    assert_eq!(added, li);
    assert_eq!(doc.text_content(added).as_deref(), Some("two"));
    assert_eq!(doc.attribute(li, "class"), Some("added"));

    let previous = doc.set_attribute(li, "CLASS", "renamed").expect("replace");
    assert_eq!(previous.as_deref(), Some("added"));
    assert_eq!(count(&doc, "li.added"), 0);
    assert_eq!(count(&doc, "li.renamed"), 1);

    doc.set_text_content(list, "reset").expect("set text");
    assert_eq!(count(&doc, "#list > li"), 0);
    assert_eq!(doc.text_content(list).as_deref(), Some("reset"));

    let body = first(&doc, "body").expect("body");
    doc.remove_child(body, list).expect("remove");
    assert_eq!(count(&doc, "#list"), 0);
}

/// JS-6: ノード数上限の拒否は Err を返し、既存の DOM を変更しない。
#[test]
fn js_6_node_limit_rejection_leaves_dom_unchanged() {
    let mut doc = parse("<body><p id=p>keep</p></body>");
    let nodes_before = doc.node_count();
    doc.set_limits(DomLimits::default().with_max_nodes(nodes_before));

    let err = dom_err(doc.create_element("div"));
    assert!(matches!(err, DomError::NodeLimitExceeded { limit } if limit == nodes_before));
    let p = first(&doc, "#p").expect("p");
    let err = dom_err(doc.set_text_content(p, "replaced"));
    assert!(matches!(err, DomError::NodeLimitExceeded { .. }));

    assert_eq!(doc.node_count(), nodes_before);
    assert_eq!(doc.text_content(p).as_deref(), Some("keep"));
}

/// JS-6: テキスト・属性値・属性個数の個別上限の拒否で DOM が変化しない。
#[test]
fn js_6_per_item_limits_reject_without_mutating() {
    let mut doc = parse("<body><div id=d data-a=1>x</div></body>");
    doc.set_limits(
        DomLimits::default()
            .with_max_text_bytes(4)
            .with_max_attribute_value_bytes(3)
            .with_max_attributes_per_element(2),
    );
    let d = first(&doc, "#d").expect("div");
    let nodes_before = doc.node_count();

    assert!(matches!(
        dom_err(doc.create_text_node("12345")),
        DomError::TextTooLarge { len: 5, limit: 4 }
    ));
    assert!(matches!(
        dom_err(doc.set_attribute(d, "id", "toolong")),
        DomError::AttributeValueTooLarge { len: 7, limit: 3 }
    ));
    assert!(matches!(
        dom_err(doc.set_attribute(d, "data-b", "1")),
        DomError::TooManyAttributes { limit: 2 }
    ));

    assert_eq!(doc.node_count(), nodes_before);
    assert_eq!(doc.attribute(d, "id"), Some("d"));
    assert_eq!(doc.attribute(d, "data-b"), None);
    assert_eq!(doc.text_content(d).as_deref(), Some("x"));
}

/// JS-6: 文書全体の保持バイト数上限。切り離したノードも数え、拒否時は DOM が変化しない。
#[test]
fn js_6_total_bytes_limit_counts_detached_nodes_and_rejects_without_mutating() {
    let mut doc = parse("<body><div id=d>x</div></body>");
    assert_eq!(DomLimits::default().max_total_bytes(), 64 * 1024 * 1024);
    let retained = retained_bytes(&mut doc);
    assert!(retained > 0);

    doc.set_limits(DomLimits::default().with_max_total_bytes(retained + 10));
    let nodes_before = doc.node_count();
    let t = doc.create_text_node("0123456789").expect("ちょうど上限");
    let d = first(&doc, "#d").expect("div");
    doc.append_child(d, t).expect("append");
    doc.remove_child(d, t).expect("detach");

    // 切り離しても保持分は減らないため、1 バイトでも拒否される。
    assert!(matches!(
        dom_err(doc.create_text_node("z")),
        DomError::TotalBytesExceeded { requested: 1, .. }
    ));
    assert!(matches!(
        dom_err(doc.set_attribute(d, "k", "v")),
        DomError::TotalBytesExceeded { requested: 2, .. }
    ));
    assert!(matches!(
        dom_err(doc.set_text_content(d, "zz")),
        DomError::TotalBytesExceeded { requested: 2, .. }
    ));

    assert_eq!(doc.node_count(), nodes_before + 1);
    assert_eq!(doc.attribute(d, "k"), None);
    assert_eq!(doc.text_content(d).as_deref(), Some("x"));
}

/// JS-6: 既存テキストの短縮で解放した分は予算に戻り、再度書き込める。
#[test]
fn js_6_total_bytes_budget_is_released_when_text_is_overwritten() {
    let mut doc = parse("<body></body>");
    let retained = retained_bytes(&mut doc);
    doc.set_limits(DomLimits::default().with_max_total_bytes(retained + 8));
    let t = doc.create_text_node("12345678").expect("ちょうど上限");
    doc.set_text_content(t, "12").expect("縮小は常に可能");
    doc.set_text_content(t, "123456").expect("解放分の範囲内");
    assert_eq!(doc.text_content(t).as_deref(), Some("123456"));
    assert!(matches!(
        dom_err(doc.set_text_content(t, "1234567890")),
        DomError::TotalBytesExceeded { .. }
    ));
    assert_eq!(doc.text_content(t).as_deref(), Some("123456"));
}

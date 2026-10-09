//! `Document` の HTML シリアライズ（`dom::serialize`）を crate 外から検証する結合テスト
//! （TASK-107・Issue #773・ビヘイビア `JS-4` / `JS-6`）。
//!
//! 後続の NavigationResult 更新（TASK-112）と `innerHTML` getter（TASK-108）は公開 API
//! のみで呼ぶため、エスケープ・raw text・void・上限境界・往復一致を公開型
//! （`Document`・`SerializeScope`・`DomLimits`・`DomError`）だけで確認する。

use fandhe_browser_core::selector::parse_selector_list;
use fandhe_browser_core::{
    DEFAULT_MAX_SERIALIZED_BYTES, Document, DomError, DomLimits, Error, NodeData, NodeId,
    ParseOptions, SerializeScope, parse_document, query_selector,
};

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

fn first(doc: &Document, selector: &str) -> NodeId {
    let list = parse_selector_list(selector).expect("セレクタは解析できるはず");
    query_selector(doc, doc.root(), &list)
        .expect("キャッシュ上限に達しない")
        .expect("要素が存在する")
}

fn body_html(html: &str) -> String {
    let doc = parse(html);
    let body = first(&doc, "body");
    doc.serialize_node(body, SerializeScope::ChildrenOnly)
        .expect("上限内")
        .html
}

/// 木構造の署名（種別・名前・名前空間・属性・内容）を反復で並行比較する。
fn assert_same_tree(a: &Document, b: &Document) {
    let mut stack = vec![(a.root(), b.root())];
    while let Some((x, y)) = stack.pop() {
        assert_eq!(signature(a, x), signature(b, y));
        let xs: Vec<_> = a.children(x).collect();
        let ys: Vec<_> = b.children(y).collect();
        assert_eq!(xs.len(), ys.len(), "子の数が不一致: {}", signature(a, x));
        stack.extend(xs.into_iter().zip(ys));
        match (a.template_contents(x), b.template_contents(y)) {
            (Some(p), Some(q)) => stack.push((p, q)),
            (None, None) => {}
            _ => panic!("template contents の有無が不一致"),
        }
    }
}

fn signature(doc: &Document, id: NodeId) -> String {
    match doc.node_data(id).expect("存在するノード") {
        NodeData::Element { name, attrs, .. } => {
            let attrs: Vec<String> = attrs
                .iter()
                .map(|a| format!("{}|{}={}", &*a.name.ns, &*a.name.local, a.value))
                .collect();
            format!("E {}|{} {attrs:?}", &*name.ns, &*name.local)
        }
        NodeData::Text { contents } => format!("T {contents:?}"),
        NodeData::Comment { contents } => format!("C {contents:?}"),
        NodeData::Doctype { name, .. } => format!("D {name}"),
        other => format!("{other:?}"),
    }
}

fn roundtrip(html: &str) {
    let original = parse(html);
    let serialized = original.serialize_html().expect("上限内").html;
    let reparsed = parse(&serialized);
    assert_same_tree(&original, &reparsed);
}

/// JS-4: テキスト・属性値のエスケープを具体値で確認する。
#[test]
fn js_4_escapes_text_and_attribute_values() {
    let html =
        body_html("<p title='say &quot;hi&quot; &amp; &lt;x&gt;'>a&lt;b&amp;c&gt;&nbsp;</p>");
    assert_eq!(
        html,
        "<p title=\"say &quot;hi&quot; &amp; &lt;x&gt;\">a&lt;b&amp;c&gt;&nbsp;</p>"
    );
}

/// JS-4: raw text 要素・コメント・doctype・void 要素。
#[test]
fn js_4_raw_text_comment_doctype_and_void() {
    let doc = parse(
        "<!DOCTYPE html><title>t</title><!-- a < b --><script>if (a < b && c) {}</script>\
         <style>p > a { }</style><br><img src=\"x\"><input>",
    );
    assert_eq!(
        doc.serialize_html().expect("上限内").html,
        "<!DOCTYPE html><html><head><title>t</title><!-- a < b --><script>if (a < b && c) {}</script>\
         <style>p > a { }</style></head><body><br><img src=\"x\"><input></body></html>"
    );
}

/// JS-4: 文書全体の完全一致。
#[test]
fn js_4_whole_document_exact() {
    let doc = parse("<!DOCTYPE html><title>t</title><p class=\"c\">x</p>");
    assert_eq!(
        doc.serialize_html().expect("上限内").html,
        "<!DOCTYPE html><html><head><title>t</title></head><body><p class=\"c\">x</p></body></html>"
    );
}

/// JS-4: void 要素へ変更 API で子を足しても子・終了タグは出ない。
#[test]
fn js_4_void_element_children_are_not_serialized() {
    let mut doc = parse("<br>");
    let br = first(&doc, "br");
    let text = doc.create_text_node("x").expect("作成できる");
    doc.append_child(br, text).expect("追加できる");
    assert_eq!(
        doc.serialize_node(br, SerializeScope::IncludeNode)
            .expect("上限内")
            .html,
        "<br>"
    );
}

/// JS-4: `<noscript>` はパース時の scripting 設定に従う。
#[test]
fn js_4_noscript_follows_parse_scripting_flag() {
    let input = "<body><noscript><p>a&lt;b</p></noscript>";
    assert_eq!(body_html(input), "<noscript><p>a&lt;b</p></noscript>");
    let doc = parse_document(input, &ParseOptions::default().with_scripting_enabled(true))
        .expect("成功する")
        .document;
    let noscript = first(&doc, "noscript");
    // scripting 有効ではテキストが raw のまま再パースされ、中身は 1 つのテキストノード。
    assert_eq!(
        doc.serialize_node(noscript, SerializeScope::IncludeNode)
            .expect("上限内")
            .html,
        "<noscript><p>a&lt;b</p></noscript>"
    );
}

/// JS-4: `pre` の先頭 LF は往復しても保たれる。
#[test]
fn js_4_pre_leading_newline_roundtrips() {
    let doc = parse("<pre>\n\nx</pre>");
    let pre = first(&doc, "pre");
    assert_eq!(doc.text_content(pre).as_deref(), Some("\nx"));
    let out = doc.serialize_html().expect("上限内").html;
    assert!(out.contains("<pre>\n\nx</pre>"), "{out}");
    let again = parse(&out);
    assert_eq!(
        again.text_content(first(&again, "pre")).as_deref(),
        Some("\nx")
    );
}

/// JS-4: `<template>` は template contents を出力する。
#[test]
fn js_4_template_contents_are_serialized() {
    assert_eq!(
        body_html("<body><template><p>t</p></template>"),
        "<template><p>t</p></template>"
    );
}

/// JS-4: scope 別の具体値。
#[test]
fn js_4_scope_children_only_and_include_node() {
    let doc = parse("<div id=\"d\"><b>x</b>y</div>");
    let div = first(&doc, "div");
    assert_eq!(
        doc.serialize_node(div, SerializeScope::ChildrenOnly)
            .expect("上限内")
            .html,
        "<b>x</b>y"
    );
    assert_eq!(
        doc.serialize_node(div, SerializeScope::IncludeNode)
            .expect("上限内")
            .html,
        "<div id=\"d\"><b>x</b>y</div>"
    );
}

/// JS-6: 上限ちょうどは成功、1 バイト足りなければ Err（部分出力なし）。
#[test]
fn js_6_limit_boundary_exact_and_one_over() {
    let mut doc = parse("<p>aaaa</p>");
    let full = doc.serialize_html().expect("既定上限内").html;
    assert_eq!(full, "<html><head></head><body><p>aaaa</p></body></html>");
    let n = full.len();
    doc.set_limits(DomLimits::default().with_max_serialized_bytes(n));
    assert_eq!(doc.serialize_html().expect("ちょうどは成功").html, full);
    doc.set_limits(DomLimits::default().with_max_serialized_bytes(n - 1));
    match doc.serialize_html() {
        Err(Error::Dom(DomError::SerializedOutputTooLarge { limit })) => {
            assert_eq!(limit, n - 1);
        }
        other => panic!("expected SerializedOutputTooLarge, got {other:?}"),
    }
}

/// JS-6: 既定上限は 8 MiB。ちょうど 8 MiB は成功、1 バイト多いと Err。
#[test]
fn js_6_default_limit_is_8_mib_boundary() {
    assert_eq!(DEFAULT_MAX_SERIALIZED_BYTES, 8 * 1024 * 1024);
    assert_eq!(DomLimits::default().max_serialized_bytes(), 8 * 1024 * 1024);
    let overhead = parse("<p></p>")
        .serialize_html()
        .expect("上限内")
        .html
        .len();
    let pad = DEFAULT_MAX_SERIALIZED_BYTES - overhead;

    let exact = parse(&format!("<p>{}</p>", "a".repeat(pad)));
    let out = exact.serialize_html().expect("ちょうど 8 MiB は成功").html;
    assert_eq!(out.len(), DEFAULT_MAX_SERIALIZED_BYTES);

    let over = parse(&format!("<p>{}</p>", "a".repeat(pad + 1)));
    match over.serialize_html() {
        Err(Error::Dom(DomError::SerializedOutputTooLarge { limit })) => {
            assert_eq!(limit, DEFAULT_MAX_SERIALIZED_BYTES);
        }
        other => panic!("expected SerializedOutputTooLarge, got {other:?}"),
    }
}

/// JS-6: 深さ 10,000 の入れ子でもスタックオーバーフローせず成功する
/// （反復実装のため深さ上限は設けない）。
#[test]
fn js_6_deep_nesting_10000_succeeds() {
    const DEPTH: usize = 10_000;
    let html = format!("{}{}", "<div>".repeat(DEPTH), "</div>".repeat(DEPTH));
    let doc = parse(&html);
    let out = doc.serialize_html().expect("深さ 10,000 は成功する").html;
    assert_eq!(out.matches("<div>").count(), DEPTH);
    assert_eq!(out.matches("</div>").count(), DEPTH);
}

/// JS-4: 代表フィクスチャ（static / form / 表）とインラインの入れ子テーブルで
/// パース→シリアライズ→再パースの構造が一致する。
#[test]
fn js_4_roundtrip_representative_fixtures() {
    for html in [
        include_str!("../../../harness/compat_fixtures/static/01-article.html"),
        include_str!("../../../harness/compat_fixtures/static/03-login-form.html"),
        include_str!("../../../harness/compat_fixtures/ssr_spa_static/02-dashboard-table.html"),
        "<table><tr><td><table><tr><td>in &amp; out</td></tr></table></td></tr></table>",
        "<svg viewBox=\"0 0 1 1\"><a xlink:href=\"#x\"><text>t</text></a></svg><p>\u{a0}\"q\"</p>",
    ] {
        roundtrip(html);
    }
}

//! `cssom::match_rules` の結合テスト（TASK-105.5・#259・ビヘイビア `CORE-5`）。公開 API のみを使う。

use fandhe_browser_core::cssom::{collect_document_styles, match_rules};
use fandhe_browser_core::dom::NodeId;
use fandhe_browser_core::{ParseOptions, Specificity, parse_document};

/// CORE-5: 文書の `<style>` 群に対するマッチ結果（添字・詳細度・宣言）と空結果。
#[test]
fn core_5_match_rules_over_document_styles() {
    let html = r#"<html><head>
        <style>p { color: red } a:hover { color: pink } #x { color: blue }</style>
        <style>.c { margin: 0 } div { top: 1px }</style>
        </head><body><p id="x" class="c">t</p><span>s</span></body></html>"#;
    let doc = parse_document(html, &ParseOptions::default())
        .expect("must parse")
        .document;
    let styles = collect_document_styles(&doc).expect("must collect");
    let sheets = || {
        styles
            .style_sheets()
            .iter()
            .map(|s| s.parsed().stylesheet())
    };
    let find = |name: &str| -> NodeId {
        doc.descendants(doc.root())
            .find(|&id| doc.local_name(id) == Some(name))
            .expect("element exists")
    };

    let got = match_rules(&doc, find("p"), sheets()).expect("must match");
    let pos: Vec<(usize, usize)> = got
        .iter()
        .map(|m| (m.sheet_index(), m.rule_index()))
        .collect();
    // `a:hover` は構築時に捨てられるため、`#x` の rule_index は 1。
    assert_eq!(pos, vec![(0, 0), (0, 1), (1, 0)]);
    assert_eq!(got[1].specificity(), Specificity::new(1, 0, 0));
    let decl = got[2].rule().declarations().first().expect("declaration");
    assert_eq!((decl.property(), decl.value()), ("margin", "0"));

    let none = match_rules(&doc, find("span"), sheets()).expect("must match");
    assert!(none.is_empty());
}

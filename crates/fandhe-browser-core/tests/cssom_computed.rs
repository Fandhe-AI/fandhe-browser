//! `cssom::computed_style` の結合テスト（TASK-105.6・#260・ビヘイビア `CORE-5`）。公開 API のみを使う。

use fandhe_browser_core::cssom::{collect_document_styles, computed_style};
use fandhe_browser_core::{DeclarationOrigin, ParseOptions, Specificity, parse_document};

/// CORE-5: 複数 `<style>` と inline の競合を文書から通しで解決する。
#[test]
fn core_5_computed_style_over_document_styles() {
    let html = r#"<html><head>
        <style>p { color: red; margin: 0 } #x { color: blue }</style>
        <style>.c { margin: 1px; top: 2px }</style>
        </head><body><p id="x" class="c" style="top: 9px">t</p></body></html>"#;
    let doc = parse_document(html, &ParseOptions::default())
        .expect("must parse")
        .document;
    let styles = collect_document_styles(&doc).expect("must collect");
    let p = doc
        .descendants(doc.root())
        .find(|&id| doc.local_name(id) == Some("p"))
        .expect("p exists");
    let got = computed_style(
        &doc,
        p,
        styles
            .style_sheets()
            .iter()
            .map(|s| s.parsed().stylesheet()),
    )
    .expect("must compute");

    let flat: Vec<(&str, &str)> = got
        .declarations()
        .iter()
        .map(|d| (d.property(), d.value()))
        .collect();
    assert_eq!(
        flat,
        vec![("color", "blue"), ("margin", "1px"), ("top", "9px")]
    );
    assert_eq!(
        got.get("color").expect("color").origin(),
        DeclarationOrigin::Rule {
            sheet_index: 0,
            rule_index: 1,
            specificity: Specificity::new(1, 0, 0)
        }
    );
    assert_eq!(
        got.get("top").expect("top").origin(),
        DeclarationOrigin::Inline
    );
}

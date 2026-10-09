//! `collect_page_scripts` を crate 外から検証する結合テスト（TASK-109・#774・
//! ビヘイビア `JS-4`・`JS-6`）。公開 API だけでパースから収集までを通す。

use fandhe_browser_core::page_script::CollectedScripts;
use fandhe_browser_core::{
    ParseOptions, ScriptCollectionOptions, ScriptDiagnosticKind, ScriptSource,
    collect_page_scripts, parse_document,
};

fn run(html: &str) -> CollectedScripts {
    let parsed = parse_document(html, &ParseOptions::default()).expect("parse");
    collect_page_scripts(&parsed.document, &ScriptCollectionOptions::default())
}

/// JS-4: 文書順・型判定・template 除外を公開 API で確認する。
#[test]
fn js_4_order_type_and_template() {
    let r = run(concat!(
        "<head><script>a()</script></head><body>",
        "<script type=\"module\">m</script>",
        "<template><script>t</script></template>",
        "<script src=\"x.js\" defer></script></body>",
    ));
    assert_eq!(r.entries().len(), 2);
    assert_eq!(
        r.entries()[0].source(),
        &ScriptSource::Inline("a()".to_string())
    );
    assert_eq!(
        r.entries()[1].source(),
        &ScriptSource::External("x.js".to_string())
    );
    assert!(r.entries()[1].is_defer());
    assert_eq!(r.diagnostics().len(), 1);
    assert_eq!(
        r.diagnostics()[0].kind(),
        ScriptDiagnosticKind::SkippedModule
    );
    assert_eq!(r.diagnostics()[0].kind().as_str(), "skipped_module");
}

/// JS-6: 65 件で超過 1 件が診断に残る。
#[test]
fn js_6_limit_65() {
    let r = run(&"<script>x</script>".repeat(65));
    assert_eq!(r.entries().len(), 64);
    assert_eq!(
        r.diagnostics()[0].kind(),
        ScriptDiagnosticKind::ScriptLimitExceeded
    );
    assert_eq!(r.diagnostics()[0].count(), Some(1));
}

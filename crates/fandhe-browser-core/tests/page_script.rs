//! `collect_page_scripts` を crate 外から検証する結合テスト（TASK-109・#774・
//! ビヘイビア `JS-4`・`JS-6`）。公開 API だけでパースから収集までを通す。

use fandhe_browser_core::page_script::CollectedScripts;
use fandhe_browser_core::{
    ParseOptions, ScriptCollectionOptions, ScriptDiagnosticKind, ScriptKind, ScriptSource,
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

const GATE_HTML: &str = concat!(
    "<script>a()</script>",
    "<script type=\"module\" src=\"m.js?token=dummy\"></script>",
    "<script nomodule src=\"legacy.js?token=dummy\"></script>",
    "<script type=\"module\">import 'x';</script>",
);

/// JS-4・#1115: gate 有効時は module を文書順に集め、nomodule classic を skip する。
#[test]
fn js_4_gate_on_collects_modules_and_skips_nomodule() {
    let parsed = parse_document(GATE_HTML, &ParseOptions::default()).expect("parse");
    let opts = ScriptCollectionOptions::default().with_execute_modules(true);
    let r = collect_page_scripts(&parsed.document, &opts);
    let got: Vec<_> = r
        .entries()
        .iter()
        .map(|e| (e.index(), e.kind(), e.source().clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                0,
                ScriptKind::Classic,
                ScriptSource::Inline("a()".to_string())
            ),
            (
                1,
                ScriptKind::Module,
                ScriptSource::External("m.js?token=dummy".to_string())
            ),
            (
                3,
                ScriptKind::Module,
                ScriptSource::Inline("import 'x';".to_string())
            ),
        ]
    );
    assert_eq!(r.diagnostics().len(), 1);
    let d = &r.diagnostics()[0];
    assert_eq!(d.kind(), ScriptDiagnosticKind::SkippedNomodule);
    assert_eq!(d.index(), Some(2));
    assert!(!d.message().contains("token"));
    assert!(!d.message().contains("legacy.js"));
}

/// JS-4・#1115: 同じ HTML を既定（gate off）で収集すると従来結果になる。
#[test]
fn js_4_gate_off_keeps_legacy_behavior() {
    let r = run(GATE_HTML);
    let got: Vec<_> = r
        .entries()
        .iter()
        .map(|e| (e.index(), e.kind(), e.is_nomodule()))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, ScriptKind::Classic, false),
            (2, ScriptKind::Classic, true)
        ]
    );
    let kinds: Vec<_> = r
        .diagnostics()
        .iter()
        .map(|d| (d.kind(), d.index()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (ScriptDiagnosticKind::SkippedModule, Some(1)),
            (ScriptDiagnosticKind::SkippedModule, Some(3)),
        ]
    );
}

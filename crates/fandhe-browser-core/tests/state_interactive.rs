//! 操作モードの確定結果を、外部の crate と同じ公開 API（`interactive`・`AppState`）で検証する結合テスト
//! （TASK-124・`AISNAP-16`・`AISNAP-20`・`AISNAP-21`）。
//!
//! 許可の入口 `AppState::allow_interactive_for_tests` を使うため feature `test-support` のときだけ動く
//! （`Cargo.toml` の `required-features`。CI の `rust-test-*` は `--all-features` で実行する）。

use std::sync::Arc;

use fandhe_browser_core::interactive::{
    HostDomSnapshot, InteractivePageId, InteractiveSync, SyncError, build_interactive_document,
};
use fandhe_browser_core::state::PageMode;
use fandhe_browser_core::{AppState, NavigationResult, StateError, query_selector_str};
use fandhe_browser_profile::Profile;

fn snapshot(session: &str, doc_seq: u64, html: &str, element_count: u64) -> HostDomSnapshot {
    HostDomSnapshot::new(
        InteractivePageId::new(session, doc_seq).unwrap(),
        "https://a.example/",
        html.to_owned(),
        element_count,
        "snap-1",
    )
    .unwrap()
}

fn sync(session: &str, doc_seq: u64, html: &str) -> InteractiveSync {
    build_interactive_document(snapshot(session, doc_seq, html, 16)).unwrap()
}

/// 一時プロファイルの上の `AppState`（操作モードを許す）。戻り値の 2 つ目は後始末するディレクトリ。
fn app_state(tag: &str) -> (AppState, std::path::PathBuf) {
    let dir = fandhe_browser_profile::test_base_dir().join(format!(
        "fandhe-core-it-interactive-{tag}-{}",
        std::process::id()
    ));
    let profile = Arc::new(Profile::open(&dir).expect("profile"));
    (
        AppState::with_disabled_renderer(profile).allow_interactive_for_tests(),
        dir,
    )
}

/// `AISNAP-20`: 番号を外した文書と、要素からホストの番号への対応表ができる。番号の不正は拒否する。
#[test]
fn aisnap20_public_api_builds_document_and_rejects_bad_numbers() {
    let s = sync(
        "s",
        1,
        r#"<html data-fandhe-n="0"><head data-fandhe-n="1"></head><body data-fandhe-n="2"><button id="b" data-fandhe-n="3">go</button></body></html>"#,
    );
    assert_eq!(s.page().doc_seq(), 1);
    let (state, dir) = app_state("build");
    let g = state.navigation().begin_navigation().unwrap();
    state.commit_interactive(g, s).unwrap();
    let latest = state.navigation().latest().unwrap();
    assert!(!latest.html().contains("data-fandhe-n"));
    let doc = latest.interactive().unwrap();
    let button = query_selector_str(doc.document(), doc.document().root(), "#b")
        .unwrap()
        .unwrap();
    assert_eq!(doc.host_number(button), Some(3));
    assert_eq!(doc.snapshot(), "snap-1");
    drop(state);
    let _ = std::fs::remove_dir_all(&dir);

    let dup = r#"<p data-fandhe-n="1"></p><p data-fandhe-n="1"></p>"#;
    assert_eq!(
        build_interactive_document(snapshot("s", 1, dup, 16)).err(),
        Some(SyncError::DuplicateNumber)
    );
    let out_of_range = r#"<p data-fandhe-n="16"></p>"#;
    assert_eq!(
        build_interactive_document(snapshot("s", 1, out_of_range, 16)).err(),
        Some(SyncError::BadNumber)
    );
}

/// `AISNAP-20`・`AISNAP-21`: 差し替えは同じ `session` と現在の世代に限る。拒否したときは前の版を保つ。
#[test]
fn aisnap20_aisnap21_public_api_replace_checks_session_and_generation() {
    let (state, dir) = app_state("replace");
    let nav = state.navigation();
    let g = nav.begin_navigation().unwrap();
    state
        .commit_interactive(g, sync("s", 1, r#"<p data-fandhe-n="0">a</p>"#))
        .unwrap();
    state
        .replace_interactive(g, sync("s", 2, r#"<p data-fandhe-n="0">b</p>"#))
        .unwrap();
    let doc = Arc::clone(nav.latest().unwrap().interactive().unwrap());
    assert_eq!((doc.revision(), doc.page().doc_seq()), (2, 2));

    // ホストが作り直された（session が変わった）結果では差し替えない（`AISNAP-21`）。
    assert_eq!(
        state.replace_interactive(g, sync("other", 3, "<p></p>")),
        Err(StateError::SessionMismatch)
    );
    assert_eq!(nav.latest().unwrap().interactive().unwrap().revision(), 2);
    // 新しい navigate が始まった後の古い世代では差し替えない。
    let g2 = nav.begin_navigation().unwrap();
    assert!(matches!(
        state.replace_interactive(g, sync("s", 4, "<p></p>")),
        Err(StateError::Superseded { .. })
    ));

    // 静的モードで確定したページは差し替えの対象外。
    nav.commit_navigation(g2, NavigationResult::new("https://b.example/", "<p></p>"))
        .unwrap();
    assert_eq!(nav.latest().unwrap().mode(), PageMode::Static);
    assert_eq!(
        state.replace_interactive(g2, sync("s", 5, "<p></p>")),
        Err(StateError::NotInteractive)
    );
    drop(state);
    let _ = std::fs::remove_dir_all(&dir);
}

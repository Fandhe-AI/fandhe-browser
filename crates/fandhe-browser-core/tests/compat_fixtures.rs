//! CORE-1（`core-dom.md`。代表タスクの成功率 70% 以上）を、ローカルフィクスチャ
//! （`harness/compat_fixtures/`）に対して再測定するための結合テスト
//! （TASK-26（26.1）・Issue #136・MS-3）。
//!
//! `examples/compat_tasks/tasks.rs` を `#[path]` で取り込み、CLI
//! （`cargo run -p fandhe-browser-core --example compat_tasks -- local`）と
//! 同じ実行・判定ロジックを使う。これにより「スクリプトが実際に動く」ことを
//! `cargo test --workspace`（3 OS CI）で常時確認する。
//!
//! 成功率レポート（`docs/design/core1-success-rate.md`）の作成は TASK-26.2
//! （Issue #137）が本テストの対象外として担う。

#[path = "../examples/compat_tasks/tasks.rs"]
mod tasks;

use tasks::{
    Category, Expected, Outcome, Status, TARGET_RATE, TaskKind, judge, run_task, sanitize_sample,
};

fn parse_fixture(bytes: &[u8]) -> fandhe_browser_core::Document {
    fandhe_browser_core::parse_document_bytes(bytes, &fandhe_browser_core::ParseOptions::default())
        .expect("ローカルフィクスチャは既知の UTF-8 HTML であり、パースは必ず成功する")
        .document
}

/// CORE-1・TASK-26（26.1）: 静的類型の各フィクスチャが期待どおりの値を
/// 抽出できることを、タスク ID 付きで具体値検証する。
#[test]
fn core_1_static_fixtures_extract_expected_values() {
    for task in tasks::LOCAL_TASKS
        .iter()
        .filter(|t| t.category == Category::Static)
    {
        let document = parse_fixture(task.fixture);
        let outcome = run_task(&document, task.kind, task.selector)
            .unwrap_or_else(|_| panic!("{}: セレクタは対応サブセット内のはず", task.id));
        let status = judge(&outcome, task.expected);
        assert_eq!(
            status,
            Status::Ok,
            "{}: 期待値と一致するはず（実際の outcome: {outcome:?}）",
            task.id
        );
    }
}

/// CORE-1・TASK-26（26.1）: SSR/SPA 静的類型の各フィクスチャが期待どおりの
/// 値を抽出できることを、タスク ID 付きで具体値検証する。
#[test]
fn core_1_ssr_spa_static_fixtures_extract_expected_values() {
    for task in tasks::LOCAL_TASKS
        .iter()
        .filter(|t| t.category == Category::SsrSpaStatic)
    {
        let document = parse_fixture(task.fixture);
        let outcome = run_task(&document, task.kind, task.selector)
            .unwrap_or_else(|_| panic!("{}: セレクタは対応サブセット内のはず", task.id));
        let status = judge(&outcome, task.expected);
        assert_eq!(
            status,
            Status::Ok,
            "{}: 期待値と一致するはず（実際の outcome: {outcome:?}）",
            task.id
        );
    }
}

/// CORE-1・TASK-26（26.1）: `tasks::run_local` を通した類型別成功率が
/// CORE-1 の目標値（70% 以上）を満たし、かつ件数が具体値どおりであることを
/// 検証する（静的 8/8・SSR/SPA 静的 5/5。フィクスチャ表と揃える）。
#[test]
fn core_1_local_success_rate_meets_target_per_category() {
    let results = tasks::run_local();
    let (static_summary, ssr_spa_summary) = tasks::summarize(&results);

    assert_eq!(static_summary.attempted, 8, "静的類型は 8 タスクのはず");
    assert_eq!(
        static_summary.reachable, 8,
        "ローカルフィクスチャは全件パース可能なはず"
    );
    assert_eq!(static_summary.success, 8, "静的類型は全件成功するはず");
    assert!(
        static_summary.rate() >= TARGET_RATE,
        "静的類型の成功率が目標未達: {}",
        static_summary.rate()
    );

    assert_eq!(
        ssr_spa_summary.attempted, 5,
        "SSR/SPA 静的類型は 5 タスクのはず"
    );
    assert_eq!(ssr_spa_summary.reachable, 5);
    assert_eq!(
        ssr_spa_summary.success, 5,
        "SSR/SPA 静的類型は全件成功するはず"
    );
    assert!(
        ssr_spa_summary.rate() >= TARGET_RATE,
        "SSR/SPA 静的類型の成功率が目標未達: {}",
        ssr_spa_summary.rate()
    );
}

/// CORE-1・TASK-26（26.1）: CSR シェル（X01）は空の結果になり、
/// `Category::Excluded` として類型別の分母（`Summary`）に含まれないことを
/// 検証する（PoC-2 の扱いに合わせる。JS 統合後の再評価は TASK-30・JS-2）。
#[test]
fn core_1_csr_shell_is_excluded_and_empty() {
    let task = tasks::LOCAL_TASKS
        .iter()
        .find(|t| t.id == "X01")
        .expect("X01 は LOCAL_TASKS に含まれるはず");
    assert_eq!(task.category, Category::Excluded);

    let document = parse_fixture(task.fixture);
    let outcome = run_task(&document, task.kind, task.selector).expect("セレクタは解析できるはず");
    assert_eq!(outcome, Outcome::Values(Vec::new()));
    assert_eq!(judge(&outcome, task.expected), Status::ExpectedEmpty);

    let results = tasks::run_local();
    let (static_summary, ssr_spa_summary) = tasks::summarize(&results);
    assert_eq!(static_summary.attempted + ssr_spa_summary.attempted, 13);
}

/// CORE-1・TASK-26（26.1）: フォーム値の組み立て（harness 専用の計測補助
/// 関数。CORE-5 (4)・未判定）が、checked の checkbox・選択された radio を
/// 含め、unchecked・disabled を除外することを検証する（S03・P03 の
/// フィクスチャで確認する）。
#[test]
fn core_1_form_values_resolve_checked_and_skip_disabled() {
    let login = tasks::LOCAL_TASKS
        .iter()
        .find(|t| t.id == "S03")
        .expect("S03 は LOCAL_TASKS に含まれるはず");
    let document = parse_fixture(login.fixture);
    let outcome = run_task(&document, login.kind, login.selector).expect("form#login は解析できる");
    assert_eq!(
        outcome,
        Outcome::Pairs(vec![
            ("username".to_string(), "user".to_string()),
            ("password".to_string(), "dummy-password".to_string()),
            ("csrf_token".to_string(), "tok-123".to_string()),
            ("remember".to_string(), "on".to_string()),
        ])
    );

    let prefs = tasks::LOCAL_TASKS
        .iter()
        .find(|t| t.id == "P03")
        .expect("P03 は LOCAL_TASKS に含まれるはず");
    let document = parse_fixture(prefs.fixture);
    let outcome = run_task(&document, prefs.kind, prefs.selector).expect("form#prefs は解析できる");
    assert_eq!(
        outcome,
        Outcome::Pairs(vec![
            ("plan".to_string(), "pro".to_string()),
            ("notify_email".to_string(), "on".to_string()),
            ("bio".to_string(), "よろしくお願いします。".to_string()),
        ])
    );
}

/// CORE-1・TASK-26（26.1）: `sanitize_sample` が ANSI エスケープ等の制御文字を
/// 除去し、マルチバイト文字の境界を保ったまま切り詰めることを検証する
/// （実サイトモードで取得したテキストを端末へ出す前に必ず通す。
/// security.md「不安全な設計」端末インジェクション対策）。
#[test]
fn core_1_sanitize_sample_strips_control_chars_and_truncates() {
    assert_eq!(sanitize_sample("a\u{1b}[31mb"), "a[31mb");

    let long_text: String = std::iter::repeat_n('あ', 90).collect();
    let sanitized = sanitize_sample(&long_text);
    let expected: String = std::iter::repeat_n('あ', 80).chain(['…']).collect();
    assert_eq!(sanitized, expected);
    assert!(sanitized.chars().count() <= 81);
}

/// [`Expected`] を実際に使うことを確認する（`clippy --all-targets` が
/// テスト側で `Expected::NonEmpty` 等の未使用 variant を dead code 扱い
/// しないようにする。`main.rs` の実サイトモードで使う variant を
/// ここでも軽く確認しておく）。
#[test]
fn core_1_expected_non_empty_matches_any_non_empty_outcome() {
    let outcome = Outcome::Values(vec!["x".to_string()]);
    assert_eq!(judge(&outcome, Expected::NonEmpty), Status::Ok);
    let empty = Outcome::Values(Vec::new());
    assert_eq!(judge(&empty, Expected::NonEmpty), Status::UnexpectedEmpty);
}

/// CORE-1・TASK-26（26.1）: `Category::label`/`Status::label`（CLI 出力の
/// 整形。`main.rs` の `local`/`real` 両モードが使う）と、実サイトモード限定の
/// `Status::Http`/`Status::FetchError` variant を検証する。`tasks.rs` は
/// example とテストの両方でコンパイルされるため、どちらか一方でしか使わない
/// 項目があると `clippy --all-targets -D warnings` が dead code として
/// 検出する。ここで検証することで、`main.rs` 専用の項目もテスト対象に含める。
#[test]
fn core_1_category_and_status_labels_format_for_cli_output() {
    assert_eq!(Category::Static.label(), "static");
    assert_eq!(Category::SsrSpaStatic.label(), "ssr_spa_static");
    assert_eq!(Category::Excluded.label(), "excluded");

    assert_eq!(Status::Ok.label(), "ok");
    assert_eq!(Status::Mismatch.label(), "mismatch");
    assert_eq!(Status::ExpectedEmpty.label(), "expected-empty");
    assert_eq!(Status::UnexpectedEmpty.label(), "unexpected-empty");
    assert_eq!(Status::SelectorUnsupported.label(), "selector-unsupported");
    assert_eq!(Status::ParseError.label(), "parse-error");
    // 以下 2 variant は `main.rs` の実サイトモード（`fetch_and_run`）専用。
    assert_eq!(Status::Http(404).label(), "http-404");
    assert_eq!(
        Status::FetchError("request-failed").label(),
        "fetch-error:request-failed"
    );
    assert!(!Status::Http(404).is_reachable());
    assert!(!Status::FetchError("request-failed").is_reachable());
}

/// CORE-1・TASK-26（26.1）: `run_local` が返す [`tasks::TaskResult`] の
/// `id`/`kind`/`sample` フィールド（CLI の出力行が使う）を検証する。
#[test]
fn core_1_local_task_result_exposes_id_kind_and_sample() {
    let results = tasks::run_local();
    let s01 = results
        .iter()
        .find(|r| r.id == "S01")
        .expect("S01 は run_local の結果に含まれるはず");
    assert_eq!(s01.kind, TaskKind::Texts);
    assert_eq!(s01.status, Status::Ok);
    assert_eq!(s01.sample, "Rust の非同期ランタイム入門 | 相田 藍子");
}

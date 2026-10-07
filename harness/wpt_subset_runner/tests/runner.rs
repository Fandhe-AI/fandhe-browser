//! ハーネスの一気通貫テスト（`PLUG-10`・TASK-101.6・Issue #278）。
//!
//! 少数のダミー testharness ケース（既知に合格・既知に失敗・未完了・結果なし・不在・
//! reftest / other）を一時ツリーへ置き、`parse_subset_tsv` → `run_subset_for_profiles` →
//! `WptReport::from_runs` → `WptReport::to_json` の経路全体を、実際に JS を実行した結果で検証する。
//! JSON は期待文字列との完全一致（golden）で固定し、`to_json` 自身から期待値を作らない。
//!
//! `tests/runner_subset.rs` は `run_entry` のファイル単位の分類を網羅する。本ファイルは
//! TSV からレポートまでの経路と、合格のみ・失敗のみ・混在の各サブセットの合否・レポートを担う。
//! 偽 testharness は `tests/fixtures/fake_testharness.js` を再利用する（WPT 本体は同梱しない）。
//!
//! V8 は子プロセス版のため `main` の先頭で `run_js_worker_if_requested` が必要で `harness = false`。
//! エンジン構成は `bundled_engines()` で分岐し、skip にしない。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fandhe_browser_core::{EngineKind, bundled_engines};
use wpt_subset_runner::runner::{WptProfile, parse_subset_tsv, run_subset_for_profiles};
use wpt_subset_runner::{FileOutcome, HarnessKind, RunOptions, SubtestStatus, Verdict, WptReport};

const FAKE_TESTHARNESS: &str = include_str!("fixtures/fake_testharness.js");

const HEAD: &str = "<!DOCTYPE html>\n<script src=\"/resources/testharness.js\"></script>\n\
                    <script src=\"/resources/testharnessreport.js\"></script>\n";

/// panic 時にも一時ツリーを消すガード。
struct TreeGuard(PathBuf);

impl Drop for TreeGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

fn page(script: &str) -> String {
    format!("{HEAD}<script>\n{script}\n</script>\n")
}

/// 一意な一時ディレクトリを原子的に新規作成する。
///
/// `create_dir` は既存パスに対して `AlreadyExists` で失敗するため、PID の再利用や
/// 他プロセスとの衝突時も既存ディレクトリを削除せず、別名で再試行する。
/// 作成に成功したディレクトリのみが `TreeGuard` の削除対象になる。
fn create_unique_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for _ in 0..100 {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wpt-runner-e2e-{}-{nanos}-{seq}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => return root,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("create temp dir: {e}"),
        }
    }
    panic!("could not create a unique temp dir");
}

/// ダミーケースの一時ツリーを作る。`dom/absent.html`・reftest・other は作らない。
fn build_tree() -> TreeGuard {
    let root = create_unique_dir();
    write(&root, "resources/testharness.js", FAKE_TESTHARNESS);
    write(
        &root,
        "dom/known-pass-single.html",
        &page("test(function () { assert_true(true); }, 'only');\ndone();"),
    );
    write(
        &root,
        "dom/known-pass-multi.html",
        &page(
            "test(function () { assert_true(true); }, 'a');\n\
             test(function () { assert_true(1 === 1); }, 'b');\n\
             test(function () { assert_true(true); }, 'c');\ndone();",
        ),
    );
    write(
        &root,
        "dom/known-fail-assert.html",
        &page(
            "test(function () { assert_true(true); }, 'ok');\n\
             test(function () { assert_true(false, 'boom'); }, 'bad');\ndone();",
        ),
    );
    write(
        &root,
        "dom/known-fail-harness-error.html",
        &page("test(function () { assert_true(true); }, 'ok');\ndone_with_error();"),
    );
    write(
        &root,
        "dom/incomplete.html",
        &page("test(function () { assert_true(true); }, 'ok');"),
    );
    write(&root, "dom/no-results.html", &page("1 + 1;"));
    TreeGuard(root)
}

fn tsv(rows: &[(&str, &str)]) -> String {
    rows.iter().map(|(h, f)| format!("{h}\t{f}\n")).collect()
}

const PASS_ROWS: [(&str, &str); 2] = [
    ("testharness", "dom/known-pass-single.html"),
    ("testharness", "dom/known-pass-multi.html"),
];
const FAIL_ROWS: [(&str, &str); 2] = [
    ("testharness", "dom/known-fail-assert.html"),
    ("testharness", "dom/known-fail-harness-error.html"),
];
const MIXED_ROWS: [(&str, &str); 9] = [
    ("testharness", "dom/known-pass-single.html"),
    ("testharness", "dom/known-pass-multi.html"),
    ("testharness", "dom/known-fail-assert.html"),
    ("testharness", "dom/known-fail-harness-error.html"),
    ("testharness", "dom/incomplete.html"),
    ("testharness", "dom/no-results.html"),
    ("testharness", "dom/absent.html"),
    ("reftest", "css/ref-a.html"),
    ("other", "misc/manual-b.html"),
];

type OutcomeCounts = [(&'static str, usize); 15];

/// `byOutcome` の期待値（キー順は `report.rs` の出力順。実行結果とは独立に書く）。
fn outcome_counts(
    skipped: usize,
    missing: usize,
    engine_unavailable: usize,
    completed: usize,
) -> OutcomeCounts {
    [
        ("skipped", skipped),
        ("missing", missing),
        ("readFailed", 0),
        ("tooLarge", 0),
        ("limitExceeded", 0),
        ("htmlParseFailed", 0),
        ("harnessNotReferenced", 0),
        ("harnessLoadFailed", 0),
        ("unsupportedScript", 0),
        ("supportScriptMissing", 0),
        ("scriptRejected", 0),
        ("scriptFailed", 0),
        ("engineUnavailable", engine_unavailable),
        ("collectFailed", 0),
        ("completed", completed),
    ]
}

/// 1 プロファイル節の期待 JSON。`counts` は (total, executed, passed)、`verdict` は
/// (pass, fail, noResults, incomplete)。
fn profile_json(
    name: &str,
    counts: (usize, usize, usize),
    rate: &str,
    verdict: [usize; 4],
    outcome: OutcomeCounts,
) -> String {
    let [p, f, n, i] = verdict;
    let outcome: Vec<String> = outcome
        .iter()
        .map(|(k, c)| format!("\"{k}\":{c}"))
        .collect();
    format!(
        "{{\"profile\":\"{name}\",\"total\":{},\"executed\":{},\"passed\":{},\"passRate\":{rate},\
         \"byVerdict\":{{\"pass\":{p},\"fail\":{f},\"noResults\":{n},\"incomplete\":{i}}},\
         \"byOutcome\":{{{}}}}}",
        counts.0,
        counts.1,
        counts.2,
        outcome.join(",")
    )
}

/// `unrunnable` 節の期待 JSON（文言まで固定）。
fn unrunnable_json(reftest: &[&str], other: &[&str]) -> String {
    let list = |v: &[&str]| {
        v.iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{{\"schemaVersion\":1,\"total\":{},\"byReason\":[\
         {{\"reason\":\"reftest-comparison-not-implemented\",\"basis\":\"confirmed\",\
         \"harness\":\"reftest\",\"description\":\"reftest needs a rendering comparison against a \
         reference page; this harness does not implement it\",\"count\":{},\"files\":[{}]}},\
         {{\"reason\":\"unverified-likely-unrunnable\",\"basis\":\"speculative\",\
         \"harness\":\"other\",\"description\":\"content not verified; likely unrunnable \
         (speculative)\",\"count\":{},\"files\":[{}]}}]}}",
        reftest.len() + other.len(),
        reftest.len(),
        list(reftest),
        other.len(),
        list(other)
    )
}

/// 全プロファイル（chrome・safari の順）が同じ節を持つレポート全体の期待 JSON。
fn report_json(
    engine: &str,
    counts: (usize, usize, usize),
    rate: &str,
    verdict: [usize; 4],
    outcome: OutcomeCounts,
    unrunnable: &str,
) -> String {
    let profiles: Vec<String> = WptProfile::ALL
        .iter()
        .map(|p| profile_json(p.as_str(), counts, rate, verdict, outcome))
        .collect();
    format!(
        "{{\"schemaVersion\":1,\"behavior\":\"PLUG-10\",\"engine\":\"{engine}\",\"profiles\":[{}],\
         \"unrunnable\":{unrunnable}}}",
        profiles.join(",")
    )
}

fn run_profiles(
    root: &Path,
    engine: Option<EngineKind>,
    rows: &[(&str, &str)],
) -> Vec<wpt_subset_runner::ProfileRun> {
    // TSV 文字列を必ず parse_subset_tsv に通す（ランナー入力の経路を含める）。
    let entries = parse_subset_tsv(&tsv(rows)).expect("parse tsv");
    assert_eq!(entries.len(), rows.len());
    let opts = RunOptions::new(root, engine);
    run_subset_for_profiles(&opts, &entries, &WptProfile::ALL).expect("profile runs")
}

fn run_report(root: &Path, engine: Option<EngineKind>, rows: &[(&str, &str)]) -> WptReport {
    let runs = run_profiles(root, engine, rows);
    WptReport::from_runs(&runs, engine).expect("report")
}

/// 指定エンジンで全シナリオを検証する。`available` は JS が実際に走るか。
fn run_scenarios(root: &Path, engine: Option<EngineKind>, available: bool) {
    let label = engine.map_or("none", |e| e.as_str());
    let none = unrunnable_json(&[], &[]);

    // A. 合格のみ（既知に合格するケース）。エンジンが無ければ合格を装わない。
    let report = run_report(root, engine, &PASS_ROWS);
    for p in &report.profiles {
        assert_eq!((p.total, p.executed), (2, 2));
        if available {
            assert_eq!(p.passed, 2);
            assert_eq!(p.pass_rate(), Some(1.0));
            assert_eq!(p.verdict_count("pass"), 2);
        } else {
            assert_eq!(p.passed, 0);
            assert_eq!(p.pass_rate(), Some(0.0));
            assert_eq!(p.outcome_count("engineUnavailable"), 2);
        }
    }
    let expected = if available {
        report_json(
            label,
            (2, 2, 2),
            "1.0000",
            [2, 0, 0, 0],
            outcome_counts(0, 0, 0, 2),
            &none,
        )
    } else {
        report_json(
            label,
            (2, 2, 0),
            "0.0000",
            [0, 0, 0, 0],
            outcome_counts(0, 0, 2, 0),
            &none,
        )
    };
    assert_eq!(report.to_json(), expected);

    // B. 失敗のみ（既知に失敗するケース）。
    let report = run_report(root, engine, &FAIL_ROWS);
    for p in &report.profiles {
        assert_eq!((p.total, p.executed, p.passed), (2, 2, 0));
        assert_eq!(p.pass_rate(), Some(0.0));
    }
    let expected = if available {
        report_json(
            label,
            (2, 2, 0),
            "0.0000",
            [0, 2, 0, 0],
            outcome_counts(0, 0, 0, 2),
            &none,
        )
    } else {
        report_json(
            label,
            (2, 2, 0),
            "0.0000",
            [0, 0, 0, 0],
            outcome_counts(0, 0, 2, 0),
            &none,
        )
    };
    assert_eq!(report.to_json(), expected);

    // C. 混在。エントリ別の期待を具体値で確認する。
    let runs = run_profiles(root, engine, &MIXED_ROWS);
    let expected_verdicts = [
        Verdict::Pass,
        Verdict::Pass,
        Verdict::Fail,
        Verdict::Fail,
        Verdict::Incomplete,
        Verdict::NoResults,
    ];
    for run in &runs {
        assert_eq!(run.results.len(), MIXED_ROWS.len());
        for (idx, (entry, outcome)) in run.results.iter().enumerate() {
            let (_, file) = MIXED_ROWS.get(idx).expect("row");
            assert_eq!(entry.file, *file);
            match (idx, available) {
                (0..=5, true) => {
                    let FileOutcome::Completed {
                        subtests, verdict, ..
                    } = outcome
                    else {
                        panic!("{file}: {outcome:?}");
                    };
                    assert_eq!(Some(verdict), expected_verdicts.get(idx), "{file}");
                    if idx == 1 {
                        let names: Vec<&str> = subtests.iter().map(|s| s.name.as_str()).collect();
                        assert_eq!(names, ["a", "b", "c"]);
                    }
                    if idx == 2 {
                        let bad = subtests.get(1).expect("second subtest");
                        assert_eq!(bad.name, "bad");
                        assert_eq!(bad.status, SubtestStatus::Fail);
                        assert_eq!(bad.message.as_deref(), Some("assert_true: boom"));
                    }
                }
                (0..=5, false) => assert!(
                    matches!(outcome, FileOutcome::EngineUnavailable { .. }),
                    "{file}: {outcome:?}"
                ),
                (6, _) => assert_eq!(outcome, &FileOutcome::Missing),
                (7, _) => assert_eq!(
                    outcome,
                    &FileOutcome::Skipped {
                        harness: HarnessKind::Reftest
                    }
                ),
                _ => assert_eq!(
                    outcome,
                    &FileOutcome::Skipped {
                        harness: HarnessKind::Other
                    }
                ),
            }
        }
    }
    // TASK-100（PLUG-8）の gating 配線前はプロファイル間で結果が同一（偽の差分を作らない）。
    // 配線後はこの等価 assert を差分検証へ置き換える。
    assert_eq!(runs[0].results, runs[1].results);

    // 分母は Skipped 以外の全件。Missing・Incomplete・NoResults も分母に入り合格に数えない。
    let report = WptReport::from_runs(&runs, engine).expect("report");
    let unrunnable = unrunnable_json(&["css/ref-a.html"], &["misc/manual-b.html"]);
    let expected = if available {
        report_json(
            label,
            (9, 7, 2),
            "0.2857",
            [2, 2, 1, 1],
            outcome_counts(2, 1, 0, 6),
            &unrunnable,
        )
    } else {
        report_json(
            label,
            (9, 7, 0),
            "0.0000",
            [0, 0, 0, 0],
            outcome_counts(2, 1, 6, 0),
            &unrunnable,
        )
    };
    assert_eq!(report.to_json(), expected);

    // 再実行しても同じレポートになる。
    assert_eq!(run_report(root, engine, &MIXED_ROWS).to_json(), expected);
}

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    let tree = build_tree();
    let root = tree.0.as_path();
    let bundled = bundled_engines();

    eprintln!("scenarios: engine none");
    run_scenarios(root, None, false);
    if bundled.contains(&EngineKind::V8) {
        eprintln!("scenarios: v8");
        run_scenarios(root, Some(EngineKind::V8), true);
    }
    if bundled.contains(&EngineKind::Boa) {
        if cfg!(target_os = "macos") {
            eprintln!("scenarios: boa is unavailable on macOS");
            run_scenarios(root, Some(EngineKind::Boa), false);
        } else {
            eprintln!("scenarios: boa");
            run_scenarios(root, Some(EngineKind::Boa), true);
        }
    }
    eprintln!("all scenarios passed");
    ExitCode::SUCCESS
}

//! WPT サブセットランナーの結合テスト（`PLUG-10`・TASK-101.2.2・Issue #554）。
//!
//! 一時ディレクトリに偽の WPT ツリー（`tests/fixtures/fake_testharness.js` を
//! `resources/testharness.js` として配置）を組み立て、`run_entry` のファイル単位の
//! 分類を具体値で検証する。WPT 本体のコードは同梱しない。
//!
//! V8 は子プロセス版のため `main` の先頭で `run_js_worker_if_requested` を呼ぶ必要があり
//! `harness = false`（`testharness_env.rs` と同じ作法）。エンジン構成は `bundled_engines()`
//! （実際にリンクされたエンジン）で分岐し、skip にしない。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fandhe_browser_core::{EngineKind, bundled_engines};
use std::time::Duration;
use wpt_subset_runner::runner::{
    LimitKind, RunLimits, RunOptions, SubsetEntry, WptProfile, run_entry, run_subset,
    run_subset_for_profiles,
};
use wpt_subset_runner::{FileOutcome, HarnessKind, SubtestStatus, Verdict};

const FAKE_TESTHARNESS: &str = include_str!("fixtures/fake_testharness.js");

const HEAD: &str = "<!DOCTYPE html>\n<script src=\"/resources/testharness.js\"></script>\n\
                    <script src=\"/resources/testharnessreport.js\"></script>\n";

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

/// 偽 WPT ツリーを一時ディレクトリに作る（`testharnessreport.js` は置かない）。
fn build_tree() -> PathBuf {
    let root = std::env::temp_dir().join(format!("wpt-runner-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    write(&root, "resources/testharness.js", FAKE_TESTHARNESS);
    write(
        &root,
        "dom/pass.html",
        &format!(
            "{HEAD}<script>\n\
             test(function () {{ assert_true(true); }}, 'first');\n\
             test(function () {{ assert_true(1 === 1); }}, 'second');\n\
             done();\n\
             </script>\n"
        ),
    );
    write(
        &root,
        "dom/fail.html",
        &format!(
            "{HEAD}<script>\n\
             test(function () {{ assert_true(true); }}, 'ok');\n\
             test(function () {{ assert_true(false, 'boom'); }}, 'bad');\n\
             </script>\n"
        ),
    );
    write(
        &root,
        "dom/harness-error.html",
        &format!(
            "{HEAD}<script>\n\
             test(function () {{ assert_true(true); }}, 'ok');\n\
             done_with_error();\n\
             </script>\n"
        ),
    );
    // completion が届かない（未完了テストが残り得る）ファイルは Pass にしない。
    write(
        &root,
        "dom/no-completion.html",
        &format!("{HEAD}<script>test(function () {{ assert_true(true); }}, 'ok');</script>\n"),
    );
    write(
        &root,
        "dom/nosubtests.html",
        &format!("{HEAD}<script>1 + 1;</script>\n"),
    );
    write(
        &root,
        "dom/support.html",
        &format!(
            "{HEAD}<script src=\"support/helper.js\"></script>\n\
             <script>test(function () {{ assert_true(helperValue === 7); }}, 'uses helper'); done();</script>\n"
        ),
    );
    write(&root, "dom/support/helper.js", "var helperValue = 7;\n");
    // ちょうど 1 MiB のサポートスクリプト。評価時に接尾辞が加わり JsRuntime の上限を超えるため、
    // 実行時のサイズ超過ではなく TooLarge として扱う。
    write(
        &root,
        "dom/big-support.html",
        &format!("{HEAD}<script src=\"support/big.js\"></script>\n"),
    );
    write(
        &root,
        "dom/support/big.js",
        &format!("//{}", "a".repeat(1024 * 1024 - 2)),
    );
    write(
        &root,
        "dom/missing-support.html",
        &format!("{HEAD}<script src=\"/resources/nope.js\"></script>\n"),
    );
    write(
        &root,
        "dom/remote.html",
        &format!("{HEAD}<script src=\"https://example.com/x.js\"></script>\n"),
    );
    write(
        &root,
        "dom/no-harness.html",
        "<!DOCTYPE html><script>1;</script>\n",
    );
    write(
        &root,
        "dom/throws.html",
        &format!("{HEAD}<script>throw new Error('script error');</script>\n"),
    );
    write(
        &root,
        "dom/module.html",
        &format!(
            "{HEAD}<script type=\"module\">this is not valid js</script>\n\
             <script>test(function () {{ assert_true(true); }}, 'only classic');</script>\n"
        ),
    );
    // template 内のスクリプトは実行されない文書の一部ではないため、計画に含めない。
    write(
        &root,
        "dom/template.html",
        &format!(
            "{HEAD}<template><script>throw new Error('in template');</script></template>\n\
             <script>test(function () {{ assert_true(true); }}, 'outside');done();</script>\n"
        ),
    );
    write(
        &root,
        "dom/template-harness-only.html",
        "<!DOCTYPE html>\n<template><script src=\"/resources/testharness.js\"></script></template>\n",
    );
    // 総量上限（PLUG-10）の回帰用。インライン 3 本 + 外部サポート 1 本。
    write(
        &root,
        "dom/many.html",
        &format!(
            "{HEAD}<script>var a=1;</script><script>var b=2;</script>\n\
             <script src=\"support/mid.js\"></script>\n"
        ),
    );
    write(
        &root,
        "dom/support/mid.js",
        &format!("//{}", "m".repeat(5000)),
    );
    root
}

fn entry(file: &str, harness: HarnessKind) -> SubsetEntry {
    SubsetEntry::new(file, harness).expect("entry")
}

fn run_cases(root: &Path, engine: Option<EngineKind>) {
    let opts = RunOptions::new(root, engine);
    let th = |f: &str| run_entry(&opts, &entry(f, HarnessKind::Testharness));

    match th("dom/pass.html") {
        FileOutcome::Completed {
            subtests, verdict, ..
        } => {
            assert_eq!(verdict, Verdict::Pass);
            let names: Vec<&str> = subtests.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["first", "second"]);
            assert!(subtests.iter().all(|s| s.status == SubtestStatus::Pass));
        }
        other => panic!("pass.html: {other:?}"),
    }

    match th("dom/fail.html") {
        FileOutcome::Completed {
            subtests, verdict, ..
        } => {
            assert_eq!(verdict, Verdict::Fail);
            let bad = subtests.get(1).expect("second subtest");
            assert_eq!(bad.name, "bad");
            assert_eq!(bad.status, SubtestStatus::Fail);
            assert_eq!(bad.message.as_deref(), Some("assert_true: boom"));
        }
        other => panic!("fail.html: {other:?}"),
    }

    // completion が OK 以外なら、サブテストが全件 PASS でも Fail。
    match th("dom/harness-error.html") {
        FileOutcome::Completed {
            subtests,
            completion,
            verdict,
        } => {
            assert!(subtests.iter().all(|s| s.status == SubtestStatus::Pass));
            assert_eq!(
                completion.map(|c| c.status),
                Some(wpt_subset_runner::HarnessStatus::Error)
            );
            assert_eq!(verdict, Verdict::Fail);
        }
        other => panic!("harness-error.html: {other:?}"),
    }

    assert_eq!(th("dom/big-support.html"), FileOutcome::TooLarge);

    match th("dom/no-completion.html") {
        FileOutcome::Completed {
            completion,
            verdict,
            ..
        } => {
            assert!(completion.is_none());
            assert_eq!(verdict, Verdict::Incomplete);
        }
        other => panic!("no-completion.html: {other:?}"),
    }

    match th("dom/nosubtests.html") {
        FileOutcome::Completed {
            subtests, verdict, ..
        } => {
            assert!(subtests.is_empty());
            assert_eq!(verdict, Verdict::NoResults);
        }
        other => panic!("nosubtests.html: {other:?}"),
    }

    match th("dom/support.html") {
        FileOutcome::Completed {
            subtests, verdict, ..
        } => {
            assert_eq!(verdict, Verdict::Pass);
            assert_eq!(subtests.first().expect("subtest").name, "uses helper");
        }
        other => panic!("support.html: {other:?}"),
    }

    // type=module を含むファイルは、classic だけで Pass を装わず実行不能にする。
    assert_eq!(
        th("dom/module.html"),
        FileOutcome::UnsupportedScript {
            script_type: "module".to_string()
        }
    );

    assert_eq!(
        th("dom/missing-support.html"),
        FileOutcome::SupportScriptMissing {
            src: "/resources/nope.js".to_string()
        }
    );
    match th("dom/remote.html") {
        FileOutcome::ScriptRejected { src, reason } => {
            assert_eq!(src, "https://example.com/x.js");
            assert_eq!(reason, "URL with scheme is not allowed");
        }
        other => panic!("remote.html: {other:?}"),
    }
    assert_eq!(th("dom/no-harness.html"), FileOutcome::HarnessNotReferenced);
    assert_eq!(th("dom/does-not-exist.html"), FileOutcome::Missing);
    match th("dom/throws.html") {
        FileOutcome::ScriptFailed { index, error } => {
            // testharness.js（0 番）の次のインラインスクリプトは文書順 1 番。
            assert_eq!(index, 1);
            assert!(error.contains("script error"), "{error}");
        }
        other => panic!("throws.html: {other:?}"),
    }

    match th("dom/template.html") {
        FileOutcome::Completed {
            subtests, verdict, ..
        } => {
            assert_eq!(verdict, Verdict::Pass);
            assert_eq!(subtests.first().expect("subtest").name, "outside");
        }
        other => panic!("template.html: {other:?}"),
    }
    assert_eq!(
        th("dom/template-harness-only.html"),
        FileOutcome::HarnessNotReferenced
    );

    // PLUG-10: 件数・合計サイズ・時間の総量上限超過はエラーとして記録する。
    let limited = |l: RunLimits| {
        run_entry(
            &RunOptions::new(root, engine).with_limits(l),
            &entry("dom/many.html", HarnessKind::Testharness),
        )
    };
    let base = RunLimits::default();
    // testharness.js + inline 2 本 + support 1 本 = 4 手順
    let mut l = base;
    l.max_steps = 3;
    assert_eq!(
        limited(l),
        FileOutcome::LimitExceeded {
            kind: LimitKind::Steps
        }
    );
    let mut l = base;
    l.max_total_bytes = 1000; // support 5002 バイトで超過
    assert_eq!(
        limited(l),
        FileOutcome::LimitExceeded {
            kind: LimitKind::TotalBytes
        }
    );
    let mut l = base;
    l.max_duration = Duration::ZERO;
    assert_eq!(
        limited(l),
        FileOutcome::LimitExceeded {
            kind: LimitKind::Duration
        }
    );
    // 上限ちょうどなら通る（境界）。
    let mut l = base;
    l.max_steps = 4;
    assert!(matches!(limited(l), FileOutcome::Completed { .. }));

    // reftest・other は読まずにスキップ（ファイルが無くても Skipped）。
    for kind in [HarnessKind::Reftest, HarnessKind::Other] {
        assert_eq!(
            run_entry(&opts, &entry("css/not-there.html", kind)),
            FileOutcome::Skipped { harness: kind }
        );
    }

    // PLUG-10（TASK-101.3）: プロファイルごとにタグ付けされた独立の結果集合が得られる。
    let entries = [
        entry("dom/pass.html", HarnessKind::Testharness),
        entry("dom/fail.html", HarnessKind::Testharness),
        entry("css/not-there.html", HarnessKind::Reftest),
    ];
    let runs = run_subset_for_profiles(&opts, &entries, &WptProfile::ALL).expect("profile runs");
    let tags: Vec<WptProfile> = runs.iter().map(|r| r.profile).collect();
    assert_eq!(tags, [WptProfile::Chrome, WptProfile::Safari]);
    for run in &runs {
        let files: Vec<&str> = run.results.iter().map(|(e, _)| e.file.as_str()).collect();
        assert_eq!(
            files,
            ["dom/pass.html", "dom/fail.html", "css/not-there.html"]
        );
        if engine.is_some() {
            let verdicts: Vec<Option<Verdict>> = run
                .results
                .iter()
                .map(|(_, o)| match o {
                    FileOutcome::Completed { verdict, .. } => Some(*verdict),
                    _ => None,
                })
                .collect();
            assert_eq!(verdicts, [Some(Verdict::Pass), Some(Verdict::Fail), None]);
        } else {
            assert!(matches!(
                &run.results[0].1,
                FileOutcome::EngineUnavailable { .. }
            ));
            assert!(matches!(
                &run.results[1].1,
                FileOutcome::EngineUnavailable { .. }
            ));
        }
        assert_eq!(
            run.results[2].1,
            FileOutcome::Skipped {
                harness: HarnessKind::Reftest
            }
        );
    }
    // TASK-100 配線後は、この等価 assert をプロファイル間の差分検証へ置き換える。
    assert_eq!(runs[0].results, runs[1].results);
    // プロファイル未指定の従来経路と結果が一致する（従来挙動を変えていない）。
    assert_eq!(run_subset(&opts, &entries), runs[0].results);
}

fn main() -> ExitCode {
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    let root = build_tree();
    let bundled = bundled_engines();

    {
        // engine=None は JS 無効。コンパイル済みエンジンがあっても既定へ落とさない。
        eprintln!("case: engine none");
        match run_entry(
            &RunOptions::new(&root, None),
            &entry("dom/pass.html", HarnessKind::Testharness),
        ) {
            FileOutcome::EngineUnavailable { .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }
    if bundled.contains(&EngineKind::V8) {
        eprintln!("cases (v8)");
        run_cases(&root, Some(EngineKind::V8));
    }
    if bundled.contains(&EngineKind::Boa) {
        if cfg!(target_os = "macos") {
            eprintln!("case: boa is unavailable on macOS");
            match run_entry(
                &RunOptions::new(&root, Some(EngineKind::Boa)),
                &entry("dom/pass.html", HarnessKind::Testharness),
            ) {
                FileOutcome::EngineUnavailable { .. } => {}
                other => panic!("unexpected: {other:?}"),
            }
        } else {
            eprintln!("cases (boa)");
            run_cases(&root, Some(EngineKind::Boa));
        }
    }
    let _ = fs::remove_dir_all(&root);
    eprintln!("all cases passed");
    ExitCode::SUCCESS
}

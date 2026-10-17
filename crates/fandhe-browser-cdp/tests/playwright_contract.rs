//! `harness/playwright-connect/stages.mjs` の結果行と Rust 側パーサー（`script_harness`）の
//! 契約テスト（TASK-44.1・#476、ビヘイビア `CDP-2`・MS-4）。
//!
//! node 必須のため既定の `cargo test` では `#[ignore]` とし、`make check-playwright-connect`
//! （`harness/playwright-connect/self-test.sh`）が `--ignored --exact` で実行する。
//! `Profile::open` を使わず `cfg(unix)` を付けない（`puppeteer_contract.rs` と同じ構成）。

#[path = "script_harness/runner.rs"]
mod script_harness;

use std::time::Duration;

use script_harness::{
    ScriptCommand, ScriptError, ScriptOutcome, StageResult, StageStatus, run_script,
};

const D: Duration = Duration::from_secs(30);

fn stage(name: &str, status: StageStatus, error: Option<(&str, &str)>) -> StageResult {
    StageResult {
        name: name.into(),
        status,
        error: error.map(|(n, m)| ScriptError {
            name: n.into(),
            message: m.into(),
        }),
    }
}

/// CDP-2: 実 `connect.mjs` と同じ生成ロジック（`stages.mjs` の `runStages`）が出す結果行を
/// Rust 側パーサーで回収できる（JS 側と Rust 側のスキーマ乖離の検知）。
#[test]
#[ignore = "requires node; run via `make check-playwright-connect`"]
fn cdp2_stages_mjs_result_line_satisfies_rust_contract() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/playwright-connect");
    let cmd = |mode: &str| ScriptCommand {
        program: "node".into(),
        args: vec!["contract-sample.mjs".into(), mode.into()],
        envs: Vec::new(),
        cwd: Some(dir.clone()),
    };
    assert_eq!(
        run_script(&cmd("ok"), "http://127.0.0.1:1", D),
        ScriptOutcome::Completed {
            ok: true,
            step: "selector".into(),
            error: None,
            stages: ["connect", "newContext", "newPage", "goto", "selector"]
                .map(|n| stage(n, StageStatus::Ok, None))
                .to_vec(),
        }
    );
    let err = ("Error", "context boom");
    assert_eq!(
        run_script(&cmd("fail_context"), "http://127.0.0.1:1", D),
        ScriptOutcome::Completed {
            ok: false,
            step: "newContext".into(),
            error: Some(ScriptError {
                name: err.0.into(),
                message: err.1.into()
            }),
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newContext", StageStatus::Failed, Some(err)),
                stage("newPage", StageStatus::NotReached, None),
                stage("goto", StageStatus::NotReached, None),
                stage("selector", StageStatus::NotReached, None),
            ],
        }
    );
    let mm = (
        "SelectorMismatch",
        "expected text \"Hello fixture\" got \"Bye\"",
    );
    assert_eq!(
        run_script(&cmd("selector_mismatch"), "http://127.0.0.1:1", D),
        ScriptOutcome::Completed {
            ok: false,
            step: "selector".into(),
            error: Some(ScriptError {
                name: mm.0.into(),
                message: mm.1.into()
            }),
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newContext", StageStatus::Ok, None),
                stage("newPage", StageStatus::Ok, None),
                stage("goto", StageStatus::Ok, None),
                stage("selector", StageStatus::Failed, Some(mm)),
            ],
        }
    );
}

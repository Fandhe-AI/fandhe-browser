//! `stages.mjs` の結果行と Rust 側パーサー（`script_harness`）の契約テスト
//! （TASK-45.2・#481、ビヘイビア `CDP-3`・MS-4）。
//!
//! node 必須のため既定の `cargo test` では `#[ignore]` とし、`make check-puppeteer-connect`
//! （`harness/puppeteer-connect/self-test.sh`。CI では `puppeteer-connect-selftest` ジョブ・3 OS）が
//! `--ignored` 付きで実行する。`Profile::open` を使わないため `puppeteer_connect.rs` と違い
//! `cfg(unix)` を付けず、Windows でも実際にコンパイル・実行する。

// サーバー起動など本ターゲットで使わない script_harness の公開項目を許容する
// （`puppeteer_connect.rs` と共有するモジュールのため）。
#[allow(dead_code)]
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

/// CDP-3: 実 `connect.mjs` と同じ生成ロジック（`stages.mjs` の `runStages`）が出す結果行を
/// Rust 側パーサーで回収できる（JS 側と Rust 側のスキーマ乖離の検知）。
/// node 必須のため `cargo test --workspace` の既定実行から外し、node 必須と明示された
/// `make check-puppeteer-connect`（`harness/puppeteer-connect/self-test.sh`）が
/// `--ignored` 付きで実行する。
#[test]
#[ignore = "requires node; run via `make check-puppeteer-connect`"]
fn cdp3_stages_mjs_result_line_satisfies_rust_contract() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/puppeteer-connect");
    let cmd = |mode: &str| ScriptCommand {
        program: "node".into(),
        args: vec!["contract-sample.mjs".into(), mode.into()],
        envs: Vec::new(),
        cwd: Some(dir.clone()),
    };
    let names = ["connect", "newPage", "goto", "selector", "disconnect"];
    assert_eq!(
        run_script(&cmd("ok"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: true,
            step: "disconnect".into(),
            error: None,
            stages: names.map(|n| stage(n, StageStatus::Ok, None)).to_vec(),
        }
    );
    // 失敗時は exit 1 のため、ok:false の結果行がそのまま回収される。
    let err = ("ProtocolError", "goto boom");
    assert_eq!(
        run_script(&cmd("fail_goto"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: false,
            step: "goto".into(),
            error: Some(ScriptError {
                name: err.0.into(),
                message: err.1.into()
            }),
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newPage", StageStatus::Ok, None),
                stage("goto", StageStatus::Failed, Some(err)),
                stage("selector", StageStatus::NotReached, None),
                stage("disconnect", StageStatus::NotReached, None),
            ],
        }
    );
}

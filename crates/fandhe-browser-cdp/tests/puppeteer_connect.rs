//! Puppeteer 接続試験の実行基盤の自己テスト（TASK-45.1・#480、ビヘイビア `CDP-3`・MS-4）。
//!
//! Node・Puppeteer に依存せず、`cargo test --workspace` で常に実行される。偽スクリプトは
//! テストバイナリ自身の再実行で実現する（`fandhe-browser-profile` の `lock_child_entry` と
//! 同じ流儀）。実 Puppeteer を使う試験ターゲットは puppeteer-core 導入の承認後に追加する
//! （harness/puppeteer-connect/README.md「導入状況」）。TASK-45.2（#481）で段階別結果（`stages`）の
//! 回収・検証を追加した。
//! `Profile::open` が非 unix で `Unsupported` を返す仕様のため `#[cfg(unix)]`
//! （任意の skip ではなく `tests/devtools_browser.rs` と同じ理由）。

#![cfg(unix)]

mod script_harness;

use std::time::Duration;

use script_harness::{
    HarnessError, INSTALL_HINT, ScriptCommand, ScriptError, ScriptOutcome, StageResult,
    StageStatus, TempDir, browser_ws_endpoint, parse_result_line, preflight_puppeteer, run_script,
    start_server,
};

const MODE_ENV: &str = "FANDHE_FAKE_SCRIPT_MODE";

/// 偽スクリプトの子側入口。`MODE_ENV` が無ければ何もしない（通常実行では no-op）。
#[test]
fn fake_script_child_entry() {
    let Ok(mode) = std::env::var(MODE_ENV) else {
        return;
    };
    // libtest は `test <name> ... ` を改行なしで出すため、結果行が行頭に来るよう改行を挟む。
    println!();
    let ep = std::env::var(script_harness::ENDPOINT_ENV).unwrap_or_default();
    match mode.as_str() {
        "ok" => println!(
            "noise\nFANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"connect\",\"error\":null}}"
        ),
        "fail" => {
            println!(
                "FANDHE_SCRIPT_RESULT {{\"ok\":false,\"step\":\"connect\",\"error\":{{\"name\":\"Error\",\"message\":\"boom\"}}}}"
            );
            std::process::exit(1);
        }
        "noresult" => {
            eprintln!("fatal: no result here");
            std::process::exit(3);
        }
        "hang" => std::thread::sleep(Duration::from_secs(120)),
        "malformed" => println!("FANDHE_SCRIPT_RESULT {{not json"),
        "oversize" => println!("FANDHE_SCRIPT_RESULT {}", "x".repeat(70 * 1024)),
        // 1MiB 超の出力の末尾に結果行を出す（結果行を取りこぼさないことの検証）。
        "flood" => {
            let line = "y".repeat(1023);
            for _ in 0..2048 {
                println!("{line}");
            }
            println!("FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"flood\",\"error\":null}}");
        }
        // 孫プロセスがパイプを握ったまま残る状況（join がハングしないことの検証）。
        "grandchild" => {
            let _ = std::process::Command::new("sleep").arg("30").spawn();
            println!("FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"gc\",\"error\":null}}");
        }
        "echo" => println!("FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"{ep}\",\"error\":null}}"),
        // ok:true の結果行を出した後に異常終了する（成功扱いにしないことの検証）。
        "okexit" => {
            println!("FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"late\",\"error\":null}}");
            eprintln!("fatal: crashed after result");
            std::process::exit(2);
        }
        // 短い結果行を大量に出す（最後の 1 行のみ保持され、無制限に溜めないことの検証）。
        "manyresults" => {
            for i in 0..200_000 {
                println!("FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"s{i}\",\"error\":null}}");
            }
        }
        // 段階別結果（TASK-45.2・#481）。結果行は `stages_line` で組み立てる。
        "stages_all_ok" => stages_line(
            true,
            "selector",
            None,
            r#"[{"name":"connect","status":"ok","error":null},{"name":"newPage","status":"ok","error":null},{"name":"goto","status":"ok","error":null},{"name":"selector","status":"ok","error":null}]"#,
        ),
        "stages_fail_at_connect" => stages_line(
            false,
            "connect",
            Some(("ProtocolError", "Method not found")),
            r#"[{"name":"connect","status":"failed","error":{"name":"ProtocolError","message":"Method not found"}},{"name":"newPage","status":"not_reached","error":null},{"name":"goto","status":"not_reached","error":null},{"name":"selector","status":"not_reached","error":null}]"#,
        ),
        "stages_fail_at_goto" => stages_line(
            false,
            "goto",
            Some(("TimeoutError", "Navigation timeout of 10000 ms exceeded")),
            r#"[{"name":"connect","status":"ok","error":null},{"name":"newPage","status":"ok","error":null},{"name":"goto","status":"failed","error":{"name":"TimeoutError","message":"Navigation timeout of 10000 ms exceeded"}},{"name":"selector","status":"not_reached","error":null}]"#,
        ),
        "stages_bad_status" => stages_line(
            false,
            "connect",
            None,
            r#"[{"name":"connect","status":"maybe","error":null}]"#,
        ),
        "stages_not_array" => stages_line(false, "connect", None, r#"{"name":"connect"}"#),
        "stages_too_many" => {
            let items: Vec<String> = (0..17)
                .map(|i| format!(r#"{{"name":"s{i}","status":"ok","error":null}}"#))
                .collect();
            stages_line(true, "s16", None, &format!("[{}]", items.join(",")));
        }
        "stages_long_name" => {
            let n = "n".repeat(65);
            stages_line(
                true,
                "x",
                None,
                &format!(r#"[{{"name":"{n}","status":"ok","error":null}}]"#),
            );
        }
        "stages_ok_true_with_failed" => stages_line(
            true,
            "connect",
            None,
            r#"[{"name":"connect","status":"failed","error":{"name":"E","message":"m"}}]"#,
        ),
        "stages_failed_without_error" => stages_line(
            false,
            "connect",
            None,
            r#"[{"name":"connect","status":"failed","error":null}]"#,
        ),
        "stages_ok_after_failed" => stages_line(
            false,
            "connect",
            None,
            r#"[{"name":"connect","status":"failed","error":{"name":"E","message":"m"}},{"name":"newPage","status":"ok","error":null}]"#,
        ),
        "stages_not_ok_but_all_ok" => stages_line(
            false,
            "selector",
            None,
            r#"[{"name":"connect","status":"ok","error":null},{"name":"selector","status":"ok","error":null}]"#,
        ),
        "stages_step_mismatch" => stages_line(
            false,
            "selector",
            Some(("E", "m")),
            r#"[{"name":"connect","status":"failed","error":{"name":"E","message":"m"}},{"name":"selector","status":"not_reached","error":null}]"#,
        ),
        "stages_not_reached_before_failed" => stages_line(
            false,
            "newPage",
            Some(("E", "m")),
            r#"[{"name":"connect","status":"not_reached","error":null},{"name":"newPage","status":"failed","error":{"name":"E","message":"m"}}]"#,
        ),
        "stages_two_failed" => stages_line(
            false,
            "connect",
            Some(("E", "m")),
            r#"[{"name":"connect","status":"failed","error":{"name":"E","message":"m"}},{"name":"newPage","status":"failed","error":{"name":"E","message":"m"}}]"#,
        ),
        "stages_error_mismatch" => stages_line(
            false,
            "connect",
            Some(("Other", "x")),
            r#"[{"name":"connect","status":"failed","error":{"name":"E","message":"m"}}]"#,
        ),
        "stages_error_missing_top_level" => stages_line(
            false,
            "connect",
            None,
            r#"[{"name":"connect","status":"failed","error":{"name":"E","message":"m"}}]"#,
        ),
        other => panic!("unknown mode {other}"),
    }
    std::process::exit(0);
}

/// `stages` 付きの結果行を出す（偽スクリプト用。`stages_json` はそのまま埋め込む）。
fn stages_line(ok: bool, step: &str, err: Option<(&str, &str)>, stages_json: &str) {
    let error = match err {
        Some((n, m)) => format!(r#"{{"name":"{n}","message":"{m}"}}"#),
        None => "null".to_string(),
    };
    println!(
        "FANDHE_SCRIPT_RESULT {{\"ok\":{ok},\"step\":\"{step}\",\"error\":{error},\"stages\":{stages_json}}}"
    );
}

fn fake(mode: &str) -> ScriptCommand {
    ScriptCommand {
        program: std::env::current_exe().expect("current_exe"),
        args: [
            "fake_script_child_entry",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ]
        .map(String::from)
        .to_vec(),
        envs: vec![(MODE_ENV.to_string(), mode.to_string())],
        cwd: None,
    }
}

const D: Duration = Duration::from_secs(30);

/// CDP-3: 成功の結果行を構造化して回収できる。
#[test]
fn collects_success_result() {
    assert_eq!(
        run_script(&fake("ok"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: true,
            step: "connect".into(),
            error: None,
            stages: vec![]
        }
    );
}

/// CDP-3: 失敗の結果行（エラー名・メッセージ）を回収できる。
#[test]
fn collects_failure_result() {
    assert_eq!(
        run_script(&fake("fail"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: false,
            step: "connect".into(),
            error: Some(ScriptError {
                name: "Error".into(),
                message: "boom".into()
            }),
            stages: vec![]
        }
    );
}

/// CDP-3: 結果行なしの終了は `NoResult`（終了コードと stderr 末尾を保持）。
#[test]
fn reports_no_result() {
    match run_script(&fake("noresult"), "ws://x", D) {
        ScriptOutcome::NoResult {
            exit_code,
            stderr_tail,
        } => {
            assert_eq!(exit_code, Some(3));
            assert!(
                stderr_tail.contains("fatal: no result here"),
                "{stderr_tail}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// CDP-3: 実行ファイル欠落などの起動失敗は panic せず `SpawnFailed`（理由付き）で返る。
#[test]
fn reports_spawn_failure() {
    let missing = TempDir::new().0.join("no-such-program");
    let cmd = ScriptCommand {
        program: missing.clone(),
        args: Vec::new(),
        envs: Vec::new(),
        cwd: None,
    };
    match run_script(&cmd, "ws://x", D) {
        ScriptOutcome::SpawnFailed { reason } => {
            assert!(reason.contains("failed to spawn"), "{reason}");
            assert!(reason.contains("no-such-program"), "{reason}");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// CDP-3: 締め切り超過は kill して `TimedOut`。
#[test]
fn times_out_and_kills() {
    let started = std::time::Instant::now();
    let o = run_script(&fake("hang"), "ws://x", Duration::from_millis(500));
    assert!(matches!(o, ScriptOutcome::TimedOut { .. }), "{o:?}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// CDP-3: 不正 JSON・長さ超過の結果行は `Malformed`。
#[test]
fn rejects_malformed_and_oversize_lines() {
    match run_script(&fake("malformed"), "ws://x", D) {
        ScriptOutcome::Malformed { reason } => {
            assert!(
                reason.starts_with("result line is not valid JSON"),
                "{reason}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(
        run_script(&fake("oversize"), "ws://x", D),
        ScriptOutcome::Malformed {
            reason: "result line exceeds 65536 bytes".into()
        }
    );
}

/// CDP-3: stdout が 1MiB を超えても末尾の結果行を回収できる。
#[test]
fn collects_result_after_large_stdout() {
    assert_eq!(
        run_script(&fake("flood"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: true,
            step: "flood".into(),
            error: None,
            stages: vec![]
        }
    );
}

/// CDP-3: ok:true の結果行の後に異常終了したら成功結果にせず `ExitedAbnormally`。
#[test]
fn abnormal_exit_after_ok_result_is_not_success() {
    match run_script(&fake("okexit"), "ws://x", D) {
        ScriptOutcome::ExitedAbnormally {
            exit_code,
            step,
            stderr_tail,
        } => {
            assert_eq!(exit_code, Some(2));
            assert_eq!(step, "late");
            assert!(
                stderr_tail.contains("crashed after result"),
                "{stderr_tail}"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// CDP-3: 結果行を大量に出しても最後の 1 行だけを保持して回収する。
#[test]
fn keeps_only_last_of_many_result_lines() {
    assert_eq!(
        run_script(&fake("manyresults"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: true,
            step: "s199999".into(),
            error: None,
            stages: vec![]
        }
    );
}

/// CDP-3: 孫プロセスがパイプを握り続けても、ハングせず出力済みの結果行も取りこぼさない。
#[test]
fn does_not_hang_when_grandchild_holds_pipe() {
    let started = std::time::Instant::now();
    let outcome = run_script(&fake("grandchild"), "ws://x", D);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        outcome,
        ScriptOutcome::Completed {
            ok: true,
            step: "gc".into(),
            error: None,
            stages: vec![]
        }
    );
}

/// CDP-3: 実サーバーの WS エンドポイントがスクリプトへそのまま渡る。
#[test]
fn passes_real_server_endpoint_to_script() {
    let server = start_server();
    let ep = browser_ws_endpoint(server.addr);
    let o = run_script(&fake("echo"), &ep, D);
    let prefix = format!("ws://127.0.0.1:{}/devtools/browser/", server.addr.port());
    match o {
        ScriptOutcome::Completed {
            ok, step, error, ..
        } => {
            assert!(ok);
            assert_eq!(step, ep);
            assert!(step.starts_with(&prefix));
            assert_eq!(error, None);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// AC3: 未導入時は理由と導入手順つきの明示的な失敗値を返す（成功を装わない）。
#[test]
fn preflight_fails_explicitly_when_not_installed() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).unwrap();
    assert_eq!(
        preflight_puppeteer(&dir.0).unwrap_err(),
        HarnessError::ToolUnavailable {
            tool: "puppeteer-core".into(),
            reason: "puppeteer-core is not installed".into(),
            hint: INSTALL_HINT.into(),
        }
    );
}

/// CDP-3: `TempDir::new` は既存ディレクトリを流用・削除せず、毎回新規に作成する。
#[test]
fn cdp3_temp_dir_never_reuses_or_removes_existing_directory() {
    let a = TempDir::new();
    let marker = a.0.join("keep.txt");
    std::fs::write(&marker, b"x").expect("write marker");
    let b = TempDir::new();
    assert_ne!(a.0, b.0);
    assert!(b.0.is_dir());
    drop(b);
    assert!(marker.is_file());
}

/// CDP-3: 結果行内の不正 UTF-8 は置換せず `Err`、結果行以外の不正バイトは無視する。
#[test]
fn cdp3_result_line_requires_valid_utf8() {
    let mut bad = b"FANDHE_SCRIPT_RESULT {\"ok\":true,\"step\":\"a".to_vec();
    bad.extend_from_slice(&[0xff, 0xfe]);
    bad.extend_from_slice(b"\",\"error\":null}\n");
    let err = parse_result_line(&bad).expect_err("invalid utf-8 must be rejected");
    assert!(err.starts_with("result line is not valid UTF-8"), "{err}");

    let mut ok = vec![0xff, b'\n'];
    ok.extend_from_slice(b"FANDHE_SCRIPT_RESULT {\"ok\":true,\"step\":\"s\",\"error\":null}\n");
    assert_eq!(
        parse_result_line(&ok),
        Ok(Some(ScriptOutcome::Completed {
            ok: true,
            step: "s".into(),
            error: None,
            stages: vec![]
        }))
    );
}

/// CDP-3: 起動引数は cwd 基準のファイル名のみ（相対 `harness_dir` でも二重 join しない）。
#[test]
fn cdp3_preflight_script_arg_is_cwd_relative_file_name() {
    let dir = TempDir::new();
    let pkg = dir.0.join("node_modules").join("puppeteer-core");
    std::fs::create_dir_all(&pkg).expect("mkdir");
    std::fs::write(pkg.join("package.json"), b"{}").expect("write");
    match preflight_puppeteer(&dir.0) {
        Ok(cmd) => {
            assert_eq!(cmd.args, vec!["connect.mjs".to_string()]);
            assert_eq!(cmd.cwd, Some(dir.0.clone()));
        }
        // node 未導入の環境では起動コマンドを作らない（引数の検証対象外）。
        Err(HarnessError::ToolUnavailable { reason, .. }) => {
            assert_eq!(reason, "node executable was not found");
        }
    }
}

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

/// CDP-3: 段階ごとの到達可否（connect / newPage / goto / selector）を回収できる。
#[test]
fn cdp3_collects_reach_of_each_stage() {
    assert_eq!(
        run_script(&fake("stages_all_ok"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: true,
            step: "selector".into(),
            error: None,
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newPage", StageStatus::Ok, None),
                stage("goto", StageStatus::Ok, None),
                stage("selector", StageStatus::Ok, None),
            ],
        }
    );
}

/// CDP-3: 失敗段階のエラー内容を保持し、後続段階は `NotReached` になる。
#[test]
fn cdp3_records_error_of_failed_stage_and_marks_rest_not_reached() {
    assert_eq!(
        run_script(&fake("stages_fail_at_goto"), "ws://x", D),
        ScriptOutcome::Completed {
            ok: false,
            step: "goto".into(),
            error: Some(ScriptError {
                name: "TimeoutError".into(),
                message: "Navigation timeout of 10000 ms exceeded".into()
            }),
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newPage", StageStatus::Ok, None),
                stage(
                    "goto",
                    StageStatus::Failed,
                    Some(("TimeoutError", "Navigation timeout of 10000 ms exceeded"))
                ),
                stage("selector", StageStatus::NotReached, None),
            ],
        }
    );
}

/// CDP-3: 到達 0 段階（connect 失敗）も「記録された結果」として回収できる。
#[test]
fn cdp3_zero_stages_reached_is_still_a_recorded_result() {
    match run_script(&fake("stages_fail_at_connect"), "ws://x", D) {
        ScriptOutcome::Completed {
            ok, step, stages, ..
        } => {
            assert!(!ok);
            assert_eq!(step, "connect");
            assert_eq!(
                stages.first(),
                Some(&stage(
                    "connect",
                    StageStatus::Failed,
                    Some(("ProtocolError", "Method not found"))
                ))
            );
            assert_eq!(
                stages
                    .iter()
                    .filter(|s| s.status == StageStatus::NotReached)
                    .count(),
                3
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// CDP-3: 不整合な段階報告は成功扱いにせず `Malformed`（具体的な理由つき）。
#[test]
fn cdp3_rejects_inconsistent_stage_reports() {
    let cases = [
        ("stages_bad_status", "invalid `stages[0].status`"),
        ("stages_not_array", "`stages` must be an array"),
        ("stages_too_many", "`stages` exceeds 16 entries"),
        ("stages_long_name", "`stages[0].name` must be 1..=64 bytes"),
        (
            "stages_ok_true_with_failed",
            "`ok` is true but a stage did not succeed",
        ),
        (
            "stages_failed_without_error",
            "`stages[0]` is failed but has no error",
        ),
        (
            "stages_ok_after_failed",
            "`stages[1]` is ok after a non-ok stage",
        ),
        (
            "stages_not_ok_but_all_ok",
            "`ok` is false but no stage failed",
        ),
        (
            "stages_step_mismatch",
            "`step` does not match the first failed stage",
        ),
        (
            "stages_not_reached_before_failed",
            "`stages[0]` is not_reached before any failed stage",
        ),
        ("stages_two_failed", "`stages[1]` is a second failed stage"),
        (
            "stages_error_mismatch",
            "top-level `error` does not match the failed stage error",
        ),
        (
            "stages_error_missing_top_level",
            "top-level `error` does not match the failed stage error",
        ),
    ];
    for (mode, reason) in cases {
        assert_eq!(
            run_script(&fake(mode), "ws://x", D),
            ScriptOutcome::Malformed {
                reason: reason.into()
            },
            "mode {mode}"
        );
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

/// CDP-3: `stages` の無い結果行は空配列として扱う（後方互換）。
#[test]
fn cdp3_result_line_without_stages_is_backward_compatible() {
    assert_eq!(
        parse_result_line(
            b"FANDHE_SCRIPT_RESULT {\"ok\":false,\"step\":\"connect\",\"error\":null}\n"
        ),
        Ok(Some(ScriptOutcome::Completed {
            ok: false,
            step: "connect".into(),
            error: None,
            stages: vec![]
        }))
    );
}

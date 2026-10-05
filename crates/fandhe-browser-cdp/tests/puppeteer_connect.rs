//! Puppeteer 接続試験の実行基盤の自己テスト（TASK-45.1・#480、ビヘイビア `CDP-3`・MS-4）。
//!
//! Node・Puppeteer に依存せず、`cargo test --workspace` で常に実行される。偽スクリプトは
//! テストバイナリ自身の再実行で実現する（`fandhe-browser-profile` の `lock_child_entry` と
//! 同じ流儀）。実 Puppeteer を使う試験ターゲットは puppeteer-core 導入の承認後に追加する
//! （harness/puppeteer-connect/README.md「導入状況」）。
//! `Profile::open` が非 unix で `Unsupported` を返す仕様のため `#[cfg(unix)]`
//! （任意の skip ではなく `tests/devtools_browser.rs` と同じ理由）。

#![cfg(unix)]

mod script_harness;

use std::time::Duration;

use script_harness::{
    HarnessError, INSTALL_HINT, ScriptCommand, ScriptError, ScriptOutcome, TempDir,
    browser_ws_endpoint, preflight_puppeteer, run_script, start_server,
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
        other => panic!("unknown mode {other}"),
    }
    std::process::exit(0);
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
            error: None
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
            })
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
            error: None
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
            error: None
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
            error: None
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
        ScriptOutcome::Completed { ok, step, error } => {
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

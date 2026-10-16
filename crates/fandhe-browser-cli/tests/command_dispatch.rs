//! サブコマンドディスパッチの実バイナリ結合テスト（TASK-47.1・Issue #282・`CLI-1`・MS-5）。
//!
//! 未登録コマンドは usage エラー（終了コード 2）でサーバーを起動せず、`--help` は
//! 終了コード 0 で usage を stdout に出すことを確かめる。回帰でサーバーが起動しても実環境の
//! プロファイルへ書かないよう子の `HOME` 等を一時ディレクトリへ差し替え、締め切り付きで待機する。

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(60);

struct Outcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(tag: &str, args: &[&str]) -> Outcome {
    run_with_config(tag, args, None)
}

/// `config` を設定ファイルとして書き、`FANDHE_BROWSER_CONFIG` で渡して起動する。
fn run_with_config(tag: &str, args: &[&str], config: Option<&str>) -> Outcome {
    let home: PathBuf =
        std::env::temp_dir().join(format!("fandhe-cli-dispatch-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&home).expect("home dir can be created");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fandhe-browser"));
    match config {
        Some(content) => {
            let file = home.join("fandhe-browser.toml");
            std::fs::write(&file, content).expect("config file can be written");
            cmd.env("FANDHE_BROWSER_CONFIG", &file);
        }
        None => {
            cmd.env_remove("FANDHE_BROWSER_CONFIG");
        }
    }
    let mut child = cmd
        .args(args)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("LOCALAPPDATA", &home)
        .env("APPDATA", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary can be spawned");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait works") {
            Some(_) => break,
            None if start.elapsed() > DEADLINE => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&home);
                panic!("binary did not exit within {DEADLINE:?} (server may have started)");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let output = child.wait_with_output().expect("output can be collected");
    let _ = std::fs::remove_dir_all(&home);
    Outcome {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn cli1_unknown_subcommand_exits_with_usage_error() {
    let out = run("unknown", &["definitely-not-a-command"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: unknown command 'definitely-not-a-command'; run 'fandhe-browser --help' for usage"
    );
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_help_flag_prints_usage_and_exits_zero() {
    let out = run("help", &["--help"]);
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.starts_with("Usage: fandhe-browser"),
        "stdout: {}",
        out.stdout
    );
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_launch_with_extra_argument_exits_with_usage_error() {
    let out = run("launch-extra", &["launch", "extra"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: unexpected argument 'extra'; run 'fandhe-browser --help' for usage"
    );
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_launch_with_unknown_option_exits_with_usage_error() {
    let out = run("launch-bogus", &["launch", "--bogus"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: unknown option '--bogus'; run 'fandhe-browser --help' for usage"
    );
}

const UNBUNDLED_ENGINE_CONFIG: &str = "[js]\nengine = \"quickjs\"\n";

#[test]
fn cli1_launch_startup_failure_returns_error() {
    let out = run_with_config("launch-fail", &["launch"], Some(UNBUNDLED_ENGINE_CONFIG));
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("\"quickjs\""), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_launch_and_no_args_share_startup_path() {
    let launch = run_with_config("same-launch", &["launch"], Some(UNBUNDLED_ENGINE_CONFIG));
    let bare = run_with_config("same-bare", &[], Some(UNBUNDLED_ENGINE_CONFIG));
    assert_eq!(launch.code, bare.code);
    assert_eq!(launch.stderr, bare.stderr);
}

#[test]
fn cli1_navigate_without_url_exits_with_usage_error() {
    let out = run("nav-missing", &["navigate"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'URL'; run 'fandhe-browser --help' for usage"
    );
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_navigate_invalid_url_exits_with_error() {
    let out = run("nav-invalid", &["navigate", "not-a-url"]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.stderr.trim(), "error: invalid URL");
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_navigate_rejects_loopback_by_default() {
    let out = run("nav-loopback", &["navigate", "http://127.0.0.1:9/"]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("not allowed"), "stderr: {}", out.stderr);
    assert!(out.stdout.is_empty());
}

#[test]
fn cli1_query_without_arguments_exits_with_usage_error() {
    let out = run("query-none", &["query"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'URL'; run 'fandhe-browser --help' for usage"
    );
    assert!(!out.stderr.contains("DevTools listening"));
}

#[test]
fn cli1_query_without_selector_exits_with_usage_error() {
    let out = run("query-nosel", &["query", "https://a.example/"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'SELECTOR'; run 'fandhe-browser --help' for usage"
    );
}

#[test]
fn cli1_query_conflicting_options_exit_with_usage_error() {
    let out = run(
        "query-conflict",
        &[
            "query",
            "--all",
            "--attr",
            "href",
            "https://a.example/",
            "a",
        ],
    );
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: options '--all' and '--attr' cannot be used together; run 'fandhe-browser --help' for usage"
    );
}

#[test]
fn cli1_query_unknown_option_exits_with_usage_error() {
    let out = run(
        "query-bogus",
        &["query", "--bogus", "https://a.example/", "a"],
    );
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: unknown option '--bogus'; run 'fandhe-browser --help' for usage"
    );
}

#[test]
fn cli1_query_invalid_url_exits_with_error() {
    let out = run("query-invalid", &["query", "not-a-url", "h1"]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.stderr.trim(), "error: invalid URL");
    assert!(out.stdout.is_empty());
}

#[test]
fn cli1_query_rejects_loopback_by_default() {
    let out = run("query-loopback", &["query", "http://127.0.0.1:9/", "h1"]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("not allowed"), "stderr: {}", out.stderr);
    assert!(out.stdout.is_empty());
}

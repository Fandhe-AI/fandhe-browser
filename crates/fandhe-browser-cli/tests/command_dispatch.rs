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
    cmd.env_remove("FANDHE_BROWSER_RENDER_HOST");
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
/// screenshot 用の出力先ディレクトリ（テストごとに作り、`Drop` で削除する）。
struct OutDir(PathBuf);

impl OutDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("fandhe-cli-shot-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("out dir can be created");
        Self(dir)
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for OutDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cli1_screenshot_without_arguments_exits_with_usage_error() {
    let out = run("shot-none", &["screenshot"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'URL'; run 'fandhe-browser --help' for usage"
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn cli1_screenshot_without_output_exits_with_usage_error() {
    let out = run("shot-noout", &["screenshot", "https://a.example/"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'OUTPUT'; run 'fandhe-browser --help' for usage"
    );
}

#[test]
fn cli1_screenshot_invalid_url_exits_with_error_before_host_resolution() {
    let dir = OutDir::new("invalid");
    let target = dir.file("out.png");
    let out = run("shot-invalid", &["screenshot", "not-a-url", &target]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    // ホスト解決（未設定なら "rendering is not available"）より前に URL 検証で止まる。
    assert_eq!(out.stderr.trim(), "error: invalid URL");
    assert!(out.stdout.is_empty());
    assert!(!std::path::Path::new(&target).exists());
}

#[test]
fn cli1_screenshot_rejects_loopback_by_default() {
    let dir = OutDir::new("loopback");
    let target = dir.file("out.png");
    let out = run(
        "shot-loopback",
        &["screenshot", "http://127.0.0.1:9/", &target],
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("not allowed"), "stderr: {}", out.stderr);
    assert!(out.stdout.is_empty());
    assert!(!std::path::Path::new(&target).exists());
}

#[test]
fn cli1_screenshot_rejects_existing_output_before_host_resolution() {
    let dir = OutDir::new("exists");
    let target = dir.file("out.png");
    std::fs::write(&target, b"keep").expect("existing file can be written");
    let out = run(
        "shot-exists",
        &["screenshot", "https://a.example/", &target],
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.stderr.trim(), "error: output file already exists");
    assert!(out.stdout.is_empty());
    // 既存ファイルは上書きされない。
    assert_eq!(std::fs::read(&target).expect("file remains"), b"keep");
}

#[test]
fn cli1_screenshot_output_is_validated_before_url() {
    // 出力先不備と URL 不備が重なるときは出力先の検証が先に報告される。
    let dir = OutDir::new("order");
    let target = dir.file("out.png");
    std::fs::write(&target, b"keep").expect("existing file can be written");
    let out = run("shot-order", &["screenshot", "not-a-url", &target]);
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(out.stderr.trim(), "error: output file already exists");
}

#[test]
fn cli1_screenshot_is_wired_and_reports_rendering_unavailable_without_host() {
    // 検証を通る入力ではレンダラ解決まで到達する（登録表 → run の接続）。
    // ホスト未設定では描画不可を英語で報告し、出力ファイルを作らない。
    let dir = OutDir::new("nohost");
    let target = dir.file("out.png");
    let out = run(
        "shot-nohost",
        &["screenshot", "https://a.example/", &target],
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: rendering is not available (render host not found; set FANDHE_BROWSER_RENDER_HOST)"
    );
    assert!(out.stdout.is_empty());
    assert!(!std::path::Path::new(&target).exists());
}

#[test]
fn cli1_screenshot_full_page_is_rejected_as_unsupported() {
    // v1 の描画ホストは full-page 未対応（固定ビューポート）。成功を装わず usage エラーで拒否し、
    // ファイルを作らない。
    let dir = OutDir::new("fullpage");
    let target = dir.file("out.png");
    let out = run(
        "shot-fullpage",
        &["screenshot", "--full-page", "https://a.example/", &target],
    );
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: option '--full-page' is not supported yet; run 'fandhe-browser --help' for usage"
    );
    assert!(out.stdout.is_empty());
    assert!(!std::path::Path::new(&target).exists());
}

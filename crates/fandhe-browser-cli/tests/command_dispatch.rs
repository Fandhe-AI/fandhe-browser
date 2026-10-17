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

// --- TASK-48.1（#290・`CLI-2`）: セットアップ専用コマンド `setup` ---

#[test]
fn cli2_setup_with_extra_argument_exits_with_usage_error() {
    let out = run("setup-extra", &["setup", "extra"]);
    assert_eq!(out.code, Some(2), "stderr: {}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: unexpected argument 'extra'; run 'fandhe-browser --help' for usage"
    );
}

/// 判定（0 か 1）は環境依存で固定しない。判定の正しさは偽ディレクトリ木の単体テストが担保する。
#[test]
fn cli2_setup_reports_prerequisites_with_consistent_exit_code() {
    let out = run("setup-real", &["setup"]);
    match out.code {
        Some(0) => {
            let ok = out
                .stdout
                .starts_with("no OS-specific prerequisites are checked on ")
                || out
                    .stdout
                    .lines()
                    .all(|l| l.starts_with("ok ") || l.starts_with("skipped "));
            assert!(ok, "stdout: {}", out.stdout);
        }
        Some(1) => assert!(
            out.stderr
                .starts_with("error: missing OS-specific build prerequisites: "),
            "stderr: {}",
            out.stderr
        ),
        other => panic!("unexpected exit code {other:?}: {}", out.stderr),
    }
}

// --- TASK-47.6（#287・`CLI-1`・`PROF-1`）: 全コマンド共通の `--profile-dir` ---

#[test]
fn cli1_profile_dir_filesystem_root_is_validation_error() {
    let out = run(
        "profile-root",
        &["navigate", "--profile-dir", "/", "about:blank"],
    );
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains("invalid profile directory"),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{}", out.stdout);
}

#[test]
fn cli1_profile_dir_without_value_is_usage_error() {
    let out = run("profile-missing", &["query", "--profile-dir"]);
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("'PATH'"), "{}", out.stderr);
}

#[test]
fn cli1_profile_alias_remains_unknown_option() {
    let out = run(
        "profile-alias",
        &["navigate", "--profile", "x", "about:blank"],
    );
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("unknown option"), "{}", out.stderr);
}

/// 祖先に symlink を含まない一時ディレクトリの基点（`PROF-1`・`PROF-4`）。
///
/// `--profile-dir` は祖先の symlink を `Profile::open` が拒否する。macOS の `temp_dir()` は
/// `/var/folders/...`（`/var` は `/private/var` への symlink）を返すため、`--profile-dir` に渡す
/// パスは `canonicalize` した実体パスを基点にする。
fn real_temp_dir() -> PathBuf {
    std::fs::canonicalize(std::env::temp_dir()).expect("temp_dir can be canonicalized")
}

#[cfg(unix)]
#[test]
fn prof1_profile_dir_creates_profile_and_runs_navigate() {
    let parent = real_temp_dir().join(format!("fandhe-cli-prof-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir(&parent).expect("parent");
    let dir = parent.join("p");
    let dir_arg = dir.to_str().expect("utf8 path");
    let out = run(
        "profile-ok",
        &["navigate", "--profile-dir", dir_arg, "about:blank"],
    );
    let lock_exists = dir.join("profile.lock").exists();
    let _ = std::fs::remove_dir_all(&parent);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout.trim(), "url=about:blank");
    assert!(lock_exists);
}

#[cfg(unix)]
#[test]
fn prof1_profile_dir_symlink_then_dotdot_is_rejected() {
    let parent = real_temp_dir().join(format!("fandhe-cli-prof-ln-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir(&parent).expect("parent");
    let other = parent.join("other");
    std::fs::create_dir(&other).expect("other");
    let link = parent.join("link");
    std::os::unix::fs::symlink(&other, &link).expect("symlink");
    let arg = format!("{}/../p", link.display());
    let out = run(
        "profile-ln",
        &["navigate", "--profile-dir", &arg, "about:blank"],
    );
    let created = parent.join("p").exists();
    let _ = std::fs::remove_dir_all(&parent);
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(!created);
}

#[test]
fn cli3_launch_invalid_port_exits_with_usage_error() {
    let out = run("port-bad", &["launch", "--port", "65536"]);
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: invalid value for option '--port'; run 'fandhe-browser --help' for usage"
    );
}

#[test]
fn cli3_launch_port_without_value_exits_with_usage_error() {
    let out = run("port-missing", &["launch", "--port"]);
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'PORT'; run 'fandhe-browser --help' for usage"
    );
}

/// `CLI-3`（#823）: ポート 0 で実起動し、stderr の 1 行に実ポートが出て `/json/version` に応答する。
/// `#[cfg(unix)]` の理由: Windows は既定プロファイルルートが `%LOCALAPPDATA%` 配下
/// （`...\AppData\Local` 終端）に限られ、一時ディレクトリでは起動できないため
/// （既存の `prof1_*` と同じ制約。3 OS 共通検査を飛ばす意図ではない）。
#[cfg(unix)]
#[test]
fn cli3_launch_port_zero_announces_assigned_port() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc;

    let home = real_temp_dir().join(format!("fandhe-cli-port0-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir(&home).expect("home dir");
    let profile = home.join("p");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser"))
        .args(["launch", "--port", "0", "--profile-dir"])
        .arg(&profile)
        .env_remove("FANDHE_BROWSER_CONFIG")
        .env_remove("FANDHE_BROWSER_RENDER_HOST")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary can be spawned");
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stderr).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(DEADLINE);
    let result = std::panic::catch_unwind(|| {
        let line = line.expect("startup line within deadline");
        let line = line.trim();
        let rest = line
            .strip_prefix("DevTools listening on ws://127.0.0.1:")
            .unwrap_or_else(|| panic!("unexpected line: {line}"));
        let (port, id) = rest
            .split_once("/devtools/browser/")
            .unwrap_or_else(|| panic!("unexpected line: {line}"));
        let port: u16 = port.parse().expect("numeric port");
        assert_ne!(port, 0);
        assert_eq!(id.len(), 32, "{id}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)),
            "{id}"
        );
        let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
        write!(
            conn,
            "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .expect("send");
        let mut resp = String::new();
        conn.read_to_string(&mut resp).expect("read");
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains(id), "{resp}");
    });
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// `query --file` の結合テスト用に一時 fixture ディレクトリを作る（呼び出し側が削除する）。
fn query_file_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fandhe-cli-qfile-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

/// #824・`PERF-7`: ローカル HTML を読み、結果を stdout に出して終了コード 0 で終わる。
#[test]
fn cli1_query_file_prints_result_and_exits_zero() {
    let dir = query_file_dir("ok");
    let page = dir.join("page.html");
    std::fs::write(&page, "<html><body><h1>Title</h1></body></html>\n").expect("fixture");
    let out = run(
        "qfile-ok",
        &["query", "--file", page.to_str().expect("utf8"), "h1"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stdout, "\"Title\"\n");
    assert_eq!(out.stderr, "");
}

#[test]
fn cli1_query_file_missing_exits_with_error() {
    let dir = query_file_dir("missing");
    let missing = dir.join("nope.html");
    let out = run(
        "qfile-missing",
        &["query", "--file", missing.to_str().expect("utf8"), "h1"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_eq!(out.stderr.trim(), "error: could not read input file");
}

#[test]
fn cli1_query_file_directory_exits_with_error() {
    let dir = query_file_dir("dir");
    let out = run(
        "qfile-dir",
        &["query", "--file", dir.to_str().expect("utf8"), "h1"],
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.code, Some(1), "{}", out.stderr);
    assert_eq!(out.stderr.trim(), "error: input path is not a regular file");
}

#[test]
fn cli1_query_file_without_value_exits_with_usage_error() {
    let out = run("qfile-novalue", &["query", "h1", "--file"]);
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert_eq!(
        out.stderr.trim(),
        "error: missing required argument 'PATH'; run 'fandhe-browser --help' for usage"
    );
}

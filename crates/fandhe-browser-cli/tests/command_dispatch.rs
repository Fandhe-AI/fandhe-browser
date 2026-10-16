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
    let home: PathBuf =
        std::env::temp_dir().join(format!("fandhe-cli-dispatch-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&home).expect("home dir can be created");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser"))
        .args(args)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("LOCALAPPDATA", &home)
        .env("APPDATA", &home)
        .env_remove("FANDHE_BROWSER_CONFIG")
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

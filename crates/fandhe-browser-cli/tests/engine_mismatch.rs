//! 未同梱 JS エンジン指定時の起動失敗を、実バイナリ `fandhe-browser` で確認する結合テスト
//! （TASK-30.5・Issue #163・`JS-2`・`JS-1`「JS エンジンの切替方式」ケース (3)(4)・MS-3）。
//!
//! cli の `startup_config` が `FANDHE_BROWSER_CONFIG` の設定を読み込み、未同梱・未知の
//! エンジン指定をプロファイル open・bind より前に非 0 終了とする契約を検証する。
//! core 側の `tests/config_js_engine.rs`（起動シーケンスを模した子プロセス）と対になる。
//!
//! 回帰でサーバー起動まで進んでも実環境のプロファイルへ書かないよう、子の `HOME` 等を
//! 一時ディレクトリへ差し替える。また締め切り付きで待機し、CI をハングさせない。

use fandhe_browser_core::{EngineKind, bundled_engines};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 子プロセスの待機締め切り。
const DEADLINE: Duration = Duration::from_secs(60);

/// drop 時に再帰削除する一時ディレクトリ。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fandhe-cli-engine-mismatch-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir can be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Outcome {
    code: Option<i32>,
    stderr: String,
}

/// `content` を設定ファイルとして書き、実バイナリを `FANDHE_BROWSER_CONFIG` 付きで起動する。
fn run_with_config(content: &str) -> Outcome {
    let dir = TempDir::new();
    let file = dir.path().join("fandhe-browser.toml");
    std::fs::write(&file, content).expect("config file can be written");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir can be created");

    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser"))
        .env("FANDHE_BROWSER_CONFIG", &file)
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
                panic!("binary did not exit within {DEADLINE:?} (server may have started)");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let output = child.wait_with_output().expect("output can be collected");
    Outcome {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn compiled_list() -> String {
    let bundled = bundled_engines();
    if bundled.is_empty() {
        "none".to_string()
    } else {
        bundled
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn assert_failed_without_server(out: &Outcome) {
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("DevTools listening"),
        "server must not start: {}",
        out.stderr
    );
}

/// 既定ビルド（V8 のみ同梱）で boa 指定は非 0 終了し、指定値・同梱一覧・必要 feature を示す。
///
/// `--all-features` では boa も同梱されて指定が通ってしまうため、既定構成に限定する
/// （ビルド構成ごとに前提が異なるための cfg。skip による CI 回避ではない）。
#[cfg(all(feature = "js-v8", not(feature = "js-boa")))]
#[test]
fn js_2_default_build_rejects_unbundled_boa() {
    let out = run_with_config("[js]\nengine = \"boa\"\n");
    assert_failed_without_server(&out);
    assert_eq!(
        out.stderr.trim(),
        "error: configuration error: js engine \"boa\" is not compiled into this binary (compiled: v8); rebuild with --features js-boa"
    );
}

/// 同梱していないエンジンはすべて起動失敗になる（構成を問わず実行。`--all-features` では 0 件）。
#[test]
fn js_2_every_unbundled_engine_fails_startup() {
    for kind in EngineKind::ALL {
        if bundled_engines().contains(&kind) {
            continue;
        }
        let out = run_with_config(&format!("[js]\nengine = \"{}\"\n", kind.as_str()));
        assert_failed_without_server(&out);
        for needle in [
            format!("\"{}\"", kind.as_str()),
            format!("compiled: {}", compiled_list()),
            format!("--features {}", kind.feature_name()),
        ] {
            assert!(
                out.stderr.contains(&needle),
                "stderr lacks {needle:?}: {}",
                out.stderr
            );
        }
    }
}

/// 未知のエンジン名（`JS-1` ケース (4)）も同梱状況に関わらず起動失敗になる。
#[test]
fn js_2_unknown_engine_fails_startup() {
    let out = run_with_config("[js]\nengine = \"quickjs\"\n");
    assert_failed_without_server(&out);
    for needle in [
        "\"quickjs\"",
        "\"v8\"",
        "\"boa\"",
        "--features js-v8",
        "--features js-boa",
        &format!("compiled: {}", compiled_list()),
    ] {
        assert!(
            out.stderr.contains(needle),
            "stderr lacks {needle:?}: {}",
            out.stderr
        );
    }
}

//! `[js] engine`（TASK-91（91.2）・Issue #215・`JS-1`）の fail-closed 起動エラーを、
//! 子プロセスの終了コード・stderr として検証する結合テスト。
//!
//! cli（TASK-41.5）がまだ無いため、起動シーケンス（設定読み込み → 失敗なら
//! stderr へ出して非 0 終了）を、このテストバイナリ自身の再実行
//! （`std::env::current_exe()`）で模す。cli 作成後は TASK-30 の結合テスト
//! （既定ビルドで `engine = "boa"` → 非 0 終了）が実バイナリで同等の検証を
//! 担う。「JS 無効」の起動時ログ出力（REPAIR-3）は本テストの対象外。
//!
//! 期待値は同梱一覧（`bundled_engines`）から実行時に組み立てるため、既定
//! feature（同梱なし）でも `--all-features`（V8・boa）でも skip されずに通る。
//! 独自の `main` を持つため `harness = false`（`Cargo.toml` 参照）。

use fandhe_browser_core::{Config, EngineKind, bundled_engines};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 子モードで設定ファイルの絶対パスを渡す環境変数名。
const CHILD_ENV: &str = "FANDHE_BROWSER_CONFIG_JS_TEST_CHILD";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// テスト用の一時ディレクトリ。drop 時に再帰削除する（`tests/config.rs` と同じ流儀）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fandhe-core-config-js-test-{}-{n}",
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

/// 子モード: 設定を読み込み、失敗なら cli と同様に stderr へ出して非 0 終了する。
fn run_child(config_path: &str) -> ExitCode {
    match Config::load(Path::new(config_path)) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Outcome {
    success: bool,
    stderr: String,
}

/// `content` を設定ファイルとして書き出し、子プロセスで読み込ませる。
fn run_case(content: &str) -> Outcome {
    let dir = TempDir::new();
    let file = dir.path().join("fandhe-browser.toml");
    std::fs::write(&file, content).expect("config file can be written");
    let exe = std::env::current_exe().expect("current exe path");
    let output = Command::new(exe)
        .env(CHILD_ENV, &file)
        .output()
        .expect("child process can be spawned");
    Outcome {
        success: output.status.success(),
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

fn check_all(failures: &mut Vec<String>) {
    let mut engine_cases = 0;
    for kind in EngineKind::ALL {
        engine_cases += 1;
        let out = run_case(&format!("[js]\nengine = \"{}\"\n", kind.as_str()));
        if bundled_engines().contains(&kind) {
            if !out.success {
                failures.push(format!(
                    "bundled engine {} should load, stderr: {}",
                    kind.as_str(),
                    out.stderr
                ));
            }
        } else {
            let expected = [
                format!("\"{}\"", kind.as_str()),
                format!("compiled: {}", compiled_list()),
                format!("--features {}", kind.feature_name()),
            ];
            if out.success {
                failures.push(format!("unbundled engine {} must fail", kind.as_str()));
            }
            for needle in expected {
                if !out.stderr.contains(&needle) {
                    failures.push(format!(
                        "stderr for {} lacks {needle:?}: {}",
                        kind.as_str(),
                        out.stderr
                    ));
                }
            }
        }
    }
    if engine_cases != EngineKind::ALL.len() {
        failures.push("every engine kind must be exercised".to_string());
    }

    // 未知の値は同梱状況に関わらず非 0 終了し、指定値・対応値・feature・同梱一覧を示す。
    let out = run_case("[js]\nengine = \"quickjs\"\n");
    if out.success {
        failures.push("unknown engine must fail".to_string());
    }
    for needle in [
        "\"quickjs\"",
        "\"v8\"",
        "\"boa\"",
        "--features js-v8",
        "--features js-boa",
        &format!("compiled: {}", compiled_list()),
    ] {
        if !out.stderr.contains(needle) {
            failures.push(format!(
                "unknown-engine stderr lacks {needle:?}: {}",
                out.stderr
            ));
        }
    }

    // 大文字小文字違いも黙って受理しない。
    if run_case("[js]\nengine = \"V8\"\n").success {
        failures.push("\"V8\" (wrong case) must fail".to_string());
    }

    // 省略（空ファイル・`[js]` のみ）はエンジンなしビルドでもエラーにしない。
    for content in ["", "[js]\n"] {
        let out = run_case(content);
        if !out.success {
            failures.push(format!(
                "omitted engine {content:?} must load: {}",
                out.stderr
            ));
        }
    }
}

fn main() -> ExitCode {
    if let Ok(path) = std::env::var(CHILD_ENV) {
        return run_child(&path);
    }

    let mut failures = Vec::new();
    check_all(&mut failures);
    if failures.is_empty() {
        println!("config_js_engine: all cases passed");
        ExitCode::SUCCESS
    } else {
        for failure in &failures {
            eprintln!("FAIL: {failure}");
        }
        ExitCode::FAILURE
    }
}

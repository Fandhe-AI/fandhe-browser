//! バイナリ `fandhe-browser` の入口（TASK-41（41.5）・MS-3・`CDP-1`・`AISNAP-6`）。
//!
//! 現状は引数を取らず、OS 既定のプロファイルと `127.0.0.1:9333` でサーバーを起動するだけの
//! 最小雛形。サブコマンド・引数解析は TASK-47（`CLI-1`）で追加する。組み立ての実体は
//! [`server`] にあり、本ファイルはランタイム構築とエラー終了コードへの写像だけを担う。

mod server;

use std::process::ExitCode;

use fandhe_browser_profile::OsDefaultStore;

use server::StartupError;

fn main() -> ExitCode {
    match run_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// current_thread ランタイムを構築してサーバーを動かす。構築失敗は panic させず
/// [`StartupError::Runtime`] として返す（release は `panic = "abort"`）。
fn run_main() -> Result<(), StartupError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    // TASK-19 で AI API ルータのファクトリを、空の `Vec` の代わりにここへ渡す。
    runtime.block_on(server::run(
        &OsDefaultStore::new(),
        server::DEFAULT_ADDR,
        Vec::new(),
    ))
}

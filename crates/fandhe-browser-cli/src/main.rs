//! バイナリ `fandhe-browser` の入口（TASK-41（41.5）・MS-3・`CDP-1`・`AISNAP-6`）。
//!
//! 現状は引数を取らず、OS 既定のプロファイルと `127.0.0.1:9333` でサーバーを起動するだけの
//! 最小雛形。サブコマンド・引数解析は TASK-47（`CLI-1`）で追加する。組み立ての実体は
//! [`server`] にあり、本ファイルは JS ワーカーフックへの分岐・ランタイム構築・エラー終了コードへの
//! 写像だけを担う（フックは TASK-30・`JS-2`・#514）。
//!
//! 環境変数 `FANDHE_BROWSER_CONFIG` で設定ファイルを指定できる（暫定。[`startup_config`]）。
//! 読み込みはプロファイル open・bind より前に行い、未同梱エンジン指定などは非 0 で終了する
//! （`JS-2`・TASK-30.5）。

//!
//! Windows では profile crate の ACL 実装（`XOS-7`〜`XOS-10`）待ちのため起動不可（fail-closed）。
//! `ProfileError::Unsupported` の場合は理由を示す固定の英語メッセージで非ゼロ終了する。
mod server;
mod startup_config;

use std::process::ExitCode;

use fandhe_browser_profile::OsDefaultStore;

use server::StartupError;

fn main() -> ExitCode {
    // JS 評価用の子プロセス（`JS-1`・`JS-2`）として自分自身が再起動された場合に、ワーカー役へ
    // 分岐する入口（TASK-30・MS-3・#514）。js crate の `worker_main` を core の再エクスポート
    // 経由で呼ぶ。マーカー環境変数が無ければ `None` で通常の処理へ進み、js-v8 を含まないビルドで
    // マーカーがあれば js 側が `FAILURE` を返す（成功を装わない）。
    // 制約: V8 protected Platform の初期化は呼び出しスレッド上で直線的に進むため、これより前に
    // スレッドを作らないこと。tokio・ロギング・設定読み込み・clap 解析（TASK-47・`CLI-1`）は
    // 必ずこの呼び出しより後ろに置く。
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
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
    // 設定エラーはプロファイル open・bind より前に確定させる（サーバーを起動しない）。
    // 選択エンジンの `JsRuntime` 配線は後続（startup_config のスタブ節参照）。
    let _config = startup_config::load_startup_config()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(StartupError::Runtime)?;
    // AI API ルータ（`/ai/*`）は既定の追加ルータとして CDP と同一リスナーへ合成する（TASK-19.3）。
    runtime.block_on(server::run(
        &OsDefaultStore::new(),
        server::DEFAULT_ADDR,
        server::default_router_factories(),
    ))
}

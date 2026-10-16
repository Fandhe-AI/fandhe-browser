//! バイナリ `fandhe-browser` の入口（TASK-41（41.5）・MS-3・`CDP-1`・`AISNAP-6`）。
//!
//! 引数なしでは `launch`（TASK-47.2・#283）と同じく OS 既定のプロファイルと `127.0.0.1:9333` でサーバーを起動する。引数は
//! [`commands`] が `args_os` の手書き解析（#281）でディスパッチし、未知の引数は usage エラー
//! （終了コード 2）にする（TASK-47.1・`CLI-1`）。
//! `navigate`（TASK-47.3・#284）は one-shot で URL を取得して結果を出力する。
//! サーバー組み立ての実体は [`server`] にあり、本ファイルは
//! JS ワーカーフックへの分岐・ランタイム構築・エラー終了コードへの写像だけを担う
//! （フックは TASK-30・`JS-2`・#514）。
//!
//! 環境変数 `FANDHE_BROWSER_CONFIG` で設定ファイルを指定できる（暫定。[`startup_config`]）。
//! 読み込みはプロファイル open・bind より前に行い、未同梱エンジン指定などは非 0 で終了する
//! （`JS-2`・TASK-30.5）。

//!
//! Windows ではプロファイルルートが `%LOCALAPPDATA%` 配下のときだけ起動できる（`XOS-7`・`XOS-10`）。
//! `ProfileError::Unsupported` の場合は理由を示す固定の英語メッセージで非ゼロ終了する。
mod commands;
mod page_scripts;
mod render_host;
mod server;
mod startup_config;

use std::process::ExitCode;

fn main() -> ExitCode {
    // JS 評価用の子プロセス（`JS-1`・`JS-2`）として自分自身が再起動された場合に、ワーカー役へ
    // 分岐する入口（TASK-30・MS-3・#514）。js crate の `worker_main` を core の再エクスポート
    // 経由で呼ぶ。マーカー環境変数が無ければ `None` で通常の処理へ進み、js-v8 を含まないビルドで
    // マーカーがあれば js 側が `FAILURE` を返す（成功を装わない）。
    // 制約: V8 protected Platform の初期化は呼び出しスレッド上で直線的に進むため、これより前に
    // スレッドを作らないこと。tokio・ロギング・設定読み込み・引数解析（TASK-47・`CLI-1`）は
    // 必ずこの呼び出しより後ろに置く。
    if let Some(code) = fandhe_browser_core::run_js_worker_if_requested() {
        return code;
    }
    let registry = commands::Registry {
        default: commands::launch::run_default,
        commands: commands::COMMANDS,
    };
    match commands::dispatch(
        std::env::args_os().skip(1),
        &registry,
        &mut std::io::stdout().lock(),
    ) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit_code()
        }
    }
}

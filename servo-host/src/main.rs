//! Servo ホスト（`fandhe-browser-servo-host`）のエントリポイント。
//!
//! 本体の workspace から分離した別 workspace に置く子プロセス用バイナリで、Servo
//! （MPL-2.0）を本体の依存グラフへ入れないための隔離境界（RENDER-1・TASK-38・#915・#935）。
//!
//! 現状の実装範囲:
//! - `--version`: 版を表示して成功終了する。
//! - `--selftest`: Servo の初期化・小さな `data:` ページの読み込み完了・終了までを行い、
//!   成功時のみ終了コード 0 を返す（RENDER-3・#988。設計 §1.4・§2.6・§2.7）。
//!   ネットワークへは出ない。呼び出し元は開発者と CI のみ。
//! - `--stdio`: 本体 render crate から起動される stdio サーバー（#993。`server`）。`load`（#231）と
//!   `screenshot`（PNG の base64 inline。#232）・`visibility`（JS 評価。#233。`layout`）は
//!   `page::ServoHandler` で実装済み。`bounding_box`（JS 評価。#234）も同じ `layout` 経由で実装済み。
//! - それ以外: 標準エラーに示し、非ゼロで終了する（固定の許可リスト。SEC-2）。
//!
//! Servo と描画コンテキストの生成・破棄は `engine` が担う（RENDER-3・#230）。
//!
//! WebView の生成・読み込み・フレーム待機は `page` が担う（RENDER-3・RENDER-4・#231）。
//!
//! PNG エンコードと base64 化は Servo 非依存の `screenshot` が担う（RENDER-3・#232）。
//!
//! 可視性判定・境界ボックス取得のスクリプト生成と結果解釈は Servo 非依存の `layout` が担う（RENDER-3・#233・#234）。

mod engine;
mod layout;
mod page;
mod screenshot;
mod server;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use engine::{Engine, EngineError};
use server::ServeOutcome;
use servo::ServoUrl;

/// ページ読み込み完了を待つ上限。超過は失敗として扱い、成功を装わない。
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(60);
/// セルフテストで読み込むページ。`data:` のためネットワークアクセスは発生しない。
const SELFTEST_URL: &str = "data:text/html,<h1>selftest</h1>";

/// `--stdio` の終了コード: プロトコル違反（応答なしで終了した）。
const EXIT_PROTOCOL_VIOLATION: u8 = 2;

/// 引数に応じて `--version`・`--selftest`・`--stdio` を処理し、他は失敗終了する。
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--stdio` は単独でのみ受け付ける（後続の引数は拒否。固定の許可リスト）。
    if args.first().map(String::as_str) == Some("--stdio") && args.len() == 1 {
        return run_stdio();
    }
    match args.first().map(String::as_str).filter(|_| args.len() == 1) {
        Some("--version") => {
            println!("fandhe-browser-servo-host {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--selftest") => match selftest() {
            Ok(elapsed) => {
                println!("selftest ok: page load completed in {elapsed:?}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("error: selftest failed: {message}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!(
                "error: fandhe-browser-servo-host supports only --version, --selftest and --stdio"
            );
            ExitCode::FAILURE
        }
    }
}

/// stdio サーバーを標準入出力で動かし、終了理由を終了コードへ対応づける。
///
/// stdout はフレーム専用（`println!` 等を stdio 経路に置かない）。ログは stderr のみ。
/// `Engine`（Servo）は hello の送信後、最初の `load` で作る（ハンドシェイクの 10 秒以内に
/// hello を出すため）。Servo 自身が stdout へ書き出すリスクは設計 §2.1 を参照。
fn run_stdio() -> ExitCode {
    let mut handler = page::ServoHandler::new();
    let outcome = server::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &mut handler,
        &mut std::io::stderr().lock(),
    );
    match outcome {
        ServeOutcome::Shutdown | ServeOutcome::InputClosed => ExitCode::SUCCESS,
        ServeOutcome::ProtocolViolation(_) => ExitCode::from(EXIT_PROTOCOL_VIOLATION),
        ServeOutcome::Io(_) => ExitCode::FAILURE,
    }
}

/// `--selftest` の進行段階を stderr に 1 行出す（#1413）。
///
/// AppContainer 内の異常終了（0xC0000409）で panic メッセージが残らない場合でも、最後に出た段階から
/// 死んだ位置を絞れるようにする。egress 隔離ハーネス（`harness/egress-isolation/`）が末尾を回収する。
/// stdout の `selftest ok:` と終了コードの契約には影響しない。
fn selftest_stage(stage: &str) {
    eprintln!("selftest: {stage}");
}

/// Servo を生成して `data:` ページの読み込み完了を待ち、破棄と解放を確かめて所要時間を返す。
///
/// 失敗（生成・URL 解析・タイムアウト・解放漏れ・二重生成の許容）は `Err` に倒す。
/// WebView の生成と読み込み待ちは `page::navigate` を使う。
fn selftest() -> Result<Duration, String> {
    let engine = Engine::new().map_err(|e| e.to_string())?;
    selftest_stage("engine created");
    let weak_context = engine.context_weak();
    let url = ServoUrl::parse(SELFTEST_URL).map_err(|e| format!("invalid selftest url: {e}"))?;
    let start = Instant::now();
    let webview = page::navigate(&engine, url, start + SELFTEST_TIMEOUT)
        .map_err(|e| format!("page load did not complete within {SELFTEST_TIMEOUT:?}: {e:?}"))?;
    let elapsed = start.elapsed();
    selftest_stage("page loaded");

    // WebView を先に解放してから Engine を破棄する（Servo の Drop が終了を待つ）。
    drop(webview);
    selftest_stage("webview dropped");
    drop(engine);
    selftest_stage("engine dropped");
    if weak_context.upgrade().is_some() {
        return Err("rendering context leaked after engine drop".to_string());
    }
    // 1 プロセス 1 回の制約: 2 回目の生成は panic せず AlreadyCreated で拒否される。
    match Engine::new() {
        Err(EngineError::AlreadyCreated) => {}
        Err(e) => return Err(format!("unexpected error on second engine: {e}")),
        Ok(_) => return Err("second engine creation was not rejected".to_string()),
    }
    selftest_stage("second engine rejected");
    Ok(elapsed)
}

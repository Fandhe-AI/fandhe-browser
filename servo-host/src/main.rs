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
//! - それ以外: 未実装として標準エラーに示し、非ゼロで終了する（REPAIR-3）。
//!
//! Servo と描画コンテキストの生成・破棄は `engine` が担う（RENDER-3・#230）。
//!
//! 将来仕様: 本体 render crate からの stdio サーバー（#993）、WebView・読み込み（#231）、
//! スクリーンショット（#232）、可視性・境界ボックス（RENDER-4）。

mod engine;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use engine::{Engine, EngineError};
use servo::{LoadStatus, ServoUrl, WebViewBuilder};

/// ページ読み込み完了を待つ上限。超過は失敗として扱い、成功を装わない。
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(60);
/// セルフテストで読み込むページ。`data:` のためネットワークアクセスは発生しない。
const SELFTEST_URL: &str = "data:text/html,<h1>selftest</h1>";

/// 引数に応じて `--version`・`--selftest` を処理し、他は未実装として失敗終了する。
fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
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
                "error: fandhe-browser-servo-host supports only --version and --selftest \
                 (stdio server is not implemented yet)"
            );
            ExitCode::FAILURE
        }
    }
}

/// Servo を生成して `data:` ページの読み込み完了を待ち、破棄と解放を確かめて所要時間を返す。
///
/// 失敗（生成・URL 解析・タイムアウト・解放漏れ・二重生成の許容）は `Err` に倒す。
/// WebView の生成と読み込み待ちは #231 で `page` へ移すまでここに置く。
fn selftest() -> Result<Duration, String> {
    let engine = Engine::new().map_err(|e| e.to_string())?;
    let weak_context = engine.context_weak();
    let url = ServoUrl::parse(SELFTEST_URL)
        .map_err(|e| format!("invalid selftest url: {e}"))?
        .into_url();
    let webview = WebViewBuilder::new(engine.servo(), engine.rendering_context())
        .url(url)
        .build();

    let start = Instant::now();
    while webview.load_status() != LoadStatus::Complete {
        if start.elapsed() > SELFTEST_TIMEOUT {
            return Err(format!(
                "page load did not complete within {SELFTEST_TIMEOUT:?}"
            ));
        }
        engine.servo().spin_event_loop();
        std::thread::sleep(Duration::from_millis(5));
    }
    let elapsed = start.elapsed();

    // WebView を先に解放してから Engine を破棄する（Servo の Drop が終了を待つ）。
    drop(webview);
    drop(engine);
    if weak_context.upgrade().is_some() {
        return Err("rendering context leaked after engine drop".to_string());
    }
    // 1 プロセス 1 回の制約: 2 回目の生成は panic せず AlreadyCreated で拒否される。
    match Engine::new() {
        Err(EngineError::AlreadyCreated) => {}
        Err(e) => return Err(format!("unexpected error on second engine: {e}")),
        Ok(_) => return Err("second engine creation was not rejected".to_string()),
    }
    Ok(elapsed)
}

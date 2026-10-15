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
//! 将来仕様: 本体 render crate からの stdio サーバー（#993）、スクリーンショット
//! （RENDER-3・#230）、可視性・境界ボックス（RENDER-4）。

use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use servo::{LoadStatus, ServoBuilder, ServoUrl, SoftwareRenderingContext, WebViewBuilder};

/// セルフテストで描画する画面の一辺（px）。初期化確認に足りる最小限にする。
const SELFTEST_SIZE: u32 = 64;
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

/// Servo を初期化して `data:` ページの読み込み完了を待ち、所要時間を返す。
///
/// 失敗（コンテキスト作成・URL 解析・タイムアウト）は `Err` に倒す。`Servo` は
/// スコープ終了の `Drop` で停止する。
fn selftest() -> Result<Duration, String> {
    let context = SoftwareRenderingContext::new((SELFTEST_SIZE, SELFTEST_SIZE).into())
        .map_err(|e| format!("cannot create software rendering context: {e:?}"))?;
    let servo = ServoBuilder::default().build();
    let url = ServoUrl::parse(SELFTEST_URL)
        .map_err(|e| format!("invalid selftest url: {e}"))?
        .into_url();
    let webview = WebViewBuilder::new(&servo, Rc::new(context))
        .url(url)
        .build();

    let start = Instant::now();
    while webview.load_status() != LoadStatus::Complete {
        if start.elapsed() > SELFTEST_TIMEOUT {
            return Err(format!(
                "page load did not complete within {SELFTEST_TIMEOUT:?}"
            ));
        }
        servo.spin_event_loop();
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(start.elapsed())
}

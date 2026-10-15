//! Servo ホスト（`fandhe-browser-servo-host`）のエントリポイント（足場）。
//!
//! 本体の workspace から分離した別 workspace に置く子プロセス用バイナリで、Servo
//! （MPL-2.0）を本体の依存グラフへ入れないための隔離境界（RENDER-1・TASK-38・#915・#935）。
//!
//! 現状は未実装のスタブ。実装済みを装わないため（REPAIR-3）、起動しても何も処理せず
//! 標準エラーへ未実装を示して非ゼロで終了する。将来仕様:
//! - Servo 組込と `--selftest`（#988・RENDER-3）
//! - 本体 render crate からの stdio サーバー（#993）

use std::process::ExitCode;

/// `--version` のみ受理し、それ以外は未実装として失敗終了する。
fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("fandhe-browser-servo-host {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    eprintln!("error: fandhe-browser-servo-host is not implemented yet (scaffold only)");
    ExitCode::FAILURE
}

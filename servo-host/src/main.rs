//! Servo ホスト（`fandhe-browser-servo-host`）のエントリポイント。
//!
//! 本体の workspace から分離した別 workspace に置く子プロセス用バイナリで、Servo
//! （MPL-2.0）を本体の依存グラフへ入れないための隔離境界（RENDER-1・TASK-38・#915・#935）。
//!
//! 現状の実装範囲:
//! - `--version`: 版を表示して成功終了する。
//! - `--selftest`: Servo の初期化・小さな `data:` ページの読み込み完了・終了までを行い、
//!   成功時のみ終了コード 0 を返す（RENDER-3・#988。設計 §1.4・§2.6・§2.7）。
//!   ネットワークへは出ない。呼び出し元は開発者と CI のみ。Windows では結果の出力後に Servo の後始末を
//!   通さず `process::exit` で終わる（AppContainer 内の ROUTER の panic 回避。#1413。`EXIT_BEFORE_SERVO_TEARDOWN`）。
//! - `--stdio`: 本体 render crate から起動される stdio サーバー（#993。`server`）。`load`（#231）と
//!   `screenshot`（PNG の base64 inline。#232）・`visibility`（JS 評価。#233。`layout`）は
//!   `page::ServoHandler` で実装済み。`bounding_box`（JS 評価。#234）も同じ `layout` 経由で実装済み。
//!   `--egress-proxy-port <port>`（macOS 向け）・`--egress-relay-uds <path>`（Linux 向け。netns 内の
//!   `relay` が `127.0.0.1` で待ち受けて UDS へ転送）のどちらか 1 つを足すと、Servo の proxy pref を
//!   `http://127.0.0.1:<port>` に向ける（`args` が検証。RENDER-3・SEC-2・#1435。設計 §2.6）。
//! - それ以外: 標準エラーに示し、非ゼロで終了する（固定の許可リスト。SEC-2）。
//!
//! Servo と描画コンテキストの生成・破棄は `engine` が担う（RENDER-3・#230）。
//!
//! WebView の生成・読み込み・フレーム待機は `page` が担う（RENDER-3・RENDER-4・#231）。
//!
//! PNG エンコードと base64 化は Servo 非依存の `screenshot` が担う（RENDER-3・#232）。
//!
//! 可視性判定・境界ボックス取得のスクリプト生成と結果解釈は Servo 非依存の `layout` が担う（RENDER-3・#233・#234）。

mod args;
mod engine;
mod layout;
mod page;
#[cfg(unix)]
mod relay;
mod screenshot;
mod server;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use args::{Command, Egress};
use engine::{Engine, EngineError, LoopbackProxy};
use server::ServeOutcome;
use servo::ServoUrl;

/// ページ読み込み完了を待つ上限。超過は失敗として扱い、成功を装わない。
const SELFTEST_TIMEOUT: Duration = Duration::from_secs(60);
/// セルフテストで読み込むページ。`data:` のためネットワークアクセスは発生しない。
const SELFTEST_URL: &str = "data:text/html,<h1>selftest</h1>";

/// `--stdio` の終了コード: プロトコル違反（応答なしで終了した）。
const EXIT_PROTOCOL_VIOLATION: u8 = 2;

/// Servo の後始末（`Engine` の drop）を通さずに `process::exit` で終わるか（#1413）。
///
/// Windows の AppContainer 内では、Servo の `Constellation` スレッドが終了処理（`handle_shutdown`）で
/// `ipc-channel` の `ROUTER`（遅延初期化）を初めて参照し、`RouterProxy::new` の `ipc::channel().unwrap()`
/// が名前付きパイプの作成拒否（`E_ACCESSDENIED`）で panic して 0xC0000409 になる。Servo・`ipc-channel`
/// は改変できない（licensing）ため、結果を出力して flush した直後に終了して後始末を飛ばす。
/// WebView の drop は `ROUTER` を参照しないので通す。AppContainer かどうかの判定には `unsafe` か依存が
/// 要るため判定せず、Windows 全体に適用する（Linux・macOS は名前付きパイプを使わず従来どおり後始末する）。
/// 実行時の引数や環境変数では切り替えない（固定の許可リスト。SEC-2）。
const EXIT_BEFORE_SERVO_TEARDOWN: bool = cfg!(windows);

/// 終了コード `code` で終わる。`EXIT_BEFORE_SERVO_TEARDOWN` が真なら stdout・stderr を flush して
/// その場で `process::exit` する（呼び出し元の `Engine` は drop されない）。偽なら `ExitCode` を返す。
fn finish(code: u8) -> ExitCode {
    if EXIT_BEFORE_SERVO_TEARDOWN {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        // stderr が閉じていても panic しない（release は `panic = "abort"` で、`eprintln!` の panic は
        // `process::exit` に届く前に abort になるため）。書き込み・flush の失敗は無視する。
        let _ = writeln!(std::io::stderr(), "host exiting before servo teardown");
        let _ = std::io::stderr().flush();
        std::process::exit(i32::from(code));
    }
    ExitCode::from(code)
}

/// 引数に応じて `--version`・`--selftest`・`--stdio` を処理し、他は失敗終了する。
fn main() -> ExitCode {
    // 非 UTF-8 の引数で panic しないよう `args_os` で受け、変換できなければ Usage と同じ失敗にする。
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(std::ffi::OsString::into_string)
        .collect();
    let parsed = args
        .map_err(|_| args::ArgError::Usage)
        .and_then(|a| args::parse(&a));
    match parsed {
        Ok(Command::Version) => {
            println!("fandhe-browser-servo-host {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Selftest) => run_selftest(),
        Ok(Command::Stdio(egress)) => run_stdio(egress),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// stdio サーバーを標準入出力で動かし、終了理由を終了コードへ対応づける。
///
/// stdout はフレーム専用（`println!` 等を stdio 経路に置かない）。ログは stderr のみ。
/// `Engine`（Servo）は hello の送信後、最初の `load` で作る（ハンドシェイクの 10 秒以内に
/// hello を出すため）。Servo 自身が stdout へ書き出すリスクは設計 §2.1 を参照。
fn run_stdio(egress: Egress) -> ExitCode {
    // 中継は hello の前に起動する（Servo 未生成のため失敗時は `finish` 不要）。プロセス終了まで保持する。
    #[cfg(unix)]
    let mut _relay = None;
    let proxy = match egress {
        Egress::None => None,
        Egress::ProxyPort(port) => Some(LoopbackProxy::new(port)),
        #[cfg(unix)]
        Egress::RelayUds(path) => match relay::start(path) {
            Ok(r) => {
                let proxy = LoopbackProxy::new(r.port());
                _relay = Some(r);
                Some(proxy)
            }
            Err(e) => {
                eprintln!("error: cannot start egress relay: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    // `handler` が `Engine` を持つ。Windows では drop の前に `finish` で終了する（#1413）。
    let mut handler = page::ServoHandler::new(proxy);
    let outcome = server::serve(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &mut handler,
        &mut std::io::stderr().lock(),
    );
    let code = match outcome {
        ServeOutcome::Shutdown | ServeOutcome::InputClosed => 0,
        ServeOutcome::ProtocolViolation(_) => EXIT_PROTOCOL_VIOLATION,
        ServeOutcome::Io(_) => 1,
    };
    finish(code)
}

/// `--selftest` の進行段階を stderr に 1 行出す（#1413）。
///
/// AppContainer 内の異常終了（0xC0000409）で panic メッセージが残らない場合でも、最後に出た段階から
/// 死んだ位置を絞れるようにする。egress 隔離ハーネス（`harness/egress-isolation/`）が末尾を回収する。
/// stdout の `selftest ok:` と終了コードの契約には影響しない。
fn selftest_stage(stage: &str) {
    eprintln!("selftest: {stage}");
}

/// `--selftest` を実行して終了コードへ写す。`Engine` はこの関数が所有し、Windows では drop の前に
/// `finish` で終了する（#1413。成功・失敗とも。失敗が 0xC0000409 に化けないようにする）。
fn run_selftest() -> ExitCode {
    let engine = match Engine::new(None) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: selftest failed: {e}");
            return finish(1);
        }
    };
    selftest_stage("engine created");
    let result = selftest_run(&engine);
    match result {
        Ok(elapsed) if EXIT_BEFORE_SERVO_TEARDOWN => {
            // Engine は生かしたまま 2 回目の生成が拒否されることだけ確かめる（drop 後の確認は Windows では省く）。
            match selftest_second_engine() {
                Ok(()) => {
                    selftest_stage("exiting before engine drop");
                    println!("selftest ok: page load completed in {elapsed:?}");
                    finish(0)
                }
                Err(message) => {
                    eprintln!("error: selftest failed: {message}");
                    finish(1)
                }
            }
        }
        Ok(elapsed) => match selftest_teardown_checks(engine) {
            Ok(()) => {
                println!("selftest ok: page load completed in {elapsed:?}");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("error: selftest failed: {message}");
                ExitCode::FAILURE
            }
        },
        Err(message) => {
            eprintln!("error: selftest failed: {message}");
            finish(1)
        }
    }
}

/// `data:` ページの読み込み完了を待ち、WebView を破棄して所要時間を返す。
///
/// 失敗（URL 解析・タイムアウト）は `Err`。WebView の生成と読み込み待ちは `page::navigate` を使う。
/// WebView の drop は Servo の `ROUTER` を参照しないので Windows でも通す（#1413）。
fn selftest_run(engine: &Engine) -> Result<Duration, String> {
    let url = ServoUrl::parse(SELFTEST_URL).map_err(|e| format!("invalid selftest url: {e}"))?;
    let start = Instant::now();
    let webview = page::navigate(engine, url, start + SELFTEST_TIMEOUT)
        .map_err(|e| format!("page load did not complete within {SELFTEST_TIMEOUT:?}: {e:?}"))?;
    let elapsed = start.elapsed();
    selftest_stage("page loaded");
    drop(webview);
    selftest_stage("webview dropped");
    Ok(elapsed)
}

/// 1 プロセス 1 回の制約: 2 回目の生成は panic せず `AlreadyCreated` で拒否される。
fn selftest_second_engine() -> Result<(), String> {
    match Engine::new(None) {
        Err(EngineError::AlreadyCreated) => {}
        Err(e) => return Err(format!("unexpected error on second engine: {e}")),
        Ok(_) => return Err("second engine creation was not rejected".to_string()),
    }
    selftest_stage("second engine rejected");
    Ok(())
}

/// Linux・macOS: Engine を破棄して描画コンテキストの解放漏れと二重生成の拒否を確かめる。
fn selftest_teardown_checks(engine: Engine) -> Result<(), String> {
    let weak_context = engine.context_weak();
    // Servo の Drop が終了を待つ。
    drop(engine);
    selftest_stage("engine dropped");
    if weak_context.upgrade().is_some() {
        return Err("rendering context leaked after engine drop".to_string());
    }
    selftest_second_engine()
}

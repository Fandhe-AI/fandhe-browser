//! Servo ホストバイナリの生成・破棄を外部プロセスとして検証する結合テスト（RENDER-3・#230）。
//!
//! `--selftest` が制限時間内に終了コード 0 で戻ること（Servo の Drop が止まらない・
//! 解放漏れ・二重生成の許容がないこと）と、任意の引数が受け付けられないこと（SEC-2）を確かめる。
//! このテストバイナリ自体は Servo をリンクしない。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(180);

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser-servo-host"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn servo host");
    // パイプ満杯で子が書き込み停止しないよう、待機中から別スレッドで並行して読み出す。
    let drain = |s: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut s) = s {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        })
    };
    let out_h = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err_h = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("servo host did not exit within {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = out_h.join().unwrap_or_default();
    let err = err_h.join().unwrap_or_default();
    (status.code(), out, err)
}

/// #1413: Windows ではホストが結果の出力後に Servo の後始末を通さず終了する（AppContainer 内の
/// ROUTER の panic 回避）ため、`engine dropped` の段階は出ず終了マーカーが出る。他 OS は従来どおり。
#[test]
fn selftest_creates_and_drops_engine_within_limit() {
    let (code, out, err) = run(&["--selftest"]);
    assert_eq!(code, Some(0), "stderr: {err}");
    assert!(out.starts_with("selftest ok:"), "stdout: {out}");
    let has = |s: &str| err.contains(s);
    for stage in [
        "engine created",
        "page loaded",
        "webview dropped",
        "second engine rejected",
    ] {
        assert!(has(&format!("selftest: {stage}")), "missing {stage}: {err}");
    }
    let marker = "host exiting before servo teardown";
    if cfg!(windows) {
        assert!(has("selftest: exiting before engine drop"), "{err}");
        assert!(has(marker), "{err}");
        assert!(!has("selftest: engine dropped"), "{err}");
    } else {
        assert!(has("selftest: engine dropped"), "{err}");
        assert!(!has(marker), "{err}");
    }
}

#[test]
fn unknown_and_missing_args_are_rejected() {
    for args in [&["--unknown"][..], &[][..]] {
        let (code, _out, err) = run(args);
        assert_eq!(code, Some(1), "args: {args:?}");
        assert!(
            err.contains("supports only --version, --selftest and --stdio"),
            "stderr: {err}"
        );
    }
}

/// RENDER-3・#1540: `--version` の出力形式は CLI 側の版の検査（`fandhe-browser-cli` の
/// `render_host/version.rs`）が解析する契約。終了コード 0・stdout は 1 行・stderr は空で固定する。
#[test]
fn version_output_is_the_contract_read_by_the_cli() {
    let (code, out, err) = run(&["--version"]);
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(
        out,
        format!("fandhe-browser-servo-host {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(err, "");
}

/// RENDER-3・SEC-2（#1435）: `--selftest` と `--version` は egress 引数を受け付けない。
#[test]
fn selftest_and_version_reject_egress_arguments() {
    for args in [
        &["--selftest", "--egress-proxy-port", "80"][..],
        &["--version", "--egress-proxy-port", "80"],
    ] {
        let (code, out, err) = run(args);
        assert_eq!(code, Some(1), "args: {args:?}");
        assert_eq!(out, "", "args: {args:?}");
        assert!(
            err.contains("supports only --version, --selftest and --stdio"),
            "stderr: {err}"
        );
    }
}

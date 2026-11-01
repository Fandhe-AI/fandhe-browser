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

#[test]
fn selftest_creates_and_drops_engine_within_limit() {
    let (code, out, err) = run(&["--selftest"]);
    assert_eq!(code, Some(0), "stderr: {err}");
    assert!(out.starts_with("selftest ok:"), "stdout: {out}");
    // #1413: 異常終了時に死んだ段階を絞れるよう、進行段階が stderr に出る。
    for stage in [
        "engine created",
        "page loaded",
        "engine dropped",
        "second engine rejected",
    ] {
        assert!(
            err.contains(&format!("selftest: {stage}")),
            "missing stage {stage}: {err}"
        );
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

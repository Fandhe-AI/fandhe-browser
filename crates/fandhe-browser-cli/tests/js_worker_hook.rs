//! cli の `main` 先頭の JS ワーカーフック（TASK-30・ビヘイビア `JS-2`・MS-3・#514）の結合テスト。
//!
//! マーカー環境変数付きで起動した実バイナリが、通常の起動処理（サーバー bind・
//! `DevTools listening` の出力・将来の clap 解析）へ進まず、フックで終了することを確かめる。
//! フックが配線されていないと通常起動に進みサーバーが居座るため、期限付きで監視して kill する。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// フック未配線でもテストが固まらないための上限。
const DEADLINE: Duration = Duration::from_secs(30);

#[test]
fn js2_worker_marker_short_circuits_cli_startup() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fandhe-browser"))
        // 数値でない値: js-v8 ありでは V8 初期化より前に即 FAILURE になり、3 OS で決定的。
        .env("FANDHE_BROWSER_JS_WORKER", "not-a-version")
        // 将来 clap が入っても、フックが引数解析より前にあることを守らせる。
        .arg("--definitely-not-a-cli-flag")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn fandhe-browser");

    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("failed to poll child") {
            break Some(s);
        }
        if start.elapsed() > DEADLINE {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let mut buf = Vec::new();
        let _ = e.read_to_end(&mut buf);
        stderr = String::from_utf8_lossy(&buf).into_owned();
    }
    let status = status.unwrap_or_else(|| {
        panic!("worker hook did not terminate the process in time; stderr: {stderr}")
    });

    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("fandhe-browser-js worker:"),
        "worker hook did not run; stderr: {stderr}"
    );
    // `--workspace` では feature 統合で js 側の js-v8 が有効になりうるため、js-v8 なしビルドの
    // メッセージは必須にしない（共通接頭辞と終了コードで検証する）。
    #[cfg(feature = "js-v8")]
    assert!(
        stderr.contains("is not a valid protocol version"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("DevTools listening on"),
        "normal startup ran; stderr: {stderr}"
    );
    assert!(
        !stderr.lines().any(|l| l.starts_with("error: ")),
        "normal startup error path ran; stderr: {stderr}"
    );
}

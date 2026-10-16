//! Playwright 接続試験の実行基盤の自己テスト（TASK-44.1・#476、ビヘイビア `CDP-2`・MS-4）。
//!
//! 共有基盤 `script_harness`（TASK-45.1・#480）に HTTP エンドポイントの受け渡し・ローカル
//! fixture サーバー・Playwright 用 preflight を足した部分の検証。偽スクリプトはテストバイナリ
//! 自身の再実行で実現する（`puppeteer_connect.rs` と同じ流儀）ため、通常の
//! `cargo test --workspace` では Node も Playwright も要らない。
//!
//! 実 Playwright を使う `cdp2_real_playwright_flow_is_collected` だけは導入が必要なため
//! `#[ignore]` とし、`make test-playwright-connect`（`harness/playwright-connect/run.sh`）が
//! `--ignored --exact` で明示実行する（0 件実行は失敗）。未導入での実行は skip せず
//! [`HarnessError::ToolUnavailable`] で panic する（成功を装わない）。段階の成否の固定
//! （newPage 到達・goto・セレクタ）は TASK-44.2〜44.4 の担当で、ここでは回収できることだけを見る。
//!
//! `Profile::open` が非 unix で `Unsupported` を返す仕様のため `#[cfg(unix)]`。

#![cfg(unix)]

mod script_harness;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use script_harness::{
    FIXTURE_URL_ENV, FixtureRoute, HarnessError, PLAYWRIGHT_INSTALL_HINT,
    PLAYWRIGHT_MODULE_DIR_ENV, ScriptCommand, ScriptError, ScriptOutcome, StageResult, StageStatus,
    TempDir, http_endpoint, preflight_playwright, run_script, start_fixture_server, start_server,
};

const MODE_ENV: &str = "FANDHE_FAKE_PLAYWRIGHT_MODE";
const D: Duration = Duration::from_secs(60);

/// 偽スクリプトの子側入口。`MODE_ENV` が無ければ何もしない（通常実行では no-op）。
#[test]
fn fake_playwright_child_entry() {
    let Ok(mode) = std::env::var(MODE_ENV) else {
        return;
    };
    // libtest は `test <name> ... ` を改行なしで出すため、結果行が行頭に来るよう改行を挟む。
    println!();
    let ep = std::env::var(script_harness::ENDPOINT_ENV).unwrap_or_default();
    let fx = std::env::var(FIXTURE_URL_ENV).unwrap_or_default();
    match mode.as_str() {
        "envs" => {
            println!("FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"{ep} {fx}\",\"error\":null}}")
        }
        "staged_ok" => println!(
            "FANDHE_SCRIPT_RESULT {{\"ok\":true,\"step\":\"newPage\",\"error\":null,\"stages\":[{{\"name\":\"connect\",\"status\":\"ok\",\"error\":null}},{{\"name\":\"newContext\",\"status\":\"ok\",\"error\":null}},{{\"name\":\"newPage\",\"status\":\"ok\",\"error\":null}}]}}"
        ),
        "staged_fail" => {
            println!(
                "FANDHE_SCRIPT_RESULT {{\"ok\":false,\"step\":\"newContext\",\"error\":{{\"name\":\"Error\",\"message\":\"Browser.setDownloadBehavior failed\"}},\"stages\":[{{\"name\":\"connect\",\"status\":\"ok\",\"error\":null}},{{\"name\":\"newContext\",\"status\":\"failed\",\"error\":{{\"name\":\"Error\",\"message\":\"Browser.setDownloadBehavior failed\"}}}},{{\"name\":\"newPage\",\"status\":\"not_reached\",\"error\":null}}]}}"
            );
            std::process::exit(1);
        }
        other => panic!("unknown mode {other}"),
    }
    std::process::exit(0);
}

fn fake(mode: &str, fixture_url: Option<String>) -> ScriptCommand {
    let mut envs = vec![(MODE_ENV.to_string(), mode.to_string())];
    if let Some(u) = fixture_url {
        envs.push((FIXTURE_URL_ENV.to_string(), u));
    }
    ScriptCommand {
        program: std::env::current_exe().expect("current_exe"),
        args: [
            "fake_playwright_child_entry",
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ]
        .map(String::from)
        .to_vec(),
        envs,
        cwd: None,
    }
}

fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).expect("write");
    let mut raw = String::new();
    s.take(64 * 1024).read_to_string(&mut raw).expect("read");
    raw
}

fn stage(name: &str, status: StageStatus, error: Option<(&str, &str)>) -> StageResult {
    StageResult {
        name: name.into(),
        status,
        error: error.map(|(n, m)| ScriptError {
            name: n.into(),
            message: m.into(),
        }),
    }
}

/// CDP-2: スクリプトへ HTTP エンドポイントと fixture URL が環境変数で渡る。
#[test]
fn cdp2_passes_http_endpoint_and_fixture_url_to_script() {
    let server = start_server();
    let fixture = start_fixture_server(vec![FixtureRoute::html("/page", "<p>x</p>")]);
    let ep = http_endpoint(server.addr);
    assert_eq!(ep, format!("http://127.0.0.1:{}", server.addr.port()));
    let url = fixture.url("/page");
    let out = run_script(&fake("envs", Some(url.clone())), &ep, D);
    assert_eq!(
        out,
        ScriptOutcome::Completed {
            ok: true,
            step: format!("{ep} {url}"),
            error: None,
            stages: vec![],
        }
    );
}

/// CDP-2: connect / newContext / newPage の段階別成功結果を回収できる。
#[test]
fn cdp2_collects_staged_success() {
    let out = run_script(&fake("staged_ok", None), "http://127.0.0.1:1", D);
    assert_eq!(
        out,
        ScriptOutcome::Completed {
            ok: true,
            step: "newPage".into(),
            error: None,
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newContext", StageStatus::Ok, None),
                stage("newPage", StageStatus::Ok, None),
            ],
        }
    );
}

/// CDP-2: 失敗段階のエラー名・メッセージと未到達段階を回収できる。
#[test]
fn cdp2_collects_failure_with_error() {
    let err = ("Error", "Browser.setDownloadBehavior failed");
    let out = run_script(&fake("staged_fail", None), "http://127.0.0.1:1", D);
    assert_eq!(
        out,
        ScriptOutcome::Completed {
            ok: false,
            step: "newContext".into(),
            error: Some(ScriptError {
                name: err.0.into(),
                message: err.1.into(),
            }),
            stages: vec![
                stage("connect", StageStatus::Ok, None),
                stage("newContext", StageStatus::Failed, Some(err)),
                stage("newPage", StageStatus::NotReached, None),
            ],
        }
    );
}

/// CDP-2: fixture サーバーは定義済みルートの本文を返し、到達パスを記録する。
#[test]
fn cdp2_fixture_server_serves_body_and_records_hits() {
    let fixture = start_fixture_server(vec![FixtureRoute::html("/a", "<h1>hi</h1>")]);
    let ok = http_get(fixture.addr, "/a");
    assert!(ok.starts_with("HTTP/1.1 200 X\r\n"), "{ok}");
    assert!(ok.ends_with("\r\n\r\n<h1>hi</h1>"), "{ok}");
    assert!(ok.contains("Content-Length: 11\r\n"), "{ok}");
    let missing = http_get(fixture.addr, "/zzz");
    assert!(missing.starts_with("HTTP/1.1 404 X\r\n"), "{missing}");
    assert_eq!(fixture.hits(), vec!["/a".to_string(), "/zzz".to_string()]);
    assert_eq!(
        fixture.url("/a"),
        format!("http://127.0.0.1:{}/a", fixture.addr.port())
    );
}

fn unavailable(reason: &str) -> HarnessError {
    HarnessError::ToolUnavailable {
        tool: "playwright-core".into(),
        reason: reason.into(),
        hint: PLAYWRIGHT_INSTALL_HINT.into(),
    }
}

/// CDP-2: 導入先が未指定なら skip せず理由付きで明示的に失敗する。
#[test]
fn cdp2_preflight_fails_explicitly_without_module_dir() {
    assert_eq!(
        preflight_playwright(None, Path::new("."), "1.63.0").unwrap_err(),
        unavailable("FANDHE_PLAYWRIGHT_MODULE_DIR is not set")
    );
}

/// CDP-2: 導入先に playwright-core が無ければ理由付きで失敗する。
#[test]
fn cdp2_preflight_fails_explicitly_when_not_installed() {
    let dir = TempDir::new();
    assert_eq!(
        preflight_playwright(Some(&dir.0), Path::new("."), "1.63.0").unwrap_err(),
        unavailable("playwright-core is not installed")
    );
}

fn install_fake_package(dir: &Path, package_json: &str) {
    let pkg = dir.join("node_modules").join("playwright-core");
    std::fs::create_dir_all(&pkg).expect("mkdir");
    std::fs::write(pkg.join("package.json"), package_json).expect("write");
}

/// CDP-2: 期待版と異なる版は理由付きで失敗する（版の取り違えを成功扱いにしない）。
#[test]
fn cdp2_preflight_fails_explicitly_on_version_mismatch() {
    let dir = TempDir::new();
    install_fake_package(&dir.0, r#"{"name":"playwright-core","version":"1.62.0"}"#);
    assert_eq!(
        preflight_playwright(Some(&dir.0), Path::new("."), "1.63.0").unwrap_err(),
        unavailable("playwright-core version mismatch: installed 1.62.0, expected 1.63.0")
    );
    let dir2 = TempDir::new();
    install_fake_package(&dir2.0, r#"{"name":"playwright-core"}"#);
    assert_eq!(
        preflight_playwright(Some(&dir2.0), Path::new("."), "1.63.0").unwrap_err(),
        unavailable("playwright-core package.json has no version")
    );
}

/// CDP-2: 版が一致すれば `node connect.mjs`（導入先を環境変数で渡す）の起動定義を返す。
#[test]
fn cdp2_preflight_returns_command_when_version_matches() {
    let dir = TempDir::new();
    install_fake_package(&dir.0, r#"{"name":"playwright-core","version":"1.63.0"}"#);
    match preflight_playwright(Some(&dir.0), Path::new("h"), "1.63.0") {
        Ok(cmd) => {
            assert_eq!(cmd.program, PathBuf::from("node"));
            assert_eq!(cmd.args, vec!["connect.mjs".to_string()]);
            assert_eq!(
                cmd.envs,
                vec![(
                    PLAYWRIGHT_MODULE_DIR_ENV.to_string(),
                    dir.0.to_string_lossy().into_owned()
                )]
            );
            assert_eq!(cmd.cwd, Some(PathBuf::from("h")));
        }
        // node が無い環境では node 不在として明示的に失敗する（判定順の確認）。
        Err(e) => assert_eq!(e, unavailable("node executable was not found")),
    }
}

/// CDP-2: 実 Playwright で `connect.mjs` を実行し、結果行を `Completed` として回収できる。
/// 段階の成否（newPage 到達等）は固定しない（TASK-44.2 以降の担当）。導入が必要なため
/// `make test-playwright-connect` が `--ignored --exact` で明示実行する。
#[test]
#[ignore = "requires playwright-core; run via `make test-playwright-connect`"]
fn cdp2_real_playwright_flow_is_collected() {
    let module_dir = std::env::var_os(PLAYWRIGHT_MODULE_DIR_ENV).map(PathBuf::from);
    let version = std::env::var("FANDHE_PLAYWRIGHT_VERSION")
        .expect("FANDHE_PLAYWRIGHT_VERSION must be set (see harness/playwright-connect/run.sh)");
    let harness_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/playwright-connect");
    let mut cmd = preflight_playwright(module_dir.as_deref(), &harness_dir, &version)
        .unwrap_or_else(|e| panic!("playwright is unavailable (not skipping): {e:?}"));

    let server = start_server();
    let fixture =
        start_fixture_server(vec![FixtureRoute::html("/index.html", "<p id=\"x\">x</p>")]);
    cmd.envs
        .push((FIXTURE_URL_ENV.to_string(), fixture.url("/index.html")));
    match run_script(&cmd, &http_endpoint(server.addr), Duration::from_secs(120)) {
        ScriptOutcome::Completed { stages, .. } => {
            let names: Vec<&str> = stages.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, vec!["connect", "newContext", "newPage"]);
        }
        other => panic!("script result was not collected: {other:?}"),
    }
}

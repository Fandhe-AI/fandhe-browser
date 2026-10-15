//! ホスト子プロセス管理の結合テスト（RENDER-3・RENDER-4・PLUG-1・PROF・SEC-2。
//! TASK-38・#994）。
//!
//! テストバイナリ自身を `--stdio` 付きで再実行して「偽ホスト」にする（`harness = false`。
//! 理由は `Cargo.toml` の `[[test]]` コメント）。偽ホストの cwd は `HostProcess` が作る専用
//! ディレクトリで、その親がテストの `temp_root`。シナリオは `temp_root/scenario`（1 行が
//! 1 回目、2 回目…の起動に対応し、足りなければ最終行を繰り返す）から読み、観測結果は
//! `temp_root` 配下へ書く。テストごとに専用の `temp_root` を使うため干渉しない。
//! 待ち時間は短縮値で検証し、実秒（10 s・30 s）は待たない。既定値の assert は単体テスト側。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。

#[cfg(unix)]
use std::collections::BTreeSet;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fandhe_browser_render::codec::{MAX_REQUEST_FRAME_BYTES, parse_json, read_frame, write_frame};
#[cfg(unix)]
use fandhe_browser_render::host_process::{FORCED_ENV_NAMES, PASSTHROUGH_ENV_NAMES};
use fandhe_browser_render::host_process::{
    HOST_ARGS, HostProcess, HostProcessConfig, HostProcessError, HostTimeouts, TimeoutPhase,
    resolve_host_binary,
};
use fandhe_browser_render::protocol::{
    BoundingBoxParams, LoadParams, MethodResult, ProtocolError, Request, ScreenshotParams,
    VisibilityParams,
};
use serde_json::{Value, json};

const HELLO_OK: &str = include_str!("fixtures/protocol/hello-ok.json");

// ---------------------------------------------------------------------------
// 偽ホスト側
// ---------------------------------------------------------------------------

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open");
    f.write_all(text.as_bytes()).expect("append");
}

fn count(root: &Path, name: &str) -> usize {
    std::fs::read_to_string(root.join(name)).map_or(0, |s| s.len())
}

fn send(out: &mut impl Write, id: u64, result: Value) {
    let msg = json!({"v": 1, "id": id, "result": result});
    write_frame(out, &serde_json::to_vec(&msg).unwrap(), 1 << 20).expect("write");
}

fn fake_host() -> ! {
    let cwd = std::env::current_dir().expect("cwd");
    let root = cwd.parent().expect("parent").to_path_buf();
    append(&root.join("spawns"), "x");
    let n = count(&root, "spawns");
    let text = std::fs::read_to_string(root.join("scenario")).expect("scenario");
    let lines: Vec<&str> = text.lines().collect();
    let scenario = lines
        .get(n - 1)
        .or(lines.last())
        .copied()
        .expect("scenario line");
    std::fs::write(
        root.join(format!("pid-{n}")),
        std::process::id().to_string(),
    )
    .expect("pid");

    #[cfg(unix)]
    if scenario == "dump" {
        use std::os::unix::fs::PermissionsExt;
        let mut names: Vec<String> = std::env::vars_os()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        names.sort();
        let obs = json!({
            "args": std::env::args().skip(1).collect::<Vec<_>>(),
            "env": names,
            "cwd": cwd.to_string_lossy(),
            "mode": std::fs::metadata(&cwd).expect("meta").permissions().mode() & 0o777,
        });
        std::fs::write(root.join("obs-dump.json"), obs.to_string()).expect("obs");
    }

    let mut stdout = std::io::stdout().lock();
    match scenario {
        "hello-timeout" => std::thread::sleep(Duration::from_secs(60)),
        "crash-on-start" => std::process::exit(1),
        _ => {}
    }
    let mut hello: Value = serde_json::from_str(HELLO_OK).expect("fixture");
    let mut hello = hello["message"].take();
    if scenario == "bad-protocol-range" {
        hello["data"]["protocol"] = json!({"min": 2, "max": 2});
    }
    write_frame(&mut stdout, &serde_json::to_vec(&hello).unwrap(), 1 << 20).expect("hello");

    let mut stdin = std::io::stdin().lock();
    let mut requests = 0u32;
    loop {
        let Ok(bytes) = read_frame(&mut stdin, MAX_REQUEST_FRAME_BYTES) else {
            std::process::exit(0);
        };
        let v = parse_json(&bytes).expect("request json");
        let id = v["id"].as_u64().expect("id");
        let method = v["method"].as_str().expect("method").to_string();
        requests += 1;
        match (scenario, method.as_str()) {
            ("crash-after-hello", _) => std::process::exit(1),
            ("crash-on-screenshot", "screenshot") => std::process::exit(1),
            ("hang-load", "load")
            | ("hang-query", "screenshot" | "visibility" | "bounding_box") => {
                std::thread::sleep(Duration::from_secs(60));
            }
            ("slow-shutdown", "shutdown") => std::thread::sleep(Duration::from_secs(60)),
            ("garbage-frame", _) => {
                write_frame(&mut stdout, b"not json", 1 << 20).expect("garbage");
                continue;
            }
            ("oversize-frame", _) => {
                stdout
                    .write_all(&0x7fff_ffffu32.to_le_bytes())
                    .expect("hdr");
                stdout.flush().expect("flush");
                std::thread::sleep(Duration::from_secs(60));
            }
            ("id-mismatch", _) => {
                send(
                    &mut stdout,
                    id + 1000,
                    json!({"final_url": "https://example.com/", "status": 200}),
                );
                continue;
            }
            ("stderr-flood", _) => {
                let mut err = std::io::stderr();
                err.write_all(&vec![b'x'; 20_000]).expect("stderr");
                err.write_all(b"\ntail-marker https://a.example/p?token=secret\n")
                    .expect("stderr");
                err.flush().expect("flush");
                std::process::exit(1);
            }
            ("rss", _) => {
                let mut mem = vec![0u8; 96 * 1024 * 1024];
                for i in (0..mem.len()).step_by(4096) {
                    mem[i] = 1;
                }
                std::hint::black_box(&mem);
                std::thread::sleep(Duration::from_secs(60));
            }
            _ => {}
        }
        let _ = requests;
        match method.as_str() {
            "load" => {
                append(&root.join("loads"), "x");
                let url = v["params"]["url"].as_str().expect("url").to_string();
                send(&mut stdout, id, json!({"final_url": url, "status": 200}));
            }
            "screenshot" => send(
                &mut stdout,
                id,
                json!({"format": "png", "width": 1280, "height": 800, "data_base64": "iVBORw0KGgo="}),
            ),
            "visibility" => send(&mut stdout, id, json!({"visible": true})),
            "bounding_box" => send(
                &mut stdout,
                id,
                json!({"x": 8.5, "y": -2, "width": 120.25, "height": 18}),
            ),
            _ => {
                send(&mut stdout, id, json!({}));
                std::process::exit(0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// テスト側
// ---------------------------------------------------------------------------

struct Root(PathBuf);

impl Root {
    fn new(name: &str, scenario: &str) -> Self {
        let p = std::env::temp_dir().join(format!("fandhe-hp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        std::fs::write(p.join("scenario"), scenario).expect("scenario");
        Self(p)
    }
    fn host(&self, timeouts: HostTimeouts) -> HostProcess {
        HostProcess::new(self.config(timeouts))
    }
    fn config(&self, timeouts: HostTimeouts) -> HostProcessConfig {
        let exe = std::env::current_exe().expect("exe");
        let copy = exe;
        let bin = resolve_host_binary(Some(&copy), None, None).expect("binary");
        HostProcessConfig::new(bin)
            .with_timeouts(timeouts)
            .with_temp_root(self.0.clone())
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn base_timeouts() -> HostTimeouts {
    HostTimeouts::default().with_shutdown_grace(Duration::from_secs(1))
}

fn short_timeouts() -> HostTimeouts {
    HostTimeouts::default()
        .with_handshake(Duration::from_secs(5))
        .with_load_default(Duration::from_millis(300))
        .with_load_grace(Duration::from_millis(100))
        .with_screenshot(Duration::from_millis(300))
        .with_element_query(Duration::from_millis(300))
        .with_shutdown_grace(Duration::from_millis(300))
}

fn page() -> LoadParams {
    LoadParams::new("https://example.com/", None).expect("page")
}

fn shot() -> Request {
    Request::Screenshot(ScreenshotParams::new(false))
}

fn child_pid(root: &Root, n: usize) -> u32 {
    std::fs::read_to_string(root.0.join(format!("pid-{n}")))
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid")
}

fn assert_gone(pid: u32) {
    if cfg!(target_os = "linux") {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child {pid} must have been killed and reaped"
        );
    }
}

fn t_ok_roundtrip() {
    let root = Root::new("ok", "ok");
    let mut h = root.host(base_timeouts());
    assert_eq!(count(&root.0, "spawns"), 0, "lazy: no spawn at new()");
    assert_eq!(h.host_generation(), 0);
    let r = h.load(&page(), 1).expect("load");
    assert_eq!(r.final_url(), "https://example.com/");
    assert_eq!(r.status(), 200);
    assert_eq!(h.host_generation(), 1);
    assert_eq!(h.hello().expect("hello").host_version, "0.0.0");
    match h.call_on_page(&page(), 1, shot()).expect("shot") {
        MethodResult::Screenshot(s) => assert_eq!((s.width(), s.height()), (1280, 800)),
        other => panic!("unexpected {other:?}"),
    }
    let vis = Request::Visibility(VisibilityParams::new("#a").unwrap());
    match h.call_on_page(&page(), 1, vis).expect("vis") {
        MethodResult::Visibility(v) => assert!(v.visible()),
        other => panic!("unexpected {other:?}"),
    }
    let bb = Request::BoundingBox(BoundingBoxParams::new("#a").unwrap());
    match h.call_on_page(&page(), 1, bb).expect("bb") {
        MethodResult::BoundingBox(b) => assert_eq!((b.x(), b.width()), (8.5, 120.25)),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(count(&root.0, "loads"), 1);
    h.shutdown();
    assert!(h.pid().is_none());
    assert_gone(child_pid(&root, 1));
}

fn t_invalid_request() {
    let root = Root::new("invalid", "ok");
    let mut h = root.host(base_timeouts());
    let e = h
        .call_on_page(&page(), 1, Request::Load(page()))
        .expect_err("load rejected");
    assert!(matches!(e, HostProcessError::InvalidRequest));
    let e = h
        .call_on_page(&page(), 1, Request::Shutdown)
        .expect_err("shutdown rejected");
    assert!(matches!(e, HostProcessError::InvalidRequest));
    assert_eq!(count(&root.0, "spawns"), 0);
}

fn t_hello_timeout() {
    let root = Root::new("hello-timeout", "hello-timeout");
    let mut h = root.host(short_timeouts().with_handshake(Duration::from_millis(200)));
    let start = Instant::now();
    let e = h.load(&page(), 1).expect_err("timeout");
    assert!(
        matches!(e, HostProcessError::Timeout(TimeoutPhase::Handshake)),
        "{e:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(h.pid().is_none());
    assert_gone(child_pid(&root, 1));
}

fn t_bad_protocol_range() {
    let root = Root::new("bad-range", "bad-protocol-range");
    let mut h = root.host(base_timeouts());
    let e = h.load(&page(), 1).expect_err("mismatch");
    assert!(matches!(e, HostProcessError::ProtocolMismatch), "{e:?}");
    assert!(h.pid().is_none());
    assert_eq!(h.host_generation(), 0);
}

fn t_crash_then_restart() {
    let root = Root::new("crash", "crash-after-hello\nok");
    let mut h = root.host(base_timeouts());
    let e = h.load(&page(), 1).expect_err("crash");
    assert!(matches!(e, HostProcessError::Crashed), "{e:?}");
    assert_eq!(h.host_generation(), 1);
    assert!(h.pid().is_none());
    h.load(&page(), 1).expect("restarted");
    assert_eq!(h.host_generation(), 2);
    assert_eq!(count(&root.0, "spawns"), 2);
}

fn t_load_timeout() {
    let root = Root::new("hang-load", "hang-load");
    let mut h = root.host(short_timeouts());
    let start = Instant::now();
    let e = h.load(&page(), 1).expect_err("timeout");
    assert!(
        matches!(e, HostProcessError::Timeout(TimeoutPhase::Load)),
        "{e:?}"
    );
    let el = start.elapsed();
    assert!(
        el >= Duration::from_millis(400) && el < Duration::from_secs(5),
        "{el:?}"
    );
    assert_gone(child_pid(&root, 1));
}

fn t_query_timeouts() {
    let root = Root::new("hang-query", "hang-query");
    let mut h = root.host(short_timeouts());
    let e = h.call_on_page(&page(), 1, shot()).expect_err("timeout");
    assert!(
        matches!(e, HostProcessError::Timeout(TimeoutPhase::Screenshot)),
        "{e:?}"
    );
    assert_gone(child_pid(&root, 1));
    let bb = Request::BoundingBox(BoundingBoxParams::new("#a").unwrap());
    let e = h.call_on_page(&page(), 1, bb).expect_err("timeout");
    assert!(
        matches!(e, HostProcessError::Timeout(TimeoutPhase::ElementQuery)),
        "{e:?}"
    );
    assert_gone(child_pid(&root, 2));
}

fn t_protocol_violations() {
    let root = Root::new("garbage", "garbage-frame");
    let mut h = root.host(base_timeouts());
    let e = h.load(&page(), 1).expect_err("garbage");
    assert!(
        matches!(
            e,
            HostProcessError::ProtocolViolation(ProtocolError::InvalidJson)
        ),
        "{e:?}"
    );
    let root = Root::new("oversize", "oversize-frame");
    let mut h = root.host(base_timeouts());
    let e = h.load(&page(), 1).expect_err("oversize");
    assert!(
        matches!(
            e,
            HostProcessError::ProtocolViolation(ProtocolError::FrameTooLarge { .. })
        ),
        "{e:?}"
    );
    let root = Root::new("id-mismatch", "id-mismatch");
    let mut h = root.host(base_timeouts());
    let e = h.load(&page(), 1).expect_err("id mismatch");
    assert!(
        matches!(
            e,
            HostProcessError::ProtocolViolation(ProtocolError::InvalidMessage(_))
        ),
        "{e:?}"
    );
    assert!(h.pid().is_none());
}

fn t_slow_shutdown_is_killed_after_grace() {
    let root = Root::new("slow-shutdown", "slow-shutdown");
    let mut h = root.host(short_timeouts());
    h.load(&page(), 1).expect("load");
    let start = Instant::now();
    h.shutdown();
    let el = start.elapsed();
    assert!(
        el >= Duration::from_millis(300) && el < Duration::from_secs(3),
        "{el:?}"
    );
    assert!(h.pid().is_none());
    assert_gone(child_pid(&root, 1));
}

#[cfg(unix)]
fn t_dump_env_args_cwd() {
    let root = Root::new("dump", "dump");
    let mut h = root.host(base_timeouts());
    h.load(&page(), 1).expect("load");
    let workdir = h.workdir_path().expect("workdir");
    let obs: Value =
        serde_json::from_str(&std::fs::read_to_string(root.0.join("obs-dump.json")).unwrap())
            .unwrap();
    let args: Vec<&str> = obs["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(args, HOST_ARGS);
    let names: BTreeSet<String> = obs["env"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for forced in FORCED_ENV_NAMES {
        assert!(names.contains(*forced), "missing forced {forced}");
    }
    let allowed: BTreeSet<String> = FORCED_ENV_NAMES
        .iter()
        .chain(PASSTHROUGH_ENV_NAMES)
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        names.difference(&allowed).count(),
        0,
        "extra env: {names:?}"
    );
    assert!(!names.contains("PATH"));
    assert_eq!(obs["cwd"].as_str().unwrap(), workdir.to_string_lossy());
    assert_eq!(obs["mode"].as_u64().unwrap(), 0o700);
    let name = workdir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("fandhe-render-"), "{name}");
    assert_eq!(workdir.parent().unwrap(), root.0.as_path());
    h.shutdown();
    assert!(!workdir.exists(), "workdir must be deleted");
}

fn t_reload_on_generation_change() {
    let root = Root::new("reload", "ok");
    let mut h = root.host(base_timeouts());
    h.call_on_page(&page(), 1, shot()).expect("1");
    h.call_on_page(&page(), 1, shot()).expect("2");
    assert_eq!(count(&root.0, "loads"), 1, "same page, same generations");
    h.call_on_page(&page(), 2, shot()).expect("3");
    assert_eq!(count(&root.0, "loads"), 2, "navigation generation changed");
}

fn t_reload_after_crash() {
    let root = Root::new("reload-crash", "crash-on-screenshot\nok");
    let mut h = root.host(base_timeouts());
    let e = h.call_on_page(&page(), 1, shot()).expect_err("crash");
    assert!(matches!(e, HostProcessError::Crashed), "{e:?}");
    assert_eq!(count(&root.0, "loads"), 1);
    h.call_on_page(&page(), 1, shot()).expect("retry");
    assert_eq!(h.host_generation(), 2);
    assert_eq!(count(&root.0, "loads"), 2, "new host must reload the page");
}

fn t_circuit_breaker() {
    let root = Root::new("breaker", "crash-on-start");
    let mut h = root.host(base_timeouts());
    for _ in 0..3 {
        let e = h.load(&page(), 1).expect_err("crash");
        assert!(matches!(e, HostProcessError::Crashed), "{e:?}");
    }
    assert_eq!(count(&root.0, "spawns"), 3);
    let e = h.load(&page(), 1).expect_err("open");
    assert!(matches!(e, HostProcessError::CircuitOpen), "{e:?}");
    assert_eq!(count(&root.0, "spawns"), 3, "no spawn while open");
}

fn t_profile_isolation() {
    let root = Root::new("prof", "ok");
    let mut a = root.host(base_timeouts());
    let mut b = root.host(base_timeouts());
    a.load(&page(), 1).expect("a");
    b.load(&page(), 1).expect("b");
    assert_ne!(a.pid(), b.pid());
    assert_ne!(a.workdir_path(), b.workdir_path());
}

fn t_stderr_tail() {
    let root = Root::new("stderr", "stderr-flood");
    let mut h = root.host(base_timeouts());
    let e = h.load(&page(), 1).expect_err("crash");
    assert!(matches!(e, HostProcessError::Crashed), "{e:?}");
    let d = h.diagnostics().stderr_tail;
    assert!(d.len() <= 8192, "{}", d.len());
    assert!(
        d.ends_with("tail-marker https://a.example/p?<redacted>\n"),
        "{d:?}"
    );
    assert!(!d.contains("secret"));
    assert!(!format!("{e:?}{e}").contains("secret"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn t_rss_limit() {
    let root = Root::new("rss", "rss");
    let cfg = root
        .config(base_timeouts())
        .with_rss_limit_bytes(48 * 1024 * 1024)
        .with_rss_poll_interval(Duration::from_millis(50));
    let mut h = HostProcess::new(cfg);
    let e = h.load(&page(), 1).expect_err("rss");
    assert!(
        matches!(e, HostProcessError::ResourceLimitExceeded),
        "{e:?}"
    );
    assert_gone(child_pid(&root, 1));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == HOST_ARGS {
        fake_host();
    }
    #[allow(unused_mut)]
    let mut tests: Vec<(&str, fn())> = vec![
        ("ok_roundtrip", t_ok_roundtrip),
        ("invalid_request", t_invalid_request),
        ("hello_timeout", t_hello_timeout),
        ("bad_protocol_range", t_bad_protocol_range),
        ("crash_then_restart", t_crash_then_restart),
        ("load_timeout", t_load_timeout),
        ("query_timeouts", t_query_timeouts),
        ("protocol_violations", t_protocol_violations),
        ("slow_shutdown", t_slow_shutdown_is_killed_after_grace),
        ("reload_on_generation_change", t_reload_on_generation_change),
        ("reload_after_crash", t_reload_after_crash),
        ("circuit_breaker", t_circuit_breaker),
        ("profile_isolation", t_profile_isolation),
        ("stderr_tail", t_stderr_tail),
    ];
    #[cfg(unix)]
    tests.push(("dump_env_args_cwd", t_dump_env_args_cwd));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    tests.push(("rss_limit", t_rss_limit));

    let mut failed = 0;
    for (name, f) in tests {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        println!("{failed} test(s) failed");
        std::process::exit(1);
    }
    println!("all host_process tests passed");
}

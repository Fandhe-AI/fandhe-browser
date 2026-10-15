//! ホスト子プロセス管理（`HostProcess`）の結合テスト（RENDER-3・RENDER-4・PLUG-1・PROF・SEC-2。
//! TASK-38・#994）。
//!
//! 偽ホストと共通ヘルパーは `support/mod.rs`（Renderer 層の `basic_api.rs` と共有）。
//! `harness = false` の理由は `Cargo.toml` の `[[test]]` コメント。
//! 待ち時間は短縮値で検証し、実秒（10 s・30 s）は待たない。既定値の assert は単体テスト側。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。

mod support;

#[cfg(unix)]
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

#[cfg(unix)]
use fandhe_browser_render::host_process::HOST_ARGS;
#[cfg(unix)]
use fandhe_browser_render::host_process::{FORCED_ENV_NAMES, PASSTHROUGH_ENV_NAMES};
use fandhe_browser_render::host_process::{
    HostProcess, HostProcessError, HostTimeouts, TimeoutPhase,
};
use fandhe_browser_render::protocol::{
    BoundingBoxParams, LoadParams, MethodResult, ProtocolError, Request, ScreenshotParams,
    VisibilityParams,
};
#[cfg(unix)]
use serde_json::Value;
use support::*;

/// `Root` から `HostProcess` を組み立てる（HostProcess 層専用のため `support` には置かない）。
trait RootHost {
    fn host(&self, timeouts: HostTimeouts) -> HostProcess;
}

impl RootHost for Root {
    fn host(&self, timeouts: HostTimeouts) -> HostProcess {
        HostProcess::new(self.config(timeouts))
    }
}

pub fn page() -> LoadParams {
    LoadParams::new("https://example.com/", None).expect("page")
}

pub fn shot() -> Request {
    Request::Screenshot(ScreenshotParams::new(false))
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
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut allowed: BTreeSet<String> = FORCED_ENV_NAMES
        .iter()
        .chain(PASSTHROUGH_ENV_NAMES)
        .map(|s| s.to_string())
        .collect();
    // macOS は `env_clear` 後でもプロセス起動時に CoreFoundation が
    // `__CF_USER_TEXT_ENCODING` を子の環境へ注入する。親から継承した値ではなく
    // 本 crate の許可リストの漏れでもないため、OS 注入名に限って許容する。
    #[cfg(target_os = "macos")]
    allowed.insert("__CF_USER_TEXT_ENCODING".to_string());
    assert_eq!(
        names.difference(&allowed).count(),
        0,
        "extra env: {names:?}"
    );
    assert!(!names.contains("PATH"));
    // macOS では `/var` が `/private/var` への symlink で、子は正規化済みの cwd を報告する。
    // 両辺を canonicalize してから完全一致で比べる。
    assert_eq!(
        std::fs::canonicalize(obs["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&workdir).unwrap()
    );
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
    maybe_run_fake_host();
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
    run("host_process", tests);
}

//! ホスト子プロセス管理（`HostProcess`）の結合テスト（RENDER-3・RENDER-4・PLUG-1・PROF・SEC-2。
//! TASK-38・#994）。
//!
//! 偽ホストと共通ヘルパーは `support/mod.rs`（Renderer 層の `basic_api.rs` と共有）。
//! `harness = false` の理由は `Cargo.toml` の `[[test]]` コメント。
//! 待ち時間は短縮値で検証し、実秒（10 s・30 s）は待たない。既定値の assert は単体テスト側。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。
//!
//! Linux の隔離系テストは入口で `egress isolation probe: ...` を 1 行出し（CI ログで bwrap の
//! 分岐を読める）、`GITHUB_ACTIONS=true` で `probe_system()` が Err なら失敗にする
//! （RENDER-3・#1451。bwrap が無くて Err 分岐だけ通り緑になるのを防ぐ）。

mod support;

#[cfg(unix)]
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use fandhe_browser_core::EgressIsolationMode;
#[cfg(unix)]
use fandhe_browser_render::host_process::EgressIsolationStatus;
#[cfg(unix)]
use fandhe_browser_render::host_process::HOST_ARGS;
#[cfg(unix)]
use fandhe_browser_render::host_process::HostProcessConfig;
use fandhe_browser_render::host_process::{
    EgressIsolationUnavailable, HostProcess, HostProcessError, HostTimeouts, IsolationProbe,
    TimeoutPhase,
};
#[cfg(unix)]
use fandhe_browser_render::host_process::{FORCED_ENV_NAMES, PASSTHROUGH_ENV_NAMES};
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
    assert_eq!(r.status(), Some(200));
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
    // 遅い CI（Windows 等）でも偽ホストが pid を書き終えてから kill されるよう、
    // ハンドシェイク待ちは起動時間より十分長い 2 秒にする。
    let mut h = root.host(short_timeouts().with_handshake(Duration::from_secs(2)));
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

// ---------------------------------------------------------------------------
// egress 隔離（RENDER-3・#1436）
// ---------------------------------------------------------------------------

/// 隔離できない環境で `required` なら起動せず、理由つきのエラーを返す。ブレーカーには数えない。
fn t_required_unavailable_does_not_spawn() {
    let root = Root::new("iso-required", "ok");
    let cfg = root
        .config(base_timeouts())
        .with_egress_isolation(EgressIsolationMode::Required)
        .with_isolation_probe(IsolationProbe::Unavailable(
            EgressIsolationUnavailable::SandboxToolNotFound,
        ));
    let mut h = HostProcess::new(cfg);
    for _ in 0..5 {
        let e = h.load(&page(), 1).expect_err("required must refuse");
        assert!(
            matches!(
                e,
                HostProcessError::EgressIsolationUnavailable(
                    EgressIsolationUnavailable::SandboxToolNotFound
                )
            ),
            "{e:?}"
        );
    }
    assert_eq!(count(&root.0, "spawns"), 0, "host must never be started");
    assert_eq!(
        HostProcessError::EgressIsolationUnavailable(
            EgressIsolationUnavailable::SandboxToolNotFound
        )
        .to_string(),
        "render host egress isolation is required but unavailable"
    );
    assert!(h.pid().is_none());
}

/// `$HOME`・WorkDir の親を隠せない理由（#1485・#1492）も `required` では起動せず、理由つきで拒否する。
fn t_required_hide_failures_do_not_spawn() {
    for reason in [
        EgressIsolationUnavailable::HomeNotHideable,
        EgressIsolationUnavailable::WorkDirParentNotHideable,
    ] {
        let root = Root::new("iso-required-hide", "ok");
        let cfg = root
            .config(base_timeouts())
            .with_egress_isolation(EgressIsolationMode::Required)
            .with_isolation_probe(IsolationProbe::Unavailable(reason));
        let mut h = HostProcess::new(cfg);
        for _ in 0..5 {
            let e = h.load(&page(), 1).expect_err("required must refuse");
            assert!(
                matches!(&e, HostProcessError::EgressIsolationUnavailable(r) if *r == reason),
                "{e:?}"
            );
        }
        assert_eq!(count(&root.0, "spawns"), 0, "host must never be started");
        assert!(h.pid().is_none());
    }
}

// 使うテストが unix 限定のため、Windows では dead_code にならないよう cfg を揃える。
#[cfg(unix)]
type Warnings = std::sync::Arc<std::sync::Mutex<Vec<EgressIsolationUnavailable>>>;

#[cfg(unix)]
fn with_warning_sink(cfg: HostProcessConfig) -> (HostProcessConfig, Warnings) {
    let sink: Warnings = Warnings::default();
    let s2 = std::sync::Arc::clone(&sink);
    let cfg = cfg.with_isolation_warning(std::sync::Arc::new(move |r| {
        s2.lock().unwrap().push(r);
    }));
    (cfg, sink)
}

/// 隔離できない環境で `best-effort` なら、警告を 1 回だけ出して従来どおり起動する。
#[cfg(unix)]
fn t_best_effort_warns_once_and_starts_unwrapped() {
    let root = Root::new("iso-besteffort", "dump");
    let reason = EgressIsolationUnavailable::UnsupportedPlatform;
    let (cfg, sink) = with_warning_sink(
        root.config(base_timeouts())
            .with_egress_isolation(EgressIsolationMode::BestEffort)
            .with_isolation_probe(IsolationProbe::Unavailable(reason)),
    );
    let mut h = HostProcess::new(cfg);
    h.load(&page(), 1).expect("starts without isolation");
    assert_eq!(
        h.egress_isolation(),
        Some(EgressIsolationStatus::Unisolated(reason))
    );
    let obs: Value =
        serde_json::from_str(&std::fs::read_to_string(root.0.join("obs-dump.json")).unwrap())
            .unwrap();
    assert_eq!(obs["args"], serde_json::json!(["--stdio"]));
    assert_eq!(*sink.lock().unwrap(), vec![reason]);
    // 再起動しても警告は HostProcess ごとに 1 回。
    h.shutdown();
    h.load(&page(), 1).expect("restart");
    assert_eq!(count(&root.0, "spawns"), 2);
    assert_eq!(*sink.lock().unwrap(), vec![reason]);
}

#[cfg(target_os = "linux")]
fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} is still alive");
}

/// CI（GitHub Actions）では隔離が使えることを必須にするか（RENDER-3・#1451）。
/// GitHub が設定する値 `true` と完全一致する場合だけ true（大文字小文字は区別する）。
fn ci_requires_sandbox(github_actions: Option<&str>) -> bool {
    github_actions == Some("true")
}

fn t_ci_guard_requires_sandbox_only_on_github_actions() {
    assert!(ci_requires_sandbox(Some("true")));
    assert!(!ci_requires_sandbox(None));
    for v in ["false", "TRUE", "1", ""] {
        assert!(!ci_requires_sandbox(Some(v)), "{v:?}");
    }
}

/// 隔離系テストの入口。`probe_system()` の結果を 1 行出し、CI 上で Err なら理由つきで panic する。
/// ローカルで bwrap が無い場合は Err を返し、呼び出し側が Err の分岐を検証する。
#[cfg(target_os = "linux")]
fn system_probe_for(test: &str) -> Result<(), EgressIsolationUnavailable> {
    let r = fandhe_browser_render::host_process::probe_system();
    match &r {
        Ok(()) => eprintln!("{test}: egress isolation probe: bwrap available"),
        Err(reason) => {
            eprintln!("{test}: egress isolation probe: unavailable ({reason})");
            if ci_requires_sandbox(std::env::var("GITHUB_ACTIONS").ok().as_deref()) {
                panic!(
                    "GITHUB_ACTIONS=true requires bwrap egress isolation on Linux, but probe failed: {reason}"
                );
            }
        }
    }
    r
}

/// 実環境（Linux）の判定に従う。bwrap が使える環境では argv・RSS の pid・終了処理を確かめ、
/// 使えない環境では `required` が起動しないことを確かめる（どちらの分岐も具体値で assert）。
#[cfg(target_os = "linux")]
fn t_system_probe_branch() {
    use fandhe_browser_render::host_process::IsolationBackend;
    match system_probe_for("egress_system_probe_branch") {
        Ok(()) => {
            let root = Root::new_visible("ok", "dump");
            // FS の可視性の観測用（RENDER-3・#1485・#1492）。`/tmp`・`/var/tmp`・`/dev/shm` の番兵
            // （ファイルと UDS。兄弟 WorkDir 相当）と `$HOME` の既存ファイル（このクレートの
            // Cargo.toml）が内側から見えないこと。偽ホストは名前から同じパスを組み立てて確かめる。
            let pid = std::process::id();
            let mut sentinels: Vec<std::path::PathBuf> = Vec::new();
            let mut listeners = Vec::new();
            for dir in ["/tmp", "/var/tmp", "/dev/shm"] {
                let file = std::path::PathBuf::from(dir).join(format!("fb-hp-sentinel-{pid}"));
                let sock = std::path::PathBuf::from(dir).join(format!("fb-hp-sentinel-{pid}.sock"));
                let _ = std::fs::remove_file(&sock);
                let made = std::fs::write(&file, b"x").is_ok()
                    && std::os::unix::net::UnixListener::bind(&sock)
                        .map(|l| listeners.push(l))
                        .is_ok();
                if made {
                    sentinels.push(file);
                    sentinels.push(sock);
                } else {
                    // CI では 3 か所とも作れることを必須にする（空振りさせない）。
                    assert!(
                        !ci_requires_sandbox(std::env::var("GITHUB_ACTIONS").ok().as_deref()),
                        "CI must be able to create sentinels under {dir}"
                    );
                    let _ = std::fs::remove_file(&file);
                }
            }
            let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let manifest_under_home = home
                .as_deref()
                .and_then(|h| std::fs::canonicalize(h).ok())
                .zip(std::fs::canonicalize(manifest).ok())
                .is_some_and(|(h, m)| m.starts_with(h));
            if ci_requires_sandbox(std::env::var("GITHUB_ACTIONS").ok().as_deref()) {
                assert!(manifest_under_home, "CI checks out the repo under $HOME");
            }
            let cfg = root
                .config_system(base_timeouts())
                .with_egress_isolation(EgressIsolationMode::Required)
                .with_rss_poll_interval(Duration::from_millis(50));
            let mut h = HostProcess::new(cfg);
            h.load(&page(), 1).expect("wrapped host starts");
            assert_eq!(
                h.egress_isolation(),
                Some(EgressIsolationStatus::Isolated(IsolationBackend::Bwrap))
            );
            let workdir = std::fs::canonicalize(h.workdir_path().unwrap()).unwrap();
            let obs: Value = serde_json::from_str(
                &std::fs::read_to_string(workdir.join("obs-dump.json")).unwrap(),
            )
            .unwrap();
            let args: Vec<&str> = obs["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let sock = workdir.join("egress-proxy.sock");
            assert_eq!(
                args,
                ["--stdio", "--egress-relay-uds", sock.to_str().unwrap()]
            );
            // pid 名前空間（RENDER-3）: ホストから本体と起動した子（外側の pid）が見えない。
            // ns 内ではホストの親は init 役の bwrap（pid 1）で、自身の pid（pid-1 ファイル）は 2。
            let wrapper_pid = h.pid().expect("wrapper pid");
            let seen: Vec<u64> = obs["proc_pids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap())
                .collect();
            assert!(seen.contains(&1), "ns init visible: {seen:?}");
            assert!(
                !seen.contains(&u64::from(std::process::id())),
                "parent pid must be hidden: {seen:?}"
            );
            assert!(
                !seen.contains(&u64::from(wrapper_pid)),
                "wrapper pid must be hidden: {seen:?}"
            );
            assert_eq!(obs["ppid"], 1);
            assert_eq!(support::child_pid_in(&workdir, 1), 2);
            // tmpfs の容量上限（RENDER-3・#1498）: bwrap が `--size` に対応していれば、内側の `/tmp` は
            // 64 MiB（mountinfo では `size=65536k`）の tmpfs。非対応の版では上限なし（`size=65536k` を
            // 含まない）。対応の有無は同じ bwrap の `--help` で判定する（本番の機能検出と同じ根拠）。
            let help = std::process::Command::new("/usr/bin/bwrap")
                .arg("--help")
                .env_clear()
                .output();
            if let Ok(help) = help {
                let supports_size = help
                    .stdout
                    .split(|b| b.is_ascii_whitespace())
                    .any(|t| t == b"--size");
                let tmp_mount = &obs["mounts"]["/tmp"];
                if tmp_mount.is_object() {
                    assert_eq!(tmp_mount["fstype"], "tmpfs", "{tmp_mount}");
                    let opts = tmp_mount["super_opts"].as_str().unwrap();
                    assert_eq!(
                        opts.split(',').any(|o| o == "size=65536k"),
                        supports_size,
                        "{tmp_mount}"
                    );
                } else {
                    // /tmp が別の場所への symlink などでマウント点として現れない環境。CI では必須にする。
                    assert!(
                        !ci_requires_sandbox(std::env::var("GITHUB_ACTIONS").ok().as_deref()),
                        "CI must show /tmp as a mount point: {}",
                        obs["mounts"]
                    );
                }
            }
            // FS の隠蔽（RENDER-3・#1485）: `/tmp` の番兵（ファイルと UDS）は見えない。`$HOME` 配下の
            // 既存ファイルも見えない。偽ホストが起動して応答できたこと自体が、`$HOME` 配下の実行
            // ファイルが `--ro-bind` で戻った証拠になる。
            let visible = &obs["visible"];
            for sentinel in &sentinels {
                assert_eq!(visible[sentinel.to_str().unwrap()], false, "{visible}");
            }
            if manifest_under_home {
                assert_eq!(visible[manifest], false, "{visible}");
            }
            for sentinel in &sentinels {
                let _ = std::fs::remove_file(sentinel);
            }
            drop(listeners);
            // RSS 監視は bwrap ではなく 2 段下（外側の pid でのホスト）を辿る。
            let deadline = Instant::now() + Duration::from_secs(5);
            while h.monitored_pid().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let host_pid = h.monitored_pid().expect("monitored host pid");
            assert_ne!(host_pid, wrapper_pid, "wrapper pid differs from host pid");
            let tree = outer_tree(wrapper_pid);
            assert_eq!(tree.len(), 2, "wrapper -> init -> host: {tree:?}");
            assert_eq!(tree[1], host_pid);
            assert_eq!(nspid(host_pid), vec![host_pid, 2], "NSpid of the host");
            // 終了処理: ラッパーを kill すると、init 役もホストも残らない。
            h.shutdown();
            assert!(!workdir.exists());
            wait_gone(host_pid);
            wait_gone(tree[0]);
            wait_gone(wrapper_pid);
        }
        Err(reason) => {
            let root = Root::new("iso-system-ng", "ok");
            let cfg = root
                .config_system(base_timeouts())
                .with_egress_isolation(EgressIsolationMode::Required);
            let mut h = HostProcess::new(cfg);
            let e = h.load(&page(), 1).expect_err("required must refuse");
            assert!(
                matches!(&e, HostProcessError::EgressIsolationUnavailable(r) if *r == reason),
                "{e:?}"
            );
            assert_eq!(count(&root.0, "spawns"), 0);
        }
    }
}

/// 外側の pid 名前空間で、`wrapper` の子孫を 1 本道で辿った pid 列（子が 1 件の間だけ）。
#[cfg(target_os = "linux")]
fn outer_tree(wrapper: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = wrapper;
    while let Ok(t) = std::fs::read_to_string(format!("/proc/{cur}/task/{cur}/children")) {
        let kids: Vec<u32> = t.split_whitespace().map(|p| p.parse().unwrap()).collect();
        match kids.as_slice() {
            [only] => {
                out.push(*only);
                cur = *only;
            }
            _ => break,
        }
    }
    out
}

/// `/proc/<pid>/status` の `NSpid`（外側から内側へ並ぶ pid）。
#[cfg(target_os = "linux")]
fn nspid(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("NSpid:"))
        .unwrap()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

/// `root` 配下を cmdline に含むプロセス（bwrap・init 役・ホスト）の外側 pid を、`stop` が立つまで
/// 集め続ける。ns 内の pid（pid-N ファイル）は外側の `/proc` では引けないため、失敗する load の
/// 後始末（ホストの消滅）を外側の pid で確かめるために使う。
#[cfg(target_os = "linux")]
fn spawn_pid_collector(
    root: &std::path::Path,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<BTreeSet<u32>> {
    let needle = root.to_string_lossy().into_owned();
    std::thread::spawn(move || {
        let mut found = BTreeSet::new();
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            if let Ok(d) = std::fs::read_dir("/proc") {
                for e in d.flatten() {
                    let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    if pid == std::process::id() {
                        continue;
                    }
                    if let Ok(c) = std::fs::read(e.path().join("cmdline"))
                        && String::from_utf8_lossy(&c).contains(&needle)
                    {
                        found.insert(pid);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        found
    })
}

/// bwrap 越しでも、タイムアウトで kill した後にホストが残らない。
#[cfg(target_os = "linux")]
fn t_system_probe_timeout_kills_wrapped_host() {
    if system_probe_for("egress_system_timeout_kills_wrapped_host").is_err() {
        return; // 隔離できない環境では上のテストが required の拒否を確かめる。
    }
    let root = Root::new_visible("to", "hang-load");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = spawn_pid_collector(&root.0, stop.clone());
    let mut h = HostProcess::new(root.config_system(short_timeouts()));
    let e = h.load(&page(), 1).expect_err("timeout");
    assert!(
        matches!(e, HostProcessError::Timeout(TimeoutPhase::Load)),
        "{e:?}"
    );
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let pids = collector.join().unwrap();
    assert!(pids.len() >= 3, "bwrap, init and host seen: {pids:?}");
    pids.into_iter().for_each(wait_gone);
}

/// bwrap 越しでも RSS 上限超過でホスト本体（bwrap の子）が落ちる。bwrap 自身の RSS では判定しない。
#[cfg(target_os = "linux")]
fn t_system_probe_rss_limit_kills_wrapped_host() {
    if system_probe_for("egress_system_rss_limit_kills_wrapped_host").is_err() {
        return;
    }
    let root = Root::new_visible("rss", "rss");
    let cfg = root
        .config_system(base_timeouts())
        .with_rss_limit_bytes(48 * 1024 * 1024)
        .with_rss_poll_interval(Duration::from_millis(50));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = spawn_pid_collector(&root.0, stop.clone());
    let mut h = HostProcess::new(cfg);
    let e = h.load(&page(), 1).expect_err("rss");
    assert!(
        matches!(e, HostProcessError::ResourceLimitExceeded),
        "{e:?}"
    );
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let pids = collector.join().unwrap();
    assert!(pids.len() >= 3, "bwrap, init and host seen: {pids:?}");
    pids.into_iter().for_each(wait_gone);
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
    tests.push((
        "egress_ci_guard_logic",
        t_ci_guard_requires_sandbox_only_on_github_actions,
    ));
    tests.push((
        "egress_required_unavailable",
        t_required_unavailable_does_not_spawn,
    ));
    tests.push((
        "egress_required_hide_failures",
        t_required_hide_failures_do_not_spawn,
    ));
    #[cfg(unix)]
    tests.push((
        "egress_best_effort_warns_once",
        t_best_effort_warns_once_and_starts_unwrapped,
    ));
    #[cfg(target_os = "linux")]
    {
        tests.push(("egress_system_probe_branch", t_system_probe_branch));
        tests.push((
            "egress_system_rss_limit_kills_wrapped_host",
            t_system_probe_rss_limit_kills_wrapped_host,
        ));
        tests.push((
            "egress_system_timeout_kills_wrapped_host",
            t_system_probe_timeout_kills_wrapped_host,
        ));
    }
    #[cfg(unix)]
    tests.push(("dump_env_args_cwd", t_dump_env_args_cwd));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    tests.push(("rss_limit", t_rss_limit));
    run("host_process", tests);
}

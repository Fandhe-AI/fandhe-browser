//! 偽ホスト結合テストの共有基盤（`host_process.rs` = HostProcess 層、`basic_api.rs` =
//! Renderer 層。RENDER-3・TASK-38・#994・#237）。
//!
//! テストバイナリ自身を `--stdio` 付きで再実行して偽ホストにし、`temp_root/scenario` の行
//! （起動回数ごと）でふるまいを切り替える。両テストバイナリから `mod support;` で取り込む。
//! 外部入力の経路ではないため `unwrap` / `expect` / `panic!` を使う。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fandhe_browser_render::codec::{
    MAX_REQUEST_FRAME_BYTES, MAX_SCREENSHOT_RESPONSE_FRAME_BYTES, parse_json, read_frame,
    write_frame,
};
use fandhe_browser_render::host_process::{
    HOST_ARGS, HostProcessConfig, HostTimeouts, resolve_host_binary,
};
use serde_json::{Value, json};

const HELLO_OK: &str = include_str!("../fixtures/protocol/hello-ok.json");

// ---------------------------------------------------------------------------
// 偽ホスト側
// ---------------------------------------------------------------------------

pub fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open");
    f.write_all(text.as_bytes()).expect("append");
}

pub fn count(root: &Path, name: &str) -> usize {
    std::fs::read_to_string(root.join(name)).map_or(0, |s| s.len())
}

pub fn send(out: &mut impl Write, id: u64, result: Value) {
    let msg = json!({"v": 1, "id": id, "result": result});
    write_frame(out, &serde_json::to_vec(&msg).unwrap(), 1 << 20).expect("write");
}

pub fn send_error(out: &mut impl Write, id: u64, kind: &str) {
    let msg = json!({"v": 1, "id": id, "error": {"kind": kind, "message": "https://secret.example/?t=1"}});
    write_frame(out, &serde_json::to_vec(&msg).unwrap(), 1 << 20).expect("write");
}

pub fn fake_host() -> ! {
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
        // 受けた要求の内容を記録する（Renderer 層の試験が URL・selector・full_page を検証する）。
        let params = &v["params"];
        let logged = match method.as_str() {
            "load" => format!("load {}", params["url"].as_str().unwrap_or("")),
            "screenshot" => format!("screenshot full_page={}", params["full_page"]),
            "visibility" | "bounding_box" => {
                format!("{method} {}", params["selector"].as_str().unwrap_or(""))
            }
            other => other.to_string(),
        };
        append(&root.join("requests"), &format!("{logged}\n"));
        match (scenario, method.as_str()) {
            ("crash-after-hello", _) => std::process::exit(1),
            ("crash-on-screenshot", "screenshot") => std::process::exit(1),
            ("hang-load", "load")
            | ("hang-query", "screenshot" | "visibility" | "bounding_box") => {
                std::thread::sleep(Duration::from_secs(60));
            }
            ("slow-shutdown", "shutdown") => std::thread::sleep(Duration::from_secs(60)),
            ("not-found", "visibility" | "bounding_box") => {
                send_error(&mut stdout, id, "not_found");
                continue;
            }
            ("bad-base64", "screenshot") => {
                send(
                    &mut stdout,
                    id,
                    json!({"format": "png", "width": 4, "height": 4, "data_base64": "!!!!"}),
                );
                continue;
            }
            ("not-png", "screenshot") => {
                send(
                    &mut stdout,
                    id,
                    json!({"format": "png", "width": 4, "height": 4, "data_base64": "aGVsbG8gd29ybGQ="}),
                );
                continue;
            }
            ("redirect-private", "load") => {
                append(&root.join("loads"), "x");
                send(
                    &mut stdout,
                    id,
                    json!({"final_url": "http://127.0.0.1/", "status": 200}),
                );
                continue;
            }
            ("crash-on-bbox", "bounding_box") => std::process::exit(1),
            ("garbage-on-screenshot", "screenshot") => {
                write_frame(&mut stdout, b"not json", 1 << 20).expect("garbage");
                continue;
            }
            ("host-timeout", "load") => {
                send_error(&mut stdout, id, "timeout");
                continue;
            }
            ("wrong-result", "screenshot") => {
                send(&mut stdout, id, json!({"visible": true}));
                continue;
            }
            // 長さヘッダだけを送る（本文は送らない）。クライアントがメソッド別上限で確保前に
            // 弾くことの確認（1 MiB 超 24 MiB 未満の visibility / 24 MiB 超の screenshot）。
            ("oversize-visibility", "visibility") => {
                stdout
                    .write_all(&(2u32 * 1024 * 1024).to_le_bytes())
                    .expect("hdr");
                stdout.flush().expect("flush");
                std::thread::sleep(Duration::from_secs(60));
            }
            ("oversize-screenshot", "screenshot") => {
                let len = u32::try_from(MAX_SCREENSHOT_RESPONSE_FRAME_BYTES + 1).expect("len");
                stdout.write_all(&len.to_le_bytes()).expect("hdr");
                stdout.flush().expect("flush");
                std::thread::sleep(Duration::from_secs(60));
            }
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
            "screenshot" => {
                let height = if params["full_page"] == json!(true) {
                    2400
                } else {
                    800
                };
                send(
                    &mut stdout,
                    id,
                    json!({"format": "png", "width": 1280, "height": height, "data_base64": "iVBORw0KGgo="}),
                );
            }
            "visibility" => {
                let visible = params["selector"].as_str() != Some("#hidden");
                send(&mut stdout, id, json!({"visible": visible}));
            }
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

pub struct Root(pub PathBuf);

impl Root {
    pub fn new(name: &str, scenario: &str) -> Self {
        let p = std::env::temp_dir().join(format!("fandhe-hp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        std::fs::write(p.join("scenario"), scenario).expect("scenario");
        Self(p)
    }
    pub fn config(&self, timeouts: HostTimeouts) -> HostProcessConfig {
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

pub fn base_timeouts() -> HostTimeouts {
    HostTimeouts::default().with_shutdown_grace(Duration::from_secs(1))
}

pub fn short_timeouts() -> HostTimeouts {
    HostTimeouts::default()
        .with_handshake(Duration::from_secs(5))
        .with_load_default(Duration::from_millis(300))
        .with_load_grace(Duration::from_millis(100))
        .with_screenshot(Duration::from_millis(300))
        .with_element_query(Duration::from_millis(300))
        .with_shutdown_grace(Duration::from_millis(300))
}

pub fn child_pid(root: &Root, n: usize) -> u32 {
    std::fs::read_to_string(root.0.join(format!("pid-{n}")))
        .expect("pid file")
        .trim()
        .parse()
        .expect("pid")
}

pub fn assert_gone(pid: u32) {
    if cfg!(target_os = "linux") {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "child {pid} must have been killed and reaped"
        );
    }
}
/// 偽ホストとして再実行された場合は `fake_host` で終了しない。通常実行なら何もしない。
pub fn maybe_run_fake_host() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == HOST_ARGS {
        fake_host();
    }
}

/// 自前テストランナー（`harness = false`）。1 件でも失敗すれば exit 1。
pub fn run(label: &str, tests: Vec<(&str, fn())>) {
    use std::panic::{AssertUnwindSafe, catch_unwind};
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
    println!("all {label} tests passed");
}

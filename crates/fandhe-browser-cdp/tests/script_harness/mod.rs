//! 外部スクリプト（Node 製クライアント等）を実サーバーへ接続させて結果を構造化回収する
//! 共有基盤（TASK-45.1・#480、ビヘイビア `CDP-3`・MS-4）。
//!
//! `tests/puppeteer_connect.rs`（基盤の自己テスト）と `tests/puppeteer_connect_live.rs`
//! （実 Puppeteer）の両ターゲットが `mod script_harness;` で取り込む。Puppeteer 固有の事柄は
//! 呼び出し側へ寄せ、このモジュールは「サーバー起動・子プロセス実行・結果行の解析」だけを
//! 担う（Playwright 側の基盤 #476 からも再利用できる形に保つ）。
//!
//! # スクリプトとの契約
//! - WS エンドポイントは環境変数 [`ENDPOINT_ENV`] で渡す（シェル文字列は経由しない）。
//! - スクリプトは stdout に [`RESULT_PREFIX`] で始まる 1 行の JSON
//!   `{"ok": bool, "step": string, "error": {"name": string, "message": string} | null}` を出す。
//!
//! # 安全性
//! 子プロセスは締め切りで kill し、stdout/stderr・結果行には読み取り上限を設ける
//! （無制限確保による DoS の防止）。サーバーは `127.0.0.1:0` にのみ bind する。

// 2 つのテストターゲットが別々の部分集合を使うため、未使用項目の警告を許容する。
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fandhe_backend_core::server::Server;
use fandhe_browser_cdp::{CdpState, endpoints};
use fandhe_browser_core::AppState;
use fandhe_browser_profile::Profile;
use serde_json::Value;

/// スクリプトへ WS エンドポイントを渡す環境変数名。
pub const ENDPOINT_ENV: &str = "FANDHE_CDP_WS_ENDPOINT";
/// 結果行の固定接頭辞（後ろに JSON が続く）。
pub const RESULT_PREFIX: &str = "FANDHE_SCRIPT_RESULT ";
/// stdout / stderr それぞれの保持上限（超過分は読み捨てる）。
pub const MAX_STREAM_BYTES: usize = 1024 * 1024;
/// 結果行 1 行の長さ上限。
pub const MAX_RESULT_LINE_BYTES: usize = 64 * 1024;
/// 既定の待機締め切り。
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);
/// `stderr_tail` に保持する末尾バイト数。
const STDERR_TAIL_BYTES: usize = 2048;

/// 未導入時に表示する導入手順（固定の英語文言。AC3）。
pub const INSTALL_HINT: &str =
    "run `npm ci --ignore-scripts` in harness/puppeteer-connect (requires Node.js 22.12 or later)";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除）。プロファイル保管場所をここへ閉じ込める。
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        Self(base.join(format!("fandhe-cdp-script-{}-{n}", std::process::id())))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 起動済みテストサーバー。`_dir` はプロファイル用で、本体の drop まで保持する。
pub struct TestServer {
    pub addr: SocketAddr,
    pub state: Arc<CdpState>,
    _dir: TempDir,
}

/// 実サーバーを専用スレッドで起動する。
///
/// 子プロセスの待機はブロッキングなので、テスト側ランタイムとは別の current_thread
/// ランタイムで動かす（workspace の tokio は `rt-multi-thread` を持たず、feature 追加は
/// 依存変更になるため行わない）。スレッドはテストプロセス終了まで残る。
pub fn start_server() -> TestServer {
    let dir = TempDir::new();
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    let state = Arc::new(CdpState::new(app));
    let st = Arc::clone(&state);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let (router, ws_config) = endpoints(&st).expect("endpoints").into_parts();
            let bound = Server::new()
                .handler(router)
                .websocket(ws_config)
                .bind("127.0.0.1:0")
                .await
                .expect("bind");
            tx.send(bound.local_addr().expect("local_addr")).ok();
            bound.run().await
        })
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server did not start");
    TestServer {
        addr,
        state,
        _dir: dir,
    }
}

/// `GET /json/version` から `webSocketDebuggerUrl` を取得する（`puppeteer.connect` の入力相当）。
pub fn browser_ws_endpoint(addr: SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        addr.port()
    );
    s.write_all(req.as_bytes()).expect("write");
    let mut raw = Vec::new();
    s.take(MAX_STREAM_BYTES as u64)
        .read_to_end(&mut raw)
        .expect("read");
    let text = String::from_utf8_lossy(&raw);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let json: Value = serde_json::from_str(body.trim()).expect("version json");
    let url = json
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .expect("webSocketDebuggerUrl")
        .to_string();
    let prefix = format!("ws://127.0.0.1:{}/devtools/browser/", addr.port());
    assert!(url.starts_with(&prefix), "unexpected endpoint: {url}");
    url
}

/// スクリプトが報告したエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    pub name: String,
    pub message: String,
}

/// スクリプト実行結果の回収形（REPAIR-4。真偽値で済ませない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// 結果行を回収できた（`ok == false` も「記録された結果」であり基盤の失敗ではない）。
    Completed {
        ok: bool,
        step: String,
        error: Option<ScriptError>,
    },
    /// 結果行なしで終了した。
    NoResult {
        exit_code: Option<i32>,
        stderr_tail: String,
    },
    /// 締め切りを超えて kill した。
    TimedOut { stderr_tail: String },
    /// 結果行が不正（JSON 不正・欠落フィールド・長さ超過）。
    Malformed { reason: String },
}

/// 実行するスクリプトコマンド。
#[derive(Debug, Clone)]
pub struct ScriptCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
}

/// 基盤の前提を満たせない場合のエラー（成功を装わず理由と導入手順を返す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    ToolUnavailable {
        tool: String,
        reason: String,
        hint: String,
    },
}

/// stdout/stderr を別スレッドで上限付きに読む。上限超過分は読み捨てる（子のパイプ詰まり防止）。
fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_STREAM_BYTES.saturating_sub(kept.len());
                    if let Some(chunk) = buf.get(..n.min(room)) {
                        kept.extend_from_slice(chunk);
                    }
                }
            }
        }
        kept
    })
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL_BYTES);
    String::from_utf8_lossy(bytes.get(start..).unwrap_or_default()).into_owned()
}

/// stdout から結果行（最後の 1 行）を取り出して解析する。結果行が無ければ `Ok(None)`。
pub fn parse_result_line(stdout: &[u8]) -> Result<Option<ScriptOutcome>, String> {
    let text = String::from_utf8_lossy(stdout);
    let Some(line) = text.lines().rev().find(|l| l.starts_with(RESULT_PREFIX)) else {
        return Ok(None);
    };
    if line.len() > MAX_RESULT_LINE_BYTES {
        return Err(format!("result line exceeds {MAX_RESULT_LINE_BYTES} bytes"));
    }
    let json: Value = serde_json::from_str(line.get(RESULT_PREFIX.len()..).unwrap_or_default())
        .map_err(|e| format!("result line is not valid JSON: {e}"))?;
    let ok = json
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or("missing boolean field `ok`")?;
    let step = json
        .get("step")
        .and_then(Value::as_str)
        .ok_or("missing string field `step`")?
        .to_string();
    let error = match json.get("error") {
        None | Some(Value::Null) => None,
        Some(e) => Some(ScriptError {
            name: e
                .get("name")
                .and_then(Value::as_str)
                .ok_or("missing string field `error.name`")?
                .to_string(),
            message: e
                .get("message")
                .and_then(Value::as_str)
                .ok_or("missing string field `error.message`")?
                .to_string(),
        }),
    };
    Ok(Some(ScriptOutcome::Completed { ok, step, error }))
}

/// スクリプトを実行し、締め切り内に結果を回収する。
pub fn run_script(cmd: &ScriptCommand, ws_endpoint: &str, deadline: Duration) -> ScriptOutcome {
    let mut c = Command::new(&cmd.program);
    c.args(&cmd.args)
        .env(ENDPOINT_ENV, ws_endpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &cmd.envs {
        c.env(k, v);
    }
    if let Some(d) = &cmd.cwd {
        c.current_dir(d);
    }
    let mut child = c.spawn().expect("spawn script");
    let out = spawn_reader(child.stdout.take().expect("stdout"));
    let err = spawn_reader(child.stderr.take().expect("stderr"));

    let start = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break (Some(s), false),
            None if start.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    if timed_out {
        return ScriptOutcome::TimedOut {
            stderr_tail: tail(&stderr),
        };
    }
    match parse_result_line(&stdout) {
        Ok(Some(o)) => o,
        Ok(None) => ScriptOutcome::NoResult {
            exit_code: status.and_then(|s| s.code()),
            stderr_tail: tail(&stderr),
        },
        Err(reason) => ScriptOutcome::Malformed { reason },
    }
}

/// Puppeteer 実行の事前確認。未導入なら理由付きの [`HarnessError::ToolUnavailable`]。
///
/// `puppeteer-core` の導入確認を先に行う（判定が環境の `node` 有無に左右されないため）。
/// インストール済み版が期待版と一致するかの照合は未実装（`package.json` の完全固定版が
/// 承認・確定した後に追加する。REPAIR-3）。
pub fn preflight_puppeteer(harness_dir: &Path) -> Result<ScriptCommand, HarnessError> {
    let unavailable = |reason: &str| HarnessError::ToolUnavailable {
        tool: "puppeteer-core".to_string(),
        reason: reason.to_string(),
        hint: INSTALL_HINT.to_string(),
    };
    let pkg = harness_dir
        .join("node_modules")
        .join("puppeteer-core")
        .join("package.json");
    if !pkg.is_file() {
        return Err(unavailable("puppeteer-core is not installed"));
    }
    let node_ok = Command::new("node")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return Err(unavailable("node executable was not found"));
    }
    Ok(ScriptCommand {
        program: PathBuf::from("node"),
        args: vec![
            harness_dir
                .join("connect.mjs")
                .to_string_lossy()
                .into_owned(),
        ],
        envs: Vec::new(),
        cwd: Some(harness_dir.to_path_buf()),
    })
}

/// `harness/puppeteer-connect` のパス（`Path::join` で組み立てる）。
pub fn puppeteer_harness_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("harness")
        .join("puppeteer-connect")
}

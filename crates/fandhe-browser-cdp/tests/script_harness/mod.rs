//! 外部スクリプト（Node 製クライアント等）を実サーバーへ接続させて結果を構造化回収する
//! 共有基盤（TASK-45.1・#480、ビヘイビア `CDP-3`・MS-4）。
//!
//! `tests/puppeteer_connect.rs`（基盤の自己テスト）が `mod script_harness;` で取り込む
//! （実 Puppeteer の試験ターゲットは導入承認後に追加し、同様に取り込む）。Puppeteer 固有の事柄は
//! 呼び出し側へ寄せ、このモジュールは「サーバー起動・子プロセス実行・結果行の解析」だけを
//! 担う（Playwright 側の基盤 #476 からも再利用できる形に保つ）。
//!
//! # スクリプトとの契約
//! - WS エンドポイントは環境変数 [`ENDPOINT_ENV`] で渡す（シェル文字列は経由しない）。
//! - スクリプトは stdout に [`RESULT_PREFIX`] で始まる 1 行の JSON
//!   `{"ok": bool, "step": string, "error": {"name": string, "message": string} | null}` を出す。
//!
//! # 安全性
//! 子プロセスは締め切りで kill し、stderr は末尾のみ・stdout は結果行のみを上限付きで保持し、
//! kill 後のリーダー待機にも上限を設ける
//! （無制限確保による DoS の防止）。サーバーは `127.0.0.1:0` にのみ bind する。

use std::future::Future;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
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
/// stderr の保持上限（末尾を保持）。stdout は結果行のみ回収する。
pub const MAX_STREAM_BYTES: usize = 1024 * 1024;
/// 結果行 1 行の長さ上限。
pub const MAX_RESULT_LINE_BYTES: usize = 64 * 1024;
/// `stderr_tail` に保持する末尾バイト数。
const STDERR_TAIL_BYTES: usize = 2048;
/// kill / 終了後にリーダー出力を待つ上限（孫プロセスがパイプを握り続ける場合の保護）。
const JOIN_GRACE: Duration = Duration::from_secs(2);

/// 未導入時に表示する導入手順（固定の英語文言。AC3）。
pub const INSTALL_HINT: &str = "puppeteer-core is not yet provisioned: harness/puppeteer-connect has no package.json or lockfile until its installation is approved (see harness/puppeteer-connect/README.md)";

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 一時ディレクトリ（drop で再帰削除）。プロファイル保管場所をここへ閉じ込める。
pub struct TempDir(pub PathBuf);

impl TempDir {
    /// 排他的に（`create_dir` で）新規作成できたパスだけを所有する。同名の既存ディレクトリ
    /// （PID 再利用・前回の異常終了の残骸）は流用も削除もせず、別名で作り直す。
    pub fn new() -> Self {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        for _ in 0..1000 {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("fandhe-cdp-script-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create temp dir: {e}"),
            }
        }
        panic!("could not create a unique temp dir");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// サーバー停止要求の共有状態（`stop` フラグと、待機中 Future の waker）。
#[derive(Default)]
struct StopSignal {
    inner: Mutex<(bool, Option<Waker>)>,
}

impl StopSignal {
    fn trigger(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.0 = true;
            if let Some(w) = g.1.take() {
                w.wake();
            }
        }
    }
}

/// `BoundServer::run_until` へ渡す停止 Future（workspace の tokio は `sync` / `time` feature を
/// 持たないため、標準ライブラリだけで実装する）。
struct StopFuture(Arc<StopSignal>);

impl Future for StopFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.0.inner.lock() {
            Ok(mut g) => {
                if g.0 {
                    Poll::Ready(())
                } else {
                    g.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
            // ロック汚染時は停止扱いにして待機を打ち切る。
            Err(_) => Poll::Ready(()),
        }
    }
}

/// サーバースレッド終了を待つ上限（超過時はプロファイルを削除せず保持して失敗にする）。
const SERVER_STOP_GRACE: Duration = Duration::from_secs(10);

/// 起動済みテストサーバー。drop 時にサーバーを停止してスレッド終了を待ち、その後に
/// `_dir`（プロファイル）を削除する（停止前に削除するとサーバーが保持中のプロファイルを
/// 消してしまうため。フィールドは宣言順に drop されるので `_dir` は最後）。
pub struct TestServer {
    pub addr: SocketAddr,
    stop: Arc<StopSignal>,
    stopped: Receiver<()>,
    _dir: Option<TempDir>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.trigger();
        // サーバースレッドが終了時に送る完了通知を待つ（送信側が drop されて Err(Disconnected)
        // になるのはスレッド終了済みの場合なので削除してよい）。時間切れは稼働中の可能性が
        // あるため、プロファイルを削除せず（leak して）失敗として報告する。
        if matches!(
            self.stopped.recv_timeout(SERVER_STOP_GRACE),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            std::mem::forget(self._dir.take());
            if !std::thread::panicking() {
                panic!("test server did not stop within {SERVER_STOP_GRACE:?}; profile dir kept");
            }
        }
    }
}

/// 実サーバーを専用スレッドで起動する。
///
/// 子プロセスの待機はブロッキングなので、テスト側ランタイムとは別の current_thread
/// ランタイムで動かす（workspace の tokio は `rt-multi-thread` を持たず、feature 追加は
/// 依存変更になるため行わない）。スレッドは [`TestServer`] の drop で停止・終了を待つ。
pub fn start_server() -> TestServer {
    let dir = TempDir::new();
    let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
    let app = Arc::new(AppState::with_disabled_renderer(profile));
    let st = Arc::new(CdpState::new(app));
    let stop = Arc::new(StopSignal::default());
    let stop_for_thread = Arc::clone(&stop);
    let (tx, rx) = std::sync::mpsc::channel();
    let (stopped_tx, stopped) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _ = rt.block_on(async move {
            let (router, ws_config) = endpoints(&st).expect("endpoints").into_parts();
            let bound = Server::new()
                .handler(router)
                .websocket(ws_config)
                .bind("127.0.0.1:0")
                .await
                .expect("bind");
            tx.send(bound.local_addr().expect("local_addr")).ok();
            bound.run_until(StopFuture(stop_for_thread)).await
        });
        // ランタイム（接続タスク含む）を落としてから完了を通知する。
        drop(rt);
        stopped_tx.send(()).ok();
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server did not start");
    TestServer {
        addr,
        stop,
        stopped,
        _dir: Some(dir),
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
    /// 結果行は `ok: true` だったが、スクリプトが異常終了した（終了コード非 0 / シグナル終了）。
    /// 成功結果として扱わない（終了状態と結果行の照合。接続試験の偽陽性防止）。
    ExitedAbnormally {
        exit_code: Option<i32>,
        step: String,
        stderr_tail: String,
    },
    /// 締め切りを超えて kill した。
    TimedOut { stderr_tail: String },
    /// スクリプトを起動できなかった（実行ファイル欠落・権限エラー等）。
    SpawnFailed { reason: String },
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

/// stderr を別スレッドで読み、末尾 [`MAX_STREAM_BYTES`] だけを保持する（子のパイプ詰まり防止）。
fn spawn_tail_reader<R: Read + Send + 'static>(mut r: R) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    kept.extend_from_slice(buf.get(..n).unwrap_or_default());
                    // 上限の 2 倍に達したら末尾 MAX_STREAM_BYTES だけ残す（償却 O(1)）。
                    if kept.len() >= MAX_STREAM_BYTES * 2 {
                        kept.drain(..kept.len() - MAX_STREAM_BYTES);
                    }
                }
            }
        }
        let start = kept.len().saturating_sub(MAX_STREAM_BYTES);
        tx.send(kept.split_off(start)).ok();
    });
    rx
}

/// stdout リーダーの共有状態。最後に見つけた結果行だけを上書き保持する（総メモリ量は
/// `MAX_RESULT_LINE_BYTES + 2` に有界。結果行を大量に出す子による無制限確保を防ぐ）。
struct ResultSlot {
    last: Arc<Mutex<Vec<u8>>>,
    done: Receiver<()>,
}

/// stdout を別スレッドで読み、`RESULT_PREFIX` で始まる行を見つけるたびに共有スロットへ
/// 上書きする（最後の結果行のみ保持）。
///
/// 出力量に関わらず末尾の結果行を取りこぼさない（先頭保持だと 1MiB 超の出力で末尾の結果行を
/// 捨ててしまうため）。1 行の保持は `MAX_RESULT_LINE_BYTES + 1` までで、超過分は読み捨てる
/// （長さ超過は `parse_result_line` が `Malformed` にする）。孫プロセスがパイプを握って EOF が
/// 来なくても、それまでに出力された結果行は [`collect_result`] が回収できる。
fn spawn_result_reader<R: Read + Send + 'static>(mut r: R) -> ResultSlot {
    let (done_tx, done) = mpsc::channel();
    let last = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&last);
    std::thread::spawn(move || {
        let cap = MAX_RESULT_LINE_BYTES + 1;
        let mut cur: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        let finish = |cur: &mut Vec<u8>| {
            if cur.starts_with(RESULT_PREFIX.as_bytes()) {
                let mut line = std::mem::take(cur);
                line.push(b'\n');
                if let Ok(mut g) = shared.lock() {
                    *g = line;
                }
            }
            cur.clear();
        };
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in buf.get(..n).unwrap_or_default() {
                        if b == b'\n' {
                            finish(&mut cur);
                        } else if cur.len() < cap {
                            cur.push(b);
                        }
                    }
                }
            }
        }
        finish(&mut cur);
        done_tx.send(()).ok();
    });
    ResultSlot { last, done }
}

/// リーダーの完了を最大 `JOIN_GRACE` 待ち、その時点の最後の結果行を返す（孫プロセスが
/// パイプを握り続けて EOF が来ない場合でも `run_script` がハングしない）。
fn collect_result(slot: &ResultSlot) -> Vec<u8> {
    let _ = slot.done.recv_timeout(JOIN_GRACE);
    slot.last.lock().map(|g| g.clone()).unwrap_or_default()
}

/// リーダーが送った最後のメッセージを締め切り付きで受け取る。孫プロセスがパイプの write 端を
/// 握り続けて EOF が来ない場合でも `run_script` がハングしないよう、合計 `JOIN_GRACE` で諦める
/// （諦めるまでに届いた分は返す）。
fn collect(rx: &Receiver<Vec<u8>>) -> Vec<u8> {
    let end = Instant::now() + JOIN_GRACE;
    let mut last = Vec::new();
    while let Ok(m) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        last = m;
    }
    last
}

/// 子プロセスのプロセスグループ全体を kill する（unix のみ。孫プロセスの孤児化防止）。
/// 子は `process_group(0)` で自身を先頭とするグループに入れてあるため pgid は子の pid に等しい。
#[cfg(unix)]
fn kill_group(pid: u32) {
    // macOS の BSD `/bin/kill` は `--` を PID と解釈して拒否するため、POSIX 準拠の
    // シェル組込み `kill`（`-s KILL -- -<pgid>`）を `sh -c` 経由で使う。pid は数値のみ。
    let _ = Command::new("sh")
        .args(["-c", "kill -s KILL -- \"-$1\"", "sh", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL_BYTES);
    String::from_utf8_lossy(bytes.get(start..).unwrap_or_default()).into_owned()
}

/// stdout から結果行（最後の 1 行）を取り出して解析する。結果行が無ければ `Ok(None)`。
pub fn parse_result_line(stdout: &[u8]) -> Result<Option<ScriptOutcome>, String> {
    // 結果行以外（ログ等）に不正な UTF-8 があっても無視できるよう、バイト列のまま行を探し、
    // 結果行だけを厳密に UTF-8 検証する（置換して成功扱いにしない）。
    let Some(raw) = stdout
        .split(|b| *b == b'\n')
        .rev()
        .find(|l| l.starts_with(RESULT_PREFIX.as_bytes()))
    else {
        return Ok(None);
    };
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.len() > MAX_RESULT_LINE_BYTES {
        return Err(format!("result line exceeds {MAX_RESULT_LINE_BYTES} bytes"));
    }
    let line =
        std::str::from_utf8(raw).map_err(|e| format!("result line is not valid UTF-8: {e}"))?;
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    for (k, v) in &cmd.envs {
        c.env(k, v);
    }
    if let Some(d) = &cmd.cwd {
        c.current_dir(d);
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            return ScriptOutcome::SpawnFailed {
                reason: format!("failed to spawn {}: {e}", cmd.program.display()),
            };
        }
    };
    let (Some(child_out), Some(child_err)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return ScriptOutcome::SpawnFailed {
            reason: "child stdout/stderr pipe was not available".to_string(),
        };
    };
    let out = spawn_result_reader(child_out);
    let err = spawn_tail_reader(child_err);

    let start = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(s)) => break (Some(s), false),
            Err(_) => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) if start.elapsed() >= deadline => {
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let stdout = collect_result(&out);
    let stderr = collect(&err);
    // 正常終了でも孫が残り得るため、回収後にグループ全体を掃除する。
    kill_group(child.id());
    if timed_out {
        return ScriptOutcome::TimedOut {
            stderr_tail: tail(&stderr),
        };
    }
    match parse_result_line(&stdout) {
        Ok(Some(ScriptOutcome::Completed { ok: true, step, .. }))
            if !status.is_some_and(|s| s.success()) =>
        {
            ScriptOutcome::ExitedAbnormally {
                exit_code: status.and_then(|s| s.code()),
                step,
                stderr_tail: tail(&stderr),
            }
        }
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

//! MCP エンベロープ測定ハーネスの stdio クライアント（TASK-97.1・Issue #385・`PLUG-5`・`MS-9`）。
//!
//! `mcp_envelope.rs` の main が、別プロセスの `fandhe-browser-mcp` バイナリを起動して
//! initialize → `notifications/initialized` → `tools/call snapshot` を stdio で送受信する。
//! バイナリは `FANDHE_BROWSER_HOST_ADDR` で `host.rs` の loopback ホストへ向け、起動時の登録
//! （`POST /ai/plugins/register`）も同ホストが処理する。
//!
//! 資源上限と期限（不安全な設計の防止）: 応答 1 行は 8 MiB（mcp 側の上限 4 MiB 以上）、stderr は
//! 64 KiB まで捕捉、1 呼び出し 30 秒・セッション全体 5 分。超過・EOF は失敗にする。バイナリは
//! シェルを経由せず `Command` へ直接渡し、PATH は探索しない。終了時は stdin を閉じて待ち、
//! 期限を過ぎたら kill する。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::jsonrpc;

/// バイナリを指定する環境変数名。
pub const BIN_ENV: &str = "FANDHE_BROWSER_MCP_BIN";
const HOST_ADDR_ENV: &str = "FANDHE_BROWSER_HOST_ADDR";
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_TIMEOUT: Duration = Duration::from_secs(300);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// 実行対象バイナリを解決する。環境変数優先（絶対パスかつ実在ファイル必須）。未指定なら
/// `<manifest_dir>/../../target/release/fandhe-browser-mcp`。どちらも無ければビルド手順付きでエラー。
pub fn resolve_bin(env_value: Option<&str>, manifest_dir: &Path) -> Result<PathBuf, String> {
    let (path, origin) = match env_value {
        Some(v) => (PathBuf::from(v), BIN_ENV),
        None => (
            manifest_dir
                .join("..")
                .join("..")
                .join("target")
                .join("release")
                .join("fandhe-browser-mcp"),
            "default location",
        ),
    };
    if env_value.is_some() && !path.is_absolute() {
        return Err(format!("{BIN_ENV} must be an absolute path"));
    }
    if !path.is_file() {
        return Err(format!(
            "fandhe-browser-mcp binary not found ({origin}); run `cargo build --release -p fandhe-browser-mcp` or set {BIN_ENV}"
        ));
    }
    Ok(path)
}

enum LineEvent {
    Line(String),
    Eof,
    TooLong,
    Io,
}

/// 起動済みの mcp セッション。Drop で子プロセスを確実に終了させる。
pub struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<LineEvent>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
    started: Instant,
}

impl McpSession {
    /// バイナリを起動し、initialize ハンドシェイクまで完了させる。
    pub fn start(bin: &Path, host_addr: std::net::SocketAddr) -> Result<Self, String> {
        let mut child = Command::new(bin)
            .env(HOST_ADDR_ENV, host_addr.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| "failed to spawn fandhe-browser-mcp".to_string())?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (tx, rx) = channel();
        if let Some(out) = stdout {
            std::thread::spawn(move || read_lines(out, tx));
        }
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(err) = stderr_pipe {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || capture_stderr(err, sink));
        }
        let mut session = Self {
            child,
            stdin,
            rx,
            stderr,
            next_id: 1,
            started: Instant::now(),
        };
        let (line, _) = session.request("initialize", jsonrpc::initialize_params())?;
        jsonrpc::expect_ok_response(&line, 1).map_err(|e| session.with_stderr(e.to_string()))?;
        session.send_line(&jsonrpc::notification_line("notifications/initialized"))?;
        Ok(session)
    }

    /// `tools/call snapshot` を送り、(応答行全体, 経過 ms, `result.content[0].text`) を返す。
    pub fn call_snapshot(&mut self) -> Result<(String, f64, String), String> {
        let id = self.next_id;
        let (line, ms) = self.request("tools/call", jsonrpc::snapshot_call_params())?;
        let text = jsonrpc::extract_snapshot_text(&line, id).map_err(|e| e.to_string())?;
        Ok((line, ms, text))
    }

    fn send_line(&mut self, line: &str) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "mcp stdin is closed".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
            .map_err(|_| self.with_stderr("failed to write to mcp stdin".to_string()))
    }

    /// 要求を送り、同じ呼び出しの応答 1 行と経過 ms を返す。
    fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(String, f64), String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = jsonrpc::request_line(id, method, params);
        let t0 = Instant::now();
        self.send_line(&line)?;
        let remaining = SESSION_TIMEOUT.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            return Err("mcp session deadline exceeded".to_string());
        }
        match self.rx.recv_timeout(CALL_TIMEOUT.min(remaining)) {
            Ok(LineEvent::Line(l)) => Ok((l, t0.elapsed().as_secs_f64() * 1000.0)),
            Ok(LineEvent::Eof) => Err(self.with_stderr("mcp closed stdout".to_string())),
            Ok(LineEvent::TooLong) => Err("mcp response line exceeds 8 MiB".to_string()),
            Ok(LineEvent::Io) | Err(RecvTimeoutError::Disconnected) => {
                Err(self.with_stderr("failed to read mcp stdout".to_string()))
            }
            Err(RecvTimeoutError::Timeout) => Err("mcp call timed out".to_string()),
        }
    }

    /// 診断用に mcp の stderr 末尾（mcp 自身が出す固定の英語文言）を添える。
    fn with_stderr(&self, msg: String) -> String {
        // 子プロセスの終了直後は stderr の取り込みが追い付かないことがあるため少し待つ。
        std::thread::sleep(Duration::from_millis(100));
        let guard = self.stderr.lock().ok();
        let text = guard
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            msg
        } else {
            format!("{msg} (mcp stderr: {text})")
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        // stdin を閉じて EOF で自然終了させ、期限を過ぎたら kill する。
        self.stdin.take();
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// stdout を 1 行ずつ読む。1 行が上限を超えたら `TooLong` で打ち切る。
fn read_lines(out: impl Read, tx: std::sync::mpsc::Sender<LineEvent>) {
    let mut reader = BufReader::new(out);
    loop {
        let mut buf = Vec::new();
        let n = match (&mut reader)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut buf)
        {
            Ok(n) => n,
            Err(_) => {
                let _ = tx.send(LineEvent::Io);
                return;
            }
        };
        if n == 0 {
            let _ = tx.send(LineEvent::Eof);
            return;
        }
        if buf.last() != Some(&b'\n') && buf.len() as u64 > MAX_LINE_BYTES {
            let _ = tx.send(LineEvent::TooLong);
            return;
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        if tx
            .send(LineEvent::Line(String::from_utf8_lossy(&buf).into_owned()))
            .is_err()
        {
            return;
        }
    }
}

/// stderr を 64 KiB まで保持し、以降は読み捨てる（子プロセスをブロックさせない）。
fn capture_stderr(mut err: impl Read, sink: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    loop {
        match err.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if let Ok(mut g) = sink.lock() {
                    let room = MAX_STDERR_BYTES.saturating_sub(g.len());
                    g.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
}

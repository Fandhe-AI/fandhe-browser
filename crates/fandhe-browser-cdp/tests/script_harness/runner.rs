//! スクリプト実行・結果行の解析（`script_harness` の中核。サーバー起動を含まない）。
//!
//! `mod.rs` から再エクスポートされるほか、`Profile::open` に依存しない契約テスト
//! （`tests/puppeteer_contract.rs`）が単独で取り込む（unix 限定にしないため）。

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// スクリプトへ WS エンドポイントを渡す環境変数名。
pub const ENDPOINT_ENV: &str = "FANDHE_CDP_WS_ENDPOINT";
/// 結果行の固定接頭辞（後ろに JSON が続く）。
pub const RESULT_PREFIX: &str = "FANDHE_SCRIPT_RESULT ";
/// stderr の保持上限（末尾を保持）。stdout は結果行のみ回収する。
pub const MAX_STREAM_BYTES: usize = 1024 * 1024;
/// `stages` の件数上限（無制限確保の防止）。
pub const MAX_STAGES: usize = 16;
/// 段階名の長さ上限（バイト）。
pub const MAX_STAGE_NAME_BYTES: usize = 64;
/// 結果行 1 行の長さ上限。
pub const MAX_RESULT_LINE_BYTES: usize = 64 * 1024;
/// `stderr_tail` に保持する末尾バイト数。
const STDERR_TAIL_BYTES: usize = 2048;
/// kill / 終了後にリーダー出力を待つ上限（孫プロセスがパイプを握り続ける場合の保護）。
const JOIN_GRACE: Duration = Duration::from_secs(2);

/// スクリプトが報告したエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    pub name: String,
    pub message: String,
}

/// 段階の到達状態（TASK-45.2・`CDP-3`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    /// 成功した。
    Ok,
    /// この段階で失敗した（`error` が必須）。
    Failed,
    /// 先行段階の失敗により実行されなかった。
    NotReached,
}

/// 1 段階の結果（接続 → newPage → goto → セレクタ取得の各段階。名前は呼び出し側が決める）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageResult {
    pub name: String,
    pub status: StageStatus,
    pub error: Option<ScriptError>,
}

/// スクリプト実行結果の回収形（REPAIR-4。真偽値で済ませない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// 結果行を回収できた（`ok == false` も「記録された結果」であり基盤の失敗ではない）。
    Completed {
        ok: bool,
        step: String,
        error: Option<ScriptError>,
        /// 段階別の到達結果（`stages` 未報告なら空）。
        stages: Vec<StageResult>,
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
    let error = parse_error(json.get("error"), "error")?;
    let stages = parse_stages(json.get("stages"))?;
    if ok && stages.iter().any(|s| s.status != StageStatus::Ok) {
        return Err("`ok` is true but a stage did not succeed".to_string());
    }
    // トップレベルと段階の相互整合（偽陽性防止）。`stages` 未報告（空）は検証対象外。
    if !ok && !stages.is_empty() {
        match stages.iter().find(|s| s.status == StageStatus::Failed) {
            None => return Err("`ok` is false but no stage failed".to_string()),
            Some(f) if f.name != step => {
                return Err("`step` does not match the first failed stage".to_string());
            }
            Some(f) if f.error != error => {
                return Err("top-level `error` does not match the failed stage error".to_string());
            }
            Some(_) => {}
        }
    }
    Ok(Some(ScriptOutcome::Completed {
        ok,
        step,
        error,
        stages,
    }))
}

/// `{name, message}` 形のエラーを解析する（null / 欠落は `None`）。`label` はエラー文言用の接頭辞。
fn parse_error(v: Option<&Value>, label: &str) -> Result<Option<ScriptError>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(e) => Ok(Some(ScriptError {
            name: e
                .get("name")
                .and_then(Value::as_str)
                .ok_or(format!("missing string field `{label}.name`"))?
                .to_string(),
            message: e
                .get("message")
                .and_then(Value::as_str)
                .ok_or(format!("missing string field `{label}.message`"))?
                .to_string(),
        })),
    }
}

/// `stages` を検証付きで解析する（外部入力。件数・名前長の上限と整合性を確認する）。
fn parse_stages(v: Option<&Value>) -> Result<Vec<StageResult>, String> {
    let arr = match v {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(a)) => a,
        Some(_) => return Err("`stages` must be an array".to_string()),
    };
    if arr.len() > MAX_STAGES {
        return Err(format!("`stages` exceeds {MAX_STAGES} entries"));
    }
    let mut out: Vec<StageResult> = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or(format!("missing string field `stages[{i}].name`"))?;
        if name.is_empty() || name.len() > MAX_STAGE_NAME_BYTES {
            return Err(format!(
                "`stages[{i}].name` must be 1..={MAX_STAGE_NAME_BYTES} bytes"
            ));
        }
        let status = match item.get("status").and_then(Value::as_str) {
            Some("ok") => StageStatus::Ok,
            Some("failed") => StageStatus::Failed,
            Some("not_reached") => StageStatus::NotReached,
            _ => return Err(format!("invalid `stages[{i}].status`")),
        };
        let error = parse_error(item.get("error"), &format!("stages[{i}].error"))?;
        match (status, &error) {
            (StageStatus::Failed, None) => {
                return Err(format!("`stages[{i}]` is failed but has no error"));
            }
            (StageStatus::Ok | StageStatus::NotReached, Some(_)) => {
                return Err(format!("`stages[{i}]` is not failed but has an error"));
            }
            _ => {}
        }
        if status == StageStatus::Ok && out.last().is_some_and(|p| p.status != StageStatus::Ok) {
            return Err(format!("`stages[{i}]` is ok after a non-ok stage"));
        }
        // 「最初の失敗で止め、残りは not_reached」契約: not_reached は失敗の後にのみ許され、
        // 失敗段階は 1 つだけ。
        let failed_seen = out.iter().any(|p| p.status == StageStatus::Failed);
        if status == StageStatus::NotReached && !failed_seen {
            return Err(format!(
                "`stages[{i}]` is not_reached before any failed stage"
            ));
        }
        if status == StageStatus::Failed && failed_seen {
            return Err(format!("`stages[{i}]` is a second failed stage"));
        }
        out.push(StageResult {
            name: name.to_string(),
            status,
            error,
        });
    }
    Ok(out)
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

//! JS 評価用の子プロセスへの親側プロキシ（`JS-1`・`TASK-29`・Issue #503
//! 「JS プロセス分離」設計書 §3.2・§3.3・§3.4・§7 W4）。
//!
//! 呼び出し元（将来）: `TASK-29.6`（Issue #157）で `create_engine` から
//! 配線され、`impl JsEngine for V8ProcessEngine` を追加する（本 Issue の
//! 時点では inherent メソッドとして `evaluate_script` を提供するに
//! 留める。設計書 §7「案 X」4）。
//!
//! [`V8ProcessEngine`] は 1 つの子プロセス（[`super::worker`] を
//! `FANDHE_BROWSER_JS_WORKER` 環境変数で起動したもの）を遅延生成し、
//! 寿命のあいだ持ち続ける。子は同じ実行ファイルを自己再実行する
//! （専用のワーカーバイナリを持たない。設計書 §3.1）。
//!
//! # スレッド構成
//!
//! - **reader スレッド**: 子の stdout からフレームを読み、`mpsc` 経由で
//!   評価呼び出し側へ届ける
//! - **stderr drain スレッド**: 子の stderr を読み続け、末尾
//!   [`STDERR_TAIL_CAPACITY_BYTES`] バイトをリングバッファへ保持する。
//!   読み続けないとパイプが詰まって子が止まるため必須（設計書 §3.3）
//!
//! # エラー変換（現在の暫定措置。実装済みを装わない。REPAIR-3）
//!
//! `JsEngineError` は現時点で `EvaluationFailed`/`BindingFailed`/
//! `ResourceLimitExceeded` の 3 variant のみを持つ。タイムアウト・
//! 子プロセスの異常終了を専用の variant（`Timeout`・`EngineUnavailable`）
//! で表す変更は `TASK-29`（Issue #503 設計書 §7 W5）で行う。本 Issue の
//! 時点では、これらの状況も `EvaluationFailed` のメッセージ文言で表す。
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - 子への書き込み（`Evaluate` フレームの送信）は呼び出しスレッドで
//!   同期的に行う。子が応答を返さないまま stdin を読まなくなった場合、
//!   OS のパイプバッファが尽きると書き込みがブロックしうる（通常運用
//!   では子は常に stdin を読み続けるため起こらない想定だが、理論上の
//!   残存リスクとして記録する）
//! - ヒープ外メモリ（`ArrayBuffer` の backing store 等）の上限は子の
//!   中に無い（[`super::v8_engine`] の `MAX_ISOLATE_HEAP_BYTES` ドキュメント
//!   コメント参照）。OS 側の制限（Linux の RLIMIT_DATA、Windows の
//!   Job Object）は後続の Issue で扱う
//! - macOS には子のメモリ使用量を強制する手段が無い
//! - 子はセキュリティ上のサンドボックスではない。資源（クラッシュ・
//!   メモリ）の分離だけを提供する
//! - stderr の文言（`Fatal JavaScript out of memory`/`Fatal process out
//!   of memory`）による `ResourceLimitExceeded` の判定はヒューリスティック
//!   であり、V8 のバージョン更新でメッセージが変われば壊れうる

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::engine_trait::{JsEngineError, JsValue};
use super::worker_protocol::{self, ErrorKind, tag};

/// ハンドシェイク（`Hello` フレームの受信）を待つ上限時間（設計書
/// §3.1「5 秒でタイムアウト」）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 1 回の評価応答を待つ上限時間（設計書 §3.3「`recv_timeout(2 秒+1 秒)`」。
/// 子の実行時間監視（[`super::v8_engine`] の `SCRIPT_EXECUTION_TIMEOUT`。
/// 2 秒）に、IPC 往復・OS スケジューリングの猶予として 1 秒を足す）。
const EVALUATE_RECV_TIMEOUT: Duration = Duration::from_secs(3);

/// 子の `Drop` 時、stdin を閉じてから強制終了するまでに正常終了を待つ
/// 上限時間（設計書 §3.2「1 秒だけ `try_wait` で待つ」）。
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// stderr の末尾を保持するバイト数（設計書 §3.5「stderr: 末尾 16 KiB」）。
const STDERR_TAIL_CAPACITY_BYTES: usize = 16 * 1024;

/// reader スレッドが評価呼び出し側へ届けるイベント。
enum ReaderEvent {
    /// フレームを 1 つ読み取れた。
    Frame(u8, Vec<u8>),
    /// フレームの区切りで子がストリームを閉じた（正常終了、または
    /// 評価の応答を送る前にプロセスが終了した）。
    Eof,
    /// フレームのデコードに失敗した（プロトコル違反）。
    Invalid(String),
}

/// 1 つの子プロセスと、そのやり取りに必要な状態一式。
struct WorkerHandle {
    child: Child,
    /// `Drop` の手順（stdin を先に閉じる）のために `Option` にする。
    stdin: Option<ChildStdin>,
    frame_rx: mpsc::Receiver<ReaderEvent>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl WorkerHandle {
    /// stdin を閉じずに即座に kill して wait する（ハンドシェイク失敗・
    /// プロトコル違反時の後始末。`Drop` の穏やかな手順とは別に、失敗が
    /// 確定した時点で速やかに資源を解放するために使う）。
    fn terminate_now(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// stderr の末尾（現時点までに読み取れた分）をスナップショットする。
    fn stderr_tail_snapshot(&self) -> String {
        match self.stderr_tail.lock() {
            Ok(tail) => String::from_utf8_lossy(&tail).into_owned(),
            Err(_) => String::new(),
        }
    }

    /// stderr の末尾に、ヒープ上限到達を示す V8 の fatal メッセージが
    /// 含まれるかどうか（`ResourceLimitExceeded` への変換に使う
    /// ヒューリスティック。設計書 §3.3・実装済みを装わない節参照）。
    fn stderr_indicates_oom(tail: &str) -> bool {
        tail.contains("Fatal JavaScript out of memory")
            || tail.contains("Fatal process out of memory")
    }
}

impl Drop for WorkerHandle {
    /// 設計書 §3.2 の手順: (1) stdin を閉じる。(2) 1 秒だけ `try_wait` で
    /// 待つ。(3) 終わらなければ `kill` する。(4) 必ず `wait` する
    /// （scratch コンテナで本体が PID 1 になる場合にゾンビを残さない）。
    fn drop(&mut self) {
        self.stdin = None;

        let deadline = Instant::now() + GRACEFUL_SHUTDOWN_TIMEOUT;
        let mut exited = false;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_status)) => {
                    exited = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }

        if !exited {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();

        // reader・stderr スレッドの後始末（best effort）。子が終了して
        // いるため、stdout/stderr は EOF に達しておりスレッドは自然に
        // 終了できるはずである。
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

/// JS 評価を子プロセスへ分離して提供するエンジン（`JS-1`・Issue #503）。
///
/// 子・パイプしか保持しないため `Send` にできる（設計書 §3.2。将来
/// core・tokio と統合するときに有利になる）。
pub(crate) struct V8ProcessEngine {
    worker: Option<WorkerHandle>,
}

impl V8ProcessEngine {
    /// 子プロセスを遅延生成する構成で初期化する（設計書 §3.1「遅延
    /// 起動」。PERF-6・PERF-7・CORE-3 を守るため、生成時点ではまだ子を
    /// 起動しない）。
    pub(crate) fn new() -> Self {
        Self { worker: None }
    }

    /// スクリプトを評価する（`JS-1`「スクリプト評価」）。
    ///
    /// 子プロセスが無ければこの呼び出しで起動する（遅延起動）。評価が
    /// タイムアウト・クラッシュ・OOM 等で失敗した場合、子プロセスを
    /// 破棄することがある（メッセージに「context was discarded」を
    /// 含めて明示する。security.md「偽装・回避機能の禁止」）。破棄した
    /// 場合、次回の呼び出しで新しい子プロセスを自動的に起動し直す
    /// （本メソッドが `self.worker` を都度 `take`/設定し直す構造その
    /// ものが、この「次回に新しい子を起動し直す」動作を実現する）。
    pub(crate) fn evaluate_script(&mut self, script: &str) -> Result<JsValue, JsEngineError> {
        // 外部入力（スクリプト文字列）の経路。子を起動する前に検査する
        // （coding-rust.md「外部入力」節）。
        if script.len() > worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD {
            return Err(JsEngineError::EvaluationFailed(format!(
                "script source exceeds the maximum supported length of {} bytes",
                worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD
            )));
        }

        let mut worker = match self.worker.take() {
            Some(worker) => worker,
            None => Self::spawn_worker()?,
        };

        let (outcome, keep_worker) = Self::send_evaluate_and_await(&mut worker, script);
        if keep_worker {
            self.worker = Some(worker);
        }
        // `keep_worker` が `false` の場合は `worker` をここで drop し、
        // `WorkerHandle::drop` の手順（stdin を閉じる → 1 秒待つ →
        // kill → wait）で後始末する。次回の `evaluate_script` 呼び出し
        // では `self.worker` が `None` のため新しい子を起動し直す。
        outcome
    }

    /// 子プロセスを起動し、ハンドシェイク（`Hello` の受信）まで完了させる。
    fn spawn_worker() -> Result<WorkerHandle, JsEngineError> {
        // 設計書 §3.1「再帰の防止」: 自分自身が既にワーカーとして起動
        // されている場合は、さらに子を起動しない。通常の cli 経路では
        // `run_js_worker_if_requested` がワーカーモードのまま `main` を
        // 続けさせることはないが、ライブラリとして誤って組み込まれた
        // 場合の多重防御として確認する。
        if std::env::var(worker_protocol::MARKER_ENV_VAR).is_ok() {
            return Err(JsEngineError::EvaluationFailed(
                "refusing to spawn a nested JS worker process: this process is itself running \
                 as a worker"
                    .to_string(),
            ));
        }

        let exe = Self::resolve_worker_exe();
        let mut command = Command::new(&exe);
        // 秘密情報を含みうる環境変数を子へ渡さない（設計書 §3.4）。
        command.env_clear();
        command.env(
            worker_protocol::MARKER_ENV_VAR,
            worker_protocol::PROTOCOL_VERSION.to_string(),
        );
        #[cfg(windows)]
        {
            // Windows のプロセス生成に必要（`SystemRoot` が無いと子が
            // 正しく動作しない可能性がある。設計書 §3.4）。
            if let Some(system_root) = std::env::var_os("SystemRoot") {
                command.env("SystemRoot", system_root);
            }
        }
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|err| {
            JsEngineError::EvaluationFailed(format!(
                "failed to spawn the JS worker process ({}): {err}",
                exe.display()
            ))
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            JsEngineError::EvaluationFailed(
                "JS worker process was spawned without a stdin pipe".to_string(),
            )
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            JsEngineError::EvaluationFailed(
                "JS worker process was spawned without a stdout pipe".to_string(),
            )
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            JsEngineError::EvaluationFailed(
                "JS worker process was spawned without a stderr pipe".to_string(),
            )
        })?;

        let (frame_tx, frame_rx) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            loop {
                match worker_protocol::read_frame(
                    &mut stdout,
                    worker_protocol::MAX_FRAME_PAYLOAD_CHILD_TO_PARENT,
                ) {
                    Ok(Some((frame_tag, payload))) => {
                        if frame_tx
                            .send(ReaderEvent::Frame(frame_tag, payload))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = frame_tx.send(ReaderEvent::Eof);
                        return;
                    }
                    Err(err) => {
                        let _ = frame_tx.send(ReaderEvent::Invalid(err.to_string()));
                        return;
                    }
                }
            }
        });

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_thread = {
            let stderr_tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => {
                            let Ok(mut tail) = stderr_tail.lock() else {
                                return;
                            };
                            tail.extend_from_slice(&buf[..n]);
                            let overflow = tail.len().saturating_sub(STDERR_TAIL_CAPACITY_BYTES);
                            if overflow > 0 {
                                tail.drain(0..overflow);
                            }
                        }
                        Err(_) => return,
                    }
                }
            })
        };

        let mut worker = WorkerHandle {
            child,
            stdin: Some(stdin),
            frame_rx,
            reader_thread: Some(reader_thread),
            stderr_tail,
            stderr_thread: Some(stderr_thread),
        };

        match worker.frame_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::HELLO => {
                match worker_protocol::decode_hello(&payload) {
                    Ok((version, _engine)) if version == worker_protocol::PROTOCOL_VERSION => {
                        Ok(worker)
                    }
                    Ok((version, _engine)) => {
                        worker.terminate_now();
                        Err(JsEngineError::EvaluationFailed(format!(
                            "JS worker process reported unsupported protocol version {version}"
                        )))
                    }
                    Err(err) => {
                        worker.terminate_now();
                        Err(JsEngineError::EvaluationFailed(format!(
                            "JS worker process sent a malformed Hello frame: {err}"
                        )))
                    }
                }
            }
            Ok(ReaderEvent::Frame(other_tag, _)) => {
                worker.terminate_now();
                Err(JsEngineError::EvaluationFailed(format!(
                    "JS worker process sent an unexpected frame (tag {other_tag}) before Hello"
                )))
            }
            Ok(ReaderEvent::Eof) => {
                let tail = worker.stderr_tail_snapshot();
                worker.terminate_now();
                Err(JsEngineError::EvaluationFailed(format!(
                    "JS worker process exited before completing the handshake; stderr: {tail}"
                )))
            }
            Ok(ReaderEvent::Invalid(desc)) => {
                worker.terminate_now();
                Err(JsEngineError::EvaluationFailed(format!(
                    "JS worker process sent a malformed frame during the handshake: {desc}"
                )))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.terminate_now();
                Err(JsEngineError::EvaluationFailed(
                    "JS worker process handshake timed out".to_string(),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                Err(JsEngineError::EvaluationFailed(
                    "JS worker process handshake channel disconnected unexpectedly".to_string(),
                ))
            }
        }
    }

    /// 自己再実行に使う実行ファイルのパスを決める（設計書 §3.1）。
    ///
    /// Linux では `/proc/self/exe` を優先する（アップグレードでバイナリが
    /// 置き換わっても、`current_exe()` と異なり実行中のバイナリの実体を
    /// 指し続けるため）。存在しなければ `current_exe()` にフォールバック
    /// する。
    ///
    /// `current_exe()` の結果をセキュリティ判断に使うなという std の
    /// 注意はここでは当てはまらない: ここでの攻撃者はローカルで
    /// ハードリンクを差し替えられる攻撃者ではなく、子プロセスの中で
    /// 実行される untrusted な JS である（設計書 §3.1）。
    fn resolve_worker_exe() -> std::path::PathBuf {
        #[cfg(target_os = "linux")]
        {
            let proc_self_exe = std::path::Path::new("/proc/self/exe");
            if proc_self_exe.exists() {
                return proc_self_exe.to_path_buf();
            }
        }
        std::env::current_exe().unwrap_or_else(|_| {
            std::path::PathBuf::from(
                std::env::args()
                    .next()
                    .unwrap_or_else(|| "fandhe-browser".to_string()),
            )
        })
    }

    /// `Evaluate` フレームを送り、応答（`Result`/`Error`/異常終了）を
    /// 待つ。戻り値の 2 つ目は「この `worker` を保持し続けてよいか」を
    /// 表す（`false` の場合、呼び出し元が `worker` を drop し、次回は
    /// 新しい子を起動し直す）。
    fn send_evaluate_and_await(
        worker: &mut WorkerHandle,
        script: &str,
    ) -> (Result<JsValue, JsEngineError>, bool) {
        let payload = worker_protocol::encode_evaluate(script);
        let write_result = match worker.stdin.as_mut() {
            Some(stdin) => worker_protocol::write_frame(stdin, tag::EVALUATE, &payload)
                .and_then(|()| stdin.flush().map_err(worker_protocol::ProtocolError::from)),
            None => Err(worker_protocol::ProtocolError::Io(std::io::Error::other(
                "JS worker process stdin is already closed",
            ))),
        };
        if let Err(err) = write_result {
            let tail = worker.stderr_tail_snapshot();
            worker.terminate_now();
            return (
                Err(JsEngineError::EvaluationFailed(format!(
                    "failed to send the script to the JS worker process (it may have crashed): \
                     {err}; stderr: {tail}; context was discarded"
                ))),
                false,
            );
        }

        match worker.frame_rx.recv_timeout(EVALUATE_RECV_TIMEOUT) {
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::RESULT => {
                match worker_protocol::decode_js_value(&payload) {
                    Ok((value, _consumed)) => (Ok(value), true),
                    Err(err) => {
                        worker.terminate_now();
                        (
                            Err(JsEngineError::EvaluationFailed(format!(
                                "JS worker process sent a malformed Result frame: {err}; context \
                                 was discarded"
                            ))),
                            false,
                        )
                    }
                }
            }
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::ERROR => {
                match worker_protocol::decode_error(&payload) {
                    // 子の watchdog による打ち切り。Context は残る
                    // （設計書 §3.3 の表の 1 行目）。
                    Ok((ErrorKind::Timeout, message)) => (
                        Err(JsEngineError::EvaluationFailed(format!(
                            "script execution timed out inside the JS worker process: {message}"
                        ))),
                        true,
                    ),
                    Ok((ErrorKind::Evaluation, message)) => {
                        (Err(JsEngineError::EvaluationFailed(message)), true)
                    }
                    Ok((ErrorKind::Binding, message)) => {
                        (Err(JsEngineError::BindingFailed(message)), true)
                    }
                    Err(err) => {
                        worker.terminate_now();
                        (
                            Err(JsEngineError::EvaluationFailed(format!(
                                "JS worker process sent a malformed Error frame: {err}; context \
                                 was discarded"
                            ))),
                            false,
                        )
                    }
                }
            }
            Ok(ReaderEvent::Frame(other_tag, _)) => {
                worker.terminate_now();
                (
                    Err(JsEngineError::EvaluationFailed(format!(
                        "JS worker process sent an unexpected frame (tag {other_tag}); context \
                         was discarded"
                    ))),
                    false,
                )
            }
            Ok(ReaderEvent::Eof) => {
                let _ = worker.child.wait();
                let tail = worker.stderr_tail_snapshot();
                let discarded_note =
                    "context was discarded; the next evaluation runs in a fresh context";
                if WorkerHandle::stderr_indicates_oom(&tail) {
                    (
                        Err(JsEngineError::ResourceLimitExceeded(format!(
                            "script execution exceeded the isolate heap limit and the JS worker \
                             process terminated; {discarded_note}; stderr: {tail}"
                        ))),
                        false,
                    )
                } else {
                    (
                        Err(JsEngineError::EvaluationFailed(format!(
                            "JS worker process terminated unexpectedly before responding; \
                             {discarded_note}; stderr: {tail}"
                        ))),
                        false,
                    )
                }
            }
            Ok(ReaderEvent::Invalid(desc)) => {
                worker.terminate_now();
                (
                    Err(JsEngineError::EvaluationFailed(format!(
                        "JS worker process sent a malformed frame: {desc}; context was discarded"
                    ))),
                    false,
                )
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.terminate_now();
                (
                    Err(JsEngineError::EvaluationFailed(
                        "script evaluation exceeded the JS worker deadline and the process was \
                         killed; context was discarded; the next evaluation runs in a fresh \
                         context"
                            .to_string(),
                    )),
                    false,
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                (
                    Err(JsEngineError::EvaluationFailed(
                        "JS worker process communication channel disconnected unexpectedly; \
                         context was discarded"
                            .to_string(),
                    )),
                    false,
                )
            }
        }
    }
}

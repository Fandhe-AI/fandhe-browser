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
//! - **writer スレッド**: 子の stdin への書き込みを専任で担当する。
//!   `Evaluate` フレームの送信は呼び出しスレッドから
//!   `mpsc::Sender<Vec<u8>>` 経由でこのスレッドへ委譲し、呼び出し
//!   スレッドは書き込み完了（または失敗）の通知を期限付きで待つ
//!   （codex・Cursor Bugbot レビュー指摘 #503 P1「`write_frame`・`flush`
//!   が呼び出しスレッドで同期的に実行され、パイプが満杯だと期限も
//!   `kill` も効かない」対応）。パイプが満杯で書き込みがブロックしても、
//!   ブロックするのは writer スレッドだけであり、呼び出しスレッドは
//!   期限どおりに `kill` を実行できる（`kill` により子の stdin 読み取り
//!   端が消え、ブロックしていた書き込みは broken pipe で解放される）
//! - **reader スレッド**: 子の stdout からフレームを読み、`mpsc` 経由で
//!   評価呼び出し側へ届ける
//! - **stderr drain スレッド**: 子の stderr を読み続け、末尾
//!   [`STDERR_TAIL_CAPACITY_BYTES`] バイトをリングバッファへ保持する。
//!   読み続けないとパイプが詰まって子が止まるため必須（設計書 §3.3）
//! - **メモリ監視スレッド**（[`MemoryMonitor`]）: 子の寿命のあいだ
//!   （評価中か待機中かに関わらず）[`super::resource_limits::RSS_POLL_INTERVAL`]
//!   ごとに子の RSS を確認し続け、しきい値を超えたらこのスレッドが
//!   自ら `kill` する（codex・Cursor Bugbot レビュー指摘 #503 P0「RSS の
//!   確認が `recv_timeout` の時間切れ分岐でしか行われておらず、短い
//!   評価を繰り返す・評価をしていない待機中はすり抜ける」対応）。
//!
//! # エラー変換（設計書 §3.3 の対応表。`TASK-29`・Issue #503 W5）
//!
//! | 状況 | 変換先 | Context |
//! |---|---|---|
//! | 子の watchdog による打ち切り（`Error{kind=Timeout}`） | [`JsEngineError::Timeout`] | 残る |
//! | 応答待ち・書き込み待ちの期限切れで親が `kill` した | [`JsEngineError::Timeout`] | 破棄（メッセージに明示） |
//! | 応答前に EOF になり、stderr に `Fatal ... out of memory` がある | [`JsEngineError::ResourceLimitExceeded`] | 破棄（ヒューリスティック） |
//! | メモリ監視スレッドが RSS 超過を検出して子を `kill` した（評価中・書き込み中・待機中いずれも） | [`JsEngineError::ResourceLimitExceeded`] | 破棄（メッセージに実測 RSS を明示。書き込み中に `kill` された場合の broken pipe も正しくここへ分類する。[`resource_limit_or_else`]。codex・Bugbot レビュー指摘 #503 P0/P1 対応） |
//! | 応答（Result/Error）受信直後の同期確認で RSS 超過が判明した | [`JsEngineError::ResourceLimitExceeded`] | 破棄（せっかく得られた応答を握りつぶす。同上） |
//! | それ以外の異常終了・プロトコル違反・起動失敗 | [`JsEngineError::EngineUnavailable`] | 破棄 |
//!
//! Context が破棄される場合、メッセージに "context was discarded" を
//! 含める（security.md「偽装・回避機能の禁止」──状態が失われたことを
//! 隠さない）。
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - ヒープ外メモリ（`ArrayBuffer` の backing store 等）には
//!   [`super::resource_limits`] が子側の OS 別強制（Linux の
//!   `RLIMIT_DATA`・Windows の Job Object working set 上限）と親側の
//!   RSS 監視（[`MemoryMonitor`]。子の寿命のあいだ継続的に動作する）に
//!   よる多層防御を設けている（codex・Bugbot レビュー指摘 #503 P0
//!   対応）。ただし実測の結果、子側の OS 別強制はいずれも厳密な上限には
//!   ならず（Linux は V8 の `CodeRange` 仮想アドレス予約のため小さい値に
//!   設定できない・Windows は working set の trim にしかならない）、
//!   **3 OS 共通で親側の RSS 監視が主たる防衛線**である。詳細・実測値は
//!   `super::resource_limits` のドキュメントコメント「OS ごとの強制の
//!   強さ」節を参照
//! - macOS には子のメモリ使用量を OS 側で強制する手段が無く
//!   （`super::resource_limits` の実機検証結果を参照）、親側の RSS 監視
//!   だけに頼る
//! - 応答直後の同期的な RSS 確認（[`apply_post_response_rss_check`]）は
//!   呼び出しスレッド上で `read_child_rss_bytes` を 1 回だけ呼ぶ。
//!   Windows では `ps` 相当が PowerShell の起動を伴うため、この 1 回の
//!   呼び出しだけで数百ミリ秒の遅延が評価のたびに乗りうる（実機・CI
//!   未検証。`super::resource_limits::RSS_POLL_INTERVAL` の Windows 向け
//!   ドキュメントコメント参照）
//! - 子はセキュリティ上のサンドボックスではない。同じユーザー権限で動作し、
//!   seccomp 等も使わない。得られるのはクラッシュ・メモリの資源分離
//!   だけである
//! - stderr の文言（`Fatal JavaScript out of memory`/`Fatal process out
//!   of memory`）による `ResourceLimitExceeded` の判定はヒューリスティック
//!   であり、V8 のバージョン更新でメッセージが変われば壊れうる
//! - [`super::v8_engine`] の `SCRIPT_EXECUTION_TIMEOUT` のドキュメント
//!   コメントが記す既知の制限（「`v8::Script::compile` 自体は
//!   `terminate_execution` では打ち切られない場合がある」）は、本モジュール
//!   の [`EVALUATE_RECV_TIMEOUT`] による強制 `kill`（[`WorkerHandle::drop`]
//!   と同じ「stdin を閉じる→待つ→kill→wait」の手順は踏まず、応答待ちの
//!   `recv_timeout` が切れた時点で直ちに `kill` する）で解消される。
//!   コンパイルがどれだけ長くかかっても、子プロセスごと強制終了できる
//!   ため、呼び出しスレッドが戻ってこないことはない

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::engine_trait::{EvaluateOptions, JsEngineError, JsValue};
use super::worker_protocol::{self, ErrorKind, tag};

/// ハンドシェイク（`Hello` フレームの受信）を待つ上限時間（設計書
/// §3.1「5 秒でタイムアウト」）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 1 回の評価（書き込み＋応答待ちの合計）に許す上限時間（設計書
/// §3.3「`recv_timeout(2 秒+1 秒)`」）。子の実行時間監視
/// （[`super::v8_engine::SCRIPT_EXECUTION_TIMEOUT`]）に、IPC 往復・OS
/// スケジューリングの猶予として 1 秒を足す。定数同士の演算のため
/// `const` のまま計算でき、`v8_engine.rs` 側の値が変わってもこの余裕
/// （1 秒）は追随する。
///
/// **書き込みも含めた合計の期限である**（codex レビュー指摘 #503 P1
/// 対応）: `Evaluate` フレームの書き込み完了を待つ時間と、応答を待つ
/// 時間の両方をこの 1 つの期限で管理する（`send_evaluate_and_await` が
/// 評価開始時に 1 度だけ期限を計算し、書き込み待ち・応答待ちの両方で
/// 「残り時間」を使い回す）。
const EVALUATE_RECV_TIMEOUT: Duration =
    super::v8_engine::SCRIPT_EXECUTION_TIMEOUT.saturating_add(Duration::from_secs(1));

/// 子の `Drop` 時、stdin を閉じてから強制終了するまでに正常終了を待つ
/// 上限時間（設計書 §3.2「1 秒だけ `try_wait` で待つ」）。
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// [`WorkerHandle::reap_and_collect_stderr`] が、期限なしで `wait` する
/// のではなく `try_wait` を期限付きでポーリングする際の上限時間（codex
/// レビュー指摘 #503 P1「EOF の後の `reap_and_collect_stderr` が期限
/// なしの `child.wait()` を呼んでいる」対応）。呼び出し元は既に子が
/// ストリームを閉じている（EOF）と判断した状態で呼ぶため、通常は
/// すぐに終了するはずだが、万一終了が遅れても永久に待たないよう
/// [`GRACEFUL_SHUTDOWN_TIMEOUT`] と同じ 1 秒の上限を共有する。
const REAP_WAIT_TIMEOUT: Duration = GRACEFUL_SHUTDOWN_TIMEOUT;

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

/// 子の寿命のあいだ、評価中か待機中かに関わらず RSS を継続的に監視する
/// 専任スレッド（codex・Cursor Bugbot レビュー指摘 #503 P0 対応）。
///
/// 呼び出し元（`WorkerHandle`）が [`MemoryMonitor::spawn`] で生成し、
/// [`WorkerHandle::drop`] で必ず [`MemoryMonitor::stop_and_join`] を
/// 呼んで止める（子の寿命を超えて監視スレッドが残り続けることはない）。
struct MemoryMonitor {
    /// 監視スレッドへ停止を伝えるフラグ。
    stop: Arc<AtomicBool>,
    /// 監視スレッドが RSS 超過を検出して `kill` した場合、そのときの
    /// RSS（バイト）を記録する。`None` は「まだ検出していない」。
    /// 呼び出し側は [`MemoryMonitor::take_killed_rss`] で消費する。
    killed_rss_bytes: Arc<Mutex<Option<u64>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MemoryMonitor {
    /// 監視スレッドを起動する（本番のしきい値
    /// [`super::resource_limits::MAX_CHILD_RSS_BYTES`] を使う）。
    fn spawn(child: Arc<Mutex<Child>>, pid: u32) -> Self {
        Self::spawn_with_threshold(child, pid, super::resource_limits::MAX_CHILD_RSS_BYTES)
    }

    /// [`MemoryMonitor::spawn`] の本体。しきい値を呼び出し元から指定
    /// できる（単体テストが、実プロセスの RSS を人為的に膨らませずに
    /// 「しきい値超過」を確実に再現するため、極端に小さいしきい値を
    /// 渡せるようにする。`tests` モジュール参照）。`child` は `kill`
    /// するために共有し、`pid` は RSS を読み取るために使う
    /// （[`std::process::Child::id`] はプロセスの寿命のあいだ不変の
    /// ため、`Mutex` 越しに毎回取得し直す必要はない）。
    fn spawn_with_threshold(child: Arc<Mutex<Child>>, pid: u32, threshold_bytes: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let killed_rss_bytes = Arc::new(Mutex::new(None));
        let stop_for_thread = Arc::clone(&stop);
        let killed_for_thread = Arc::clone(&killed_rss_bytes);

        let thread = std::thread::spawn(move || {
            // `stop` の確認は `RSS_POLL_INTERVAL` より高い頻度で行い、
            // `Drop` 時の停止を早める（厳密な間隔でなくてよい）。
            const STOP_CHECK_GRANULARITY: Duration = Duration::from_millis(10);
            loop {
                let mut waited = Duration::ZERO;
                while waited < super::resource_limits::RSS_POLL_INTERVAL {
                    if stop_for_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    let step = STOP_CHECK_GRANULARITY
                        .min(super::resource_limits::RSS_POLL_INTERVAL - waited);
                    std::thread::sleep(step);
                    waited += step;
                }
                if stop_for_thread.load(Ordering::Relaxed) {
                    return;
                }

                // RSS の取得に失敗した場合は fail-open で次のポーリングへ
                // 進む（`read_child_rss_bytes` のドキュメントコメント
                // 参照。監視は多層防御の 1 つであり、一時的な取得失敗の
                // たびに子を kill すると正常な評価まで巻き込む）。
                let Some(rss_bytes) = super::resource_limits::read_child_rss_bytes(pid) else {
                    continue;
                };
                if rss_bytes <= threshold_bytes {
                    continue;
                }

                // 状態の記録を kill より先に行う（advisor 指摘。`kill`
                // すると reader スレッドがほぼ即座に `Eof` を検出しうる
                // ため、先に kill してしまうと `send_evaluate_and_await`
                // 側が `take_killed_rss` を確認する前に `Eof` へ到達し、
                // 「なぜ落ちたか」の理由を stderr ヒューリスティックの
                // 誤判定に譲ってしまう競合が起きる。「この RSS だから
                // kill を決めた」という順序のほうが実態にも即している）。
                if let Ok(mut killed) = killed_for_thread.lock() {
                    *killed = Some(rss_bytes);
                }
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                }
                // 子は終了させたので、この監視スレッドの役目は終わる。
                return;
            }
        });

        Self {
            stop,
            killed_rss_bytes,
            thread: Some(thread),
        }
    }

    /// 監視スレッドが記録した「RSS 超過で `kill` した」状態を取り出す
    /// （取り出すと消費され、以後は `None` になる。呼び出し元がこの値を
    /// 元にエラーへ変換した後、別の呼び出し元が同じ状態を二重に消費して
    /// 矛盾したメッセージを出さないようにするため）。
    fn take_killed_rss(&self) -> Option<u64> {
        self.killed_rss_bytes
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// 監視スレッドに停止を伝え、終了を待つ（`WorkerHandle::drop` から
    /// 呼ぶ。子の寿命を超えて監視スレッドが残らないことを保証する）。
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// 1 つの子プロセスと、そのやり取りに必要な状態一式。
struct WorkerHandle {
    /// `Mutex` で包む理由: [`MemoryMonitor`] がこの子を独立したスレッド
    /// から `kill` できる必要があるため（codex・Bugbot レビュー指摘
    /// #503 P0 対応。評価呼び出しの有無に関わらず監視・強制終了できる
    /// ようにする）。
    child: Arc<Mutex<Child>>,
    /// `Child::id()` は生成時から不変のため、`Mutex` 越しに毎回取得し
    /// 直さずに済むようここへコピーしておく。
    pid: u32,
    /// `Evaluate` フレームの書き込みを担当する writer スレッドへの
    /// チャネル。`None` にする（`Drop` の段階 1）ことで writer スレッド
    /// へ「もう書き込みは無い」ことを伝える（codex レビュー指摘 #503 P1
    /// 対応）。
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// writer スレッドからの書き込み完了通知（`Ok(())`）・失敗
    /// （`Err(理由)`）を受け取る。
    write_ack_rx: mpsc::Receiver<Result<(), String>>,
    writer_thread: Option<std::thread::JoinHandle<()>>,
    frame_rx: mpsc::Receiver<ReaderEvent>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    memory_monitor: MemoryMonitor,
}

impl WorkerHandle {
    /// 既に spawn 済みの `Child`（stdin/stdout/stderr を pipe で確保して
    /// いること）から `WorkerHandle` を組み立てる（writer・reader・
    /// stderr drain・メモリ監視の各スレッドを起動する）。
    ///
    /// `spawn_worker`（本番。自己再実行した V8 ワーカー）と、
    /// `tests` モジュールの単体テスト（`sleep` 等の代役プロセスを使い、
    /// V8 プロトコルに依存しない書き込みタイムアウト・reap タイムアウト
    /// の挙動だけを検証する）の両方から使う共通の組み立てロジック
    /// （codex・Cursor Bugbot レビュー指摘 #503 の回帰テストのため、
    /// 本番と同じスレッド構成を単体テストでも使えるようにする）。
    fn from_child(mut child: Child) -> Result<Self, JsEngineError> {
        let stdin = child.stdin.take().ok_or_else(|| {
            JsEngineError::EngineUnavailable(
                "JS worker process was spawned without a stdin pipe".to_string(),
            )
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            JsEngineError::EngineUnavailable(
                "JS worker process was spawned without a stdout pipe".to_string(),
            )
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            JsEngineError::EngineUnavailable(
                "JS worker process was spawned without a stderr pipe".to_string(),
            )
        })?;

        let pid = child.id();
        let child = Arc::new(Mutex::new(child));

        // writer スレッド（codex レビュー指摘 #503 P1 対応）: 書き込み・
        // flush をこのスレッドへ委譲し、呼び出しスレッドは
        // `write_ack_rx` を期限付きで待つだけにする。
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let (write_ack_tx, write_ack_rx) = mpsc::channel::<Result<(), String>>();
        let writer_thread = std::thread::spawn(move || {
            let mut stdin = stdin;
            while let Ok(bytes) = stdin_rx.recv() {
                let result = stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .map_err(|err| err.to_string());
                if write_ack_tx.send(result).is_err() {
                    return;
                }
            }
            // 送信側（`stdin_tx`）が drop された（`WorkerHandle::drop`
            // の段階 1）。ループを抜けると `stdin`（`ChildStdin`）が
            // ここで drop され、パイプの書き込み端が閉じる。
        });

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

        let memory_monitor = MemoryMonitor::spawn(Arc::clone(&child), pid);

        Ok(WorkerHandle {
            child,
            pid,
            stdin_tx: Some(stdin_tx),
            write_ack_rx,
            writer_thread: Some(writer_thread),
            frame_rx,
            reader_thread: Some(reader_thread),
            stderr_tail,
            stderr_thread: Some(stderr_thread),
            memory_monitor,
        })
    }

    /// stdin を閉じずに即座に kill して wait する（ハンドシェイク失敗・
    /// プロトコル違反時の後始末。`Drop` の穏やかな手順とは別に、失敗が
    /// 確定した時点で速やかに資源を解放するために使う）。
    fn terminate_now(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// stderr の末尾（現時点までに読み取れた分）をスナップショットする。
    fn stderr_tail_snapshot(&self) -> String {
        match self.stderr_tail.lock() {
            Ok(tail) => String::from_utf8_lossy(&tail).into_owned(),
            Err(_) => String::new(),
        }
    }

    /// 子の終了を確定させ（期限付きの `try_wait` ポーリング。期限内に
    /// 終了しなければ `kill` してから `wait` する。codex レビュー指摘
    /// #503 P1 対応）、stderr drain スレッドが EOF まで読み切るのを
    /// 待って（`join`）から、stderr の末尾をスナップショットする。
    ///
    /// **`wait` → `join` → スナップショットの順序が必須**: `wait` が
    /// 返った時点では子の stderr 書き込み端は閉じているが、drain
    /// スレッドがまだ最後のチャンク（`Fatal JavaScript out of memory`
    /// を含みうる）を読み切っているとは限らない。`join` の前に
    /// スナップショットすると、負荷の高い CI 環境では fatal メッセージを
    /// 取りこぼし、`ResourceLimitExceeded` になるべきところが
    /// `EngineUnavailable` に誤判定されうる。
    ///
    /// 呼び出し前提: 子は既に終了しているか、これから終了する（`wait`
    /// 自体が終了を待つ）。呼び出し元がこの前提を保証する（例:
    /// `Ok(ReaderEvent::Eof)` は子がストリームを閉じた合図であり、
    /// 通常は既に終了しているか、まもなく終了する）。前提が崩れていても
    /// [`REAP_WAIT_TIMEOUT`] で `kill` にフォールバックするため、
    /// 無期限に待ち続けることはない。
    fn reap_and_collect_stderr(&mut self) -> String {
        if let Ok(mut child) = self.child.lock() {
            let deadline = Instant::now() + REAP_WAIT_TIMEOUT;
            let mut has_exited = false;
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        has_exited = true;
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            if !has_exited {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        self.stderr_tail_snapshot()
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
    /// 手順:
    /// 0. メモリ監視スレッドを止める（codex・Bugbot レビュー指摘 #503 P0
    ///    対応。子の寿命を超えて監視スレッドが残らないようにする）。
    /// 1. stdin への送信チャネルを drop する（writer スレッドへ「もう
    ///    書き込みは無い」ことを伝える。codex レビュー指摘 #503 P1
    ///    対応で書き込みを専任スレッドへ移したことに伴う変更）。
    /// 2. 1 秒だけ `try_wait` で待つ。
    /// 3. 終わらなければ `kill` する。
    /// 4. 必ず `wait` する（scratch コンテナで本体が PID 1 になる場合に
    ///    ゾンビを残さない）。
    /// 5. writer・reader・stderr スレッドの終了を待つ。
    ///
    /// **段階 2〜4（`kill` を含む）を、writer スレッドの `join`
    /// （旧実装）より先に行う**（advisor レビュー指摘 C 対応）:
    /// writer スレッドが `write_all` でブロックしている場合、段階 1 で
    /// 送信チャネルを drop するだけでは書き込みは解放されない
    /// （`ChildStdin` の実体は writer スレッドのクロージャの中にあり、
    /// 送信側チャネルの drop だけではまだ writer スレッドはループを
    /// 抜けられていない）。子を `kill` して初めて、ブロックしていた
    /// 書き込みが broken pipe で解放され、writer スレッドがループを
    /// 抜けられる。先に `join` してしまうと、writer スレッドがブロック
    /// したままの場合に `Drop` 自体が無期限に止まりうる。
    fn drop(&mut self) {
        self.memory_monitor.stop_and_join();

        self.stdin_tx = None;

        if let Ok(mut child) = self.child.lock() {
            let deadline = Instant::now() + GRACEFUL_SHUTDOWN_TIMEOUT;
            let mut has_exited = false;
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        has_exited = true;
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            if !has_exited {
                let _ = child.kill();
            }
            let _ = child.wait();
        }

        // writer・reader・stderr スレッドの後始末（best effort）。子が
        // 終了しているため、stdin への書き込みは broken pipe で失敗し、
        // stdout/stderr は EOF に達しており、いずれのスレッドも自然に
        // 終了できるはずである。
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

/// テスト専用: 子プロセスの起動条件を上書きする（Issue #503 設計書
/// §7 W6「テスト用に小さいヒープ上限を渡す経路（テスト専用。本番では
/// 無効）」・「ハンドシェイク失敗」の検証に使う）。
///
/// 本番の [`V8ProcessEngine::new`] は常に既定値（`Default::default()`。
/// 本番のヒープ上限・プロトコルバージョン）を使う。この構造体は
/// `tests/v8_worker.rs`（`harness = false`。W6）が
/// [`V8ProcessEngine::new_for_test`] 経由でのみ使う想定であり、本 crate の
/// 他のコードは触らない。
///
/// `#[doc(hidden)]` を付けたうえで `pub` にする理由: `tests/v8_worker.rs`
/// は結合テスト（別クレートとしてコンパイルされる）であり、`pub(crate)`
/// の項目を参照できない。テスト専用の入口だけを最小限 `pub` にする
/// （[coding-rust.md] の「JS エンジンはトレイト抽象越しに使い、V8 の
/// 具象型を上位 crate へ漏らさない」という制約は、`V8ProcessEngine` が
/// `Child`・パイプ・チャネルしか保持せず `v8` crate の型を一切参照しない
/// ため抵触しない）。
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct WorkerSpawnConfigForTest {
    /// 子へ `super::worker::TEST_HEAP_LIMIT_ENV_VAR` として渡すヒープ
    /// 上限（バイト）。`None` の場合は渡さない（本番と同じ既定値になる）。
    ///
    /// **下げることしかできない**（codex レビュー指摘 #503 P0 対応）:
    /// 本番の既定値（[`super::v8_engine::MAX_ISOLATE_HEAP_BYTES`]。128 MiB）
    /// を上回る値を指定しても、[`super::v8_engine::clamp_test_heap_limit_bytes`]
    /// により既定値へクランプされる（`spawn_worker` が環境変数を組み立てる
    /// 直前に適用する）。下限側も同関数がクランプする（`0` や極端に小さい
    /// 値を渡しても、実用上動作する最小値まで引き上げられる。詳細は同関数の
    /// ドキュメントコメント参照）。子プロセス側（`super::worker`）でも
    /// 同じクランプを独立に適用しており、親を経由しない直接起動に対する
    /// 防御になっている（多層防御）。
    pub heap_limit_bytes: Option<usize>,
    /// 子へ渡すプロトコルバージョンの上書き値（ハンドシェイク失敗を
    /// 決定的に再現するためのテスト専用経路）。`None` の場合は
    /// [`worker_protocol::PROTOCOL_VERSION`] を使う。
    pub protocol_version_override: Option<u16>,
}

/// JS 評価を子プロセスへ分離して提供するエンジン（`JS-1`・Issue #503）。
///
/// 子・パイプしか保持しないため `Send` にできる（設計書 §3.2。将来
/// core・tokio と統合するときに有利になる）。
///
/// `#[doc(hidden)] pub` である理由は [`WorkerSpawnConfigForTest`] の
/// ドキュメントコメントを参照。`TASK-29.6`（Issue #157）で
/// `create_engine` から配線し、`impl JsEngine for V8ProcessEngine` を
/// 追加する（本 Issue の時点では inherent メソッドに留める）。
#[doc(hidden)]
pub struct V8ProcessEngine {
    worker: Option<WorkerHandle>,
    spawn_config: WorkerSpawnConfigForTest,
}

impl V8ProcessEngine {
    /// 子プロセスを遅延生成する構成で初期化する（設計書 §3.1「遅延
    /// 起動」。PERF-6・PERF-7・CORE-3 を守るため、生成時点ではまだ子を
    /// 起動しない）。常に本番の既定値（ヒープ上限・プロトコルバージョン）
    /// を使う。
    #[doc(hidden)]
    pub fn new() -> Self {
        Self {
            worker: None,
            spawn_config: WorkerSpawnConfigForTest::default(),
        }
    }

    /// [`V8ProcessEngine::new`] と同じだが、[`WorkerSpawnConfigForTest`]
    /// で子プロセスの起動条件を上書きできる（テスト専用。W6）。
    #[doc(hidden)]
    pub fn new_for_test(spawn_config: WorkerSpawnConfigForTest) -> Self {
        Self {
            worker: None,
            spawn_config,
        }
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
    ///
    /// 子を再利用する前に、メモリ監視スレッド（[`MemoryMonitor`]）が
    /// 待機中（前回の評価の後、今回の呼び出しまでのあいだ）に RSS 超過を
    /// 検出して既に `kill` していないかを確認する（codex・Cursor Bugbot
    /// レビュー指摘 #503 P0「評価と評価の間も監視されていない」対応）。
    /// 検出していれば、その子は使わず [`JsEngineError::ResourceLimitExceeded`]
    /// を返し、次回の呼び出しで新しい子を起動し直す。
    ///
    /// `_options` は現時点で未使用（[`EvaluateOptions`] は空構造体。
    /// `engine_trait.rs` 参照）。`TASK-29.6` で `impl JsEngine` へ集約する
    /// 際、トレイトのシグネチャとそのまま揃えるために受け取っておく。
    #[doc(hidden)]
    pub fn evaluate_script(
        &mut self,
        script: &str,
        _options: &EvaluateOptions,
    ) -> Result<JsValue, JsEngineError> {
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
            None => self.spawn_worker()?,
        };

        // codex・Bugbot レビュー指摘 #503 P0: 前回の評価から今回の
        // 呼び出しまでのあいだ（待機中）に、監視スレッドが RSS 超過で
        // 既に `kill` していないかを確認する。
        if let Some(rss_bytes) = worker.memory_monitor.take_killed_rss() {
            let tail = worker.reap_and_collect_stderr();
            // `worker` はここで drop され（`self.worker` へ戻さない）、
            // 次回の呼び出しで新しい子を起動し直す。
            return Err(JsEngineError::ResourceLimitExceeded(format!(
                "JS worker process RSS ({rss_bytes} bytes) exceeded the parent's monitoring \
                 threshold ({} bytes) while idle between evaluations; context was discarded; \
                 stderr: {tail}",
                super::resource_limits::MAX_CHILD_RSS_BYTES
            )));
        }

        let (outcome, keep_worker) = Self::send_evaluate_and_await(&mut worker, script);
        if keep_worker {
            self.worker = Some(worker);
        }
        // `keep_worker` が `false` の場合は `worker` をここで drop し、
        // `WorkerHandle::drop` の手順で後始末する。次回の
        // `evaluate_script` 呼び出しでは `self.worker` が `None` のため
        // 新しい子を起動し直す。
        outcome
    }

    /// テスト専用: 子プロセスの stdin へ生バイト列をそのまま書き込む
    /// （プロトコル違反を注入する結合テスト用。設計書 §7 W6）。子が
    /// 未起動なら、この呼び出しで起動する（[`Self::evaluate_script`] と
    /// 同じ遅延起動の作法）。書き込みは本番経路と同じく writer スレッド
    /// 経由で行い、[`HANDSHAKE_TIMEOUT`] を上限に完了を待つ。
    #[doc(hidden)]
    pub fn send_raw_frame_for_test(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.worker.is_none() {
            self.worker = Some(self.spawn_worker().map_err(|err| {
                std::io::Error::other(format!(
                    "failed to spawn the JS worker process for a test: {err}"
                ))
            })?);
        }
        // `expect` ではなく `if let` で取り出す: 直前で `Some` を保証
        // しているが、外部入力を扱う経路の作法（unwrap/expect を避ける。
        // coding-rust.md）に合わせて明示的に処理する。
        let Some(worker) = self.worker.as_mut() else {
            return Err(std::io::Error::other(
                "JS worker process is unexpectedly absent after spawning",
            ));
        };
        let Some(stdin_tx) = worker.stdin_tx.as_ref() else {
            return Err(std::io::Error::other(
                "JS worker process stdin is already closed",
            ));
        };
        stdin_tx.send(bytes.to_vec()).map_err(|_| {
            std::io::Error::other("JS worker process writer thread is no longer running")
        })?;
        match worker.write_ack_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(std::io::Error::other(err)),
            Err(_) => Err(std::io::Error::other(
                "timed out waiting for the JS worker process writer thread to acknowledge the \
                 write",
            )),
        }
    }

    /// テスト専用: 現在保持している子プロセスの PID を返す（子が未起動
    /// なら `None`）。codex レビュー指摘 #503 P0「ヒープ外メモリが無制限」
    /// 対応の回帰テストが、Linux で `/proc/<pid>/limits` から実際に
    /// `RLIMIT_DATA` が設定されていることを確認するために使う
    /// （`super::resource_limits` のドキュメントコメント参照）。
    #[doc(hidden)]
    pub fn worker_pid_for_test(&self) -> Option<u32> {
        self.worker.as_ref().map(|worker| worker.pid)
    }

    /// 子プロセスを起動し、ハンドシェイク（`Hello` の受信）まで完了させる。
    fn spawn_worker(&self) -> Result<WorkerHandle, JsEngineError> {
        // 設計書 §3.1「再帰の防止」: 自分自身が既にワーカーとして起動
        // されている場合は、さらに子を起動しない。通常の cli 経路では
        // `run_js_worker_if_requested` がワーカーモードのまま `main` を
        // 続けさせることはないが、ライブラリとして誤って組み込まれた
        // 場合の多重防御として確認する。
        if std::env::var(worker_protocol::MARKER_ENV_VAR).is_ok() {
            return Err(JsEngineError::EngineUnavailable(
                "refusing to spawn a nested JS worker process: this process is itself running \
                 as a worker"
                    .to_string(),
            ));
        }

        let exe = Self::resolve_worker_exe();
        let mut command = Command::new(&exe);
        // 秘密情報を含みうる環境変数を子へ渡さない（設計書 §3.4）。
        command.env_clear();
        let protocol_version = self
            .spawn_config
            .protocol_version_override
            .unwrap_or(worker_protocol::PROTOCOL_VERSION);
        command.env(
            worker_protocol::MARKER_ENV_VAR,
            protocol_version.to_string(),
        );
        if let Some(heap_limit_bytes) = self.spawn_config.heap_limit_bytes {
            // テスト専用（Issue #503 設計書 §7 W6）。本番の `new()` は
            // `spawn_config.heap_limit_bytes` が常に `None` のため、この
            // 環境変数は本番の子プロセスには渡らない。
            //
            // 本番の既定値を超える値を子へ渡せないよう、環境変数を
            // 組み立てる直前にクランプする（`WorkerSpawnConfigForTest::
            // heap_limit_bytes` のドキュメントコメント参照。codex レビュー
            // 指摘 #503 P0 対応）。
            let clamped_heap_limit_bytes =
                super::v8_engine::clamp_test_heap_limit_bytes(heap_limit_bytes);
            command.env(
                super::worker::TEST_HEAP_LIMIT_ENV_VAR,
                clamped_heap_limit_bytes.to_string(),
            );
        }
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

        let child = command.spawn().map_err(|err| {
            JsEngineError::EngineUnavailable(format!(
                "failed to spawn the JS worker process ({}): {err}",
                exe.display()
            ))
        })?;

        let mut worker = WorkerHandle::from_child(child)?;

        match worker.frame_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::HELLO => {
                match worker_protocol::decode_hello(&payload) {
                    Ok((version, _engine)) if version == worker_protocol::PROTOCOL_VERSION => {
                        Ok(worker)
                    }
                    Ok((version, _engine)) => {
                        worker.terminate_now();
                        Err(JsEngineError::EngineUnavailable(format!(
                            "JS worker process reported unsupported protocol version {version}"
                        )))
                    }
                    Err(err) => {
                        worker.terminate_now();
                        Err(JsEngineError::EngineUnavailable(format!(
                            "JS worker process sent a malformed Hello frame: {err}"
                        )))
                    }
                }
            }
            Ok(ReaderEvent::Frame(other_tag, _)) => {
                worker.terminate_now();
                Err(JsEngineError::EngineUnavailable(format!(
                    "JS worker process sent an unexpected frame (tag {other_tag}) before Hello"
                )))
            }
            Ok(ReaderEvent::Eof) => {
                // 子は既にストリームを閉じている（終了済みか、まもなく
                // 終了する）ため、kill を挟まずに直接 reap する
                // （`reap_and_collect_stderr` のドキュメントコメント
                // 「wait → join → スナップショットの順序が必須」参照）。
                let tail = worker.reap_and_collect_stderr();
                Err(JsEngineError::EngineUnavailable(format!(
                    "JS worker process exited before completing the handshake; stderr: {tail}"
                )))
            }
            Ok(ReaderEvent::Invalid(desc)) => {
                worker.terminate_now();
                Err(JsEngineError::EngineUnavailable(format!(
                    "JS worker process sent a malformed frame during the handshake: {desc}"
                )))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.terminate_now();
                Err(JsEngineError::EngineUnavailable(
                    "JS worker process handshake timed out".to_string(),
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                Err(JsEngineError::EngineUnavailable(
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
    ///
    /// 書き込み・応答待ちの両方を、評価開始時に 1 度だけ計算する
    /// 期限（[`EVALUATE_RECV_TIMEOUT`]）で管理する（codex レビュー指摘
    /// #503 P1「書き込みが呼び出しスレッドで同期的に実行され、パイプが
    /// 満杯だと期限も kill も効かない」対応。書き込み自体は writer
    /// スレッドへ委譲し、このスレッドは `write_ack_rx` を期限付きで
    /// 待つだけにする）。
    fn send_evaluate_and_await(
        worker: &mut WorkerHandle,
        script: &str,
    ) -> (Result<JsValue, JsEngineError>, bool) {
        let deadline = Instant::now() + EVALUATE_RECV_TIMEOUT;

        let payload = worker_protocol::encode_evaluate(script);
        let mut frame_bytes = Vec::new();
        // インメモリの `Vec<u8>` への書き込みは実質失敗しないが、
        // `write_frame` のシグネチャ（`impl Write`）に合わせて `Result`
        // を扱う（`coding-rust.md`「外部入力」節。ここでの `payload` は
        // 呼び出し元のスクリプト文字列に由来する外部入力である）。
        if let Err(err) = worker_protocol::write_frame(&mut frame_bytes, tag::EVALUATE, &payload) {
            return (
                Err(JsEngineError::EngineUnavailable(format!(
                    "failed to encode the Evaluate frame: {err}"
                ))),
                true,
            );
        }

        let Some(stdin_tx) = worker.stdin_tx.as_ref() else {
            worker.terminate_now();
            let tail = worker.reap_and_collect_stderr();
            let err = resource_limit_or_else(worker, &tail, || {
                JsEngineError::EngineUnavailable(format!(
                    "JS worker process stdin is already closed; stderr: {tail}"
                ))
            });
            return (Err(err), false);
        };
        if stdin_tx.send(frame_bytes).is_err() {
            // writer スレッドが既に終了している（stdin が既に閉じている
            // 等）。子は既に死んでいる可能性が高い。監視スレッドが RSS
            // 超過で kill した直後（stdin 側が先に閉じる）である可能性も
            // あるため、そちらを優先して判定する（advisor 指摘 B 対応）。
            worker.terminate_now();
            let tail = worker.reap_and_collect_stderr();
            let err = resource_limit_or_else(worker, &tail, || {
                JsEngineError::EngineUnavailable(format!(
                    "JS worker process writer thread is no longer running; stderr: {tail}"
                ))
            });
            return (Err(err), false);
        }

        let write_wait = deadline.saturating_duration_since(Instant::now());
        match worker.write_ack_rx.recv_timeout(write_wait) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                // 子は既に死んでいる可能性が高い（broken pipe 等）。kill
                // は冪等（既に終了したプロセスへの kill はエラーを返す
                // だけで副作用は無い）ため、まず kill してから reap する
                // （`reap_and_collect_stderr` の順序前提を満たす）。
                //
                // このブロークンパイプは、監視スレッドが RSS 超過を検出
                // して子を kill した直後（子の stdin 読み取り端が閉じ、
                // writer スレッドの書き込みが失敗する）にも起こりうる。
                // その場合は原因を正しく `ResourceLimitExceeded` に帰属
                // させる（advisor 指摘 B 対応。書き込み中に kill される
                // タイミングでも `EngineUnavailable` に誤判定しない）。
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let converted = resource_limit_or_else(worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "failed to send the script to the JS worker process (it may have \
                         crashed): {err}; stderr: {tail}; context was discarded"
                    ))
                });
                return (Err(converted), false);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 書き込みが期限内に終わらなかった（パイプが満杯で
                // writer スレッドがブロックしている等。codex レビュー
                // 指摘 #503 P1 対応）。kill すれば子の stdin 読み取り端が
                // 消え、ブロックしていた書き込みは broken pipe で解放
                // される（writer スレッドは `WorkerHandle::drop` で
                // 後始末される）。
                worker.terminate_now();
                return (
                    Err(JsEngineError::Timeout(
                        "sending the script to the JS worker process exceeded the deadline and \
                         the process was killed; context was discarded; the next evaluation \
                         runs in a fresh context"
                            .to_string(),
                    )),
                    false,
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = resource_limit_or_else(worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process writer thread disconnected unexpectedly; stderr: \
                         {tail}"
                    ))
                });
                return (Err(err), false);
            }
        }

        let recv_wait = deadline.saturating_duration_since(Instant::now());
        match worker.frame_rx.recv_timeout(recv_wait) {
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::RESULT => {
                let (outcome, keep) = match worker_protocol::decode_js_value(&payload) {
                    Ok((value, _consumed)) => (Ok(value), true),
                    Err(err) => {
                        worker.terminate_now();
                        (
                            Err(JsEngineError::EngineUnavailable(format!(
                                "JS worker process sent a malformed Result frame: {err}; context \
                                 was discarded"
                            ))),
                            false,
                        )
                    }
                };
                apply_post_response_rss_check(worker, outcome, keep)
            }
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::ERROR => {
                let (outcome, keep) = match worker_protocol::decode_error(&payload) {
                    // 子の watchdog による打ち切り。子プロセスは生き続け、
                    // Context も残る（設計書 §3.3 の表の 1 行目）。
                    Ok((ErrorKind::Timeout, message)) => (
                        Err(JsEngineError::Timeout(format!(
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
                            Err(JsEngineError::EngineUnavailable(format!(
                                "JS worker process sent a malformed Error frame: {err}; context \
                                 was discarded"
                            ))),
                            false,
                        )
                    }
                };
                apply_post_response_rss_check(worker, outcome, keep)
            }
            Ok(ReaderEvent::Frame(other_tag, _)) => {
                worker.terminate_now();
                (
                    Err(JsEngineError::EngineUnavailable(format!(
                        "JS worker process sent an unexpected frame (tag {other_tag}); context \
                         was discarded"
                    ))),
                    false,
                )
            }
            Ok(ReaderEvent::Eof) => {
                // 子は既にストリームを閉じている（終了済みか、まもなく
                // 終了する）ため、kill を挟まずに直接 reap する。
                let tail = worker.reap_and_collect_stderr();
                let discarded_note =
                    "context was discarded; the next evaluation runs in a fresh context";
                if let Some(rss_bytes) = worker.memory_monitor.take_killed_rss() {
                    // メモリ監視スレッドが RSS 超過を検出して kill した
                    // ことが判明した（codex・Bugbot レビュー指摘 #503 P0
                    // 対応）。stderr のヒューリスティックより優先する
                    // （`kill` はシグナルであり、V8 の fatal ハンドラが
                    // 走るとは限らないため、stderr に手掛かりが残らない
                    // ことがある）。
                    (
                        Err(JsEngineError::ResourceLimitExceeded(format!(
                            "JS worker process RSS ({rss_bytes} bytes) exceeded the parent's \
                             monitoring threshold ({} bytes) and was killed; {discarded_note}; \
                             stderr: {tail}",
                            super::resource_limits::MAX_CHILD_RSS_BYTES
                        ))),
                        false,
                    )
                } else if WorkerHandle::stderr_indicates_oom(&tail) {
                    (
                        Err(JsEngineError::ResourceLimitExceeded(format!(
                            "script execution exceeded the isolate heap limit and the JS worker \
                             process terminated; {discarded_note}; stderr: {tail}"
                        ))),
                        false,
                    )
                } else {
                    (
                        Err(JsEngineError::EngineUnavailable(format!(
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
                    Err(JsEngineError::EngineUnavailable(format!(
                        "JS worker process sent a malformed frame: {desc}; context was discarded"
                    ))),
                    false,
                )
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.terminate_now();
                (
                    Err(JsEngineError::Timeout(
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
                    Err(JsEngineError::EngineUnavailable(
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

/// `worker`（既に `terminate_now` 済みで、`tail` は `reap_and_collect_stderr`
/// の結果）について、メモリ監視スレッドが RSS 超過で `kill` していた
/// ことが判明した場合は `ResourceLimitExceeded` を、そうでなければ
/// `fallback()` の結果をそのまま返す（advisor レビュー指摘 B 対応:
/// 書き込み失敗・writer スレッドの切断が、実は監視スレッドの `kill` に
/// よる broken pipe だった場合に `EngineUnavailable` へ誤帰属させない。
/// 書き込み中に `kill` されるタイミングでも `ResourceLimitExceeded` に
/// 正しく分類できるようにする）。
fn resource_limit_or_else(
    worker: &WorkerHandle,
    tail: &str,
    fallback: impl FnOnce() -> JsEngineError,
) -> JsEngineError {
    match worker.memory_monitor.take_killed_rss() {
        Some(rss_bytes) => JsEngineError::ResourceLimitExceeded(format!(
            "JS worker process RSS ({rss_bytes} bytes) exceeded the parent's monitoring \
             threshold ({} bytes) and was killed; context was discarded; the next evaluation \
             runs in a fresh context; stderr: {tail}",
            super::resource_limits::MAX_CHILD_RSS_BYTES
        )),
        None => fallback(),
    }
}

/// 応答（`Result`/`Error`）を受け取った直後、子を保持し続ける予定
/// （`keep_worker == true`）の場合にだけ、同期的に RSS を確認する
/// （codex・Cursor Bugbot レビュー指摘 #503 P0「応答直後にも確認する」
/// 対応）。上限を超えていれば、せっかく得られた応答を握りつぶして
/// `ResourceLimitExceeded` へ差し替え、子を `kill` して次回は新しい子を
/// 使わせる。
///
/// `keep_worker == false` の場合（既にプロトコル違反等で `kill` 済み）は
/// 何もしない（二重に `kill` を試みても副作用は無いが、意味のある
/// チェックにならないため早期に返す）。
fn apply_post_response_rss_check(
    worker: &mut WorkerHandle,
    outcome: Result<JsValue, JsEngineError>,
    keep_worker: bool,
) -> (Result<JsValue, JsEngineError>, bool) {
    if !keep_worker {
        return (outcome, keep_worker);
    }
    let Some(rss_bytes) = super::resource_limits::read_child_rss_bytes(worker.pid) else {
        return (outcome, keep_worker);
    };
    if rss_bytes <= super::resource_limits::MAX_CHILD_RSS_BYTES {
        return (outcome, keep_worker);
    }

    worker.terminate_now();
    // メモリ監視スレッドがまだ気づいていない可能性があるため、状態を
    // 消費しておく（次回の「監視スレッドが kill 済みか」チェックが
    // 既に破棄した子について二重にエラーメッセージを出さないように
    // するため。子は既に `terminate_now` で終了させているため、監視
    // スレッドが独自に検出しても実害は無い）。
    let _ = worker.memory_monitor.take_killed_rss();
    let tail = worker.reap_and_collect_stderr();
    (
        Err(JsEngineError::ResourceLimitExceeded(format!(
            "JS worker process RSS ({rss_bytes} bytes) exceeded the parent's monitoring \
             threshold ({} bytes) immediately after responding; context was discarded; stderr: \
             {tail}",
            super::resource_limits::MAX_CHILD_RSS_BYTES
        ))),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト専用: `sleep`（Unix）／`ping`（Windows）を、V8 プロトコルに
    /// 依存しない代役の「子プロセス」として起動する。書き込みタイムアウト
    /// ・reap タイムアウト・メモリ監視のように、V8 の Hello/Evaluate
    /// フレームに依存しない挙動だけを検証したいテストで使う。
    fn spawn_long_lived_child_for_test() -> Child {
        #[cfg(unix)]
        {
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn `sleep` for the test")
        }
        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", "ping", "-n", "31", "127.0.0.1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn a long-lived process for the test")
        }
    }

    /// codex・Cursor Bugbot レビュー指摘 #503 P0「RSS の確認が
    /// `recv_timeout` の時間切れ分岐でしか行われておらず、評価をして
    /// いない待機中はすり抜ける」の単体テスト: `MemoryMonitor` は
    /// 評価呼び出しが一切無くても（`sleep` を代役の「子」に見立て、
    /// `evaluate_script` を一度も呼ばない）、生成直後から継続的に RSS を
    /// 確認し続け、しきい値を超えたら子を `kill` してその事実を記録
    /// すること。しきい値には「代役プロセスの通常の RSS が確実に超える」
    /// 極端に小さい値（1 バイト）を使い、実際のメモリを膨らませずに
    /// 決定的に再現する。
    #[test]
    fn js_1_memory_monitor_kills_and_records_state_while_idle() {
        let child = spawn_long_lived_child_for_test();
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));

        let mut monitor = MemoryMonitor::spawn_with_threshold(Arc::clone(&child), pid, 1);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut killed_rss = None;
        while Instant::now() < deadline {
            if let Some(rss) = monitor.take_killed_rss() {
                killed_rss = Some(rss);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            killed_rss.is_some_and(|rss| rss > 0),
            "expected the memory monitor to kill the idle child (no evaluation ever ran) and \
             record a nonzero RSS, got: {killed_rss:?}"
        );

        let mut guard = child.lock().expect("child mutex must not be poisoned");
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < wait_deadline {
            if matches!(guard.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            exited,
            "expected the idle child to have exited after the memory monitor killed it"
        );
        drop(guard);

        monitor.stop_and_join();
    }

    /// codex レビュー指摘 #503 P1「`write_frame`・`flush` が呼び出し
    /// スレッドで同期的に実行され、パイプが満杯だと期限も `kill` も
    /// 効かない」の単体テスト: stdin を一切読まない子（`sleep`）へ、
    /// OS のパイプバッファを確実に超える量のデータを書き込もうとすると、
    /// 短い期限内に `kill` されて `write_ack_rx` がタイムアウトし、子が
    /// 実際に終了すること。
    #[test]
    fn js_1_write_to_a_stalled_child_is_bounded_by_a_deadline_and_kills_the_child() {
        let child = spawn_long_lived_child_for_test();
        let mut worker =
            WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        // OS のパイプバッファ（Linux は既定 64 KiB、macOS はさらに
        // 小さいことが多い）を確実に超える量を書く。`sleep` は stdin を
        // 一切読まないため、writer スレッドはこの書き込みでブロックする
        // はずである。
        const OVERSIZED_PAYLOAD_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
        let stdin_tx = worker
            .stdin_tx
            .as_ref()
            .expect("worker must have a writer thread channel right after construction");
        stdin_tx
            .send(vec![0u8; OVERSIZED_PAYLOAD_BYTES])
            .expect("sending to the writer thread's channel must succeed");

        const TEST_WRITE_DEADLINE: Duration = Duration::from_secs(3);
        match worker.write_ack_rx.recv_timeout(TEST_WRITE_DEADLINE) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 期待どおり: 書き込みが期限内に終わらなかった。本番の
                // `send_evaluate_and_await` と同じ手順で `kill` する。
                worker.terminate_now();
            }
            other => panic!(
                "expected the write to still be blocked after {TEST_WRITE_DEADLINE:?} (so that \
                 a real deadline would trigger a kill), got: {other:?}"
            ),
        }

        let exited = {
            let mut guard = worker
                .child
                .lock()
                .expect("child mutex must not be poisoned");
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut exited = false;
            while Instant::now() < deadline {
                if matches!(guard.try_wait(), Ok(Some(_))) {
                    exited = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            exited
        };
        assert!(
            exited,
            "expected killing the child to unblock the stalled write and terminate the process"
        );
    }

    /// codex レビュー指摘 #503 P1「EOF の後の `reap_and_collect_stderr`
    /// が期限なしの `child.wait()` を呼んでいる」の単体テスト:
    /// `reap_and_collect_stderr` を、まだ生きている子に対して呼んでも
    /// （＝子が「すぐに終了するはず」という前提が崩れていても）、
    /// [`REAP_WAIT_TIMEOUT`] を大きく超えて無期限に待ち続けず、期限を
    /// 過ぎたら `kill` してから返ること。
    #[test]
    fn js_1_reap_and_collect_stderr_has_a_deadline_and_kills_a_still_running_child() {
        let child = spawn_long_lived_child_for_test();
        let mut worker =
            WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        // 呼び出し前提（子は既に終了しているか、まもなく終了する）を
        // あえて満たさない状態（`sleep 30` はまだ何十秒も生き続ける）で
        // 呼び、それでも `REAP_WAIT_TIMEOUT` 程度で戻ってくることを
        // 確認する。
        let started = Instant::now();
        let _ = worker.reap_and_collect_stderr();
        let elapsed = started.elapsed();

        assert!(
            elapsed < REAP_WAIT_TIMEOUT + Duration::from_secs(2),
            "expected reap_and_collect_stderr to return within roughly REAP_WAIT_TIMEOUT \
             ({REAP_WAIT_TIMEOUT:?}) instead of waiting indefinitely, took: {elapsed:?}"
        );

        let mut guard = worker
            .child
            .lock()
            .expect("child mutex must not be poisoned");
        assert!(
            matches!(guard.try_wait(), Ok(Some(_))),
            "expected the still-running child to have been killed by reap_and_collect_stderr's \
             deadline fallback"
        );
    }
}

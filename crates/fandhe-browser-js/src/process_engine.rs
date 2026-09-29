//! JS 評価用の子プロセスへの親側プロキシ（`JS-1`・`TASK-29`・Issue #503
//! 「JS プロセス分離」設計書 §3.2・§3.3・§3.4・§7 W4）。
//!
//! 呼び出し元（将来）: `TASK-29.6`（Issue #157）で `create_engine` から
//! 配線され、`impl JsEngine for V8ProcessEngine` を追加する（本 Issue の
//! 時点では inherent メソッドとして `evaluate_script` を提供するに
//! 留める。設計書 §7「案 X」4）。
//!
//! [`V8ProcessEngine`] は 1 つの子プロセス（`super::worker` を
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
//!   `STDERR_TAIL_CAPACITY_BYTES` バイトをリングバッファへ保持する。
//!   読み続けないとパイプが詰まって子が止まるため必須（設計書 §3.3）
//! - **メモリ監視スレッド**（`MemoryMonitor`）: 子の寿命のあいだ
//!   （評価中か待機中かに関わらず）`super::resource_limits::RSS_POLL_INTERVAL`
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
//! | メモリ監視スレッドが RSS 超過を検出して子を `kill` した（評価中・書き込み中・待機中いずれも） | [`JsEngineError::ResourceLimitExceeded`] | 破棄（メッセージに実測 RSS を明示。書き込み中に `kill` された場合の broken pipe も正しくここへ分類する。`discard_context_error`。codex・Bugbot レビュー指摘 #503 P0/P1 対応） |
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
//!   `super::resource_limits` が子側の OS 別強制（Linux の
//!   `RLIMIT_DATA`・Windows の Job Object working set 上限）と親側の
//!   RSS 監視（`MemoryMonitor`。子の寿命のあいだ継続的に動作する）に
//!   よる多層防御を設けている（codex・Bugbot レビュー指摘 #503 P0
//!   対応）。ただし実測の結果、子側の OS 別強制はいずれも厳密な上限には
//!   ならず（Linux は V8 の `CodeRange` 仮想アドレス予約のため小さい値に
//!   設定できない・Windows は working set の trim にしかならない）、
//!   **Linux・macOS では親側の監視が主たる防衛線、Windows では子側の
//!   OS 強制（`ProcessMemoryLimit`）が主導する**。詳細・実測値は
//!   `super::resource_limits` のドキュメントコメント「OS ごとの強制の
//!   強さ」節を参照
//! - macOS には子のメモリ使用量を OS 側で強制する手段が無く
//!   （`super::resource_limits` の実機検証結果を参照）、親側の RSS 監視
//!   だけに頼る
//! - 応答直後の同期的な RSS 確認（`apply_post_response_rss_check`）は
//!   呼び出しスレッド上で `read_child_rss_bytes` を 1 回だけ呼ぶ。Windows
//!   は `GetProcessMemoryInfo`（psapi.dll の直接呼び出し）を使うため、
//!   PowerShell 起動のようなプロセス生成コストは無い
//! - 子はセキュリティ上のサンドボックスではない。同じユーザー権限で動作し、
//!   seccomp 等も使わない。得られるのはクラッシュ・メモリの資源分離
//!   だけである
//! - stderr の文言（`Fatal JavaScript out of memory`/`Fatal process out
//!   of memory`）による `ResourceLimitExceeded` の判定はヒューリスティック
//!   であり、V8 のバージョン更新でメッセージが変われば壊れうる
//! - `super::v8_engine` の `SCRIPT_EXECUTION_TIMEOUT` のドキュメント
//!   コメントが記す既知の制限（「`v8::Script::compile` 自体は
//!   `terminate_execution` では打ち切られない場合がある」）は、本モジュール
//!   の `EVALUATE_RECV_TIMEOUT` による強制 `kill`（`WorkerHandle::drop`
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

use super::engine_trait::{EngineKind, EvaluateOptions, JsEngineError, JsValue};
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

/// `child`（と `pid`）から、[`super::resource_limits::read_child_rss_bytes`]
/// に渡す [`super::resource_limits::ChildMemoryProbe`] を得る。
///
/// Windows では `std::process::Child` のハンドルを
/// `std::os::windows::io::AsRawHandle` で取得して使う（ユーザー承認
/// 2026-09-28「ハンドルは std の Child から AsRawHandle で得る」）。
/// `Mutex` のロックは値を読み取るあいだだけ保持し、その後すぐ解放する
/// （ハンドルの値そのものは `Child` が生存し続ける限り有効であり、
/// `Child` は呼び出し元の `WorkerHandle` が解放されるまで生存すること
/// を、この関数の呼び出し元が保証する）。Linux・macOS では pid をそのまま
/// 使う（`OpenProcess` 相当の追加のシステムコールが要らない）。
fn child_memory_probe(
    child: &Arc<Mutex<Child>>,
    pid: u32,
) -> Option<super::resource_limits::ChildMemoryProbe> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawHandle;
        // Windows では pid ではなくハンドルを使うため、`pid` 引数は
        // 使わない（Linux・macOS 向けの分岐と同じシグネチャに揃えるため
        // 残している）。
        let _ = pid;
        let guard = child.lock().ok()?;
        Some(guard.as_raw_handle())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = child;
        Some(pid)
    }
}

/// [`MemoryMonitor`] が子のメモリ使用量を取得する際に呼ぶ関数の型
/// （[`MemoryMonitor::spawn_with_threshold_and_probe`] が受け取る。
/// clippy `type_complexity` 対応で型エイリアスに切り出した）。
type MemoryProbeFn = Box<dyn Fn(&Arc<Mutex<Child>>, u32) -> Option<u64> + Send>;

/// [`MemoryMonitor`] が実際に使う、本番のメモリ使用量取得（[`child_memory_probe`]
/// ＋ [`super::resource_limits::read_child_rss_bytes`] の組み合わせ）。
/// 単体テストは、この関数の代わりに常に `None` を返す・N 回目まで
/// `None` を返す等のフェイク実装を注入できる
/// （[`MemoryMonitor::spawn_with_threshold_and_probe`] 参照。codex レビュー
/// 指摘 #503 P0「取得失敗を注入する経路を `#[cfg(test)]` で用意する」
/// 対応。テスト専用の分岐を関数内に増やすのではなく、関数そのものを
/// 差し替え可能にすることで、本番経路のコードを変えずに検証できる）。
fn default_memory_probe(child: &Arc<Mutex<Child>>, pid: u32) -> Option<u64> {
    let probe = child_memory_probe(child, pid)?;
    super::resource_limits::read_child_rss_bytes(probe)
}

/// 監視スレッドが子を `kill` した理由（codex レビュー指摘 #503 P0
/// 「メモリ使用量の取得ができなくなったときは fail-closed にする」対応
/// で、既存の「RSS 超過」に加えて「監視自体が壊れて確認できなくなった」
/// という別の理由を区別できるようにする）。
#[derive(Debug, Clone, Copy)]
enum MonitorKillReason {
    /// メモリ使用量がしきい値を超えたことを実際に確認できた（従来どおり。
    /// 値は観測したバイト数）。
    ResourceLimitExceeded(u64),
    /// メモリ使用量の取得（Linux の `/proc` 読み取り・macOS の `ps`・
    /// Windows の `GetProcessMemoryInfo`）が
    /// [`MAX_CONSECUTIVE_PROBE_FAILURES`] 回連続で失敗した。
    /// **メモリ使用量が実際に超過したかどうかは分からない**（監視その
    /// ものが機能していない状態）。
    ProbeUnavailable { consecutive_failures: u32 },
}

/// メモリ使用量の取得が連続して何回失敗したら fail-closed（子を kill し、
/// 以後の評価をエラーにする）にするか（codex レビュー指摘 #503 P0
/// 「macOS で `ps` を起動できない場合、監視が `None` を返し続けるだけで
/// 上限が事実上なくなる」対応）。
///
/// 1 回だけの失敗で fail-closed にすると、`ps`・`GetProcessMemoryInfo`
/// の一時的な失敗（システム負荷等）で正常な評価まで巻き込んでしまう
/// （[`default_memory_probe`] 呼び出し元のドキュメントコメント
/// 「fail-open」の説明を参照）。3 回連続（[`super::resource_limits::RSS_POLL_INTERVAL`]
/// が既定 75 ミリ秒であるため、合計で最大 225 ミリ秒程度）は、単発の
/// 一時的な失敗を吸収しつつ、`ps` が PATH に無い・実行ファイルが
/// 見つからない等の恒常的な失敗（＝監視が事実上機能しない状態）は
/// 数百ミリ秒以内に検出できる値として選んだ。
const MAX_CONSECUTIVE_PROBE_FAILURES: u32 = 3;

/// 子の寿命のあいだ、評価中か待機中かに関わらずメモリ使用量を継続的に
/// 監視する専任スレッド（codex・Cursor Bugbot レビュー指摘 #503 P0
/// 対応）。
///
/// 呼び出し元（`WorkerHandle`）が [`MemoryMonitor::spawn_with_threshold`] で生成し、
/// [`WorkerHandle::drop`] で必ず [`MemoryMonitor::stop_and_join`] を
/// 呼んで止めようとする。通常は速やかに停止するが、
/// `stop_and_join` は無期限には待たない（期限内に停止しなければ
/// detach する。同メソッドのドキュメントコメント参照）ため、まれに
/// 監視スレッドが子プロセスの `Drop` より後まで生き残ることがある
/// （その場合でも `stop` フラグは既に立っており、次に処理が返って
/// きた時点で必ず終了する）。
struct MemoryMonitor {
    /// 監視スレッドへ停止を伝えるフラグ。
    stop: Arc<AtomicBool>,
    /// 監視スレッドが子を `kill` した場合、その理由を記録する。`None` は
    /// 「まだ kill していない」。呼び出し側は
    /// [`MemoryMonitor::take_kill_reason`] で消費する。
    kill_reason: Arc<Mutex<Option<MonitorKillReason>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MemoryMonitor {
    /// 監視スレッドを起動する（呼び出し元が指定したしきい値・本番の
    /// メモリ使用量取得経路を使う）。
    ///
    /// しきい値を固定引数にしていない理由: 本番の
    /// `spawn_worker`（[`super::resource_limits::MAX_CHILD_RSS_BYTES`]。
    /// 320 MiB を渡す）と、`WorkerSpawnConfigForTest::rss_threshold_bytes_override`
    /// 経由のテスト（それより小さい値を渡す。codex レビュー指摘 #503 P0
    /// 「macOS の JS ワーカーに強制的なメモリ上限がない」対応で
    /// `ArrayBuffer` 確保に上限を設けたことに伴い、既存の結合テストが
    /// RSS 監視自体の検証という本来の意図を保てなくなった問題への
    /// 対応）と、単体テスト（`tests` モジュール。しきい値超過を人為的に
    /// 再現するため極端に小さい値を渡す）の 3 者が、いずれもこの単一の
    /// 関数を経由して呼び出し元から具体的なしきい値を指定できる必要が
    /// あるため。
    fn spawn_with_threshold(child: Arc<Mutex<Child>>, pid: u32, threshold_bytes: u64) -> Self {
        Self::spawn_with_threshold_and_probe(
            child,
            pid,
            threshold_bytes,
            Box::new(default_memory_probe),
        )
    }

    /// [`MemoryMonitor::spawn_with_threshold`] の本体。`probe` を呼び出し元
    /// から差し替えられる（codex レビュー指摘
    /// #503 P0「取得失敗を注入する経路を `#[cfg(test)]` で用意する」
    /// 対応。単体テストは常に `None` を返す・N 回目まで `None` を返す等の
    /// フェイクを渡すことで、実際の `ps`／`/proc`／`GetProcessMemoryInfo`
    /// を壊さずに fail-closed の挙動を決定的に検証できる）。`child` は
    /// `kill` するために共有し、`pid` は `probe` に渡す
    /// （[`std::process::Child::id`] はプロセスの寿命のあいだ不変の
    /// ため、`Mutex` 越しに毎回取得し直す必要はない）。
    fn spawn_with_threshold_and_probe(
        child: Arc<Mutex<Child>>,
        pid: u32,
        threshold_bytes: u64,
        probe: MemoryProbeFn,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let kill_reason = Arc::new(Mutex::new(None));
        let stop_for_thread = Arc::clone(&stop);
        let kill_reason_for_thread = Arc::clone(&kill_reason);

        let thread = std::thread::spawn(move || {
            // `stop` の確認は `RSS_POLL_INTERVAL` より高い頻度で行い、
            // `Drop` 時の停止を早める（厳密な間隔でなくてよい）。
            const STOP_CHECK_GRANULARITY: Duration = Duration::from_millis(10);
            let mut consecutive_probe_failures: u32 = 0;
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

                let Some(usage_bytes) = probe(&child, pid) else {
                    // メモリ使用量の取得に失敗した。プローブ失敗の原因は
                    // 大きく 2 つに分かれる:
                    //
                    // 1. 監視そのものが機能していない（`ps` が PATH に
                    //    無い等）。子は生きたままなので、連続失敗を
                    //    数えて fail-closed にする意味がある。
                    // 2. 子が V8 の fatal OOM 等で**既に終了している**。
                    //    この場合 `/proc`・`ps`・`GetProcessMemoryInfo`
                    //    は「プロセスが存在しない」ことを理由に `None` を
                    //    返すのが正常な挙動であり、監視が壊れたわけでは
                    //    ない（Cursor Bugbot レビュー指摘 #503「fail-closed
                    //    の経路が、子がすでに終了している場合のプローブ
                    //    失敗も『監視できない』と数えている」対応）。
                    //    ここで `ProbeUnavailable` を記録すると、
                    //    `discard_context_error` がこれを「なぜ落ちたか」の
                    //    理由として扱ってしまい、本来の OOM による
                    //    `ResourceLimitExceeded` が `EngineUnavailable` に
                    //    誤分類される（本レビュー指摘の症状そのもの）。
                    //
                    // そのため、プローブ失敗のたびにまず子が既に終了して
                    // いないかを確認する。終了していれば、この監視スレッド
                    // の役目は既に終わっている（子の終了理由の判定は
                    // `send_evaluate_and_await` 側の stderr ヒューリスティック
                    // に譲る）ため、`kill_reason` を記録せず静かに終了する
                    // （連続失敗としても数えない）。
                    let already_exited = child
                        .lock()
                        .ok()
                        .and_then(|mut guard| guard.try_wait().ok())
                        .flatten()
                        .is_some();
                    if already_exited {
                        return;
                    }

                    // 単発の失敗は fail-open（次のポーリングで再試行）に
                    // するが、連続で [`MAX_CONSECUTIVE_PROBE_FAILURES`] 回
                    // 失敗したら、監視そのものが機能していないとみなして
                    // fail-closed にする（codex レビュー指摘 #503 P0
                    // 対応。`MAX_CONSECUTIVE_PROBE_FAILURES` のドキュメント
                    // コメント参照）。
                    consecutive_probe_failures += 1;
                    if consecutive_probe_failures < MAX_CONSECUTIVE_PROBE_FAILURES {
                        continue;
                    }
                    if let Ok(mut reason) = kill_reason_for_thread.lock() {
                        *reason = Some(MonitorKillReason::ProbeUnavailable {
                            consecutive_failures: consecutive_probe_failures,
                        });
                    }
                    if let Ok(mut child) = child.lock() {
                        let _ = child.kill();
                    }
                    return;
                };
                consecutive_probe_failures = 0;
                if usage_bytes <= threshold_bytes {
                    continue;
                }

                // 状態の記録を kill より先に行う（advisor 指摘。`kill`
                // すると reader スレッドがほぼ即座に `Eof` を検出しうる
                // ため、先に kill してしまうと `send_evaluate_and_await`
                // 側が `take_kill_reason` を確認する前に `Eof` へ到達し、
                // 「なぜ落ちたか」の理由を stderr ヒューリスティックの
                // 誤判定に譲ってしまう競合が起きる。「この使用量だから
                // kill を決めた」という順序のほうが実態にも即している）。
                if let Ok(mut reason) = kill_reason_for_thread.lock() {
                    *reason = Some(MonitorKillReason::ResourceLimitExceeded(usage_bytes));
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
            kill_reason,
            thread: Some(thread),
        }
    }

    /// 監視スレッドが記録した「子を `kill` した」状態を取り出す
    /// （取り出すと消費され、以後は `None` になる。呼び出し元がこの値を
    /// 元にエラーへ変換した後、別の呼び出し元が同じ状態を二重に消費して
    /// 矛盾したメッセージを出さないようにするため）。
    fn take_kill_reason(&self) -> Option<MonitorKillReason> {
        self.kill_reason
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// 監視スレッドに停止を伝え、終了を待つ（`WorkerHandle::drop` から
    /// 呼ぶ。子の寿命を超えて監視スレッドが残らないことを保証する）。
    ///
    /// `std::thread::JoinHandle` には期限付き `join` が無いため、実際の
    /// `join` は使い捨てのヘルパースレッドへ委ね、その完了通知だけを
    /// 期限付きで待つ（codex/Bugbot レビュー指摘 #503 P1「stop_and_join
    /// が無期限に止まりうる」対応。監視スレッドが `read_child_rss_bytes`
    /// の中でブロックしている最悪ケース（macOS の `ps` 呼び出し等）でも、
    /// この呼び出し自体は [`STOP_JOIN_TIMEOUT`] 以内に必ず戻る）。期限内に
    /// 完了通知が来なければ、ヘルパースレッドを待たずに戻る（監視スレッド
    /// は detach された状態で継続し、いずれ自然に終了する。既に `stop`
    /// は立ててあるため、次にブロックが解けたポーリング機会には必ず
    /// 抜ける。監視スレッドが detach 後に kill 済みの子へ触れても、
    /// `read_child_rss_bytes` は「プロセスが存在しない」を意味する
    /// エラーとして扱い `None` を返すだけであり、panic はしない
    /// （`read_child_rss_bytes` のドキュメントコメント参照））。
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.thread.take() else {
            return;
        };

        const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        let _ = done_rx.recv_timeout(STOP_JOIN_TIMEOUT);
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
    /// [`memory_monitor`](Self::memory_monitor) に渡したのと同じ RSS
    /// しきい値（バイト）。[`apply_post_response_rss_check`] が応答直後の
    /// 同期確認で使う（`MemoryMonitor` 自身はしきい値を外部から読み出せる
    /// API を持たないため、`WorkerHandle` 側にも複製して保持する）。
    /// `WorkerSpawnConfigForTest::rss_threshold_bytes_override` のドキュメント
    /// コメント参照。
    rss_threshold_bytes: u64,
}

impl WorkerHandle {
    /// 既に spawn 済みの `Child`（stdin/stdout/stderr を pipe で確保して
    /// いること）から `WorkerHandle` を組み立てる（writer・reader・
    /// stderr drain・メモリ監視の各スレッドを起動する）。
    ///
    /// `tests` モジュールの単体テスト（`sleep` 等の代役プロセスを使い、
    /// V8 プロトコルに依存しない書き込みタイムアウト・reap タイムアウト
    /// の挙動だけを検証する）専用の入口。本番の既定しきい値
    /// （[`super::resource_limits::MAX_CHILD_RSS_BYTES`]）を使う
    /// [`WorkerHandle::from_child_with_rss_threshold`] への薄いラッパー。
    ///
    /// 本番（`spawn_worker`）はしきい値を差し替えられる必要がある
    /// （`WorkerSpawnConfigForTest::rss_threshold_bytes_override` 参照）
    /// ため、直接 [`WorkerHandle::from_child_with_rss_threshold`] を呼ぶ。
    #[cfg(test)]
    fn from_child(child: Child) -> Result<Self, JsEngineError> {
        Self::from_child_with_rss_threshold(child, super::resource_limits::MAX_CHILD_RSS_BYTES)
    }

    /// [`WorkerHandle::from_child`] の本体。RSS 監視のしきい値を呼び出し元
    /// から指定できる（`spawn_worker` が
    /// `WorkerSpawnConfigForTest::rss_threshold_bytes_override` を渡す際に
    /// 使う。同フィールドのドキュメントコメント参照）。
    fn from_child_with_rss_threshold(
        mut child: Child,
        rss_threshold_bytes: u64,
    ) -> Result<Self, JsEngineError> {
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

        let memory_monitor =
            MemoryMonitor::spawn_with_threshold(Arc::clone(&child), pid, rss_threshold_bytes);

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
            rss_threshold_bytes,
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

/// テスト専用: [`V8ProcessEngine::send_raw_frame_for_test`] が受け付ける
/// 生バイト列の上限（バイト。codex レビュー指摘 #503 P0「`bytes` を長さ
/// 検証なしで `to_vec()` しており、無制限の確保経路になる」対応）。
///
/// `send_raw_frame_for_test` と同じく feature `test-support` でのみ
/// 存在する（Issue #528）。gate しないと `js-v8` のみ（`test-support`
/// 無効）のビルドで未使用（`dead_code`）になり、`-D warnings` で失敗する。
///
/// この定数は「親から子への 1 フレーム」を模して生バイト列をそのまま
/// 子の stdin へ書き込むテスト専用の入口の上限であり、その実効的な上限は
/// [`worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`]（親から子への
/// 1 フレームのペイロード上限）に、フレームヘッダー分（`u32` の長さ
/// プレフィックス 4 バイト＋`u8` のタグ 1 バイト＝5 バイト）の余裕を
/// 足した値にする。呼び出し元（`tests/v8_worker.rs`）が意図的に破損した
/// フレーム（プロトコル違反の注入）を送る際も、実際に送るバイト数は
/// 数バイト〜数十バイト程度であり、この上限に触れることはない。
#[cfg(feature = "test-support")]
const MAX_RAW_FRAME_BYTES_FOR_TEST: usize = worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD + 5;

// `WorkerSpawnConfigForTest` は `V8ProcessEngine` の非公開フィールド
// （`spawn_config`）の型であり、本番の `spawn_worker` も参照するため
// `#[cfg(feature = "test-support")]` だけでは消せない（フィールドが本番
// ビルドにも残る。Issue #528 の方式 (a)）。非公開サブモジュールへ実体を
// 置き、crate 内での見せ方（再エクスポートの可視性）だけを feature で
// 切り替える: `test-support` 有効時は `pub use`（`tests/v8_worker.rs` が
// 結合テストとして参照できるようにする）、無効時は非公開 `use`（crate 外
// から名前を一切付けられなくする）にする。
mod spawn_config {
    /// テスト専用: 子プロセスの起動条件を上書きする（Issue #503 設計書
    /// §7 W6「テスト用に小さいヒープ上限を渡す経路（テスト専用。本番では
    /// 無効）」・「ハンドシェイク失敗」の検証に使う）。
    ///
    /// 本番の [`super::V8ProcessEngine::new`] は常に既定値
    /// （`Default::default()`。本番のヒープ上限・プロトコルバージョン）を
    /// 使う。この構造体は `tests/v8_worker.rs`（`harness = false`。W6）が
    /// [`super::V8ProcessEngine::new_for_test`] 経由でのみ使う想定であり、
    /// 本 crate の他のコードは触らない。
    ///
    /// feature `test-support` が有効なときだけ crate 外から参照できる
    /// （`super`（`process_engine` モジュール）の `pub use` /
    /// 非公開 `use` で可視性を切り替える。Issue #528・TASK-29・`JS-1`）。
    #[derive(Debug, Clone, Default)]
    pub struct WorkerSpawnConfigForTest {
        /// 子へ `super::super::worker::TEST_HEAP_LIMIT_ENV_VAR` として
        /// 渡すヒープ上限（バイト）。`None` の場合は渡さない（本番と同じ
        /// 既定値になる）。
        ///
        /// **下げることしかできない**（codex レビュー指摘 #503 P0 対応）:
        /// 本番の既定値
        /// （[`super::super::v8_engine::MAX_ISOLATE_HEAP_BYTES`]。128 MiB）
        /// を上回る値を指定しても、
        /// [`super::super::v8_engine::clamp_test_heap_limit_bytes`]
        /// により既定値へクランプされる（`spawn_worker` が環境変数を組み立てる
        /// 直前に適用する）。下限側も同関数がクランプする（`0` や極端に小さい
        /// 値を渡しても、実用上動作する最小値まで引き上げられる。詳細は同関数の
        /// ドキュメントコメント参照）。子プロセス側（`super::super::worker`）でも
        /// 同じクランプを独立に適用しており、親を経由しない直接起動に対する
        /// 防御になっている（多層防御）。
        pub heap_limit_bytes: Option<usize>,
        /// 子へ渡すプロトコルバージョンの上書き値（ハンドシェイク失敗を
        /// 決定的に再現するためのテスト専用経路）。`None` の場合は
        /// [`super::worker_protocol::PROTOCOL_VERSION`] を使う。
        pub protocol_version_override: Option<u16>,
        /// `true` にすると、子へ
        /// [`super::super::worker::TEST_HELLO_ENGINE_OVERRIDE_ENV_VAR`]
        /// を渡し、子が `Hello` で `EngineKind::Boa` を名乗るようにする（codex
        /// レビュー指摘 #503 P1「Hello のエンジン種別を無視している」の回帰
        /// テスト専用。実際の子プロセスに別のエンジン種別を名乗らせて、親
        /// （`spawn_worker`）が拒否することを確認する）。
        pub hello_wrong_engine_for_test: bool,
        /// `true` にすると、子へ
        /// [`super::super::worker::TEST_HELLO_EXTRA_BYTE_ENV_VAR`]
        /// を渡し、子が送る `Hello` の末尾に余分な 1 バイトを付け足させる
        /// （codex レビュー指摘 #503 P1「decode_hello が末尾の余分なバイトを
        /// 拒否していない」の回帰テスト専用）。
        pub hello_extra_byte_for_test: bool,
        /// 親側の RSS 監視（継続監視・応答直後の同期確認の両方）が使う
        /// しきい値の上書き値（バイト）。`None` の場合は
        /// [`super::super::resource_limits::MAX_CHILD_RSS_BYTES`]（320 MiB）を使う
        /// （codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的な
        /// メモリ上限がない」対応で `ArrayBuffer` 確保に独自の上限
        /// （[`super::super::resource_limits::MAX_ARRAY_BUFFER_ALLOCATION_BYTES`]。
        /// 128 MiB）を設けたことに伴う回帰テスト専用の経路。この上限の
        /// 導入前は、`ArrayBuffer` を際限なく確保するスクリプトで実プロセスの
        /// RSS を本番のしきい値（320 MiB）まで実際に押し上げて RSS 監視を
        /// 検証していたが、`ArrayBuffer` の確保自体が 128 MiB で
        /// `RangeError` になるようになったため、その手段が使えなくなった。
        /// RSS 監視という仕組みそのものの検証を保つため、`ArrayBuffer` の
        /// 上限をテストのために引き上げるのではなく（引き上げは、検証したい
        /// 安全機構自体を回避する経路になり得るため避ける）、RSS 監視の
        /// しきい値を**本番の値より小さくする**ことだけを許す（下げることしか
        /// できない。`heap_limit_bytes` と同じ「テスト専用の上書きは安全側
        /// にしか動かせない」原則）。
        ///
        /// **下げることしかできない**: 本番の既定値
        /// （[`super::super::resource_limits::MAX_CHILD_RSS_BYTES`]）を上回る値を
        /// 指定しても、`spawn_worker` が `MemoryMonitor` を起動する直前に
        /// `min(要求値, MAX_CHILD_RSS_BYTES)` へクランプする。
        pub rss_threshold_bytes_override: Option<u64>,
        /// Windows の子プロセスの Job Object `ProcessMemoryLimit`（バイト）の
        /// 上書き値（`JS-1`・`TASK-29`・Issue #531）。親側の RSS 監視
        /// （`rss_threshold_bytes_override` を `None` にした本番しきい値）が
        /// 先に発動しない構成で、OS が確保を拒否する経路を検証するための
        /// テスト専用経路。`None` なら本番値（384 MiB）。
        ///
        /// **下げることしかできない**: 本番値を超える要求は `spawn_worker` が
        /// クランプし、子（`super::super::worker`）でも独立にクランプする。
        /// Windows 以外では無視される。
        pub windows_process_memory_limit_bytes_override: Option<usize>,
    }
}

// feature `test-support` 有効時: `tests/v8_worker.rs`（結合テスト。別
// クレート）が `fandhe_browser_js::process_engine::WorkerSpawnConfigForTest`
// として参照できるよう再公開する（`#[doc(hidden)]` で公開 API ドキュメント
// には出さない）。
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub use spawn_config::WorkerSpawnConfigForTest;
// feature `test-support` 無効時: crate 内でだけ使えるようにし、crate 外
// から名前を一切付けられなくする（受入基準 1。Issue #528）。
#[cfg(not(feature = "test-support"))]
use spawn_config::WorkerSpawnConfigForTest;

/// JS 評価を子プロセスへ分離して提供するエンジン（`JS-1`・Issue #503）。
///
/// 子・パイプしか保持しないため `Send` にできる（設計書 §3.2。将来
/// core・tokio と統合するときに有利になる）。
///
/// `#[doc(hidden)] pub` である理由: `tests/v8_worker.rs`（結合テスト。
/// 別クレートとしてコンパイルされる）が feature `test-support` 有効時に
/// `new_for_test`・`evaluate_script` 等のテスト専用入口を参照するには、
/// 型自体も `pub` である必要がある（`pub(crate)` は別クレートから見えない）。
/// テスト専用メソッド自体は `#[cfg(feature = "test-support")]` で個別に
/// gate する（[`WorkerSpawnConfigForTest`] のドキュメントコメント参照）。
/// `TASK-29.6`（Issue #157）で `create_engine` から配線し、
/// `impl JsEngine for V8ProcessEngine` を追加する（本 Issue の時点では
/// inherent メソッドに留める）。
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
    /// で子プロセスの起動条件を上書きできる（テスト専用。W6）。feature
    /// `test-support` 有効時のみ存在する（Issue #528）。
    #[cfg(feature = "test-support")]
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
        // （coding-rust.md「外部入力」節）。codex レビュー指摘 #503 P1
        // 「親は MAX_FRAME_PAYLOAD_PARENT_TO_CHILD（1 MiB+64 KiB）まで
        // 受け付けるが、子は MAX_SCRIPT_SOURCE_BYTES（1 MiB）までしか
        // 受け付けない」対応: 親の事前検証を子の実効的な上限
        // （`super::v8_engine::MAX_SCRIPT_SOURCE_BYTES`）に揃える。
        // `MAX_FRAME_PAYLOAD_PARENT_TO_CHILD` はフレームのタグ・長さ
        // プレフィックス分の余白を含むプロトコル上の上限であり、
        // アプリケーション側の実効的な入力上限とは別物である
        // （`worker_protocol.rs` のドキュメントコメント参照）。ここで
        // 弾いておけば、子を起動して書き込みを試みたあとに子側の
        // `V8Engine::evaluate_script` が同じ理由で `EvaluationFailed`
        // を返す（＝子プロセスを 1 つ無駄に起動する）ことを避けられる。
        if script.len() > super::v8_engine::MAX_SCRIPT_SOURCE_BYTES {
            return Err(JsEngineError::EvaluationFailed(format!(
                "script source exceeds the maximum supported length of {} bytes",
                super::v8_engine::MAX_SCRIPT_SOURCE_BYTES
            )));
        }

        let mut worker = match self.worker.take() {
            Some(worker) => worker,
            None => self.spawn_worker()?,
        };

        // codex・Bugbot レビュー指摘 #503 P0: 前回の評価から今回の
        // 呼び出しまでのあいだ（待機中）に、監視スレッドが既に `kill`
        // していないかを確認する。
        if let Some(reason) = worker.memory_monitor.take_kill_reason() {
            let tail = worker.reap_and_collect_stderr();
            let err = monitor_kill_reason_to_error(
                reason,
                &tail,
                "while idle between evaluations",
                worker.rss_threshold_bytes,
            );
            // `worker` はここで drop され（`self.worker` へ戻さない）、
            // 次回の呼び出しで新しい子を起動し直す。
            return Err(ensure_discarded_phrase(err));
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
    ///
    /// `#[cfg(test)]` を付けられない理由: `tests/v8_worker.rs` は別クレート
    /// としてコンパイルされる結合テストであり、`#[cfg(test)]` の項目を
    /// 参照できない（[`WorkerSpawnConfigForTest`] のドキュメントコメント
    /// と同じ事情）。その代わり feature `test-support` で gate し（Issue
    /// #528）、本番ビルド（`test-support` 無効）からは呼べないようにする。
    /// 多層防御として、外部入力と同様に `bytes` の長さも検証する（codex
    /// レビュー指摘 #503 P0「`bytes` を
    /// 長さ検証なしで `to_vec()` しており、無制限の確保経路になる」
    /// 対応）。
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn send_raw_frame_for_test(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // 複製（`to_vec()`）の前に長さを検証する（coding-rust.md「長さ・
        // 件数を上限検証してからアロケーションに使う」）。上限は、この
        // 関数が模す「親から子への 1 フレーム」の実効的な最大長
        // （[`worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`] ＋
        // フレームヘッダー分。[`MAX_RAW_FRAME_BYTES_FOR_TEST`] 参照）に
        // 揃える。
        if bytes.len() > MAX_RAW_FRAME_BYTES_FOR_TEST {
            return Err(std::io::Error::other(format!(
                "raw frame for a test exceeds the maximum supported length of \
                 {MAX_RAW_FRAME_BYTES_FOR_TEST} bytes (got {} bytes)",
                bytes.len()
            )));
        }
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
    /// （`super::resource_limits` のドキュメントコメント参照）。feature
    /// `test-support` 有効時のみ存在する（Issue #528）。
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn worker_pid_for_test(&self) -> Option<u32> {
        self.worker.as_ref().map(|worker| worker.pid)
    }

    /// テスト専用: 子プロセスの「起動＋ハンドシェイク」だけを行い、
    /// スクリプト評価は行わない（`TASK-29`・Issue #555・`CORE-3`・
    /// `PERF-6`・`PERF-7`）。`crates/fandhe-browser-js/benches/
    /// worker_spawn_latency.rs`（Issue #555）が、この呼び出し全体の
    /// 所要時間を計測してレイテンシの実測に使う。
    ///
    /// 計測区間は `spawn_worker` 呼び出し（`Command::spawn` による
    /// fork/exec・`env_clear`）から、子の Platform/Isolate 初期化・
    /// 永続 Context 生成・`Hello` フレームの送信・親側での受信と
    /// decode・検証完了までを含む（本ファイル内の `spawn_worker` の
    /// ドキュメントコメント参照。private のため intra-doc link は張らない）。
    ///
    /// 既に子プロセスを保持している場合は二重起動せず何もしない
    /// （[`Self::evaluate_script`] と同じ「子が無ければ起動する」遅延
    /// 起動の作法に合わせる）。feature `test-support` 有効時のみ存在する
    /// （Issue #528 と同じ隔離方針）。
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn spawn_worker_for_test(&mut self) -> Result<(), JsEngineError> {
        if self.worker.is_none() {
            self.worker = Some(self.spawn_worker()?);
        }
        Ok(())
    }

    /// テスト専用: [`evaluate_script`](Self::evaluate_script) が事前検証
    /// に使うスクリプト長上限（バイト）を返す。codex レビュー指摘 #503
    /// P1「親は子より大きい入力を受け付ける」の回帰テストが、`tests/`
    /// 配下の結合テスト（別クレート）から `super::v8_engine::MAX_SCRIPT_SOURCE_BYTES`
    /// （`pub(crate)`）へ直接アクセスできないため、定数の値だけを最小限
    /// 公開する（`v8` crate の型は一切介さない）。feature `test-support`
    /// 有効時のみ存在する（Issue #528）。
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn max_script_source_bytes_for_test() -> usize {
        super::v8_engine::MAX_SCRIPT_SOURCE_BYTES
    }

    /// テスト専用: [`send_raw_frame_for_test`](Self::send_raw_frame_for_test)
    /// が受け付ける生バイト列の上限（バイト）を返す。codex レビュー指摘
    /// #503 P0「`send_raw_frame_for_test` が `bytes` を長さ検証なしで
    /// `to_vec()` しており、無制限の確保経路になる」の回帰テストが、
    /// [`max_script_source_bytes_for_test`](Self::max_script_source_bytes_for_test)
    /// と同じ理由（`tests/` 配下の結合テストは別クレートであり、
    /// `MAX_RAW_FRAME_BYTES_FOR_TEST`（非 `pub`）へ直接アクセスできない）
    /// で、定数の値だけを最小限公開する。feature `test-support` 有効時
    /// のみ存在する（Issue #528）。
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn max_raw_frame_bytes_for_test() -> usize {
        MAX_RAW_FRAME_BYTES_FOR_TEST
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
        if self.spawn_config.hello_wrong_engine_for_test {
            // テスト専用（codex レビュー指摘 #503 P1）。本番の `new()` は
            // 常に `false` のため、この環境変数は本番の子プロセスには
            // 渡らない。
            command.env(super::worker::TEST_HELLO_ENGINE_OVERRIDE_ENV_VAR, "boa");
        }
        if self.spawn_config.hello_extra_byte_for_test {
            // テスト専用（codex レビュー指摘 #503 P1）。値の内容は
            // `super::worker` 側では読まず、変数の有無だけを見る。
            command.env(super::worker::TEST_HELLO_EXTRA_BYTE_ENV_VAR, "1");
        }
        #[cfg(windows)]
        {
            if let Some(requested) = self
                .spawn_config
                .windows_process_memory_limit_bytes_override
            {
                // テスト専用（Issue #531）。本番の `new()` は常に `None`。
                // 本番値を超えられないよう、環境変数を組み立てる直前にクランプする。
                command.env(
                    super::worker::TEST_WINDOWS_PROCESS_MEMORY_LIMIT_ENV_VAR,
                    super::resource_limits::clamp_test_windows_process_memory_limit_bytes(
                        requested,
                    )
                    .to_string(),
                );
            }
        }
        #[cfg(not(windows))]
        {
            // Windows 専用の上書き。他 OS では効果が無いため読み捨てる。
            let _ = self
                .spawn_config
                .windows_process_memory_limit_bytes_override;
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

        // codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ
        // 上限がない」対応で `ArrayBuffer` 確保に独自の上限を設けたことに
        // 伴い、既存の RSS 監視の回帰テストが本来の意図（RSS 監視自体の
        // 検証）を保てるよう、テストだけがしきい値を下げられるようにする
        // （`WorkerSpawnConfigForTest::rss_threshold_bytes_override` の
        // ドキュメントコメント参照。本番の `new()` は常に `None` のため、
        // 本番の子には影響しない）。**下げることしかできない**: 要求値を
        // 本番の既定値（`MAX_CHILD_RSS_BYTES`）でクランプする。
        let rss_threshold_bytes = self
            .spawn_config
            .rss_threshold_bytes_override
            .map(|requested| requested.min(super::resource_limits::MAX_CHILD_RSS_BYTES))
            .unwrap_or(super::resource_limits::MAX_CHILD_RSS_BYTES);
        let mut worker = WorkerHandle::from_child_with_rss_threshold(child, rss_threshold_bytes)?;

        match worker.frame_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::HELLO => {
                match worker_protocol::decode_hello(&payload) {
                    Ok((version, engine))
                        if version == worker_protocol::PROTOCOL_VERSION
                            && engine == EngineKind::V8 =>
                    {
                        Ok(worker)
                    }
                    Ok((version, engine)) if version == worker_protocol::PROTOCOL_VERSION => {
                        // codex レビュー指摘 #503 P1「Hello のエンジン種別を
                        // 無視している」対応: 本 crate（`V8ProcessEngine`）は
                        // V8 の子プロセス専用であり、`Boa` を名乗る `Hello`
                        // は起動した実行ファイル・プロトコルの取り違えを
                        // 疑うべき異常事態である。
                        worker.terminate_now();
                        let tail = worker.reap_and_collect_stderr();
                        Err(handshake_discard_error(&worker, &tail, || {
                            JsEngineError::EngineUnavailable(format!(
                                "JS worker process reported unexpected engine kind {engine:?} \
                                 (expected V8); stderr: {tail}"
                            ))
                        }))
                    }
                    Ok((version, _engine)) => {
                        worker.terminate_now();
                        let tail = worker.reap_and_collect_stderr();
                        Err(handshake_discard_error(&worker, &tail, || {
                            JsEngineError::EngineUnavailable(format!(
                                "JS worker process reported unsupported protocol version \
                                 {version}; stderr: {tail}"
                            ))
                        }))
                    }
                    Err(err) => {
                        worker.terminate_now();
                        let tail = worker.reap_and_collect_stderr();
                        Err(handshake_discard_error(&worker, &tail, || {
                            JsEngineError::EngineUnavailable(format!(
                                "JS worker process sent a malformed Hello frame: {err}; stderr: \
                                 {tail}"
                            ))
                        }))
                    }
                }
            }
            Ok(ReaderEvent::Frame(other_tag, _)) => {
                // `NATIVE_CALL`（tag 5）はハンドシェイク前には来ないはず
                // （送信は子側の永続 Context が評価中に限られる。§3.1）。
                // 親側の `NATIVE_CALL` dispatch 自体は別 Issue（#526）で
                // 追加するが、ハンドシェイク段階で受け取った場合はそれでも
                // プロトコル違反として fail-closed に kill する（本分岐は
                // #526 完了後も変わらない）。
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                Err(handshake_discard_error(&worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process sent an unexpected frame (tag {other_tag}) before \
                         Hello; stderr: {tail}"
                    ))
                }))
            }
            Ok(ReaderEvent::Eof) => {
                // 子は既にストリームを閉じている（終了済みか、まもなく
                // 終了する）ため、kill を挟まずに直接 reap する
                // （`reap_and_collect_stderr` のドキュメントコメント
                // 「wait → join → スナップショットの順序が必須」参照）。
                let tail = worker.reap_and_collect_stderr();
                Err(handshake_discard_error(&worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process exited before completing the handshake; stderr: \
                         {tail}"
                    ))
                }))
            }
            Ok(ReaderEvent::Invalid(desc)) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                Err(handshake_discard_error(&worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process sent a malformed frame during the handshake: {desc}; \
                         stderr: {tail}"
                    ))
                }))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                Err(handshake_discard_error(&worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process handshake timed out; stderr: {tail}"
                    ))
                }))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                Err(handshake_discard_error(&worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process handshake channel disconnected unexpectedly; stderr: \
                         {tail}"
                    ))
                }))
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
            let err = discard_context_error(worker, &tail, || {
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
            let err = discard_context_error(worker, &tail, || {
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
                let converted = discard_context_error(worker, &tail, || {
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
                // 後始末される）。監視スレッドが同じタイミングで RSS
                // 超過を検出していた場合はそちらを優先する（Bugbot
                // レビュー指摘 #503 対応。全経路で `discard_context_error`
                // を通す）。
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
                    JsEngineError::Timeout(format!(
                        "sending the script to the JS worker process exceeded the deadline and \
                         the process was killed; the next evaluation runs in a fresh context; \
                         stderr: {tail}"
                    ))
                });
                return (Err(err), false);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
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
                    // codex レビュー指摘 #503 P1「decode_js_value が返す
                    // 消費バイト数を捨てている」対応: `Result` フレームの
                    // ペイロードは常にちょうど 1 つの `JsValue` でなければ
                    // ならない契約であり（`decode_js_value` 自体は複数値の
                    // 連結読み取りにも使える汎用関数のため、単体では
                    // 「1 つだけ」を強制しない）、消費バイト数が
                    // ペイロード全体の長さと一致しなければ、末尾に余分な
                    // バイトが付いたプロトコル違反として扱う。
                    Ok((value, consumed)) if consumed == payload.len() => (Ok(value), true),
                    Ok((_value, consumed)) => {
                        worker.terminate_now();
                        let tail = worker.reap_and_collect_stderr();
                        let converted = discard_context_error(worker, &tail, || {
                            JsEngineError::EngineUnavailable(format!(
                                "JS worker process sent a Result frame with {} trailing bytes \
                                 after the decoded value ({consumed} of {} bytes consumed); \
                                 stderr: {tail}",
                                payload.len() - consumed,
                                payload.len()
                            ))
                        });
                        (Err(converted), false)
                    }
                    Err(err) => {
                        worker.terminate_now();
                        let tail = worker.reap_and_collect_stderr();
                        let converted = discard_context_error(worker, &tail, || {
                            JsEngineError::EngineUnavailable(format!(
                                "JS worker process sent a malformed Result frame: {err}; stderr: \
                                 {tail}"
                            ))
                        });
                        (Err(converted), false)
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
                        let tail = worker.reap_and_collect_stderr();
                        let converted = discard_context_error(worker, &tail, || {
                            JsEngineError::EngineUnavailable(format!(
                                "JS worker process sent a malformed Error frame: {err}; stderr: \
                                 {tail}"
                            ))
                        });
                        (Err(converted), false)
                    }
                };
                apply_post_response_rss_check(worker, outcome, keep)
            }
            Ok(ReaderEvent::Frame(other_tag, _)) => {
                // JS-1・Issue #511: 子は `NATIVE_CALL`（tag 5）を送りうる
                // （子側のプロキシ関数が呼ばれた場合）。親側の dispatch
                // （`NativeFn` の実行・`NATIVE_RETURN` の返信）は別 Issue
                // （#526）で追加する。それまでは `NATIVE_CALL` を含む
                // あらゆる想定外のタグをプロトコル違反として扱い、
                // fail-closed に子を kill する（本番の子は #155 完了まで
                // プロキシを登録する手段が無いため、この分岐には到達しない）。
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process sent an unexpected frame (tag {other_tag}); stderr: \
                         {tail}"
                    ))
                });
                (Err(err), false)
            }
            Ok(ReaderEvent::Eof) => {
                // 子は既にストリームを閉じている（終了済みか、まもなく
                // 終了する）ため、kill を挟まずに直接 reap する。
                //
                // `discard_context_error` が「監視スレッドの RSS 超過
                // kill」「stderr の V8 fatal OOM メッセージ」の両方を
                // `fallback` より先に判定する（同関数のドキュメントコメント
                // 参照。codex・Bugbot レビュー指摘 #503 対応）ため、この
                // `fallback` は「どちらの証拠も無かった場合」にだけ使われる。
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process terminated unexpectedly before responding; stderr: \
                         {tail}"
                    ))
                });
                (Err(err), false)
            }
            Ok(ReaderEvent::Invalid(desc)) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process sent a malformed frame: {desc}; stderr: {tail}"
                    ))
                });
                (Err(err), false)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
                    JsEngineError::Timeout(format!(
                        "script evaluation exceeded the JS worker deadline and the process was \
                         killed; the next evaluation runs in a fresh context; stderr: {tail}"
                    ))
                });
                (Err(err), false)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.terminate_now();
                let tail = worker.reap_and_collect_stderr();
                let err = discard_context_error(worker, &tail, || {
                    JsEngineError::EngineUnavailable(format!(
                        "JS worker process communication channel disconnected unexpectedly; \
                         stderr: {tail}"
                    ))
                });
                (Err(err), false)
            }
        }
    }
}

/// [`MonitorKillReason`] を、破棄時のエラーへ変換する（`context_suffix`
/// は呼び出し元の文脈を表す短い句。例: "during the handshake"・"and was
/// killed"。`threshold_bytes` はメッセージに含める、実際に使われた RSS
/// 監視のしきい値。本番は常に
/// [`super::resource_limits::MAX_CHILD_RSS_BYTES`] だが、
/// `WorkerSpawnConfigForTest::rss_threshold_bytes_override` を使う
/// テストではそれより小さい値になりうるため、呼び出し元
/// （`worker.rss_threshold_bytes`）から渡してもらう）。
///
/// **`ResourceLimitExceeded`・`EngineUnavailable` のどちらを使うかは
/// 理由ごとに固定している**（codex レビュー指摘 #503 P0「エラーの種別は
/// 理由が分かる文言を付けた EngineUnavailable、または
/// ResourceLimitExceeded のどちらか」対応。選んだ理由をここに書く）:
///
/// - [`MonitorKillReason::ResourceLimitExceeded`][]: メモリ使用量の超過を
///   実際に観測できたため、そのまま `ResourceLimitExceeded` にする
///   （偽装ではない。実測値が根拠にある）。
/// - [`MonitorKillReason::ProbeUnavailable`][]: メモリ使用量の取得自体が
///   連続で失敗しており、**実際に超過したかどうかは分からない**。
///   ここで `ResourceLimitExceeded` を名乗ると、超過を検出したかのように
///   偽装することになり security.md「偽装・回避機能の禁止」に反する。
///   「監視という前提が壊れたため安全側に倒して終了させた」という
///   `EngineUnavailable`（＝エンジンを一時的に使えないものとして扱う）が
///   実態に即した分類である。
fn monitor_kill_reason_to_error(
    reason: MonitorKillReason,
    tail: &str,
    context_suffix: &str,
    threshold_bytes: u64,
) -> JsEngineError {
    match reason {
        MonitorKillReason::ResourceLimitExceeded(bytes) => {
            JsEngineError::ResourceLimitExceeded(format!(
                "JS worker process memory usage ({bytes} bytes) exceeded the parent's \
                 monitoring threshold ({threshold_bytes} bytes) {context_suffix}; stderr: {tail}"
            ))
        }
        MonitorKillReason::ProbeUnavailable {
            consecutive_failures,
        } => JsEngineError::EngineUnavailable(format!(
            "JS worker process memory monitoring failed {consecutive_failures} consecutive \
             times (memory usage could not be verified); the child was killed as a \
             fail-closed precaution {context_suffix}; stderr: {tail}"
        )),
    }
}

/// `spawn_worker` のハンドシェイク段階で子を破棄する経路が使う（Cursor
/// Bugbot レビュー指摘 #503「Monitor kills misclassified」対応。「子を
/// 破棄するすべての経路で take_kill_reason を先に確認する」契約を
/// ハンドシェイク段階にも適用する）。
///
/// [`discard_context_error`] と異なり "context was discarded" は
/// **付けない**: ハンドシェイク中はまだ `Hello` すら受信しておらず、
/// 呼び出し元が使える Context が一度も存在しないため、「破棄された」と
/// 述べるのは不正確である（実装済みを装わない。REPAIR-3）。
fn handshake_discard_error(
    worker: &WorkerHandle,
    tail: &str,
    fallback: impl FnOnce() -> JsEngineError,
) -> JsEngineError {
    match worker.memory_monitor.take_kill_reason() {
        Some(reason) => monitor_kill_reason_to_error(
            reason,
            tail,
            "during the handshake",
            worker.rss_threshold_bytes,
        ),
        None => fallback(),
    }
}

/// 子を破棄する（`keep_worker = false` にする）**すべての**経路の末尾で
/// 呼び、エラーメッセージを次の 2 点で仕上げる（Cursor Bugbot レビュー
/// 指摘 #503「Monitor kills misclassified」「Discarded-context errors
/// omit required phrase」対応。1 箇所へ集約する）。
///
/// 1. メモリ監視スレッドが既に `kill` していた場合、渡された `fallback`
///    を使わず [`monitor_kill_reason_to_error`] へ差し替える（読み取り・
///    書き込み・プロトコル違反・期限切れなど、`kill` の「本当の理由」が
///    監視スレッドにあったケースを、経路ごとに異なる
///    `EngineUnavailable`/`Timeout` へ誤って分類させない）。
/// 2. それ以外の場合は `fallback()` をそのまま使うが、本モジュールの
///    契約（エラー変換表。「Context が破棄される場合、メッセージに
///    "context was discarded" を含める」）どおり、文言が抜けていれば
///    ここで補う（呼び出し側が書き忘れても壊れない）。
///
/// 呼び出し前提: `worker` は既に `terminate_now`（または監視スレッドに
/// よる `kill`）で終了させてあり、`tail` はその後 `reap_and_collect_stderr`
/// で得た stderr の末尾である。
fn discard_context_error(
    worker: &WorkerHandle,
    tail: &str,
    fallback: impl FnOnce() -> JsEngineError,
) -> JsEngineError {
    let kill_reason = worker.memory_monitor.take_kill_reason();
    const KILL_CONTEXT_SUFFIX: &str = "and was killed; the next evaluation runs in a fresh context";

    // 判定順（Cursor Bugbot レビュー指摘 #503「fail-closed の経路が、
    // 子がすでに終了している場合のプローブ失敗も『監視できない』と
    // 数えている」対応。監視スレッド側で「子が既に終了していれば
    // `ProbeUnavailable` を記録しない」対策を入れたが（`spawn_with_threshold_and_probe`
    // 参照）、監視スレッドと `send_evaluate_and_await` は別スレッドで
    // 進むため、完全な排他は無い。ここでも多層防御として優先順位を
    // 設ける）:
    //
    // 1. 監視スレッドが実測 RSS 超過を記録していた（`ResourceLimitExceeded`）
    //    ── 最も直接的な証拠であり、最優先で使う。
    // 2. stderr に V8 自身の fatal OOM メッセージが残っている
    //    （[`WorkerHandle::stderr_indicates_oom`]）── 1 が無くても、子
    //    自身が「ヒープ上限に達した」と報告している場合はそれを信じる。
    //    これを `ProbeUnavailable` より先に見る理由: 実際には OOM で
    //    落ちたのに、監視スレッドが（子の終了検出との競合で）
    //    `ProbeUnavailable` を記録してしまうケースが万一残っていても、
    //    stderr の一次情報のほうが「監視ができなかった」という二次的な
    //    事実より優先されるべきだから。
    // 3. 上記のいずれでもなければ、監視スレッドが記録した理由
    //    （この時点では `ProbeUnavailable` のみ）を使う。
    // 4. それも無ければ呼び出し元の `fallback` を使う。
    if let Some(MonitorKillReason::ResourceLimitExceeded(bytes)) = kill_reason {
        let err = monitor_kill_reason_to_error(
            MonitorKillReason::ResourceLimitExceeded(bytes),
            tail,
            KILL_CONTEXT_SUFFIX,
            worker.rss_threshold_bytes,
        );
        return ensure_discarded_phrase(err);
    }
    if WorkerHandle::stderr_indicates_oom(tail) {
        return ensure_discarded_phrase(JsEngineError::ResourceLimitExceeded(format!(
            "script execution exceeded the isolate heap limit and the JS worker process \
             terminated; stderr: {tail}"
        )));
    }
    if let Some(reason) = kill_reason {
        let err = monitor_kill_reason_to_error(
            reason,
            tail,
            KILL_CONTEXT_SUFFIX,
            worker.rss_threshold_bytes,
        );
        return ensure_discarded_phrase(err);
    }
    ensure_discarded_phrase(fallback())
}

/// エラーメッセージに "context was discarded" が含まれていなければ
/// 追記する（[`discard_context_error`] が呼ぶ内部ヘルパー。Cursor Bugbot
/// レビュー指摘 #503「Discarded-context errors omit required phrase」
/// 対応）。
fn ensure_discarded_phrase(err: JsEngineError) -> JsEngineError {
    const PHRASE: &str = "context was discarded";
    match err {
        JsEngineError::EngineUnavailable(msg) if !msg.contains(PHRASE) => {
            JsEngineError::EngineUnavailable(format!("{msg}; {PHRASE}"))
        }
        JsEngineError::Timeout(msg) if !msg.contains(PHRASE) => {
            JsEngineError::Timeout(format!("{msg}; {PHRASE}"))
        }
        JsEngineError::ResourceLimitExceeded(msg) if !msg.contains(PHRASE) => {
            JsEngineError::ResourceLimitExceeded(format!("{msg}; {PHRASE}"))
        }
        other => other,
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

    // 監視スレッドが、応答フレームの到達とほぼ同時に上限超過を検出して
    // 子を `kill` している場合を最優先で確認する（codex レビュー指摘
    // #503 P1「メモリ上限超過で子が終了しても評価結果を成功として
    // 返し得る」対応）。この確認を後回しにして、この関数自身の同期的な
    // プローブ（`child_memory_probe`／`read_child_rss_bytes`）だけに
    // 頼ると、監視スレッドの `kill` によって子が既に終了しているため
    // プローブが `None` を返し、「異常なし」と誤認して `outcome`
    // （評価成功、または `Timeout`/`EvaluationFailed`/`BindingFailed`
    // のように子を生かしたまま扱うつもりだったエラー）をそのまま
    // 返してしまう。呼び出し元には成功（または誤った種類のエラー）が
    // 通知される一方で Context は既に失われており、次回の評価で初めて
    // エラーになる（security.md「偽装・回避機能の禁止」に抵触する
    // 「実際には破棄された Context を、使えるものとして返す」挙動）。
    if let Some(reason) = worker.memory_monitor.take_kill_reason() {
        worker.terminate_now();
        let tail = worker.reap_and_collect_stderr();
        let err = monitor_kill_reason_to_error(
            reason,
            &tail,
            "immediately after responding; the next evaluation runs in a fresh context",
            worker.rss_threshold_bytes,
        );
        return (Err(ensure_discarded_phrase(err)), false);
    }

    let Some(probe) = child_memory_probe(&worker.child, worker.pid) else {
        return (outcome, keep_worker);
    };
    let Some(rss_bytes) = super::resource_limits::read_child_rss_bytes(probe) else {
        return (outcome, keep_worker);
    };
    if rss_bytes <= worker.rss_threshold_bytes {
        return (outcome, keep_worker);
    }

    worker.terminate_now();
    // メモリ監視スレッドがまだ気づいていない可能性があるため、状態を
    // 消費しておく（次回の「監視スレッドが kill 済みか」チェックが
    // 既に破棄した子について二重にエラーメッセージを出さないように
    // するため。子は既に `terminate_now` で終了させているため、監視
    // スレッドが独自に検出しても実害は無い）。
    let _ = worker.memory_monitor.take_kill_reason();
    let tail = worker.reap_and_collect_stderr();
    (
        Err(JsEngineError::ResourceLimitExceeded(format!(
            "JS worker process RSS ({rss_bytes} bytes) exceeded the parent's monitoring \
             threshold ({} bytes) immediately after responding; context was discarded; stderr: \
             {tail}",
            worker.rss_threshold_bytes
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
    ///
    /// **`ping.exe` を `cmd /C` 越しに起動しない**（Windows 版 3 OS CI の
    /// 実失敗から判明。run 36341069374）: `cmd /C ping -n 31 127.0.0.1`
    /// は `cmd.exe` が `ping.exe` を子プロセスとして起動する。
    /// `Child::kill` は直接の子（`cmd.exe`）だけを `TerminateProcess`
    /// し、孫の `ping.exe` は終了させない。Windows では既定で子プロセスは
    /// 親の終了と連動して終了しない（Job Object 等で明示しない限り）ため、
    /// 取り残された `ping.exe` が stdout/stderr の書き込み端を
    /// （`cmd.exe` から継承したハンドルとして）保持し続け、reader/stderr
    /// スレッドが EOF を検出できなくなる。実際にこのテストは Windows CI
    /// で `ping -n 31`（約 31 秒）がすべて完了するまで固まった
    /// （`REAP_WAIT_TIMEOUT` の 1 秒ではなく、観測値は約 30 秒）。
    /// `ping.exe` を `cmd` を介さず直接 `spawn` すれば、`kill` は
    /// `ping.exe` 自身を直接終了させるため、この問題は起こらない
    /// （PowerShell の `Start-Sleep` も候補だったが、標準入力を pipe で
    /// 渡した場合に入力形式の自動判定で stdin を読みにいく可能性があり、
    /// `js_1_write_to_a_stalled_child_is_bounded_by_a_deadline_and_kills_the_child`
    /// が前提とする「子は stdin を一切読まない」という性質を壊しうる
    /// ため避ける）。
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
            Command::new("ping")
                .args(["-n", "31", "127.0.0.1"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn `ping` for the test")
        }
    }

    /// [`spawn_long_lived_child_for_test`] と対になる、すぐに終了する
    /// 代役の「子プロセス」（Cursor Bugbot レビュー指摘 #503「fail-closed
    /// の経路が、子がすでに終了している場合のプローブ失敗も『監視できない』
    /// と数えている」の回帰テスト専用。V8 の fatal OOM で子が終了した
    /// 状況を、実際に OOM を起こさずに「子が既に終了している」という
    /// 性質だけ再現するために使う）。
    fn spawn_short_lived_child_for_test() -> Child {
        #[cfg(unix)]
        {
            Command::new("sleep")
                .arg("0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn a short-lived test child")
        }
        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", "exit 0"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to spawn a short-lived test child")
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
        let mut killed_reason = None;
        while Instant::now() < deadline {
            if let Some(reason) = monitor.take_kill_reason() {
                killed_reason = Some(reason);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        match killed_reason {
            Some(MonitorKillReason::ResourceLimitExceeded(rss)) => {
                assert!(
                    rss > 0,
                    "expected the memory monitor to record a nonzero RSS, got: {rss}"
                );
            }
            other => panic!(
                "expected the memory monitor to kill the idle child (no evaluation ever ran) \
                 with ResourceLimitExceeded, got: {other:?}"
            ),
        }

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

    /// codex レビュー指摘 #503 P0「macOS で `ps` を起動できない場合、
    /// 監視が `None` を返し続けるだけで上限が事実上なくなる」の単体
    /// テスト: 実際の `ps`／`/proc`／`GetProcessMemoryInfo` を壊さず、
    /// 常に `None` を返すフェイクの取得経路を注入して、
    /// [`MAX_CONSECUTIVE_PROBE_FAILURES`] 回連続で失敗した時点で
    /// fail-closed（子を `kill` し、理由を [`MonitorKillReason::ProbeUnavailable`]
    /// として記録）になることを確認する。しきい値超過（`ResourceLimitExceeded`）
    /// とは異なる理由で kill されることを区別できているかどうかも
    /// あわせて検証する。
    #[test]
    fn js_1_memory_monitor_kills_after_consecutive_probe_failures() {
        let child = spawn_long_lived_child_for_test();
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));

        // しきい値は `u64::MAX`（絶対に超過しない）にして、
        // 「取得の失敗」だけが kill の原因になることを確実にする。
        let mut monitor = MemoryMonitor::spawn_with_threshold_and_probe(
            Arc::clone(&child),
            pid,
            u64::MAX,
            Box::new(|_child, _pid| None),
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut killed_reason = None;
        while Instant::now() < deadline {
            if let Some(reason) = monitor.take_kill_reason() {
                killed_reason = Some(reason);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        match killed_reason {
            Some(MonitorKillReason::ProbeUnavailable {
                consecutive_failures,
            }) => {
                assert_eq!(
                    consecutive_failures, MAX_CONSECUTIVE_PROBE_FAILURES,
                    "expected exactly MAX_CONSECUTIVE_PROBE_FAILURES consecutive failures to be \
                     recorded before the fail-closed kill, got: {consecutive_failures}"
                );
            }
            other => panic!(
                "expected the memory monitor to kill the child after repeated probe failures \
                 with ProbeUnavailable (not ResourceLimitExceeded, since the threshold was never \
                 exceeded), got: {other:?}"
            ),
        }

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
            "expected the child to have exited after the memory monitor killed it due to \
             repeated probe failures (fail-closed)"
        );
        drop(guard);

        monitor.stop_and_join();
    }

    /// Cursor Bugbot レビュー指摘 #503「fail-closed の経路が、子がすでに
    /// 終了している場合のプローブ失敗も『監視できない』と数えている」の
    /// 単体テスト: 子が（V8 の fatal OOM 等で）**既に終了している**状態で
    /// プローブが `None` を返し続けても、`MonitorKillReason::ProbeUnavailable`
    /// を記録しないこと（＝連続失敗として数えず、静かに監視を終える）。
    /// 実際の OOM を起こす代わりに、すぐに終了する代役の子プロセス
    /// （[`spawn_short_lived_child_for_test`]）と、常に `None` を返す
    /// フェイクのプローブを組み合わせて、症状（子の終了検出より前に
    /// プローブ失敗が連続してカウントされる）を決定的に再現する。
    #[test]
    fn js_1_memory_monitor_does_not_record_probe_unavailable_after_child_already_exited() {
        let child = spawn_short_lived_child_for_test();
        let pid = child.id();
        let child = Arc::new(Mutex::new(child));

        // 子が実際に終了するまで待つ（`sleep 0`／`cmd /C exit 0` は
        // ほぼ即座に終了するはずである）。
        let exit_deadline = Instant::now() + Duration::from_secs(5);
        let mut already_exited = false;
        while Instant::now() < exit_deadline {
            let mut guard = child.lock().expect("child mutex must not be poisoned");
            if matches!(guard.try_wait(), Ok(Some(_))) {
                already_exited = true;
                break;
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            already_exited,
            "expected the short-lived test child to have exited before the monitor starts"
        );

        // しきい値は `u64::MAX`（超過では kill させない）、プローブは
        // 常に `None`（実際の `ps`／`/proc` が「プロセスが存在しない」を
        // 理由に返す `None` を模す）。
        let mut monitor = MemoryMonitor::spawn_with_threshold_and_probe(
            Arc::clone(&child),
            pid,
            u64::MAX,
            Box::new(|_child, _pid| None),
        );

        // `MAX_CONSECUTIVE_PROBE_FAILURES` 回分のポーリング間隔を大幅に
        // 超えて待ち、fail-closed が発火するだけの時間を与える。
        std::thread::sleep(
            super::super::resource_limits::RSS_POLL_INTERVAL * (MAX_CONSECUTIVE_PROBE_FAILURES + 3),
        );

        let kill_reason = monitor.take_kill_reason();
        assert!(
            kill_reason.is_none(),
            "a probe failure against an already-exited child must not be recorded as \
             ProbeUnavailable (the monitor should recognize the child has exited and stop \
             silently instead of miscounting it as a broken probe), got: {kill_reason:?}"
        );

        monitor.stop_and_join();
    }

    /// codex レビュー指摘 #503 P0（上記テストと対）: `ProbeUnavailable`
    /// による kill は `discard_context_error` を通すと
    /// `JsEngineError::EngineUnavailable`（`ResourceLimitExceeded` では
    /// ない）へ変換されること。メモリ超過を確認できていない以上、
    /// 「しきい値を超えたと確認した」と偽らないようにする設計判断
    /// （[`monitor_kill_reason_to_error`] のドキュメントコメント参照。
    /// security.md「偽装・回避機能の禁止」に対応）を回帰させる。
    #[test]
    fn js_1_discard_context_error_reclassifies_probe_unavailable_as_engine_unavailable() {
        let child = spawn_long_lived_child_for_test();
        let worker = WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        *worker
            .memory_monitor
            .kill_reason
            .lock()
            .expect("kill_reason mutex must not be poisoned") =
            Some(MonitorKillReason::ProbeUnavailable {
                consecutive_failures: MAX_CONSECUTIVE_PROBE_FAILURES,
            });

        let err = discard_context_error(&worker, "irrelevant stderr tail", || {
            JsEngineError::Timeout(
                "must not be used because the monitor already killed".to_string(),
            )
        });
        match err {
            JsEngineError::EngineUnavailable(msg) => {
                assert!(
                    msg.contains(&MAX_CONSECUTIVE_PROBE_FAILURES.to_string()),
                    "expected the consecutive failure count in the message, got: {msg}"
                );
                assert!(
                    msg.contains("context was discarded"),
                    "expected the required phrase, got: {msg}"
                );
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
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

    /// Cursor Bugbot レビュー指摘 #503「Monitor kills misclassified」の
    /// 単体テスト: 監視スレッドが RSS 超過を検出して既に記録していた
    /// 場合、`discard_context_error` は渡された `fallback` を無視し、
    /// 実測 RSS を含む `ResourceLimitExceeded` を返すこと（具体値
    /// 12,345,678 を使い、メッセージにその数値が現れることを確認する）。
    #[test]
    fn js_1_discard_context_error_reclassifies_monitor_kills_as_resource_limit_exceeded() {
        let child = spawn_long_lived_child_for_test();
        let mut worker =
            WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        // 監視スレッドが RSS 超過を検出して記録した状態を、実際に
        // しきい値へ到達させずに直接再現する（`kill_reason` は同一
        // モジュール内のため private フィールドへ直接アクセスできる）。
        *worker
            .memory_monitor
            .kill_reason
            .lock()
            .expect("kill_reason mutex must not be poisoned") =
            Some(MonitorKillReason::ResourceLimitExceeded(12_345_678));

        let err = discard_context_error(&worker, "irrelevant stderr tail", || {
            JsEngineError::Timeout(
                "must not be used because the monitor already killed".to_string(),
            )
        });
        match err {
            JsEngineError::ResourceLimitExceeded(msg) => {
                assert!(
                    msg.contains("12345678"),
                    "expected the exact observed RSS (12345678) in the message, got: {msg}"
                );
                assert!(
                    msg.contains("context was discarded"),
                    "expected the required phrase, got: {msg}"
                );
            }
            other => panic!("expected ResourceLimitExceeded, got: {other:?}"),
        }
        worker.terminate_now();
    }

    /// Cursor Bugbot レビュー指摘 #503「Monitor kills misclassified」の
    /// 単体テスト: 監視スレッドが何も記録していない場合、
    /// `discard_context_error` は `fallback` の結果をそのまま使う
    /// （ただし次のテストが検証する "context was discarded" の補完は
    /// 別途行う）。
    #[test]
    fn js_1_discard_context_error_uses_fallback_when_no_monitor_kill_recorded() {
        let child = spawn_long_lived_child_for_test();
        let mut worker =
            WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        let err = discard_context_error(&worker, "some stderr", || {
            JsEngineError::EngineUnavailable("boom".to_string())
        });
        match err {
            JsEngineError::EngineUnavailable(msg) => {
                assert_eq!(msg, "boom; context was discarded");
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
        worker.terminate_now();
    }

    /// Cursor Bugbot レビュー指摘 #503「fail-closed の経路が、子がすでに
    /// 終了している場合のプローブ失敗も『監視できない』と数えている」の
    /// 単体テスト（防御その 2）: 監視スレッドが競合により
    /// `ProbeUnavailable` を記録してしまっていても、stderr に V8 の
    /// fatal OOM メッセージが残っていれば、`discard_context_error` は
    /// `ResourceLimitExceeded` を返すこと（`EngineUnavailable` にしない）。
    /// `monitor_kill_reason_to_error`・`discard_context_error` のドキュメント
    /// コメントに書いた優先順位（1. 実測 RSS 超過 2. stderr の OOM
    /// メッセージ 3. ProbeUnavailable 4. fallback）のうち、2 が 3 より
    /// 優先されることを具体値で確認する。
    #[test]
    fn js_1_discard_context_error_prefers_stderr_oom_evidence_over_probe_unavailable() {
        let child = spawn_long_lived_child_for_test();
        let worker = WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        // 監視スレッドが（本来なら起きてほしくない競合により）
        // `ProbeUnavailable` を記録してしまった状態を直接再現する。
        *worker
            .memory_monitor
            .kill_reason
            .lock()
            .expect("kill_reason mutex must not be poisoned") =
            Some(MonitorKillReason::ProbeUnavailable {
                consecutive_failures: MAX_CONSECUTIVE_PROBE_FAILURES,
            });

        let stderr_tail = "Fatal JavaScript out of memory: Reached heap limit";
        let err = discard_context_error(&worker, stderr_tail, || {
            JsEngineError::Timeout("must not be used because stderr shows OOM evidence".to_string())
        });
        match err {
            JsEngineError::ResourceLimitExceeded(msg) => {
                assert!(
                    msg.contains("heap limit"),
                    "expected the message to describe the heap limit being exceeded, got: {msg}"
                );
                assert!(
                    msg.contains("context was discarded"),
                    "expected the required phrase, got: {msg}"
                );
            }
            other => panic!(
                "expected ResourceLimitExceeded (stderr OOM evidence must win over a \
                 ProbeUnavailable kill_reason), got: {other:?}"
            ),
        }
    }

    /// codex レビュー指摘 #503 P1「メモリ上限超過で子が終了しても評価
    /// 結果を成功として返し得る」の単体テスト: 監視スレッドが応答フレーム
    /// の到達とほぼ同時に上限超過を検出して既に `kill` していた場合、
    /// `apply_post_response_rss_check` は（この関数自身の同期プローブが
    /// 「子が既に居ない」ことにより `None` を返すため見た目上は無害に
    /// 見えても）`outcome` がたとえ `Ok`（評価成功）であっても、それを
    /// 握りつぶして `ResourceLimitExceeded` を返し、`keep_worker` を
    /// `false` にすること。
    #[test]
    fn js_1_apply_post_response_rss_check_discards_success_when_monitor_already_killed() {
        let child = spawn_long_lived_child_for_test();
        let mut worker =
            WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        // 監視スレッドが RSS 超過を検出して既に kill 済みの状態を、
        // 実際にしきい値へ到達させずに直接再現する（他のテストと同様の
        // 手法。`kill_reason` は同一モジュール内のため private フィールド
        // へ直接アクセスできる）。
        *worker
            .memory_monitor
            .kill_reason
            .lock()
            .expect("kill_reason mutex must not be poisoned") =
            Some(MonitorKillReason::ResourceLimitExceeded(999_999_999));

        let success_outcome = Ok(JsValue::Number(42.0));
        let (outcome, keep) = apply_post_response_rss_check(&mut worker, success_outcome, true);

        assert!(
            !keep,
            "the worker must not be kept when the monitor thread had already killed it"
        );
        match outcome {
            Err(JsEngineError::ResourceLimitExceeded(msg)) => {
                assert!(
                    msg.contains("999999999"),
                    "expected the exact observed RSS (999999999) in the message, got: {msg}"
                );
                assert!(
                    msg.contains("context was discarded"),
                    "expected the required phrase, got: {msg}"
                );
            }
            other => panic!(
                "expected a successful evaluation result to be discarded and replaced with \
                 ResourceLimitExceeded once the monitor thread's kill is observed, got: {other:?}"
            ),
        }
    }

    /// 上記テストと対（codex レビュー指摘 #503 P1 の同じ観点）: `outcome`
    /// が（子を生かしたまま扱うつもりだった）`EvaluationFailed` のような
    /// エラーであっても、監視スレッドが既に `kill` していれば、それを
    /// `ResourceLimitExceeded` へ差し替えること。
    #[test]
    fn js_1_apply_post_response_rss_check_discards_keep_alive_error_when_monitor_already_killed() {
        let child = spawn_long_lived_child_for_test();
        let mut worker =
            WorkerHandle::from_child(child).expect("failed to build a test WorkerHandle");

        *worker
            .memory_monitor
            .kill_reason
            .lock()
            .expect("kill_reason mutex must not be poisoned") =
            Some(MonitorKillReason::ResourceLimitExceeded(123_456));

        let keep_alive_error_outcome = Err(JsEngineError::EvaluationFailed(
            "ReferenceError: x is not defined".to_string(),
        ));
        let (outcome, keep) =
            apply_post_response_rss_check(&mut worker, keep_alive_error_outcome, true);

        assert!(
            !keep,
            "the worker must not be kept when the monitor thread had already killed it"
        );
        match outcome {
            Err(JsEngineError::ResourceLimitExceeded(msg)) => {
                assert!(
                    msg.contains("123456"),
                    "expected the exact observed RSS (123456) in the message, got: {msg}"
                );
            }
            other => panic!(
                "expected a keep-alive-style error (EvaluationFailed) to be discarded and \
                 replaced with ResourceLimitExceeded once the monitor thread's kill is observed, \
                 got: {other:?}"
            ),
        }
    }

    /// Cursor Bugbot レビュー指摘 #503「Discarded-context errors omit
    /// required phrase」の単体テスト: `ensure_discarded_phrase` が
    /// `EngineUnavailable`・`Timeout`・`ResourceLimitExceeded` のすべてに
    /// 文言を補完し、既に含まれている場合は重複させない（具体値で
    /// assert する）。
    #[test]
    fn js_1_ensure_discarded_phrase_appends_missing_phrase_for_all_discard_variants() {
        match ensure_discarded_phrase(JsEngineError::EngineUnavailable("boom".to_string())) {
            JsEngineError::EngineUnavailable(msg) => {
                assert_eq!(msg, "boom; context was discarded");
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
        match ensure_discarded_phrase(JsEngineError::Timeout("slow".to_string())) {
            JsEngineError::Timeout(msg) => {
                assert_eq!(msg, "slow; context was discarded");
            }
            other => panic!("expected Timeout, got: {other:?}"),
        }
        match ensure_discarded_phrase(JsEngineError::ResourceLimitExceeded("oom".to_string())) {
            JsEngineError::ResourceLimitExceeded(msg) => {
                assert_eq!(msg, "oom; context was discarded");
            }
            other => panic!("expected ResourceLimitExceeded, got: {other:?}"),
        }
        // 既に文言が含まれる場合は重複させない。
        match ensure_discarded_phrase(JsEngineError::EngineUnavailable(
            "boom; context was discarded".to_string(),
        )) {
            JsEngineError::EngineUnavailable(msg) => {
                assert_eq!(msg, "boom; context was discarded");
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
    }
}

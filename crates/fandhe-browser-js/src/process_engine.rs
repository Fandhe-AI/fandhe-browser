//! JS 評価用の子プロセスへの親側プロキシ（`JS-1`・`TASK-29`・Issue #503
//! 「JS プロセス分離」設計書 §3.2・§3.3・§3.4・§7 W4）。
//!
//! 呼び出し元（将来）: `TASK-29.6`（Issue #157）で `create_engine` から
//! 配線され、`impl JsEngine for V8ProcessEngine` を追加する（現時点では
//! inherent メソッドとして `evaluate_script`・`inject_global_function` を
//! 提供するに留める。設計書 §7「案 X」4）。
//!
//! [`V8ProcessEngine`] は 1 つの子プロセス（`super::worker` を
//! `FANDHE_BROWSER_JS_WORKER` 環境変数で起動したもの）を遅延生成し、
//! 寿命のあいだ持ち続ける。子は同じ実行ファイルを自己再実行する
//! （専用のワーカーバイナリを持たない。設計書 §3.1）。
//!
//! 遅延起動のタイミングは「`new()` の後の**初回の注入または評価**」
//! である（`TASK-29.4`・Issue #155。[`V8ProcessEngine::inject_global_function`]
//! は登録を即時に子へ届けて成否を返すため、子が無ければ注入時に起動する）。
//! `new()` の時点では起動しない（PERF-6・PERF-7・CORE-3）。
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
//! | 子から不正な `NativeCall` フレームを受け取った（デコード失敗・未登録 id） | [`JsEngineError::EngineUnavailable`] | 破棄（`dispatch_native_call`・`TASK-29`・Issue #526） |
//! | `NativeFn` の実行終了後、評価開始時の期限を過ぎていた | [`JsEngineError::Timeout`] | 破棄（`NativeReturn` を送らずに `kill` する。`TASK-29`・Issue #526） |
//! | それ以外の異常終了・プロトコル違反・起動失敗 | [`JsEngineError::EngineUnavailable`] | 破棄 |
//!
//! Context が破棄される場合、メッセージに "context was discarded" を
//! 含める（security.md「偽装・回避機能の禁止」──状態が失われたことを
//! 隠さない）。
//!
//! # 親側 `NativeCall` dispatch（`TASK-29`・Issue #526）
//!
//! 子から `NATIVE_CALL`（[`worker_protocol::tag::NATIVE_CALL`]）フレームが
//! 届くと、[`V8ProcessEngine::native_fns`] に登録済みの [`NativeFn`] を
//! 実行し、結果を `NATIVE_RETURN` フレームとして送り返す（[`dispatch_native_call`]
//! ・[`V8ProcessEngine::send_evaluate_and_await`]）。登録は本番の注入 API
//! [`V8ProcessEngine::inject_global_function`]（親→子の登録フレーム
//! `REGISTER_GLOBAL_FUNCTION`。`TASK-29.4`・Issue #155）が担う。子の再起動
//! 時の再登録は次節のとおり Issue #527 で扱う。
//!
//! # 子の再起動と登録し直し（`TASK-29`・Issue #527）
//!
//! 子が OOM・クラッシュ・`kill`・プロトコル違反で破棄されると、次の
//! [`V8ProcessEngine::evaluate_script`] が新しい子を起動する。このとき:
//!
//! - **登録し直すもの**: ホストが登録した注入関数の「グローバル名と id」
//!   （`V8ProcessEngine::host_bindings`。エンジンの寿命のあいだ親が保持
//!   する）。子の起動（初回・再起動とも）のたびに、`spawn_worker` が Hello
//!   検証の直後に登録簿を 1 件ずつ登録フレームで新しい子へ送って応答を
//!   待つ（`TASK-29.4`・Issue #155）。親側の `native_fns`（id から
//!   [`NativeFn`] への対応）はもともとエンジンに残る
//! - **登録し直さないもの**: スクリプトが作ったグローバル変数・関数・状態
//!   のすべて。失われたことは、破棄時のエラーメッセージ（"context was
//!   discarded" と "the next evaluation runs in a fresh context"）で
//!   明示する（security.md「偽装・回避機能の禁止」）
//!
//! 稼働中の子への登録も同じ登録フレームで即時に行う
//! （[`V8ProcessEngine::inject_global_function`]）。DOM 風オブジェクト（`bind_dom_like_object`）の登録し直しは未対応で、
//! 前提の #524・#525（`TASK-29.5a`・`TASK-29.5b`）で登録簿へ種別を足して
//! 同じ差し込み口から扱う。
//!
//! `NativeFn` の実行時間は、評価開始時に 1 度だけ計算する期限
//! （[`EVALUATE_RECV_TIMEOUT`]）に含まれる。この期限は
//! [`super::engine_trait::NativeCallContext`] 経由で `NativeFn` 自身にも
//! 渡す（協調的に打ち切れるようにするため）。加えて親は `NativeFn` を
//! 専用スレッドで実行し、期限まで待ってから戻らなければ待機を打ち切り、
//! 子を `kill` して `JsEngineError::Timeout` を返す（`NativeFn` が期限を
//! 無視しても `evaluate_script` は期限内に戻る。codex レビュー指摘対応）。
//! そのため親側の関数型は `Send` を要求する [`ParentNativeFn`] とする。
//! 戻った時点で既に期限を過ぎていれば `NATIVE_RETURN` を送らずに `kill`
//! する（`NativeFn` の実行が 2 秒
//! （[`super::v8_engine::SCRIPT_EXECUTION_TIMEOUT`]）以上 3 秒
//! （[`EVALUATE_RECV_TIMEOUT`]）未満であれば、返信を受けた子の watchdog が
//! 先に発火し `Error{kind=Timeout}` を返す）。
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - DOM 風オブジェクトの再登録は #525 で対応する（Issue #527）。
//!   `impl JsEngine for V8ProcessEngine`（`TASK-29.6`・Issue #157）は未実装で、
//!   トレイトの `NativeFn`（`Send` なし）と本型の [`ParentNativeFn`]（`Send`
//!   あり）の不一致の橋渡しもそこで決める
//! - 期限を過ぎて放棄された `NativeFn` のスレッドが `Mutex` を握ったままの
//!   登録は、子を再起動しても `try_lock` に失敗し続けて JS 側へエラーを
//!   返す（再起動しても直らない。Issue #527）
//! - 期限を超えて戻らない `NativeFn` のスレッドは強制終了できず、戻るか
//!   プロセスが終了するまで残る。ただしホスト全体の生存数は
//!   `MAX_LIVE_NATIVE_THREADS`（64）で強制的に頭打ちにし、超過した呼び出し
//!   はスレッドを起動せず JS 向けのエラーにする。期限切れ時は
//!   [`super::engine_trait::NativeCallContext::is_cancelled`] を立てて
//!   放棄を通知する（協調的な契約。無視する `ParentNativeFn` は止められず、
//!   その副作用の重複実行の防止は登録側の責務。強制停止にはプロセス分離が
//!   必要で別途判断を要する）
//! - release ビルドは `panic = "abort"` のため、`NativeFn` が panic すると
//!   ホストプロセスごと終了する（unwind するビルドではスレッド内に閉じる
//!   が、release では防げない。プロセス分離が必要で別途判断を要する）。
//!   panic させないのは `NativeFn` を実装する呼び出し元の責務である
//! - [`NativeFn`] へ渡す引数は子から届く値（JS 由来）であり、untrusted な
//!   入力として検証するのは `NativeFn` を実装する側の責務である
//!   （`engine_trait.rs` の [`NativeFn`] ドキュメントコメント参照）
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

use super::engine_trait::{EngineKind, EvaluateOptions, JsEngineError, JsValue, NativeCallContext};
// ドキュメントコメントのリンク解決専用（実行時には使わない）。
#[cfg(doc)]
use super::engine_trait::NativeFn;
use super::worker_protocol::{self, ErrorKind, NativeReturn, tag};

/// 親側の `NativeCall` dispatch が実行するネイティブ関数の型
/// （`TASK-29`・Issue #526）。
///
/// [`NativeFn`] と同じ呼び出し規約だが、`Send` 境界を付ける。親プロセスの
/// dispatch は `NativeFn` を専用スレッドで実行し、期限内に戻らなくても
/// 呼び出し側が制御を回復できるようにするため（codex レビュー指摘
/// 「`NativeFn` が戻らない場合に評価期限を強制できない」対応）。V8 の
/// `Isolate` は子プロセス側にしか存在しないため、親側の関数に `Send` を
/// 要求しても V8 実装の制約とは衝突しない。公開トレイト
/// （`engine_trait.rs` の [`NativeFn`]）は変更しない。
#[doc(hidden)]
pub type ParentNativeFn =
    Box<dyn FnMut(&[JsValue], &NativeCallContext) -> Result<JsValue, JsEngineError> + Send>;

/// [`V8ProcessEngine::native_fns`] の 1 要素。実行用スレッドへ渡せるよう
/// `Arc<Mutex<..>>` で保持する。戻らない呼び出しが `Mutex` を保持し続けた
/// 場合、次回以降の呼び出しは `try_lock` が失敗して即座にエラーとなる
/// （待機スレッドを積み上げない）。
type NativeEntry = Arc<Mutex<ParentNativeFn>>;

/// ハンドシェイク（`Hello` フレームの受信）を待つ上限時間（設計書
/// §3.1「5 秒でタイムアウト」）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 登録フレーム 1 回分（書き込み＋応答待ちの合計。`spawn_worker` の
/// 再登録は全件で 1 回だけ計算する）の上限時間（`TASK-29.4`・Issue #155）。
/// ハンドシェイクと同じ値を使う。
const REGISTER_ACK_TIMEOUT: Duration = HANDSHAKE_TIMEOUT;

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
///
/// `NativeCall` の往復（`TASK-29`・Issue #526）もこの期限に含まれる:
/// `NativeFn` の実行時間・`NATIVE_RETURN` の書き込み待ちのいずれも、この
/// 1 つの期限を使い回す（`dispatch_native_call`・[`write_frame_with_deadline`]
/// のドキュメントコメント参照。期限を延長する経路は無い）。
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

/// [`V8ProcessEngine::native_fns`] に登録できる [`NativeFn`] の最大件数
/// （`TASK-29`・Issue #526）。
///
/// 登録 id は `Vec` の添字（`u32`）であり、登録 API
/// （[`V8ProcessEngine::inject_global_function`]。`TASK-29.4`・Issue #155）が
/// 無制限確保の経路にならないよう上限を設ける（coding-rust.md「長さ・件数を
/// 上限検証してからアロケーションに使う」）。子が独立に検査する上限
/// （[`worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS`]）と同じ値を共有する。
const MAX_NATIVE_FUNCTIONS: usize = worker_protocol::MAX_REGISTERED_GLOBAL_FUNCTIONS;

/// ホストプロセス内で同時に生存できる `NativeFn` 実行スレッドの上限
/// （`TASK-29`・Issue #526・codex レビュー指摘「期限切れの処理を
/// ホスト内に残し続けない」対応）。
///
/// 期限を超えて戻らない `NativeFn` のスレッドは強制終了できないため、
/// 評価を繰り返されると関数ごと・エンジンごとにスレッドが残りうる。
/// 生存数をこの値で頭打ちにし、超過した呼び出しは新たにスレッドを
/// 起動せず JS 向けの `NativeReturn::Err` にする（戻ったスレッドの
/// 分だけ枠は自動的に空く）。
const MAX_LIVE_NATIVE_THREADS: usize = 64;

/// 生存中の `NativeFn` 実行スレッド数（[`MAX_LIVE_NATIVE_THREADS`] で制限）。
static LIVE_NATIVE_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `NativeFn` 実行スレッドの枠。drop（正常終了・unwind）で枠を返す。
struct NativeThreadSlot<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> NativeThreadSlot<'a> {
    /// 生存数が `max` 未満なら枠を確保する。満杯なら `None`。
    fn try_acquire(counter: &'a std::sync::atomic::AtomicUsize, max: usize) -> Option<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()
            .map(|_| Self(counter))
    }
}

impl Drop for NativeThreadSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

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
        /// 子の起動直後（Hello を送る前）に事前登録させる逆方向 RPC
        /// プロキシの一覧（`name`・`id` の組。`TASK-29`・Issue #526）。
        /// 空なら何もしない（本番の `new()` は常に空のため、本番の子には
        /// 影響しない）。
        ///
        /// 受け入れ条件 2（親側の `NativeCall` dispatch が子の watchdog に
        /// 含まれること）は、プロキシを登録した**実際の子プロセス**で
        /// しか検証できない。親側に `NativeFn` の無い未登録 id のプロキシを
        /// 作るテスト専用の経路として、`spawn_worker` がこの一覧を
        /// `super::worker::TEST_NATIVE_PROXIES_ENV_VAR` へ組み立てて渡す
        /// （空なら環境変数自体を渡さない。空かどうかの確認自体は feature
        /// の有無に関わらず行い、環境変数名への参照だけを
        /// `#[cfg(feature = "test-support")]` で個別に gate する。
        /// `heap_limit_bytes` 等、他のフィールドと同じ作法）。
        pub native_proxies_for_test: Vec<(String, u32)>,
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

/// ホストが登録したグローバル関数の名前と id（`TASK-29`・Issue #527）。
///
/// [`V8ProcessEngine`] がエンジンの寿命のあいだ保持し、`spawn_worker` が
/// 子を起動するたびに登録フレーム経由で新しい子へ登録し直す。`id` は `V8ProcessEngine::native_fns` の添字と対応する。
/// DOM 風オブジェクトの登録は未対応で、#525（`TASK-29.5b`）が種別を足す。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostBinding {
    name: String,
    id: u32,
}

/// JS 評価を子プロセスへ分離して提供するエンジン（`JS-1`・Issue #503）。
///
/// 親側に登録するネイティブ関数は [`ParentNativeFn`]（`Send`）で、
/// 期限強制のため専用スレッドで実行する（`TASK-29`・Issue #526。
/// [`dispatch_native_call`] 参照）。
///
/// `#[doc(hidden)] pub` である理由: `tests/v8_worker.rs`（結合テスト。
/// 別クレートとしてコンパイルされる）が feature `test-support` 有効時に
/// `new_for_test`・`evaluate_script` 等のテスト専用入口を参照するには、
/// 型自体も `pub` である必要がある（`pub(crate)` は別クレートから見えない）。
/// テスト専用メソッド自体は `#[cfg(feature = "test-support")]` で個別に
/// gate する（[`WorkerSpawnConfigForTest`] のドキュメントコメント参照）。
/// `TASK-29.6`（Issue #157）で `create_engine` から配線し、
/// `impl JsEngine for V8ProcessEngine` を追加する（現時点では
/// inherent メソッドに留める）。
#[doc(hidden)]
pub struct V8ProcessEngine {
    worker: Option<WorkerHandle>,
    spawn_config: WorkerSpawnConfigForTest,
    /// 親側の `NativeCall` dispatch が実行する [`NativeFn`] の登録一覧
    /// （`TASK-29`・Issue #526）。登録 id は本 `Vec` の添字（`u32` へ変換
    /// して使う）であり、子から届く `NativeCall` フレームの `id` と対応
    /// づける（[`dispatch_native_call`] 参照）。[`MAX_NATIVE_FUNCTIONS`]
    /// を超えて登録することはできない
    /// （[`inject_global_function`](Self::inject_global_function) が検証する）。
    ///
    /// 登録経路は [`inject_global_function`](Self::inject_global_function)
    /// （`TASK-29.4`・Issue #155）だけで、`new()` の直後は空である。
    native_fns: Vec<NativeEntry>,
    /// ホストが登録した注入関数の名前と id の登録簿（`TASK-29`・Issue #527）。
    /// `worker` を破棄しても残り、次に起動する子へ `spawn_worker` が
    /// 登録し直す（`native_fns` と同じ寿命）。
    host_bindings: Vec<HostBinding>,
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
            native_fns: Vec::new(),
            host_bindings: Vec::new(),
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
            native_fns: Vec::new(),
            host_bindings: Vec::new(),
        }
    }

    /// グローバル関数 `name` として [`ParentNativeFn`] を注入する
    /// （`JS-1`「グローバル関数注入」・`TASK-29.4`・Issue #155）。
    ///
    /// 子プロセスの永続 Context のグローバルスコープへ、逆方向 RPC の
    /// プロキシ関数を登録フレーム（`REGISTER_GLOBAL_FUNCTION`）で即時に
    /// 登録し、応答を待って成否を返す。成功した関数は JS から通常の関数
    /// として呼べ、呼び出しは `NATIVE_CALL` で親へ届いて `func` が実行され、
    /// 戻り値が JS へ返る（[`dispatch_native_call`]）。登録した名前と id は
    /// エンジンの寿命のあいだ保持され、子が破棄された後の再起動でも
    /// 登録し直される（Issue #527。スクリプトが作った状態は戻らない）。
    ///
    /// 子が無ければこの呼び出しで起動する（`globalThis` 上で non-configurable
    /// な名前の注入失敗を、原因から離れた後続の起動で初めて表面化させない
    /// ため）。`new()` の時点では起動しない点は変わらない（PERF-6・PERF-7）。
    ///
    /// 次のいずれかなら [`JsEngineError::BindingFailed`] を返し、登録簿を
    /// 変更しない（fail-closed）。`Binding` 失敗では子と Context は残る:
    ///
    /// - `name` が空・長すぎる・既存の登録と重複する、件数が
    ///   [`MAX_NATIVE_FUNCTIONS`] に達している
    /// - 子が登録を拒否した（`undefined` 等 non-configurable な名前）
    ///
    /// 子の応答が想定外・期限切れ・子の異常終了なら、子を破棄して
    /// `EngineUnavailable` 等を返す（メッセージに "context was discarded"）。
    ///
    /// 未実装（REPAIR-3）: トレイト [`super::engine_trait::JsEngine`] の
    /// `inject_global_function` への集約は `TASK-29.6`（Issue #157）。
    /// トレイトの [`NativeFn`]（`Send` なし）とここでの [`ParentNativeFn`]
    /// （`Send` あり）の橋渡しも 29.6 で決める。
    #[doc(hidden)]
    pub fn inject_global_function(
        &mut self,
        name: &str,
        func: ParentNativeFn,
    ) -> Result<(), JsEngineError> {
        // 何も確保・送信する前に検証する（fail-closed）。
        let id = u32::try_from(self.native_fns.len()).map_err(|_| {
            JsEngineError::BindingFailed(
                "native function registry index does not fit in a u32".to_string(),
            )
        })?;
        validate_new_host_binding(
            &self.host_bindings,
            &self.spawn_config.native_proxies_for_test,
            name,
        )?;

        let mut worker = self.take_live_worker()?;
        let deadline = Instant::now() + REGISTER_ACK_TIMEOUT;
        match register_one(&mut worker, id, name, deadline) {
            Ok(RegisterAck::Registered) => {
                self.native_fns.push(Arc::new(Mutex::new(func)));
                self.host_bindings.push(HostBinding {
                    name: name.to_string(),
                    id,
                });
                self.worker = Some(worker);
                Ok(())
            }
            Ok(RegisterAck::Rejected(message)) => {
                // 登録簿は変更しない。子と Context は生きている。
                self.worker = Some(worker);
                Err(JsEngineError::BindingFailed(message))
            }
            // `register_one` が子を破棄済み。`worker` はここで drop される。
            Err(err) => Err(err),
        }
    }

    /// 生きている子を取り出す（無ければ起動する）。待機中に監視スレッドが
    /// RSS 超過で `kill` していた子は使わずエラーにする
    /// （[`Self::evaluate_script`]・[`Self::inject_global_function`] 共通。
    /// codex・Bugbot レビュー指摘 #503 P0「評価と評価の間も監視されて
    /// いない」対応）。呼び出し元は使い終えたら `self.worker` へ戻す。
    fn take_live_worker(&mut self) -> Result<WorkerHandle, JsEngineError> {
        let mut worker = match self.worker.take() {
            Some(worker) => worker,
            None => self.spawn_worker()?,
        };
        if let Some(reason) = worker.memory_monitor.take_kill_reason() {
            let tail = worker.reap_and_collect_stderr();
            let err = monitor_kill_reason_to_error(
                reason,
                &tail,
                "while idle between evaluations",
                worker.rss_threshold_bytes,
            );
            // `worker` はここで drop され、次回の呼び出しで起動し直す。
            return Err(ensure_discarded_phrase(err));
        }
        Ok(worker)
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

        // 待機中の kill の確認を含む（`take_live_worker`）。
        let mut worker = self.take_live_worker()?;

        let (outcome, keep_worker) =
            Self::send_evaluate_and_await(&mut worker, script, &self.native_fns);
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

    /// 子プロセスを起動し、ハンドシェイクの後、ホスト登録簿の注入関数を
    /// 登録フレームで登録し直す（初回・再起動とも。`TASK-29.4`・Issue #155、
    /// #527）。登録に失敗した子は破棄して `EngineUnavailable` を返す。
    fn spawn_worker(&self) -> Result<WorkerHandle, JsEngineError> {
        let mut worker = self.spawn_and_handshake()?;
        // 期限は Hello 受信後に 1 回だけ計算する。
        let deadline = Instant::now() + REGISTER_ACK_TIMEOUT;
        for binding in &self.host_bindings {
            match register_one(&mut worker, binding.id, &binding.name, deadline)? {
                RegisterAck::Registered => {}
                RegisterAck::Rejected(message) => {
                    // 一度成功した名前が fresh な Context で失敗するのは想定外
                    // （子の組み込みが変わった等）。半端な状態の子を使わない。
                    worker.terminate_now();
                    let tail = worker.reap_and_collect_stderr();
                    return Err(discard_context_error(&worker, &tail, || {
                        JsEngineError::EngineUnavailable(format!(
                            "JS worker process rejected re-registration of the global function \
                             {:?}: {message}; stderr: {tail}",
                            binding.name
                        ))
                    }));
                }
            }
        }
        Ok(worker)
    }

    /// 子プロセスを起動し、ハンドシェイク（`Hello` の受信）まで完了させる。
    fn spawn_and_handshake(&self) -> Result<WorkerHandle, JsEngineError> {
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
        // テスト専用: 親側に `NativeFn` の無い未登録 id のプロキシ一覧。
        // 本番の注入は登録フレームで行う（`spawn_worker`）。
        if let Some(value) =
            child_native_proxies_env_value(&self.spawn_config.native_proxies_for_test)?
        {
            #[cfg(feature = "test-support")]
            command.env(super::worker::TEST_NATIVE_PROXIES_ENV_VAR, value);
            #[cfg(not(feature = "test-support"))]
            {
                // `native_proxies_for_test` は本番の `new()` では常に空で、この
                // 分岐へは到達しない。空でない値を黙って捨てない
                // （実装済みを装わない。REPAIR-3）。
                let _ = value;
                return Err(JsEngineError::EngineUnavailable(
                    "test-only native proxies can only be passed to the JS worker process in \
                     builds with the test-support feature"
                        .to_string(),
                ));
            }
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
                // 親側の `NATIVE_CALL` dispatch（`dispatch_native_call`。
                // `TASK-29`・Issue #526）は評価中に届いた場合の経路であり、
                // ハンドシェイク段階で受け取った場合はプロトコル違反として
                // fail-closed に kill する。
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

    /// `Evaluate` フレームを送り、応答（`Result`/`Error`/`NativeCall`/
    /// 異常終了）を待つ。戻り値の 2 つ目は「この `worker` を保持し続けて
    /// よいか」を表す（`false` の場合、呼び出し元が `worker` を drop し、
    /// 次回は新しい子を起動し直す）。
    ///
    /// 書き込み・応答待ち・`NativeCall` の往復すべてを、評価開始時に
    /// 1 度だけ計算する期限（[`EVALUATE_RECV_TIMEOUT`]）で管理する（codex
    /// レビュー指摘 #503 P1「書き込みが呼び出しスレッドで同期的に実行され、
    /// パイプが満杯だと期限も kill も効かない」対応。書き込み自体は writer
    /// スレッドへ委譲し、このスレッドは `write_ack_rx` を期限付きで
    /// 待つだけにする。書き込み処理は [`write_frame_with_deadline`] へ
    /// 抽出し、`Evaluate` の送信・`NativeReturn` の返信の両方で使う）。
    ///
    /// `native_fns` は [`V8ProcessEngine::native_fns`] の可変借用
    /// （`TASK-29`・Issue #526）。子から `NATIVE_CALL` フレームが届くたびに
    /// [`dispatch_native_call`] でここから `NativeFn` を実行し、
    /// `NATIVE_RETURN` を返信してから次のフレームを待つ（受け入れ条件 1）。
    fn send_evaluate_and_await(
        worker: &mut WorkerHandle,
        script: &str,
        native_fns: &[NativeEntry],
    ) -> (Result<JsValue, JsEngineError>, bool) {
        let deadline = Instant::now() + EVALUATE_RECV_TIMEOUT;

        let payload = worker_protocol::encode_evaluate(script);
        if let Err(err) =
            write_frame_with_deadline(worker, tag::EVALUATE, &payload, deadline, "the script")
        {
            return (Err(err), false);
        }

        loop {
            let recv_wait = deadline.saturating_duration_since(Instant::now());
            // codex レビュー指摘（P1）対応: `recv_timeout(Duration::ZERO)` は
            // キューに残っているフレームを返せるため、期限後に届いていた
            // `RESULT` を成功として返したり `NATIVE_CALL` の `NativeFn` を
            // 新たに実行したりしてしまう。受信結果の種類を問わず、フレームを
            // 処理する前に期限を判定し、期限切れなら子を破棄して `Timeout`
            // を返す（`Timeout` 分岐と同じ扱い）。
            let received = match worker.frame_rx.recv_timeout(recv_wait) {
                Ok(_) if Instant::now() >= deadline => Err(mpsc::RecvTimeoutError::Timeout),
                other => other,
            };
            match received {
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
                        // codex レビュー指摘 #593 P1: 子が送る `ObjectHandle` は
                        // untrusted で、親の登録簿と照合する契約
                        // （`ObjectHandle` のドキュメント）。登録簿は Issue #525
                        // （`TASK-29.5b`）で導入するため、それまでは子 → 親の
                        // `ObjectHandle` を拒否する（fail-closed。未登録 ID が
                        // 親側で bind 済みオブジェクトとして流通しない）。
                        Ok((JsValue::ObjectHandle(handle), _)) => {
                            worker.terminate_now();
                            let tail = worker.reap_and_collect_stderr();
                            let converted = discard_context_error(worker, &tail, || {
                                JsEngineError::EngineUnavailable(format!(
                                    "JS worker process sent an unregistered object handle \
                                     {} in a Result frame; stderr: {tail}",
                                    handle.raw()
                                ))
                            });
                            (Err(converted), false)
                        }
                        Ok((value, consumed)) if consumed == payload.len() => (Ok(value), true),
                        Ok((_value, consumed)) => {
                            worker.terminate_now();
                            let tail = worker.reap_and_collect_stderr();
                            let converted = discard_context_error(worker, &tail, || {
                                JsEngineError::EngineUnavailable(format!(
                                    "JS worker process sent a Result frame with {} trailing \
                                     bytes after the decoded value ({consumed} of {} bytes \
                                     consumed); stderr: {tail}",
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
                                    "JS worker process sent a malformed Result frame: {err}; \
                                     stderr: {tail}"
                                ))
                            });
                            (Err(converted), false)
                        }
                    };
                    return apply_post_response_rss_check(worker, outcome, keep);
                }
                Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::ERROR => {
                    let (outcome, keep) = match worker_protocol::decode_error(&payload) {
                        // 子の watchdog による打ち切り。子プロセスは生き続け、
                        // Context も残る（設計書 §3.3 の表の 1 行目）。
                        Ok((ErrorKind::Timeout, message)) => (
                            Err(JsEngineError::Timeout(format!(
                                "script execution timed out inside the JS worker process: \
                                 {message}"
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
                                    "JS worker process sent a malformed Error frame: {err}; \
                                     stderr: {tail}"
                                ))
                            });
                            (Err(converted), false)
                        }
                    };
                    return apply_post_response_rss_check(worker, outcome, keep);
                }
                Ok(ReaderEvent::Frame(frame_tag, payload)) if frame_tag == tag::NATIVE_CALL => {
                    // `JS-1`・`TASK-29`・Issue #526: 子側のプロキシ関数
                    // （`v8_engine::native_proxy_callback`）が呼ばれた。
                    // `NativeFn` を実行し、結果を `NATIVE_RETURN` として
                    // 返信してから次のフレームを待つ（一問一答。
                    // `worker::StdioTransport::call` の契約と対になる）。
                    let native_return_payload =
                        match dispatch_native_call(native_fns, &payload, deadline) {
                            Ok(bytes) => bytes,
                            Err(NativeDispatchViolation::DeadlineExceeded) => {
                                // 戻らない `NativeFn` でも、ここで待機を打ち切り
                                // 子を kill して制御を回復する（`NativeFn` を
                                // 実行しているスレッドは強制終了できず放置される
                                // が、評価は期限内に `Timeout` で終わる）。
                                worker.terminate_now();
                                let tail = worker.reap_and_collect_stderr();
                                let err = discard_context_error(worker, &tail, || {
                                    JsEngineError::Timeout(format!(
                                        "a native function invoked by the JS worker process did \
                                         not return before the evaluation deadline; stderr: \
                                         {tail}"
                                    ))
                                });
                                return (Err(err), false);
                            }
                            Err(violation) => {
                                worker.terminate_now();
                                let tail = worker.reap_and_collect_stderr();
                                let err = discard_context_error(worker, &tail, || {
                                    JsEngineError::EngineUnavailable(format!(
                                        "JS worker process sent an invalid NativeCall frame \
                                     ({violation}); stderr: {tail}"
                                    ))
                                });
                                return (Err(err), false);
                            }
                        };
                    // 受け入れ条件 2: `NativeFn` の実行時間もこの評価全体の
                    // 期限に含める。`NativeFn` から戻った直後、期限を
                    // 過ぎていれば `NATIVE_RETURN` を送らずに kill する
                    // （実行に 2 秒以上 3 秒未満かかった場合は、返信を
                    // 受けた子の watchdog が先に発火し `Error{Timeout}` を
                    // 返す。3 秒以上ならここで先に打ち切る）。
                    if Instant::now() >= deadline {
                        worker.terminate_now();
                        let tail = worker.reap_and_collect_stderr();
                        let err = discard_context_error(worker, &tail, || {
                            JsEngineError::Timeout(format!(
                                "a native function invoked by the JS worker process exceeded \
                                 the evaluation deadline; stderr: {tail}"
                            ))
                        });
                        return (Err(err), false);
                    }
                    if let Err(err) = write_frame_with_deadline(
                        worker,
                        tag::NATIVE_RETURN,
                        &native_return_payload,
                        deadline,
                        "the native call result",
                    ) {
                        return (Err(err), false);
                    }
                    continue;
                }
                Ok(ReaderEvent::Frame(other_tag, _)) => {
                    // 親が受け取るはずのないタグ（`HELLO`・`EVALUATE`・
                    // `SHUTDOWN` 等）。`RESULT`・`ERROR`・`NATIVE_CALL` は
                    // 上の分岐で扱い済みのため、ここに来るのは純粋な
                    // プロトコル違反である。fail-closed に子を kill する。
                    worker.terminate_now();
                    let tail = worker.reap_and_collect_stderr();
                    let err = discard_context_error(worker, &tail, || {
                        JsEngineError::EngineUnavailable(format!(
                            "JS worker process sent an unexpected frame (tag {other_tag}); \
                             stderr: {tail}"
                        ))
                    });
                    return (Err(err), false);
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
                            "JS worker process terminated unexpectedly before responding; \
                             stderr: {tail}"
                        ))
                    });
                    return (Err(err), false);
                }
                Ok(ReaderEvent::Invalid(desc)) => {
                    worker.terminate_now();
                    let tail = worker.reap_and_collect_stderr();
                    let err = discard_context_error(worker, &tail, || {
                        JsEngineError::EngineUnavailable(format!(
                            "JS worker process sent a malformed frame: {desc}; stderr: {tail}"
                        ))
                    });
                    return (Err(err), false);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    worker.terminate_now();
                    let tail = worker.reap_and_collect_stderr();
                    let err = discard_context_error(worker, &tail, || {
                        JsEngineError::Timeout(format!(
                            "script evaluation exceeded the JS worker deadline and the process \
                             was killed; the next evaluation runs in a fresh context; stderr: \
                             {tail}"
                        ))
                    });
                    return (Err(err), false);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    worker.terminate_now();
                    let tail = worker.reap_and_collect_stderr();
                    let err = discard_context_error(worker, &tail, || {
                        JsEngineError::EngineUnavailable(format!(
                            "JS worker process communication channel disconnected \
                             unexpectedly; stderr: {tail}"
                        ))
                    });
                    return (Err(err), false);
                }
            }
        }
    }
}

/// [`V8ProcessEngine::send_evaluate_and_await`] から、`Evaluate` の送信と
/// `NativeReturn` の返信の両方で使う書き込み処理（`stdin_tx` の有無・
/// `send` の失敗・ack の `Ok(Err)`・`Timeout`・`Disconnected` の 4 分岐）を
/// 抽出したもの（`TASK-29`・Issue #526）。
///
/// `frame_tag`・`payload` を [`worker_protocol::write_frame`] でフレーミング
/// してから writer スレッドへ渡し、`deadline` までに書き込み完了の通知
/// （`write_ack_rx`）を待つ。**失敗した場合は必ず子を `kill` → reap →
/// [`discard_context_error`] を通す**（フレーミング自体の失敗を含む。
/// 呼び出し元は本関数が `Err` を返したら常に `(Err(e), false)` を返す）。
/// フレーミング自体の失敗（実質到達不能）まで kill 対象にしているのは、
/// 複数の書き込み失敗経路をこの関数へ統合した結果であり、「エンコード
/// 失敗は子の責任」という設計判断を表すものではない（経路ごとの理由は
/// 各分岐のコメントを参照）。
/// `what` はエラーメッセージに埋め込む短い句（例: "the script"・"the
/// native call result"）。
fn write_frame_with_deadline(
    worker: &mut WorkerHandle,
    frame_tag: u8,
    payload: &[u8],
    deadline: Instant,
    what: &str,
) -> Result<(), JsEngineError> {
    let mut frame_bytes = Vec::new();
    // インメモリの `Vec<u8>` への書き込みは実質失敗しないが、
    // `write_frame` のシグネチャ（`impl Write`）に合わせて `Result` を
    // 扱う（coding-rust.md「外部入力」節。`payload` はスクリプト文字列・
    // `NativeFn` の戻り値に由来する外部入力である）。
    //
    // このブランチに到達した場合も他の失敗経路と同じく kill → reap →
    // `discard_context_error` を通す（Err を返したら呼び出し元は常に
    // `(Err(e), false)` にする関数の契約に合わせる）。子は何も悪いことを
    // していないため本来は `keep_worker=true` にしたい経路だが、
    // `send_evaluate_and_await` が個別に持っていた書き込み処理を
    // `write_frame_with_deadline` へ統合した際（TASK-29・Issue #526）に
    // 生じた副作用であり、意図した設計判断ではない。エンコード対象は
    // 呼び出し前に検証済みの script 文字列・`NativeReturn` であり実質
    // 到達不能のため、区別のための複雑化は見送っている（コードレビュー
    // 指摘）。
    if let Err(err) = worker_protocol::write_frame(&mut frame_bytes, frame_tag, payload) {
        worker.terminate_now();
        let tail = worker.reap_and_collect_stderr();
        return Err(discard_context_error(worker, &tail, || {
            JsEngineError::EngineUnavailable(format!(
                "failed to encode {what} as a frame: {err}; stderr: {tail}"
            ))
        }));
    }

    let Some(stdin_tx) = worker.stdin_tx.as_ref() else {
        worker.terminate_now();
        let tail = worker.reap_and_collect_stderr();
        return Err(discard_context_error(worker, &tail, || {
            JsEngineError::EngineUnavailable(format!(
                "JS worker process stdin is already closed while sending {what}; stderr: {tail}"
            ))
        }));
    };
    if stdin_tx.send(frame_bytes).is_err() {
        // writer スレッドが既に終了している（stdin が既に閉じている
        // 等）。子は既に死んでいる可能性が高い。監視スレッドが RSS
        // 超過で kill した直後（stdin 側が先に閉じる）である可能性も
        // あるため、そちらを優先して判定する（advisor 指摘 B 対応。
        // `discard_context_error` が優先順位を持つ）。
        worker.terminate_now();
        let tail = worker.reap_and_collect_stderr();
        return Err(discard_context_error(worker, &tail, || {
            JsEngineError::EngineUnavailable(format!(
                "JS worker process writer thread is no longer running while sending {what}; \
                 stderr: {tail}"
            ))
        }));
    }

    let write_wait = deadline.saturating_duration_since(Instant::now());
    match worker.write_ack_rx.recv_timeout(write_wait) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            // 子は既に死んでいる可能性が高い（broken pipe 等）。kill は
            // 冪等（既に終了したプロセスへの kill はエラーを返すだけで
            // 副作用は無い）ため、まず kill してから reap する
            // （`reap_and_collect_stderr` の順序前提を満たす）。
            worker.terminate_now();
            let tail = worker.reap_and_collect_stderr();
            Err(discard_context_error(worker, &tail, || {
                JsEngineError::EngineUnavailable(format!(
                    "failed to send {what} to the JS worker process (it may have crashed): \
                     {err}; stderr: {tail}; context was discarded"
                ))
            }))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 書き込みが期限内に終わらなかった（パイプが満杯で writer
            // スレッドがブロックしている等。codex レビュー指摘 #503 P1
            // 対応）。kill すれば子の stdin 読み取り端が消え、ブロック
            // していた書き込みは broken pipe で解放される（writer
            // スレッドは `WorkerHandle::drop` で後始末される）。
            worker.terminate_now();
            let tail = worker.reap_and_collect_stderr();
            Err(discard_context_error(worker, &tail, || {
                JsEngineError::Timeout(format!(
                    "sending {what} to the JS worker process exceeded the deadline and the \
                     process was killed; the next evaluation runs in a fresh context; stderr: \
                     {tail}"
                ))
            }))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            worker.terminate_now();
            let tail = worker.reap_and_collect_stderr();
            Err(discard_context_error(worker, &tail, || {
                JsEngineError::EngineUnavailable(format!(
                    "JS worker process writer thread disconnected unexpectedly while sending \
                     {what}; stderr: {tail}"
                ))
            }))
        }
    }
}

/// [`dispatch_native_call`] が検出した、子から届いた `NATIVE_CALL`
/// フレームのプロトコル違反（`TASK-29`・Issue #526）。
///
/// coding-rust.md「戻り値は将来拡張できる構造を持つ型にする」に従い、
/// 違反の種類を判別できる enum にする（真偽値・フラットな文字列にしない）。
/// 呼び出し元（`send_evaluate_and_await`）はいずれの variant もプロトコル
/// 違反として子を kill する（両者を区別してリトライ等を行う想定は無いが、
/// エラーメッセージで原因を明示するために分けている）。
#[derive(Debug)]
enum NativeDispatchViolation {
    /// [`worker_protocol::decode_native_call`] がペイロードのデコードに
    /// 失敗した（不正なバイト列・引数件数の上限超過等）。
    DecodeFailed(worker_protocol::ProtocolError),
    /// `id` に対応する [`NativeFn`] が登録されていない。`id` は子側
    /// （`v8_engine::native_proxy_callback`）が `data()` に設定したもので
    /// あり、JS 側から任意の値を選べるものではない。登録内容との食い違いは
    /// JS の実行時エラーではなく内部の不整合（または子プロセスの異常）と
    /// みなし、fail-closed に扱う。
    UnknownId { id: u32 },
    /// `NativeFn` が評価期限までに戻らなかった。プロトコル違反ではなく
    /// 期限切れであり、呼び出し元は [`JsEngineError::Timeout`] へ変換する。
    DeadlineExceeded,
    /// 引数に [`JsValue::ObjectHandle`] が含まれていた。親の handle 登録簿
    /// は Issue #525（`TASK-29.5b`）で導入するため、それまでは照合できない
    /// 子発の handle を fail-closed で拒否する。
    UnregisteredHandle { handle: u32 },
}

impl std::fmt::Display for NativeDispatchViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecodeFailed(err) => write!(f, "failed to decode the NativeCall frame: {err}"),
            Self::UnknownId { id } => write!(f, "no native function is registered for id {id}"),
            Self::DeadlineExceeded => {
                write!(f, "the native function did not return before the deadline")
            }
            Self::UnregisteredHandle { handle } => {
                write!(f, "object handle {handle} is not registered in the parent")
            }
        }
    }
}

/// 子から届いた 1 件の `NATIVE_CALL` フレームを処理する純粋関数
/// （`TASK-29`・Issue #526）。戻り値は成功時に送信する `NATIVE_RETURN`
/// フレームのペイロード。
///
/// 呼び出し元: [`V8ProcessEngine::send_evaluate_and_await`]。
///
/// 1. [`worker_protocol::decode_native_call`] でペイロードをデコードする。
///    失敗したら [`NativeDispatchViolation::DecodeFailed`] を返す
/// 2. `id` に対応する `native_fns` の要素を [`<[_]>::get_mut`] で取る
///    （添字アクセスは使わない。coding-rust.md「外部入力」節）。無ければ
///    [`NativeDispatchViolation::UnknownId`] を返す
/// 3. `NativeFn` を実行する。呼び出し元（`send_evaluate_and_await`）が
///    評価開始時に計算した `deadline` を [`NativeCallContext`] に載せて
///    渡す（`TASK-29`・Issue #526・codex レビュー指摘「`NativeFn` が戻ら
///    ない場合に評価期限が機能しない」対応。`NativeFn` が協調的に期限を
///    確認できるようにするための引数であり、下記「既知の制限」が示す
///    とおり強制する手段ではない）。`Ok(v)` なら [`NativeReturn::Ok`]、
///    `Err(e)` なら [`NativeReturn::Err`]（`e` は [`JsEngineError`]。
///    メッセージを [`worker_protocol::truncate_for_wire`] で
///    [`worker_protocol::MAX_ERROR_MESSAGE_BYTES`] 以内へ切り詰めてから
///    送る。子側の `decode_native_return` は上限超過の `Err` を fatal として
///    拒否するため、送信前に切り詰める必要がある）
/// 4. [`encode_bounded_native_return`] で、上限内に収まることを保証した
///    バイト列へ符号化する
///
/// # 実行境界と既知の制限（実装済みを装わない。REPAIR-3）
///
/// - `NativeFn` は呼び出しごとの専用スレッドで実行し、呼び出しスレッドは
///   `deadline` まで待つだけにする。期限内に戻らなければ待機を打ち切り
///   [`NativeDispatchViolation::DeadlineExceeded`] を返す（呼び出し元が子を
///   kill して `Timeout` にする）。ブロックしたスレッドは強制終了できず
///   放置されるが、同じ関数の次回呼び出しは `try_lock` 失敗で即エラーに
///   なり、ホスト全体でも生存スレッド数を [`MAX_LIVE_NATIVE_THREADS`]
///   （64）で頭打ちにする（超過分は起動せず `NativeReturn::Err`）
/// - 期限切れ済み（呼び出し時点、およびスレッド内の関数呼び出し直前）の
///   場合は `NativeFn` を起動しない
/// - panic は unwind するビルド（dev・test）では専用スレッド内に閉じ、JS への
///   `NativeReturn::Err` になる。release は `panic = "abort"` のため
///   `NativeFn` が panic するとホストプロセスごと終了する（スレッド分離では
///   防げない。防ぐにはプロセス分離が必要で、別途判断を要する）。
///   panic させないのは `NativeFn` を実装する呼び出し元の責務である
/// - `args` は子から届く値（JS 由来）であり、untrusted な入力として
///   検証するのは `NativeFn` を実装する側の責務である
///   （`engine_trait.rs` の [`NativeFn`] ドキュメントコメント参照）
fn dispatch_native_call(
    native_fns: &[NativeEntry],
    payload: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>, NativeDispatchViolation> {
    let (id, args) = worker_protocol::decode_native_call(payload)
        .map_err(NativeDispatchViolation::DecodeFailed)?;
    // 子発の `ObjectHandle` は親の登録簿（Issue #525）と照合できるまで拒否する。
    if let Some(JsValue::ObjectHandle(handle)) =
        args.iter().find(|v| matches!(v, JsValue::ObjectHandle(_)))
    {
        return Err(NativeDispatchViolation::UnregisteredHandle {
            handle: handle.raw(),
        });
    }
    // `id` は子が `data()` に設定した `u32`。`usize` への変換が失敗する
    // ことは現実的な対象プラットフォームでは起こらないが、外部入力の
    // 経路であるため `unwrap`/`expect` は使わず、変換に失敗した場合は
    // 「対応する登録が無い」と同じ扱いにする。
    let idx = usize::try_from(id).unwrap_or(usize::MAX);
    let Some(entry) = native_fns.get(idx) else {
        return Err(NativeDispatchViolation::UnknownId { id });
    };
    // 期限切れ後は `NativeFn` を起動しない。呼び出し元は既に `Timeout` として
    // 評価を破棄する側にいるため、ここから副作用を起こしてはならない
    // （`NativeCallContext` の契約。Issue #526・codex レビュー指摘）。
    if Instant::now() >= deadline {
        return Err(NativeDispatchViolation::DeadlineExceeded);
    }
    let entry = Arc::clone(entry);
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ctx = NativeCallContext::new(deadline, Arc::clone(&cancelled));
    // 期限切れで残ったスレッドが積み上がらないよう、生存数に上限を設ける。
    // 枠はスレッド内へ move し、終了（unwind 含む）まで保持する。
    let Some(slot) = NativeThreadSlot::try_acquire(&LIVE_NATIVE_THREADS, MAX_LIVE_NATIVE_THREADS)
    else {
        return Ok(encode_bounded_native_return(&NativeReturn::Err(
            "too many native function calls are still running".to_string(),
        )));
    };
    let (tx, rx) = mpsc::channel::<NativeReturn>();
    let spawned = std::thread::Builder::new()
        .name("fandhe-native-call".to_string())
        .spawn(move || {
            let _slot = slot;
            let ret = match entry.try_lock() {
                Ok(mut func) => {
                    // スレッド起動・ロック取得の間に期限切れ／取り消しになった場合は
                    // 副作用を起こさないよう、関数呼び出し直前に再確認する。
                    if ctx.is_cancelled() || Instant::now() >= ctx.deadline {
                        NativeReturn::Err(
                            "the native call was cancelled before it started".to_string(),
                        )
                    } else {
                        match func(&args, &ctx) {
                            Ok(value) => NativeReturn::Ok(value),
                            Err(err) => NativeReturn::Err(worker_protocol::truncate_for_wire(
                                &err.to_string(),
                            )),
                        }
                    }
                }
                // 以前の呼び出しが戻らないまま保持している。待機せず即エラーにし、
                // ブロックしたスレッドを積み上げない。
                Err(std::sync::TryLockError::WouldBlock) => NativeReturn::Err(
                    "the native function is still busy with a previous call".to_string(),
                ),
                Err(std::sync::TryLockError::Poisoned(_)) => NativeReturn::Err(
                    "the native function is unavailable after a previous panic".to_string(),
                ),
            };
            // 受信側が期限切れで既に手を引いていれば送信は失敗するが無害。
            let _ = tx.send(ret);
        });
    if spawned.is_err() {
        return Ok(encode_bounded_native_return(&NativeReturn::Err(
            "failed to start a thread for the native function".to_string(),
        )));
    }
    let wait = deadline.saturating_duration_since(Instant::now());
    let native_return = match rx.recv_timeout(wait) {
        Ok(ret) => ret,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 放棄を協調的に通知する（`NativeCallContext::is_cancelled`）。
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            return Err(NativeDispatchViolation::DeadlineExceeded);
        }
        // 送信前にスレッドが panic（unwind）した。ホストは落とさず JS 側の
        // 例外として返す（release の `panic = "abort"` では panic 時点で
        // プロセスごと終了するため、この経路には来ない。既知の制限を参照）。
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            NativeReturn::Err("the native function panicked".to_string())
        }
    };
    Ok(encode_bounded_native_return(&native_return))
}

/// [`dispatch_native_call`] が組み立てた [`NativeReturn`] を、送信可能な
/// バイト列へ符号化する（`TASK-29`・Issue #526）。
///
/// [`worker_protocol::encoded_native_return_len`] で、実際に符号化する
/// **前**に符号化後の長さを計算し、
/// [`worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`] 以内であることを
/// 確認してから [`worker_protocol::encode_native_return`] を呼ぶ（coding-rust.md
/// 「長さ・件数を上限検証してからアロケーションに使う」。大きな
/// `JsValue::String` を返す `NativeFn` であっても、上限超過が判明した
/// 時点では実際の値の確保・複製を伴う符号化をまだ行っていない。codex
/// レビュー指摘「`NativeReturn` のサイズを確保前に検証する」対応）。
/// 長さ計算に失敗した場合（`String` 長が `u32` に収まらない等）、または
/// 上限を超える場合は、黙って切り詰めず、必ず上限内に収まる固定文言の
/// [`NativeReturn::Err`] へ差し替えてから符号化する（上限超過を明示する
/// エラーにする。security.md「偽装・回避機能の禁止」）。フォールバック
/// 自体の符号化が失敗することは実質無いが、`unwrap`/`expect` は使わず
/// 空の `Vec`（子側は `Truncated` として拒否し、既存の経路でプロトコル
/// 違反として扱われる）にフォールバックする。
fn encode_bounded_native_return(value: &NativeReturn) -> Vec<u8> {
    let fits_within_limit = worker_protocol::encoded_native_return_len(value)
        .is_ok_and(|len| len <= worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);
    if fits_within_limit && let Ok(bytes) = worker_protocol::encode_native_return(value) {
        return bytes;
    }
    let fallback = NativeReturn::Err(format!(
        "native function result exceeds the maximum supported frame size of {} bytes",
        worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD
    ));
    worker_protocol::encode_native_return(&fallback).unwrap_or_default()
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

/// 注入するグローバル関数名として使えるかを検査する（空・長すぎるを拒否。
/// 名前は登録フレームで子へ渡り、`create_data_property` のキーになるだけで
/// スクリプトとして評価されないため、文字種は制限しない。`TASK-29.4`・
/// Issue #155）。
fn validate_binding_name(name: &str) -> Result<(), JsEngineError> {
    if name.is_empty() {
        return Err(JsEngineError::BindingFailed(
            "native function name must not be empty".to_string(),
        ));
    }
    if name.len() > worker_protocol::MAX_GLOBAL_FUNCTION_NAME_BYTES {
        return Err(JsEngineError::BindingFailed(format!(
            "native function name exceeds the maximum supported length of {} bytes",
            worker_protocol::MAX_GLOBAL_FUNCTION_NAME_BYTES
        )));
    }
    Ok(())
}

/// 環境変数の値（`name=id` を `,` で連結）を組み立てる。件数は
/// 呼び出し側が検証済みであること。
fn join_proxies<'a>(entries: impl Iterator<Item = (&'a str, u32)>) -> String {
    entries
        .map(|(name, id)| format!("{name}={id}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 新しいホスト登録（`name`）を追加してよいかを、確保・送信の前に検証する
/// （[`V8ProcessEngine::inject_global_function`] から呼ばれる純粋関数。
/// 子プロセスを使わずに単体テストできる。`TASK-29.4`・Issue #155）。
///
/// `existing` は登録済みの登録簿、`extra` は
/// `WorkerSpawnConfigForTest::native_proxies_for_test`（テスト専用の
/// 未登録 id プロキシ）。名前の重複は大文字小文字を区別した完全一致で
/// 判定する。件数は [`MAX_NATIVE_FUNCTIONS`] まで。
fn validate_new_host_binding(
    existing: &[HostBinding],
    extra: &[(String, u32)],
    name: &str,
) -> Result<(), JsEngineError> {
    validate_binding_name(name)?;
    let duplicate = existing.iter().any(|b| b.name == name) || extra.iter().any(|(n, _)| n == name);
    if duplicate {
        return Err(JsEngineError::BindingFailed(format!(
            "native function {name:?} is already registered"
        )));
    }
    if existing.len() >= MAX_NATIVE_FUNCTIONS {
        return Err(JsEngineError::BindingFailed(format!(
            "cannot register more than {MAX_NATIVE_FUNCTIONS} native functions"
        )));
    }
    Ok(())
}

/// テスト専用の未登録 id プロキシ（`native_proxies_for_test`）を子へ環境変数で
/// 渡す値に組み立てる（`spawn_and_handshake` から**毎回**呼ばれる）。
///
/// 環境変数の区切り（`,`・`=`）を壊す名前は拒否する（登録フレームには
/// 掛けない制約）。空なら `None`。件数・長さ・重複は多層防御として
/// 確認し、違反なら子を起動する前に `EngineUnavailable` を返す。
fn child_native_proxies_env_value(
    extra: &[(String, u32)],
) -> Result<Option<String>, JsEngineError> {
    if extra.is_empty() {
        return Ok(None);
    }
    let unavailable = |msg: String| JsEngineError::EngineUnavailable(msg);
    if extra.len() > super::worker::MAX_TEST_NATIVE_PROXIES {
        return Err(unavailable(format!(
            "too many native functions to install into the JS worker process (maximum {})",
            super::worker::MAX_TEST_NATIVE_PROXIES
        )));
    }
    for (i, (name, _)) in extra.iter().enumerate() {
        validate_binding_name(name).map_err(|e| unavailable(e.to_string()))?;
        if name.contains([',', '=', '\0']) {
            return Err(unavailable(
                "native function name must not contain ',', '=' or NUL".to_string(),
            ));
        }
        if extra.iter().take(i).any(|(n, _)| n == name) {
            return Err(unavailable(format!(
                "native function {name:?} is registered more than once"
            )));
        }
    }
    let value = join_proxies(extra.iter().map(|(n, i)| (n.as_str(), *i)));
    if value.len() > super::worker::MAX_TEST_NATIVE_PROXIES_ENV_VAR_BYTES {
        return Err(unavailable(format!(
            "native function registrations exceed the maximum supported length of {} bytes",
            super::worker::MAX_TEST_NATIVE_PROXIES_ENV_VAR_BYTES
        )));
    }
    Ok(Some(value))
}

/// 登録フレームへの子の応答の解釈結果（`TASK-29.4`・Issue #155）。
#[derive(Debug, PartialEq, Eq)]
enum RegisterAck {
    /// 登録に成功した（`RESULT(Undefined)`）。
    Registered,
    /// 子が登録を拒否した（`ERROR{Binding}`）。子と Context は生きている。
    Rejected(String),
}

/// 登録フレームへの想定外の応答（プロトコル違反）。子は破棄する。
#[derive(Debug, PartialEq, Eq)]
enum RegisterAckViolation {
    /// `RESULT` の値が `Undefined` ではない・decode できない。
    UnexpectedResult(String),
    /// `RESULT` の末尾に余分なバイトがある。
    TrailingBytes { extra: usize },
    /// `Binding` 以外の `ERROR`、または decode できない `ERROR`。
    UnexpectedError(String),
    /// `RESULT`・`ERROR` 以外のフレーム（`NATIVE_CALL` を含む）。
    UnexpectedFrame(u8),
}

impl std::fmt::Display for RegisterAckViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedResult(desc) => {
                write!(f, "unexpected Result frame for a registration: {desc}")
            }
            Self::TrailingBytes { extra } => {
                write!(f, "Result frame has {extra} trailing bytes")
            }
            Self::UnexpectedError(desc) => {
                write!(f, "unexpected Error frame for a registration: {desc}")
            }
            Self::UnexpectedFrame(tag) => {
                write!(f, "unexpected frame (tag {tag}) for a registration")
            }
        }
    }
}

/// 登録フレームへの子の応答フレームを厳密に解釈する純粋関数
/// （真偽値ではなく enum で返す。REPAIR-4）。
///
/// 成功は「ちょうど `Undefined` 1 個の `RESULT`」、拒否は「kind が `Binding`
/// の `ERROR`」のみ。それ以外はすべて違反として呼び出し元が子を破棄する。
fn interpret_register_ack(
    frame_tag: u8,
    payload: &[u8],
) -> Result<RegisterAck, RegisterAckViolation> {
    match frame_tag {
        tag::RESULT => match worker_protocol::decode_js_value(payload) {
            Ok((JsValue::Undefined, consumed)) if consumed == payload.len() => {
                Ok(RegisterAck::Registered)
            }
            Ok((JsValue::Undefined, consumed)) => Err(RegisterAckViolation::TrailingBytes {
                extra: payload.len().saturating_sub(consumed),
            }),
            Ok((value, _)) => Err(RegisterAckViolation::UnexpectedResult(format!(
                "expected undefined, got {value:?}"
            ))),
            Err(err) => Err(RegisterAckViolation::UnexpectedResult(err.to_string())),
        },
        tag::ERROR => match worker_protocol::decode_error(payload) {
            Ok((ErrorKind::Binding, message)) => Ok(RegisterAck::Rejected(message)),
            Ok((kind, message)) => Err(RegisterAckViolation::UnexpectedError(format!(
                "kind {kind:?}: {message}"
            ))),
            Err(err) => Err(RegisterAckViolation::UnexpectedError(err.to_string())),
        },
        other => Err(RegisterAckViolation::UnexpectedFrame(other)),
    }
}

/// 登録フレームを 1 件送り、応答を期限付きで待つ（`TASK-29.4`・Issue #155。
/// [`V8ProcessEngine::inject_global_function`] と `spawn_worker` の共通部）。
///
/// `Err` を返すときは必ず子を破棄済み（kill → reap → "context was
/// discarded" を含むエラー）。`Ok(Rejected)` では子と Context は生きている。
fn register_one(
    worker: &mut WorkerHandle,
    id: u32,
    name: &str,
    deadline: Instant,
) -> Result<RegisterAck, JsEngineError> {
    let payload = worker_protocol::encode_register_global_function(id, name);
    write_frame_with_deadline(
        worker,
        tag::REGISTER_GLOBAL_FUNCTION,
        &payload,
        deadline,
        "a global function registration",
    )?;

    let recv_wait = deadline.saturating_duration_since(Instant::now());
    // 期限後にキューに残っていた応答を成功として扱わない（評価待ちと同じ扱い）。
    let received = match worker.frame_rx.recv_timeout(recv_wait) {
        Ok(_) if Instant::now() >= deadline => Err(mpsc::RecvTimeoutError::Timeout),
        other => other,
    };
    let (description, timed_out): (String, bool) = match received {
        Ok(ReaderEvent::Frame(frame_tag, payload)) => {
            match interpret_register_ack(frame_tag, &payload) {
                Ok(ack) => return Ok(ack),
                Err(violation) => (
                    format!(
                        "JS worker process sent an invalid registration response ({violation})"
                    ),
                    false,
                ),
            }
        }
        Ok(ReaderEvent::Eof) => {
            // 子は既にストリームを閉じている。kill を挟まず reap する。
            let tail = worker.reap_and_collect_stderr();
            return Err(discard_context_error(worker, &tail, || {
                JsEngineError::EngineUnavailable(format!(
                    "JS worker process terminated unexpectedly during a global function \
                     registration; stderr: {tail}"
                ))
            }));
        }
        Ok(ReaderEvent::Invalid(desc)) => (
            format!("JS worker process sent a malformed frame during a registration: {desc}"),
            false,
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => (
            "global function registration exceeded the deadline and the JS worker process was \
             killed"
                .to_string(),
            true,
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => (
            "JS worker process communication channel disconnected unexpectedly during a \
             registration"
                .to_string(),
            false,
        ),
    };
    worker.terminate_now();
    let tail = worker.reap_and_collect_stderr();
    Err(discard_context_error(worker, &tail, || {
        if timed_out {
            JsEngineError::Timeout(format!("{description}; stderr: {tail}"))
        } else {
            JsEngineError::EngineUnavailable(format!("{description}; stderr: {tail}"))
        }
    }))
}
/// エラーメッセージに "context was discarded" が含まれていなければ
/// 追記する（[`discard_context_error`] が呼ぶ内部ヘルパー。Cursor Bugbot
/// レビュー指摘 #503「Discarded-context errors omit required phrase」
/// 対応）。
fn ensure_discarded_phrase(err: JsEngineError) -> JsEngineError {
    const PHRASE: &str = "context was discarded";
    const FRESH: &str = "the next evaluation runs in a fresh context";
    // 失われた状態と次回の挙動の両方を必ず明示する（Issue #527）。
    let complete = |mut msg: String| {
        if !msg.contains(PHRASE) {
            msg = format!("{msg}; {PHRASE}");
        }
        if !msg.contains(FRESH) {
            msg = format!("{msg}; {FRESH}");
        }
        msg
    };
    match err {
        JsEngineError::EngineUnavailable(msg) => JsEngineError::EngineUnavailable(complete(msg)),
        JsEngineError::Timeout(msg) => JsEngineError::Timeout(complete(msg)),
        JsEngineError::ResourceLimitExceeded(msg) => {
            JsEngineError::ResourceLimitExceeded(complete(msg))
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
        Err(ensure_discarded_phrase(
            JsEngineError::ResourceLimitExceeded(format!(
                "JS worker process RSS ({rss_bytes} bytes) exceeded the parent's monitoring \
                 threshold ({} bytes) immediately after responding; context was discarded; \
                 stderr: {tail}",
                worker.rss_threshold_bytes
            )),
        )),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(func: ParentNativeFn) -> NativeEntry {
        Arc::new(Mutex::new(func))
    }

    /// `TASK-29`・Issue #526: 登録件数の上限が具体値どおりであること
    /// （回帰確認）。
    #[test]
    fn js_1_max_native_functions_is_1024() {
        assert_eq!(MAX_NATIVE_FUNCTIONS, 1024);
    }

    /// `TASK-29`・Issue #526: 成功経路。登録済みの `NativeFn` が呼ばれ、
    /// 受け取った引数がそのまま反映された [`NativeReturn::Ok`] を返すこと。
    #[test]
    fn js_1_dispatch_native_call_invokes_the_registered_native_fn() {
        let seen_args: Arc<Mutex<Vec<JsValue>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_args_for_closure = Arc::clone(&seen_args);
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            move |args: &[JsValue], _ctx: &NativeCallContext| {
                *seen_args_for_closure.lock().unwrap() = args.to_vec();
                Ok(JsValue::Number(42.0))
            },
        ))];

        let payload = worker_protocol::encode_native_call(
            0,
            &[JsValue::Number(1.0), JsValue::String("a".to_string())],
        )
        .expect("encode must succeed");
        let result_payload = dispatch_native_call(
            &native_fns,
            &payload,
            Instant::now() + Duration::from_secs(30),
        )
        .expect("dispatch must succeed");

        assert_eq!(
            worker_protocol::decode_native_return(&result_payload).expect("decode must succeed"),
            NativeReturn::Ok(JsValue::Number(42.0))
        );
        assert_eq!(
            *seen_args.lock().unwrap(),
            vec![JsValue::Number(1.0), JsValue::String("a".to_string())]
        );
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘「`NativeFn` が戻らない
    /// 場合に評価期限が機能しない」対応: `dispatch_native_call` に渡した
    /// `deadline` が、そのまま [`NativeCallContext::deadline`] として
    /// `NativeFn` へ届くこと（`NativeFn` が協調的に期限を確認するための
    /// 契約が実際に機能していることの回帰確認）。
    #[test]
    fn js_1_dispatch_native_call_passes_the_deadline_via_native_call_context() {
        let seen_deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let seen_deadline_for_closure = Arc::clone(&seen_deadline);
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            move |_args: &[JsValue], ctx: &NativeCallContext| {
                *seen_deadline_for_closure.lock().unwrap() = Some(ctx.deadline);
                Ok(JsValue::Undefined)
            },
        ))];
        let payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");
        let deadline = Instant::now() + Duration::from_secs(30);

        dispatch_native_call(&native_fns, &payload, deadline).expect("dispatch must succeed");

        assert_eq!(
            *seen_deadline.lock().unwrap(),
            Some(deadline),
            "the NativeFn must observe the exact deadline passed to dispatch_native_call"
        );
    }

    /// `TASK-29`・Issue #526: `NativeFn` が `Err` を返した場合、
    /// [`NativeReturn::Err`] になり、長いメッセージは
    /// [`worker_protocol::MAX_ERROR_MESSAGE_BYTES`] 以内（文字境界を壊さない）
    /// に切り詰められること。
    #[test]
    fn js_1_dispatch_native_call_truncates_an_oversized_error_message() {
        let long_message = "a".repeat(worker_protocol::MAX_ERROR_MESSAGE_BYTES + 100);
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new({
            let long_message = long_message.clone();
            move |_args: &[JsValue], _ctx: &NativeCallContext| {
                Err(JsEngineError::EvaluationFailed(long_message.clone()))
            }
        }))];
        let payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");

        let result_payload = dispatch_native_call(
            &native_fns,
            &payload,
            Instant::now() + Duration::from_secs(30),
        )
        .expect("dispatch must succeed");
        match worker_protocol::decode_native_return(&result_payload).expect("decode must succeed") {
            NativeReturn::Err(message) => {
                assert!(message.len() <= worker_protocol::MAX_ERROR_MESSAGE_BYTES);
                assert!(std::str::from_utf8(message.as_bytes()).is_ok());
            }
            other => panic!("expected NativeReturn::Err, got: {other:?}"),
        }
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘: 期限を無視して戻らない
    /// `NativeFn` でも、`dispatch_native_call` は期限付近で
    /// [`NativeDispatchViolation::DeadlineExceeded`] を返すこと。
    #[test]
    fn js_1_dispatch_native_call_returns_deadline_exceeded_for_a_blocking_native_fn() {
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            |_args: &[JsValue], _ctx: &NativeCallContext| {
                std::thread::sleep(Duration::from_secs(5));
                Ok(JsValue::Undefined)
            },
        ))];
        let payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");

        let started = Instant::now();
        let result = dispatch_native_call(
            &native_fns,
            &payload,
            Instant::now() + Duration::from_millis(200),
        );
        assert!(matches!(
            result,
            Err(NativeDispatchViolation::DeadlineExceeded)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "dispatch must return near the deadline, took {:?}",
            started.elapsed()
        );

        // 戻らない呼び出しが残っている間の再呼び出しは、待たずに Err になる。
        let second = dispatch_native_call(
            &native_fns,
            &payload,
            Instant::now() + Duration::from_secs(30),
        )
        .expect("dispatch must succeed");
        match worker_protocol::decode_native_return(&second).expect("decode must succeed") {
            NativeReturn::Err(message) => assert!(message.contains("busy"), "got: {message}"),
            other => panic!("expected NativeReturn::Err, got: {other:?}"),
        }
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘: 生存スレッド枠が満杯なら
    /// 枠を確保できず、戻すと再び確保できること。
    #[test]
    fn js_1_native_thread_slot_is_capped_and_released_on_drop() {
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let a = NativeThreadSlot::try_acquire(&counter, 2).expect("first slot");
        let _b = NativeThreadSlot::try_acquire(&counter, 2).expect("second slot");
        assert!(NativeThreadSlot::try_acquire(&counter, 2).is_none());
        assert_eq!(counter.load(Ordering::Acquire), 2);
        drop(a);
        assert_eq!(counter.load(Ordering::Acquire), 1);
        assert!(NativeThreadSlot::try_acquire(&counter, 2).is_some());
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘: panic した `NativeFn` は
    /// （unwind するテストビルドでは）ホストを落とさず `NativeReturn::Err`
    /// になること。release の `panic = "abort"` では防げない（既知の制限）。
    #[test]
    fn js_1_dispatch_native_call_contains_a_native_fn_panic_when_unwinding() {
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            |_args: &[JsValue], _ctx: &NativeCallContext| -> Result<JsValue, JsEngineError> {
                panic!("intentional panic for test");
            },
        ))];
        let payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");

        let result_payload = dispatch_native_call(
            &native_fns,
            &payload,
            Instant::now() + Duration::from_secs(30),
        )
        .expect("dispatch must succeed");
        match worker_protocol::decode_native_return(&result_payload).expect("decode must succeed") {
            NativeReturn::Err(message) => {
                assert_eq!(message, "the native function panicked")
            }
            other => panic!("expected NativeReturn::Err, got: {other:?}"),
        }
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘: 期限切れ後は `NativeFn` を
    /// 起動せず `DeadlineExceeded` を返すこと（副作用を起こさない）。
    #[test]
    fn js_1_dispatch_native_call_does_not_invoke_native_fn_after_deadline() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_for_closure = Arc::clone(&called);
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            move |_args: &[JsValue], _ctx: &NativeCallContext| -> Result<JsValue, JsEngineError> {
                called_for_closure.store(true, std::sync::atomic::Ordering::Release);
                Ok(JsValue::Null)
            },
        ))];
        let payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");

        let result = dispatch_native_call(&native_fns, &payload, Instant::now());
        assert!(matches!(
            result,
            Err(NativeDispatchViolation::DeadlineExceeded)
        ));
        assert!(!called.load(std::sync::atomic::Ordering::Acquire));
    }

    /// `TASK-29`・Issue #526・codex レビュー指摘: 期限切れで待機を打ち切った際、
    /// 実行中の `ParentNativeFn` へ取り消しが通知されること。
    #[test]
    fn js_1_dispatch_native_call_signals_cancellation_after_deadline() {
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed_for_closure = Arc::clone(&observed);
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            move |_args: &[JsValue], ctx: &NativeCallContext| {
                let start = Instant::now();
                while !ctx.is_cancelled() && start.elapsed() < Duration::from_secs(10) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                observed_for_closure
                    .store(ctx.is_cancelled(), std::sync::atomic::Ordering::Release);
                Ok(JsValue::Undefined)
            },
        ))];
        let payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");
        let result = dispatch_native_call(
            &native_fns,
            &payload,
            Instant::now() + Duration::from_millis(50),
        );
        assert!(matches!(
            result,
            Err(NativeDispatchViolation::DeadlineExceeded)
        ));
        let wait_start = Instant::now();
        while !observed.load(std::sync::atomic::Ordering::Acquire)
            && wait_start.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(observed.load(std::sync::atomic::Ordering::Acquire));
    }

    /// `TASK-29`・Issue #526: 未登録の `id` は
    /// [`NativeDispatchViolation::UnknownId`] になること。
    #[test]
    fn js_1_dispatch_native_call_rejects_unknown_id() {
        let native_fns: Vec<NativeEntry> = Vec::new();
        let payload = worker_protocol::encode_native_call(7, &[]).expect("encode must succeed");
        assert!(matches!(
            dispatch_native_call(
                &native_fns,
                &payload,
                Instant::now() + Duration::from_secs(30)
            ),
            Err(NativeDispatchViolation::UnknownId { id: 7 })
        ));
    }

    /// Issue #593 P1: 子発の `ObjectHandle` 引数は登録簿が無い間拒否される。
    #[test]
    fn js_1_dispatch_native_call_rejects_unregistered_object_handle() {
        let native_fns: Vec<NativeEntry> =
            vec![entry(Arc::new(|_: &[JsValue], _: &NativeCallContext| {
                Ok(JsValue::Undefined)
            }))];
        let payload = worker_protocol::encode_native_call(
            0,
            &[JsValue::ObjectHandle(
                crate::engine_trait::ObjectHandle::from_raw(9),
            )],
        )
        .expect("encode must succeed");
        assert!(matches!(
            dispatch_native_call(
                &native_fns,
                &payload,
                Instant::now() + Duration::from_secs(30)
            ),
            Err(NativeDispatchViolation::UnregisteredHandle { handle: 9 })
        ));
    }

    /// `TASK-29`・Issue #526: 壊れたペイロード（途中で切れたもの）は
    /// [`NativeDispatchViolation::DecodeFailed`] になること（添字アクセスで
    /// panic しないことの確認）。
    #[test]
    fn js_1_dispatch_native_call_rejects_truncated_payload() {
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            |_: &[JsValue], _ctx: &NativeCallContext| Ok(JsValue::Undefined),
        ))];
        let payload = [0u8, 1u8, 2u8]; // id・argc のどちらの u32 も揃わない
        assert!(matches!(
            dispatch_native_call(
                &native_fns,
                &payload,
                Instant::now() + Duration::from_secs(30)
            ),
            Err(NativeDispatchViolation::DecodeFailed(_))
        ));
    }

    /// `TASK-29`・Issue #526: 余分な末尾バイトが付いたペイロードも
    /// `DecodeFailed` になること（`decode_native_call` の
    /// `TrailingBytes` 検証を通じて拒否される）。
    #[test]
    fn js_1_dispatch_native_call_rejects_payload_with_trailing_bytes() {
        let native_fns: Vec<NativeEntry> = vec![entry(Box::new(
            |_: &[JsValue], _ctx: &NativeCallContext| Ok(JsValue::Undefined),
        ))];
        let mut payload = worker_protocol::encode_native_call(0, &[]).expect("encode must succeed");
        payload.push(0xff);
        assert!(matches!(
            dispatch_native_call(
                &native_fns,
                &payload,
                Instant::now() + Duration::from_secs(30)
            ),
            Err(NativeDispatchViolation::DecodeFailed(_))
        ));
    }

    /// `TASK-29`・Issue #526: 結果の符号化後サイズが
    /// [`worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD`] を超える場合、
    /// 送信ペイロードが上限以内の `NativeReturn::Err` に差し替わること
    /// （黙って切り詰めず、明示的なエラーにする）。
    #[test]
    fn js_1_encode_bounded_native_return_replaces_oversized_results_with_a_bounded_error() {
        let oversized = "x".repeat(worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD + 1024);
        let value = NativeReturn::Ok(JsValue::String(oversized));

        let bytes = encode_bounded_native_return(&value);
        assert!(bytes.len() <= worker_protocol::MAX_FRAME_PAYLOAD_PARENT_TO_CHILD);
        match worker_protocol::decode_native_return(&bytes).expect("decode must succeed") {
            NativeReturn::Err(message) => {
                assert!(message.contains("exceeds the maximum supported frame size"));
            }
            other => panic!("expected NativeReturn::Err, got: {other:?}"),
        }
    }

    /// `TASK-29`・Issue #526: 上限以内の結果はそのまま符号化されること
    /// （回帰確認）。
    #[test]
    fn js_1_encode_bounded_native_return_keeps_small_results_untouched() {
        let value = NativeReturn::Ok(JsValue::Number(1.0));
        let bytes = encode_bounded_native_return(&value);
        assert_eq!(
            worker_protocol::decode_native_return(&bytes).expect("decode must succeed"),
            value
        );
    }

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
                assert_eq!(
                    msg,
                    "boom; context was discarded; the next evaluation runs in a fresh context"
                );
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

    /// Cursor Bugbot 指摘（#588）: 応答直後の同期 RSS 確認でしきい値超過を
    /// 検出した経路のメッセージにも、「失われた状態」と「次回の挙動」の
    /// 両フレーズが含まれること（Issue #527）。しきい値を 0 にして実 RSS
    /// で必ず超過させる。
    #[test]
    fn js_1_apply_post_response_rss_check_over_threshold_message_has_both_phrases() {
        let child = spawn_long_lived_child_for_test();
        let mut worker = WorkerHandle::from_child_with_rss_threshold(child, 0)
            .expect("failed to build a test WorkerHandle");

        let (outcome, keep) =
            apply_post_response_rss_check(&mut worker, Ok(JsValue::Number(1.0)), true);

        assert!(!keep, "the over-threshold worker must not be kept");
        match outcome {
            Err(JsEngineError::ResourceLimitExceeded(msg)) => {
                assert!(
                    msg.contains("context was discarded"),
                    "expected the discard phrase, got: {msg}"
                );
                assert!(
                    msg.contains("the next evaluation runs in a fresh context"),
                    "expected the fresh-context phrase, got: {msg}"
                );
            }
            other => panic!("expected ResourceLimitExceeded, got: {other:?}"),
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
                assert_eq!(
                    msg,
                    "boom; context was discarded; the next evaluation runs in a fresh context"
                );
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
        match ensure_discarded_phrase(JsEngineError::Timeout("slow".to_string())) {
            JsEngineError::Timeout(msg) => {
                assert_eq!(
                    msg,
                    "slow; context was discarded; the next evaluation runs in a fresh context"
                );
            }
            other => panic!("expected Timeout, got: {other:?}"),
        }
        match ensure_discarded_phrase(JsEngineError::ResourceLimitExceeded("oom".to_string())) {
            JsEngineError::ResourceLimitExceeded(msg) => {
                assert_eq!(
                    msg,
                    "oom; context was discarded; the next evaluation runs in a fresh context"
                );
            }
            other => panic!("expected ResourceLimitExceeded, got: {other:?}"),
        }
        // 既に文言が含まれる場合は重複させない。
        match ensure_discarded_phrase(JsEngineError::EngineUnavailable(
            "boom; context was discarded; the next evaluation runs in a fresh context".to_string(),
        )) {
            JsEngineError::EngineUnavailable(msg) => {
                assert_eq!(
                    msg,
                    "boom; context was discarded; the next evaluation runs in a fresh context"
                );
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
    }

    /// `ensure_discarded_phrase`: 片方の文言だけがあるときは足りない方だけを足す。
    #[test]
    fn js_1_ensure_discarded_phrase_appends_only_the_missing_half() {
        match ensure_discarded_phrase(JsEngineError::EngineUnavailable(
            "boom; context was discarded".to_string(),
        )) {
            JsEngineError::EngineUnavailable(msg) => assert_eq!(
                msg,
                "boom; context was discarded; the next evaluation runs in a fresh context"
            ),
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
        match ensure_discarded_phrase(JsEngineError::Timeout(
            "x and was killed; the next evaluation runs in a fresh context".to_string(),
        )) {
            JsEngineError::Timeout(msg) => assert_eq!(
                msg,
                "x and was killed; the next evaluation runs in a fresh context; \
                 context was discarded"
            ),
            other => panic!("expected Timeout, got: {other:?}"),
        }
    }

    fn hb(name: &str, id: u32) -> HostBinding {
        HostBinding {
            name: name.to_string(),
            id,
        }
    }

    fn binding_failed_msg(r: Result<(), JsEngineError>) -> String {
        match r {
            Err(JsEngineError::BindingFailed(msg)) => msg,
            other => panic!("expected BindingFailed, got: {other:?}"),
        }
    }

    fn frame_payload_undefined() -> Vec<u8> {
        let mut out = Vec::new();
        worker_protocol::encode_js_value(&JsValue::Undefined, &mut out).expect("encode");
        out
    }

    /// TASK-29.4: 登録時検証（空・長すぎる・重複）。区切り文字は登録フレーム
    /// 経由なので許可される。
    #[test]
    fn js_1_validate_new_host_binding_rejects_invalid_names() {
        assert_eq!(
            binding_failed_msg(validate_new_host_binding(&[], &[], "")),
            "native function name must not be empty"
        );
        let long = "a".repeat(super::super::v8_engine::MAX_NATIVE_PROXY_NAME_BYTES + 1);
        assert_eq!(
            binding_failed_msg(validate_new_host_binding(&[], &[], &long)),
            "native function name exceeds the maximum supported length of 256 bytes"
        );
        for ok in ["a,b", "a=b", "a\0b"] {
            assert!(validate_new_host_binding(&[], &[], ok).is_ok(), "{ok:?}");
        }
        assert_eq!(
            binding_failed_msg(validate_new_host_binding(&[hb("f", 0)], &[], "f")),
            "native function \"f\" is already registered"
        );
        assert_eq!(
            binding_failed_msg(validate_new_host_binding(&[], &[("g".to_string(), 0)], "g")),
            "native function \"g\" is already registered"
        );
        // 大文字小文字は区別する。
        assert!(validate_new_host_binding(&[hb("f", 0)], &[], "F").is_ok());
    }

    /// TASK-29.4: 件数上限（1025 件目）。
    #[test]
    fn js_1_validate_new_host_binding_enforces_the_count_limit() {
        let existing: Vec<HostBinding> = (0..MAX_NATIVE_FUNCTIONS as u32)
            .map(|i| hb(&format!("f{i}"), i))
            .collect();
        assert_eq!(
            binding_failed_msg(validate_new_host_binding(&existing, &[], "extra")),
            "cannot register more than 1024 native functions"
        );
        assert!(validate_new_host_binding(&existing[1..], &[], "extra").is_ok());
    }

    /// テスト専用の環境変数経路の値の組み立て（空は None・区切り文字は拒否）。
    #[test]
    fn js_1_child_native_proxies_env_value_only_carries_test_proxies() {
        assert_eq!(child_native_proxies_env_value(&[]).expect("ok"), None);
        let value = child_native_proxies_env_value(&[("h".to_string(), 7), ("i".to_string(), 8)])
            .expect("ok");
        assert_eq!(value.as_deref(), Some("h=7,i=8"));
        match child_native_proxies_env_value(&[("f".to_string(), 1), ("f".to_string(), 2)]) {
            Err(JsEngineError::EngineUnavailable(msg)) => {
                assert_eq!(msg, "native function \"f\" is registered more than once");
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
        match child_native_proxies_env_value(&[("a,b".to_string(), 1)]) {
            Err(JsEngineError::EngineUnavailable(msg)) => {
                assert_eq!(msg, "native function name must not contain ',', '=' or NUL");
            }
            other => panic!("expected EngineUnavailable, got: {other:?}"),
        }
    }

    /// TASK-29.4: 登録応答の解釈（成功）。
    #[test]
    fn js_1_interpret_register_ack_accepts_exactly_undefined() {
        assert_eq!(
            interpret_register_ack(tag::RESULT, &frame_payload_undefined()),
            Ok(RegisterAck::Registered)
        );
    }

    /// TASK-29.4: `Binding` の `ERROR` は登録失敗（子は生きている）。
    #[test]
    fn js_1_interpret_register_ack_maps_binding_error_to_rejected() {
        let payload = worker_protocol::encode_error(ErrorKind::Binding, "nope");
        assert_eq!(
            interpret_register_ack(tag::ERROR, &payload),
            Ok(RegisterAck::Rejected("nope".to_string()))
        );
    }

    /// TASK-29.4: それ以外の応答はすべてプロトコル違反にする。
    #[test]
    fn js_1_interpret_register_ack_rejects_everything_else() {
        // 値が違う RESULT。
        let mut number = Vec::new();
        worker_protocol::encode_js_value(&JsValue::Number(1.0), &mut number).expect("encode");
        assert!(matches!(
            interpret_register_ack(tag::RESULT, &number),
            Err(RegisterAckViolation::UnexpectedResult(_))
        ));
        // 末尾に余分なバイトがある RESULT。
        let mut trailing = frame_payload_undefined();
        let len = trailing.len();
        trailing.push(0);
        assert_eq!(
            interpret_register_ack(tag::RESULT, &trailing),
            Err(RegisterAckViolation::TrailingBytes { extra: 1 })
        );
        assert_eq!(len + 1, trailing.len());
        // decode できない RESULT。
        assert!(matches!(
            interpret_register_ack(tag::RESULT, &[]),
            Err(RegisterAckViolation::UnexpectedResult(_))
        ));
        // Binding 以外の ERROR。
        for kind in [ErrorKind::Evaluation, ErrorKind::Timeout] {
            let payload = worker_protocol::encode_error(kind, "x");
            assert!(matches!(
                interpret_register_ack(tag::ERROR, &payload),
                Err(RegisterAckViolation::UnexpectedError(_))
            ));
        }
        // NATIVE_CALL・未知の tag。
        assert_eq!(
            interpret_register_ack(tag::NATIVE_CALL, &[]),
            Err(RegisterAckViolation::UnexpectedFrame(tag::NATIVE_CALL))
        );
        assert_eq!(
            interpret_register_ack(200, &[]),
            Err(RegisterAckViolation::UnexpectedFrame(200))
        );
    }
}

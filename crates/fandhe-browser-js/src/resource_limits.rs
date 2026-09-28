//! 子プロセスのヒープ外メモリに OS 側の上限を掛ける（`JS-1`・`TASK-29`・
//! Issue #503「JS プロセス分離」設計書 §5・codex レビュー指摘 #503 P0
//! 「`v8_engine.rs` の `MAX_ISOLATE_HEAP_BYTES` はヒープ外メモリを制限
//! できず、子プロセスの中では際限なく増え続けうる」対応）。
//!
//! [`super::v8_engine`] の `MAX_ISOLATE_HEAP_BYTES`（128 MiB）は V8 が
//! 管理するヒープにしか効かない。`ArrayBuffer` の backing store 等、
//! ヒープ外のメモリ確保はその上限の対象外である（OWASP A04「不安全な
//! 設計」対策の抜け穴）。本モジュールは、ユーザー承認済みの設計
//! （「OS による制限＋親の監視」案。2026-09-28）に従い、次の多層防御を
//! 実装する。
//!
//! - **子側（起動直後・V8 初期化前）**: OS のプロセス単位メモリ上限を
//!   自分自身へ設定する（[`enforce_child_memory_limit`]。
//!   `super::worker::worker_main` から呼ぶ）。設定に失敗したら評価を
//!   始めずに終了する（fail-closed。呼び出し元が `EngineUnavailable`
//!   へ変換する）
//! - **子側（Isolate 生成時）**: `ArrayBuffer` の backing store（`new
//!   ArrayBuffer(...)`・`new Uint8Array(...)` 等がヒープ外に確保する
//!   実体）の確保量を数え、合計が
//!   [`MAX_ARRAY_BUFFER_ALLOCATION_BYTES`] を超える確保を拒否する独自
//!   アロケータを V8 の `CreateParams::array_buffer_allocator` に渡す
//!   （[`new_bounded_array_buffer_allocator`]。codex レビュー指摘 #503
//!   P0「macOS の JS ワーカーに強制的なメモリ上限がない」対応。ユーザー
//!   承認 2026-09-28。`v8` crate 152.2.0 の `new_rust_allocator` を使う。
//!   3 OS 共通で効く点が、OS 側の手段が無い macOS にとって特に重要
//!   ── OS に頼らず V8 自身のレベルで確保経路を塞ぐため）
//! - **子側（Context 生成直後）**: WebAssembly を無効化する
//!   （`super::v8_engine::V8Engine::new_with_heap_limit` が Context の
//!   グローバルオブジェクトの `WebAssembly` プロパティを `undefined` へ
//!   上書きする）。wasm のメモリは上記の `ArrayBuffer` アロケータを
//!   通らない別経路であり、この上限の対象外になってしまうため、経路
//!   ごと塞ぐ。V8 152.2.0 にはこの版のグローバル露出を切り替える単一
//!   フラグ（`--no-expose-wasm` 等）が存在しないため、フラグではなく
//!   Context 生成直後のグローバルオブジェクト操作で行う（詳細は
//!   `V8Engine::new_with_heap_limit` の実装コメント参照）。この操作は
//!   戻り値（代入できたか）を確認していないため、多層防御として
//!   [`configure_wasm_max_mem_pages_flag`]（単一メモリインスタンスへの
//!   上限。wasm 無効化の前から存在した手段）も引き続き設定する
//! - **親側（子の寿命のあいだ継続的に）**: 子のメモリ使用量を定期的に
//!   監視し、上限を超えたら kill する（[`read_child_rss_bytes`]。
//!   `super::process_engine::MemoryMonitor` が子の寿命いっぱい動かす
//!   専任スレッドから、また応答直後・次回評価前にも同期的に呼ぶ）。
//!   **子の異常終了（Linux）・確保の失敗（Windows）・OS 側の手段が無い
//!   場合（macOS）のいずれであっても、`ResourceLimitExceeded` への
//!   分類と子の作り直しは最終的にこの親側の監視・確認が行う**（実測に
//!   基づく判断。理由は次節）
//! - **親側（監視そのものが壊れた場合）**: `read_child_rss_bytes` が
//!   連続で失敗し続けると（`ps` が `PATH` に無い・実行できない等）、
//!   監視は事実上ずっと `None` を返し続け、上限が事実上なくなる（codex
//!   レビュー指摘 #503 P0）。`super::process_engine::MemoryMonitor` は
//!   連続失敗の回数を数え、
//!   [`super::process_engine::MAX_CONSECUTIVE_PROBE_FAILURES`]（3 回）に
//!   達したら fail-closed（子を `kill` し、以後の評価を
//!   `JsEngineError::EngineUnavailable` にする）に倒す。単発の失敗は
//!   引き続き fail-open（次のポーリングで再試行）のままである
//!
//! macOS の `ps` は `PATH` に依存せず絶対パス（[`PS_ABSOLUTE_PATH`]）で
//! 起動する（codex レビュー指摘 #503 P0 対応。`PATH` に `ps` が無い・
//! 別の実行ファイルが先に解決される等の環境差による取得不能を避ける）。
//!
//! # OS ごとの強制の強さ（実装済みを装わない。REPAIR-3）
//!
//! [`MAX_CHILD_RSS_BYTES`]（親側の RSS 監視しきい値）と
//! [`LINUX_RLIMIT_DATA_CEILING_BYTES`]（Linux の `RLIMIT_DATA`）は
//! **意図的に別の定数にして値を分離した**。当初は両者を同じ予算
//! （V8 ヒープ 128 MiB＋ヒープ外許容 128 MiB＝256 MiB）に揃える設計を
//! 試みたが、本対応の実装時に Linux コンテナ（Debian・カーネル 7.0 系）
//! で子プロセスを実際に起動して `/proc/<pid>/status` の `VmData` を
//! 測定したところ、**V8 が起動時に確保する `CodeRange`（JIT コード用の
//! 仮想アドレス予約。実際に触れて物理メモリを消費する分ではなく、
//! 将来のコード生成に備えた予約であっても `RLIMIT_DATA` の会計対象
//! （匿名 private mmap の仮想サイズ。`/proc/<pid>/status` の `VmData` が
//! 該当）に含まれる）だけで、何もスクリプトを評価していない
//! ハンドシェイク直後の時点で aarch64 で約 268 MiB、x86_64
//! （`--platform linux/amd64` のコンテナで計測。テストスイート全体の
//! 実行中のピークで約 762 MiB）に達することが分かった。256 MiB では
//! `RLIMIT_DATA` を設定した時点で V8 の初期化そのものが
//! `Fatal process out of memory: Failed to reserve virtual memory for
//! CodeRange` で失敗し、子プロセスが一切起動できなくなることを実機で
//! 確認した。
//!
//! `CodeRange` の予約サイズはアーキテクチャ・V8 のビルドオプションに
//! 依存し、本 crate 側では制御できない。したがって Linux の
//! `RLIMIT_DATA` は「ヒープ外メモリの厳密な上限」としては機能させられず、
//! 観測された最大値（x86_64 で約 762 MiB）に十分な安全マージンを
//! 載せた保守的な値（[`LINUX_RLIMIT_DATA_CEILING_BYTES`]。2 GiB）を
//! 「際限のない確保だけは防ぐ最終防衛線」として設定するに留める。
//! 通常の攻撃的なスクリプト（`while (true) { chunks.push(new
//! Uint8Array(...)) }` のような繰り返し確保）は、この 2 GiB に達する
//! はるか手前で親側の RSS 監視（[`MAX_CHILD_RSS_BYTES`]。320 MiB）に
//! 捕まる。詳細は本モジュールの実装時に得た知見として次の表にまとめる。
//!
//! | OS | 強制方法 | 強さ |
//! |---|---|---|
//! | Linux | `RLIMIT_DATA`（[`enforce_child_memory_limit`]。`rustix::process::setrlimit`。上限 [`LINUX_RLIMIT_DATA_CEILING_BYTES`]＝2 GiB）＋[`new_bounded_array_buffer_allocator`]（`ArrayBuffer` 確保の上限。3 OS 共通） | **OS がある程度強制するが、厳密な上限としては機能しない**。`RLIMIT_DATA` は匿名 `MAP_PRIVATE` の `mmap`（大きな `ArrayBuffer` の backing store が実際に使う経路。glibc malloc は既定のしきい値 128 KiB を超える確保を `mmap` に回す）にも、実際に触れていない仮想予約にも適用される（`setrlimit(2)` の「data segment のみ」という古い説明は、匿名 mmap を会計に含めない実装を前提にしており、本 crate が対象とする現行 Linux カーネルの挙動とは異なる）。しかし V8 自身の `CodeRange` 予約だけで数百 MiB（アーキテクチャ依存。実測は上記のとおり）を消費するため、[`HEAP_EXTERNAL_ALLOWANCE_BYTES`] 相当の小さい値には設定できず、`RLIMIT_DATA` 単体では実質的な防御を親側の監視に委ねていた。**`ArrayBuffer` の確保だけは、この `RLIMIT_DATA` の限界とは無関係に、V8 自身のレベル（[`new_bounded_array_buffer_allocator`]）で [`MAX_ARRAY_BUFFER_ALLOCATION_BYTES`] を確実に強制する** |
//! | Windows | Job Object（[`enforce_child_memory_limit`]。`windows-sys` で Win32 API を直接呼ぶ。`JOB_OBJECT_LIMIT_PROCESS_MEMORY`。上限 [`WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`]＝384 MiB）＋[`new_bounded_array_buffer_allocator`]（`ArrayBuffer` 確保の上限。3 OS 共通） | **OS がコミットの天井を強制するが、天井を超えても子プロセスは終了しない**。当初は `win32job =2.0.3` の安全な API（`limit_working_memory`／`JOB_OBJECT_LIMIT_WORKINGSET`）を使っていたが、これは物理メモリの常駐量（working set）を trim させるだけでコミットチャージの上限にはならないという P0 指摘（codex）を受け、`win32job` の使用をやめて `windows-sys` で Win32 API を直接呼ぶ実装に切り替えた（ユーザー承認 2026-09-28。承認された unsafe の範囲は本モジュールの `windows_job` サブモジュール・`read_rss_windows` に限定）。`JOB_OBJECT_LIMIT_PROCESS_MEMORY`／`ProcessMemoryLimit` は Linux の `kill` ベースの強制とは性質が異なり、コミットチャージが上限を超えると**その先の確保が失敗するだけ**である（`VirtualAlloc`/`HeapAlloc` 相当が失敗を返す。プロセス自体は生き続ける）。ただし本対応（codex レビュー指摘 #503 P0「macOS の JS ワーカーに強制的なメモリ上限がない」）以降、V8 の既定の `ArrayBuffer::Allocator` は使わず [`new_bounded_array_buffer_allocator`] に置き換えたため、`ArrayBuffer` の確保は通常 [`MAX_ARRAY_BUFFER_ALLOCATION_BYTES`]（128 MiB）でこのアロケータ自身が拒否し、Job Object のコミット上限（384 MiB）に到達する事態は基本的に起こらない（到達しうるのは `ArrayBuffer` 以外の確保。その場合も引き続き親側の監視（[`read_child_rss_bytes`] が返す `PrivateUsage`。working set ではなくコミット量を見るように変更した）が `ResourceLimitExceeded` への分類・子の破棄・作り直しを担う）。この Windows 専用実装（windows-sys 版）は本コミット時点では未検証であり、CI の Windows ランナーでの確認を前提とする（回帰テスト参照）|
//! | macOS | OS 側の手段は無い（[`enforce_child_memory_limit`] は何もしない）。ただし [`new_bounded_array_buffer_allocator`]（`ArrayBuffer` 確保の上限。3 OS 共通で有効）と WebAssembly 無効化（`super::v8_engine::V8Engine::new_with_heap_limit`。3 OS 共通で有効）が、OS に頼らず V8 自身のレベルでヒープ外メモリの主要な確保経路を塞ぐ | **OS によるプロセス単位の強制は無いが、ヒープ外メモリの 2 大確保経路（`ArrayBuffer`・wasm）は V8 自身のレベルで塞がれている**。本実装時にこの macOS 環境で実機検証したところ、`RLIMIT_DATA`・`RLIMIT_AS`・`RLIMIT_RSS` はいずれも `setrlimit(2)` の呼び出し自体が `EINVAL` で失敗した（「上限をかけたが効かない」ではなく「そもそも設定できない」）。Job Object 相当の OS 機構も無い。したがって [`enforce_child_memory_limit`] は macOS では何もせず常に成功を返す（呼び出そうとしても確実に失敗するため、fail-closed にすると macOS 上で子プロセスが常に起動できなくなってしまう）。残る確保経路（V8 自身のコード領域・snapshot・Rust ホストバイナリの通常の確保等）は、引き続き親側の RSS 監視だけに委ねる |
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - **`ResourceLimitExceeded` への分類・子の作り直しは、3 OS 共通で
//!   最終的に親側の監視が担う**。子側の OS 別強制（Linux の
//!   `RLIMIT_DATA`・Windows の `ProcessMemoryLimit`）は「際限のない
//!   確保だけは食い止める」ところまでしか行わない。Linux は
//!   `CodeRange` 予約のため厳密な上限にできない。Windows は上限到達時に
//!   確保を失敗させるだけで子プロセスを終了させない（上表参照）。
//!   macOS は OS 側の手段が無い（[`new_bounded_array_buffer_allocator`]・
//!   WebAssembly 無効化は OS に頼らず V8 自身のレベルで強制するため、
//!   この限りではない。次点参照）
//! - **[`new_bounded_array_buffer_allocator`] が拒否した確保は
//!   `RangeError`（catchable な例外）としてスクリプトへ返るだけで、
//!   子プロセスは終了しない**。子は生き続け、Context もそのまま残る
//!   （`ResourceLimitExceeded` ではなく `EvaluationFailed` に分類される。
//!   `MAX_ARRAY_BUFFER_ALLOCATION_BYTES` は「この 1 プロセスが確保できる
//!   `ArrayBuffer` の合計」を制限するものであり、際限のない繰り返し確保
//!   そのものは防ぐが、それを理由に子を作り直したい場合は、これまでどおり
//!   親側の RSS 監視が `ResourceLimitExceeded` として検出する
//! - WebAssembly は本対応で完全に無効化した
//!   （`super::v8_engine::V8Engine::new_with_heap_limit`）。
//!   `wasm` のメモリは [`new_bounded_array_buffer_allocator`] を通らない
//!   別経路であり、対応前は個別の上限（`--wasm-max-mem-pages`）を
//!   掛けていたが、上限を掛けても経路自体は残るため、経路ごと塞ぐ判断に
//!   変更した。**Web 互換の制約**: `WebAssembly` グローバルが常に
//!   `undefined` になり、`.wasm` を扱うスクリプトは動作しない（本 crate が
//!   対象とする用途は現時点で通常の JS 評価であり、TASK-30（core 統合）
//!   以降で wasm サポートが必要になった場合は、この無効化の是非を含めて
//!   ユーザーへ再度判断を仰ぐ）
//! - 親側の監視は [`RSS_POLL_INTERVAL`] の分だけ後追いになる。1 回の
//!   JS 実行が割り込みチェックを挟まずにポーリング間隔内で大量に確保
//!   すると、実際のピークメモリは一時的に [`MAX_CHILD_RSS_BYTES`] を
//!   超えてから検出・終了する
//! - メモリ使用量の取得（`/proc/<pid>/status`・`ps`・
//!   `GetProcessMemoryInfo`）が単発で失敗した場合は監視を諦めてその
//!   ポーリング回だけスキップする（fail-open。[`read_child_rss_bytes`]
//!   のドキュメントコメント参照）。ただし
//!   [`super::process_engine::MAX_CONSECUTIVE_PROBE_FAILURES`] 回
//!   連続で失敗した場合は fail-closed に切り替わる（codex レビュー指摘
//!   #503 P0 対応。`super::process_engine::MonitorKillReason` 参照）。
//!   [`enforce_child_memory_limit`] 自体の失敗は最初から fail-closed
//!   （評価を始めずに終了）であり、起動時強制と監視とで
//!   fail-open/fail-closed の扱いが異なることに注意
//! - Windows の `ProcessMemoryLimit`・`GetProcessMemoryInfo` を使う
//!   windows-sys 版の実装は、本コミット時点では実機・CI での検証が
//!   できていない（開発環境が Windows ではないため。上表参照）

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc};
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::io::Read;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

/// [`super::v8_engine`] の `MAX_ISOLATE_HEAP_BYTES`（V8 の Isolate ヒープ
/// 上限。128 MiB）に加えて、ヒープ外メモリ（`ArrayBuffer` の backing
/// store・wasm メモリ・V8 自身のコード領域や snapshot・Rust ホスト
/// バイナリの通常の確保）に許容する上乗せ分（バイト）。
///
/// 128 MiB という値の根拠: 本対応の実装時に、子プロセス（デバッグ
/// ビルド）を実際に起動し、ハンドシェイク直後・トリビアルなスクリプト
/// 評価後の RSS を `ps`／`/proc/<pid>/status` で測定したところ、いずれも
/// 10〜25 MiB 程度であった（`tests/v8_worker.rs` の各ケースを実行し
/// ながら計測。Isolate 起動・snapshot 展開・永続 Context 生成に伴う
/// **常駐**メモリのオーバーヘッドは実測で 25 MiB を大きく下回る。ただし
/// これは RSS＝実際に触れた物理メモリの話であり、仮想アドレス予約
/// （`VmData`）は別の話である。後者は本モジュールのドキュメントコメント
/// 「OS ごとの強制の強さ」節が説明するとおり、Linux の `RLIMIT_DATA` の
/// 実効値を大きく左右する）。128 MiB はこの RSS 実測値の約 5〜8 倍の
/// 余裕を持たせており、本 crate が現時点でサポートする単純なスクリプト
/// 評価・DOM 風バインディングが誤って上限に触れることを避けつつ、
/// 際限のない確保だけは確実に止めることを狙った値である。
const HEAP_EXTERNAL_ALLOWANCE_BYTES: u64 = 128 * 1024 * 1024;

/// 親が子の RSS を監視する際のしきい値（バイト）。V8 の Isolate ヒープ
/// 上限（[`super::v8_engine`] の `MAX_ISOLATE_HEAP_BYTES`。128 MiB）と
/// [`HEAP_EXTERNAL_ALLOWANCE_BYTES`]（128 MiB）の合計 256 MiB に対し、
/// RSS には共有ライブラリ・コード領域・スレッドスタックなど V8 の
/// ヒープ／backing store 以外の分も乗るため、誤検知を避ける余裕として
/// 64 MiB を上乗せする（合計 320 MiB）。`ResourceLimitExceeded` への
/// 分類・子の作り直しは 3 OS 共通で最終的にこのしきい値に基づく親側の
/// 監視が担う（モジュール冒頭のドキュメントコメント参照）。
/// [`RSS_POLL_INTERVAL`] のドキュメントコメントが説明する「後追い」の
/// 限界とあわせて運用する。
///
/// [`LINUX_RLIMIT_DATA_CEILING_BYTES`]・[`WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`]
/// とは意図的に値を分離している（Linux は本モジュールのドキュメント
/// コメント「OS ごとの強制の強さ」節の実測結果を参照。Windows は
/// [`WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`] のドキュメントコメントが説明
/// するとおり、本しきい値より 64 MiB 大きい値にしている）。
pub(crate) const MAX_CHILD_RSS_BYTES: u64 = super::v8_engine::MAX_ISOLATE_HEAP_BYTES as u64
    + HEAP_EXTERNAL_ALLOWANCE_BYTES
    + 64 * 1024 * 1024;

/// 親が子のメモリ使用量をポーリングする間隔。50〜100 ミリ秒の範囲で、
/// 監視の追随性（短いほど検出が速い）と監視自体のコスト（`ps` の起動を
/// 伴う macOS では特に、短すぎると監視自体が負荷になる）の折衷として
/// 75 ミリ秒を選んだ。
///
/// 3 OS 共通の値にしている: 以前は Windows だけ PowerShell の起動
/// コストを理由に 500 ミリ秒にしていたが、Windows の監視を
/// `GetProcessMemoryInfo`（psapi.dll の直接呼び出し。プロセス起動を
/// 伴わない）へ切り替えたことで、その理由が無くなった
/// （[`read_rss_windows`] 参照）。
pub(crate) const RSS_POLL_INTERVAL: Duration = Duration::from_millis(75);

/// Linux の `RLIMIT_DATA`（soft/hard 両方）に設定したい上限（バイト。
/// 2 GiB）。[`MAX_CHILD_RSS_BYTES`]（320 MiB）とは意図的に値を分離して
/// いる。理由・実測値はモジュール冒頭のドキュメントコメント「OS ごとの
/// 強制の強さ」節を参照（V8 の `CodeRange` 仮想アドレス予約だけで
/// アーキテクチャによっては数百 MiB に達するため、256 MiB のような
/// 小さい値には設定できない）。実測で得た最大値（x86_64 で約 762 MiB）
/// に対しておよそ 2.7 倍の安全マージンを載せた値である。
///
/// この値は「厳密なヒープ外メモリの上限」ではなく、「際限のない確保・
/// 極端なプロセス全体のメモリ膨張だけは食い止める最終防衛線」として
/// 機能する。通常の攻撃的なスクリプトは、この上限に達するはるか手前で
/// 親側の RSS 監視（[`MAX_CHILD_RSS_BYTES`]）に捕まる想定である。
///
/// **これは「引き上げたい」上限であり、実際に設定する値ではない**
/// （codex レビュー指摘 #503 P1「既存の RLIMIT_DATA を超える hard limit
/// を設定しない」対応）。Linux の子プロセスは親の `RLIMIT_DATA` を
/// 継承するため、親の hard limit が本値より小さい環境（コンテナ・
/// systemd の `LimitDATA=` 等で既に制限されている場合）では、非特権
/// プロセスは hard limit を**引き上げられない**（`setrlimit(2)` は
/// `CAP_SYS_RESOURCE` が無い限り hard limit の引き上げを許さない）。
/// そのため実際に設定する値は、[`compute_rlimit_data_target`] が
/// 「既存の hard limit と本値の小さいほう」として計算する（既存の hard
/// limit が無制限なら本値をそのまま使う）。この計算により、本関数は
/// 既存の hard limit を**下げる方向にしか変更しない**ため、非特権
/// プロセスでも常に成功する。
#[cfg(target_os = "linux")]
const LINUX_RLIMIT_DATA_CEILING_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// [`enforce_via_rlimit_data`] が実際に `RLIMIT_DATA` の soft/hard 両方に
/// 設定する値を、既存の hard limit（`getrlimit` で取得した
/// [`rustix::process::Rlimit::maximum`]）から計算する（codex レビュー
/// 指摘 #503 P1 対応。純粋関数として切り出し、実際に `setrlimit` を
/// 呼ばずに単体テストできるようにする）。
///
/// - 既存の hard limit が無制限（`None`）なら [`LINUX_RLIMIT_DATA_CEILING_BYTES`]
///   をそのまま使う。
/// - 既存の hard limit が [`LINUX_RLIMIT_DATA_CEILING_BYTES`] 以上なら、
///   [`LINUX_RLIMIT_DATA_CEILING_BYTES`] まで**下げる**（本モジュールが
///   前提とする「V8 の `CodeRange` 予約を許すが、際限のない膨張だけは
///   防ぐ」設計に合わせるため。既存の hard limit をそのまま使い続けると、
///   本モジュールの想定より緩い上限になってしまう）。
/// - 既存の hard limit がそれより小さいなら、既存の hard limit を
///   そのまま使う（引き上げない。非特権プロセスでも `setrlimit` が
///   成功することを最優先する）。
///
/// **既存の hard limit が、V8 の `CodeRange` 予約（実測で x86_64 約
/// 762 MiB）すら満たせないほど小さい環境の扱い**: そのまま設定すると、
/// この後の V8 初期化（`super::v8_engine::V8Engine::new` 相当）が
/// `RLIMIT_DATA` 到達によりクラッシュし、子プロセスは `Hello` を送らずに
/// 終了する。これは意図的な挙動である（fail-closed。実装済みを装わない。
/// REPAIR-3）: 本関数は「既存の環境制約の範囲内で、できるだけ V8 が
/// 動く上限を設定する」ことしかできず、環境自体が V8 の実行に必要な
/// メモリを許していない場合にまで動作を保証することはできない。親
/// （`super::process_engine::spawn_worker`）はこれを「`Hello` 受信前に
/// 子プロセスが終了した」ハンドシェイク失敗として検出し、
/// `JsEngineError::EngineUnavailable` に変換する（既存の経路。本対応で
/// 新設したものではない）。呼び出し元には「このプロセスでは動かせない」
/// ことが明確なエラーとして伝わり、`ResourceLimitExceeded` を検出したと
/// 偽ることはない。
#[cfg(target_os = "linux")]
fn compute_rlimit_data_target(existing_hard_limit: Option<u64>) -> u64 {
    match existing_hard_limit {
        Some(hard) => hard.min(LINUX_RLIMIT_DATA_CEILING_BYTES),
        None => LINUX_RLIMIT_DATA_CEILING_BYTES,
    }
}

/// Windows の Job Object `ProcessMemoryLimit`（コミットメモリ上限。
/// バイト）。codex レビュー指摘 #503 P0「Windows では working set の
/// 上限と監視だけではコミット量を制限できない」対応で、working set の
/// trim ではなく、コミットチャージの上限を課す本来の
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` を使う。
///
/// **[`MAX_CHILD_RSS_BYTES`]（親側の監視しきい値。320 MiB）とは意図的に
/// 別の値にし、64 MiB の余裕を載せている**（合計 384 MiB）。理由:
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` は Linux の `RLIMIT_DATA` や
/// `kill` とは異なり、上限を超えたら**プロセスを終了させるのではなく、
/// その先の確保（`VirtualAlloc`/`HeapAlloc` 相当）を失敗させる**だけで
/// ある。V8 の既定の `ArrayBuffer::Allocator` はこの確保失敗を GC を
/// 挟んで再試行したうえで、最終的に catchable な
/// `RangeError: Array buffer allocation failed` を投げる（プロセスは
/// 終了しない）。仮に本値を [`MAX_CHILD_RSS_BYTES`] と同じ 320 MiB に
/// 揃えると、コミット量がちょうど 320 MiB 付近で確保が失敗して
/// `RangeError` が飛び、親側の RSS 監視（`read_child_rss_bytes` が返す
/// `PrivateUsage`）もその時点でちょうど 320 MiB 以下（`>` 判定を満たさ
/// ない）にとどまってしまい、`ResourceLimitExceeded` に分類されず
/// `EvaluationFailed` のまま Context が生き残ってしまう（際限のない
/// 確保を許してしまう回帰）。64 MiB の余裕（テストが使う 1 チャンク
/// 10 MiB より大きい）を持たせることで、OS が確保を拒み始めた後も
/// コミット量は [`MAX_CHILD_RSS_BYTES`] を確実に上回った状態になり、
/// 親側の監視（応答直後の同期確認・継続監視のいずれか）が
/// `ResourceLimitExceeded` として検出できる。
///
/// 親側の監視も Windows では `GetProcessMemoryInfo` の `PrivateUsage`
/// （コミット量）を見るように変更した（[`read_rss_windows`]）ため、
/// 子側の OS 強制と親側の監視は同じ指標を見る。Linux（`CodeRange`
/// 予約のため厳密な上限にできない）とは異なり、Windows では OS が
/// コミットの天井を実際に強制する。ただし「天井に達したら確保が失敗
/// する」だけであり、プロセスの終了・`ResourceLimitExceeded` への分類・
/// 子の作り直しは、これまでどおり親側の監視が担う（実装済みを装わない。
/// REPAIR-3）。
#[cfg(target_os = "windows")]
const WINDOWS_PROCESS_MEMORY_LIMIT_BYTES: usize = (MAX_CHILD_RSS_BYTES + 64 * 1024 * 1024) as usize;

/// [`new_bounded_array_buffer_allocator`] が拒否するまでに確保できる
/// `ArrayBuffer` backing store の合計バイト数（codex レビュー指摘 #503
/// P0「macOS の JS ワーカーに強制的なメモリ上限がない」対応。ユーザー
/// 承認 2026-09-28）。
///
/// [`HEAP_EXTERNAL_ALLOWANCE_BYTES`]（128 MiB）と同じ値にしている理由:
/// この定数は元々「V8 の Isolate ヒープ上限（128 MiB）に加えて許容する
/// ヒープ外メモリの上乗せ分」として定義されており、`ArrayBuffer` の
/// backing store はその主要な内訳の 1 つである。同じ予算をそのまま
/// `ArrayBuffer` 専用の強制上限として使うことで、
/// [`super::v8_engine::MAX_ISOLATE_HEAP_BYTES`]（128 MiB）＋本定数
/// （128 MiB）＝256 MiB が、[`MAX_CHILD_RSS_BYTES`]（親側の RSS 監視
/// しきい値。320 MiB）を 64 MiB 下回る状態を保てる。V8 自身のコード
/// 領域・snapshot・Rust ホストバイナリの通常の確保等（この上限の対象
/// 外）にその 64 MiB の余裕を割り当てる設計である。
pub(crate) const MAX_ARRAY_BUFFER_ALLOCATION_BYTES: usize = HEAP_EXTERNAL_ALLOWANCE_BYTES as usize;

/// [`new_bounded_array_buffer_allocator`] が確保に使うアラインメント
/// （バイト）。`Float64Array`・SIMD 演算等が要求しうる最大アラインメント
/// を満たすため、一般的な malloc 実装が保証する 16 バイトに合わせる。
const ARRAY_BUFFER_ALLOCATION_ALIGN: usize = 16;

/// wasm の 1 ページのバイト数（V8 の仕様で固定。64 KiB）。
const WASM_MEMORY_PAGE_BYTES: u64 = 64 * 1024;

/// wasm の単一メモリインスタンスに許す最大ページ数
/// （`--wasm-max-mem-pages=<N>`。[`configure_wasm_max_mem_pages_flag`]
/// 参照）。[`HEAP_EXTERNAL_ALLOWANCE_BYTES`]（128 MiB）と同じ予算を
/// 割り当てる。`HEAP_EXTERNAL_ALLOWANCE_BYTES / WASM_MEMORY_PAGE_BYTES`
/// ＝ 2048 ページ。
///
/// **多層防御としてのみ機能する**（advisor 指摘。codex レビュー指摘
/// #503 P0「macOS の JS ワーカーに強制的なメモリ上限がない」対応で
/// WebAssembly を無効化した（`v8_engine::V8Engine::new_with_heap_limit`
/// が Context 生成直後に `WebAssembly` グローバルを上書きする）際、この
/// 上書きが**失敗しても**（戻り値は確認していない。同関数の実装コメント
/// 参照）、無制限の wasm メモリ確保という抜け穴を残さないための保険と
/// して、対応前から存在したこの上限フラグを引き続き設定する）。
const WASM_MAX_MEM_PAGES: u64 = HEAP_EXTERNAL_ALLOWANCE_BYTES / WASM_MEMORY_PAGE_BYTES;

/// [`WASM_MAX_MEM_PAGES`] を V8 のフラグとして設定する（V8 152.2.0 の
/// `v8/src/flags/flag-definitions.h` に定義された `wasm_max_mem_pages`
/// フラグに対応。コマンドライン形式は `--wasm-max-mem-pages=<N>`）。
///
/// **`v8::V8::initialize_platform` より前に呼ぶ契約**（フラグは V8 の
/// 初期化前にしか反映されない。`super::v8_engine::ensure_v8_initialized`
/// 参照）。
///
/// WebAssembly を無効化する主たる対策（`v8_engine::V8Engine::
/// new_with_heap_limit` の `WebAssembly` グローバル上書き）とは独立した
/// 多層防御である（[`WASM_MAX_MEM_PAGES`] のドキュメントコメント参照）。
/// V8 152.2.0 には WebAssembly のグローバル露出そのものを切り替える
/// 単一フラグ（`--no-expose-wasm` 等）が存在しない（実機で確認済み）
/// ため、単一メモリインスタンスへの上限という、対応前から存在した
/// 手段をそのまま残す。
pub(crate) fn configure_wasm_max_mem_pages_flag() {
    v8::V8::set_flags_from_string(&format!("--wasm-max-mem-pages={WASM_MAX_MEM_PAGES}"));
}

/// [`enforce_child_memory_limit`] が成功した際に返すガード（codex
/// レビュー指摘 #503 P1「`enforce_via_job_object` が Job をローカル変数の
/// まま返しているため、関数を抜けた時点でハンドルが閉じてしまう」
/// 対応）。
///
/// Windows では Win32 の Job Object はハンドルへの参照が無くなると
/// 閉じられ、`AssignProcessToJobObject` で設定した `ProcessMemoryLimit`
/// が維持される保証が無くなる。呼び出し元（`super::worker::worker_main`）
/// は、このガードを子プロセスの寿命いっぱい（`worker_main` 関数の
/// スコープの終わりまで）保持しなければならない。
///
/// Linux（`setrlimit`）・macOS（何もしない）では追加のハンドルを必要と
/// しないため、このガードは中身を持たないユニット型として振る舞う。
#[must_use = "drop するとメモリ上限が失われる場合がある（Windows）。プロセスの寿命いっぱい保持すること"]
pub(crate) struct ChildMemoryLimitGuard {
    #[cfg(target_os = "windows")]
    _job: windows_job::JobHandle,
}

/// 子プロセス自身に OS のメモリ上限を設定する（起動直後・V8 初期化前に
/// 呼ぶ契約。`super::worker::worker_main` 参照）。戻り値の
/// [`ChildMemoryLimitGuard`] は、呼び出し元が子プロセスの寿命いっぱい
/// 保持しなければならない（ドキュメントコメント参照）。
///
/// 失敗した場合は fail-closed（呼び出し元は評価を始めずに終了する）。
/// 親（`super::process_engine`）はハンドシェイク未達として検出し、
/// `EngineUnavailable` へ変換する。OS ごとの強制の強さはモジュール冒頭
/// の表を参照（macOS は常に成功を返し、実際の防御は行わない）。
pub(crate) fn enforce_child_memory_limit() -> Result<ChildMemoryLimitGuard, String> {
    #[cfg(target_os = "linux")]
    {
        enforce_via_rlimit_data()?;
        Ok(ChildMemoryLimitGuard {})
    }
    #[cfg(target_os = "windows")]
    {
        let job = windows_job::create_and_assign(WINDOWS_PROCESS_MEMORY_LIMIT_BYTES)?;
        Ok(ChildMemoryLimitGuard { _job: job })
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        // macOS 等: OS 側の強制手段が無い（モジュール冒頭の表・実機検証
        // 参照）。ここで `RLIMIT_DATA` 等を試みても確実に失敗するだけで
        // あり、fail-closed にすると当該 OS で子プロセスが常に起動でき
        // なくなってしまうため、何もせず成功を返す。
        Ok(ChildMemoryLimitGuard {})
    }
}

/// Linux: `RLIMIT_DATA` の soft/hard 両方を、既存の hard limit を超えない
/// 値（[`compute_rlimit_data_target`]）に設定する。`rustix::process::
/// getrlimit`・`setrlimit` は safe API のため `unsafe` を追加しない
/// （codex レビュー指摘 #503 P1「既存の RLIMIT_DATA を超える hard limit
/// を設定しない」対応。既存の hard limit を取得せずに固定値
/// [`LINUX_RLIMIT_DATA_CEILING_BYTES`] へ引き上げようとすると、親（この
/// プロセスを起動したシェル・コンテナランタイム・systemd 等）が既に
/// より厳しい hard limit を設定している環境で、非特権プロセスには
/// 許されない「hard limit の引き上げ」を試みることになり `setrlimit`
/// が失敗して子プロセスが評価を始めずに終了してしまう）。
#[cfg(target_os = "linux")]
fn enforce_via_rlimit_data() -> Result<(), String> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    let existing = getrlimit(Resource::Data);
    let target = compute_rlimit_data_target(existing.maximum);
    let limit = Rlimit {
        current: Some(target),
        maximum: Some(target),
    };
    setrlimit(Resource::Data, limit).map_err(|err| {
        format!(
            "failed to set RLIMIT_DATA to {target} bytes (existing hard limit: \
             {:?}): {err}",
            existing.maximum
        )
    })
}

/// Windows: Job Object の作成・`ProcessMemoryLimit` の設定・自プロセスの
/// 割り当てを Win32 API を直接呼んで行う（codex レビュー指摘 #503 P0
/// 「Windows では working set の上限と監視だけではコミット量を制限
/// できない」対応）。
///
/// `win32job =2.0.3` の安全な公開 API では
/// `JOB_OBJECT_LIMIT_PROCESS_MEMORY`/`ProcessMemoryLimit` を設定できない
/// （`ExtendedLimitInfo` が内部に持つ `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`
/// が `pub(crate)` のため）。そのため `win32job` の使用をやめ、
/// `windows-sys` で Win32 API を直接呼ぶ（ユーザー承認 2026-09-28。
/// 依存・unsafe の追加はこの用途に限定）。`unsafe` はいずれも最小の
/// ブロックにし、`// SAFETY:` を付す。
#[cfg(target_os = "windows")]
mod windows_job {
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Job Object のハンドルを保持し、`Drop` で `CloseHandle` する
    /// （[`super::ChildMemoryLimitGuard`] がこれを子プロセスの寿命
    /// いっぱい保持する）。
    pub(super) struct JobHandle(HANDLE);

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` は `CreateJobObjectW` が返した有効な
            // HANDLE であり、`JobHandle` はこの型を通じてのみ生成される
            // （`create_and_assign` 参照）ため、二重に close されることは
            // ない。プロセス終了時に一度だけ呼ばれる。
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// 匿名の Job Object を作成し、`ProcessMemoryLimit`（コミットメモリ
    /// 上限）を `limit_bytes` に設定したうえで、現在のプロセスをその
    /// Job へ割り当てる。
    ///
    /// working set の上限（旧実装の `win32job` 版）ではなく、コミット
    /// チャージの上限を超えたらプロセスを強制終了する
    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` を使う（モジュール冒頭の表
    /// 「OS ごとの強制の強さ」参照）。
    pub(super) fn create_and_assign(limit_bytes: usize) -> Result<JobHandle, String> {
        // SAFETY: 第 1 引数（セキュリティ属性）・第 2 引数（名前）に
        // null を渡し、名前無しの匿名 Job を作る（本プロセス専用であり
        // 他プロセスから `OpenJobObject` で参照される想定が無いため）。
        // 戻り値の HANDLE は失敗時に null になり、直後に検査する。
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "CreateJobObjectW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // 生成直後に `JobHandle` へ包む: これ以降の早期 return（`?`）でも
        // `Drop` が `CloseHandle` を呼び、ハンドルをリークしない。
        let job = JobHandle(handle);

        // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`（と
        // `JOBOBJECT_BASIC_LIMIT_INFORMATION`）は `Default` を実装して
        // いる（数値フィールドのみの POD 構造体）ため、`unsafe` な
        // ゼロ初期化は不要（承認された unsafe の範囲を Win32 呼び出しに
        // 限定する）。フィールド構文で必要な 2 箇所だけを埋め、残りは
        // `..Default::default()` に任せる（clippy
        // `field_reassign_with_default` 対応）。
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_PROCESS_MEMORY,
                ..Default::default()
            },
            ProcessMemoryLimit: limit_bytes,
            ..Default::default()
        };

        // SAFETY: `job.0` は直前で作成した有効な Job ハンドル。`info` は
        // 正しいサイズで初期化済みのローカル変数であり、この呼び出しの
        // 間だけ有効な参照として渡す（呼び出し後は使わない）。
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SetInformationJobObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        // SAFETY: `GetCurrentProcess` は疑似ハンドル（`CloseHandle` 不要。
        // プロセスの生存期間中ずっと有効）を返す。`job.0` は有効な Job
        // ハンドル。
        let ok = unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) };
        if ok == 0 {
            return Err(format!(
                "AssignProcessToJobObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        Ok(job)
    }
}

/// [`new_rust_allocator`](v8::new_rust_allocator) に渡す、`ArrayBuffer`
/// backing store の確保量をカウントする内部状態（codex レビュー指摘
/// #503 P0「macOS の JS ワーカーに強制的なメモリ上限がない」対応。
/// ユーザー承認 2026-09-28）。
///
/// `Arc` で包んで [`new_bounded_array_buffer_allocator`] から
/// `Arc::into_raw` で渡し、V8 がこのアロケータ（延いては Isolate）を
/// 破棄する際に呼ぶ `drop` vtable 関数（[`bounded_array_buffer_drop`]）
/// で `Arc::from_raw` により復元して解放する。`Send + Sync` は
/// `AtomicUsize` と `usize` のみで構成されるため自動的に満たされ、
/// `new_rust_allocator` が要求する `T: Sized + Send + Sync + 'static`
/// 境界を満たす。
struct BoundedArrayBufferAllocatorState {
    /// 現在確保済みの合計バイト数。
    allocated_bytes: AtomicUsize,
    /// 確保を拒否し始める上限（バイト）。
    limit_bytes: usize,
}

/// `counter` に `additional` バイトを加算しても `limit` を超えない場合に
/// 限り、実際に加算する（`compare_exchange_weak` によるリトライで、
/// 複数スレッドからの同時呼び出しでも競合を起こさない。`RustAllocatorVtable`
/// のドキュメントコメントが「アロケータはスレッドセーフでなければ
/// ならない」と要求しているため）。加算後の合計が `usize` で表現できない
/// 場合（オーバーフロー）も拒否する。
fn try_reserve(counter: &AtomicUsize, additional: usize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let Some(new_total) = current.checked_add(additional) else {
            return false;
        };
        if new_total > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, new_total, Ordering::AcqRel, Ordering::Relaxed)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// [`try_reserve`] で加算した分を減算する（`free` から呼ぶ）。
fn release_reservation(counter: &AtomicUsize, amount: usize) {
    // `fetch_sub` は `amount` がカウンタの現在値を超えると（V8 の契約
    // 違反や本関数のバグにより）アンダーフローして wrap するが、
    // `fetch_update` と `saturating_sub` を使い、万一そのような呼び出しが
    // あっても 0 未満にはならず fail-closed（カウンタが 0 に留まり、以後の
    // 確保はより厳しく制限される側に倒れる）にする。advisor 指摘（V8 の
    // 契約はこれを防ぐはずだが、契約違反時にも安全側に倒す多層防御）。
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(amount))
    });
}

/// `len == 0` の確保要求に対して返す、非 null かつ整列済みのダミー
/// ポインタ（`std::alloc::alloc`／`alloc_zeroed` はサイズ 0 の `Layout`
/// を渡すと未定義動作になるため、実際の確保を行わずに済ませる）。
/// `NonNull::dangling` は確保を伴わない安全な操作であり、このポインタを
/// 通じて実際にメモリを読み書きすることは無い契約（`len == 0` の
/// backing store には触れる場所が無い）ため、`free` 側でも対応する
/// `len == 0` の分岐でこのポインタを `dealloc` しない。
fn zero_length_allocation() -> *mut c_void {
    NonNull::<u8>::dangling().as_ptr() as *mut c_void
}

/// `len` バイトを [`ARRAY_BUFFER_ALLOCATION_ALIGN`] 揃えで確保する
/// （`zeroed` が `true` なら `alloc_zeroed`、そうでなければ `alloc`）。
/// 上限超過・`Layout` 生成失敗（`len` が大きすぎて整列後のサイズが
/// `isize::MAX` を超える等）・OS 側の確保失敗のいずれでも `null` を
/// 返す（V8 はこれを `RangeError: Array buffer allocation failed` に
/// 変換する。呼び出し元の 2 つの vtable 関数から共有するロジックを
/// ここへ集約する）。
fn bounded_alloc(
    state: &BoundedArrayBufferAllocatorState,
    len: usize,
    zeroed: bool,
) -> *mut c_void {
    if len == 0 {
        return zero_length_allocation();
    }
    let Ok(layout) = Layout::from_size_align(len, ARRAY_BUFFER_ALLOCATION_ALIGN) else {
        return std::ptr::null_mut();
    };
    if !try_reserve(&state.allocated_bytes, len, state.limit_bytes) {
        return std::ptr::null_mut();
    }
    // SAFETY: `layout` has a non-zero size (`len != 0`, checked above) and
    // `ARRAY_BUFFER_ALLOCATION_ALIGN` is a compile-time constant power of
    // two, so `layout` is a valid `Layout` for `alloc`/`alloc_zeroed`. The
    // returned pointer (if non-null) is freed exactly once, by
    // `bounded_array_buffer_free` reconstructing the identical `Layout`
    // from the same `len` (see that function's SAFETY comment).
    let ptr = unsafe {
        if zeroed {
            alloc_zeroed(layout)
        } else {
            alloc(layout)
        }
    };
    if ptr.is_null() {
        // OS 側の確保自体が失敗した。カウンタへ加算した分を戻す。
        release_reservation(&state.allocated_bytes, len);
    }
    ptr as *mut c_void
}

/// [`v8::RustAllocatorVtable::allocate`] の実体: ゼロ初期化した
/// `len` バイトを確保する。
///
/// # SAFETY
/// V8 は `handle` として、[`new_bounded_array_buffer_allocator`] が
/// `v8::new_rust_allocator` に渡した生ポインタ（`Arc::into_raw` の
/// 戻り値）を指す参照を渡す。そのポインタは、対応する `Allocator` が
/// 破棄されて [`bounded_array_buffer_drop`] が呼ばれるまで有効であり、
/// V8 はその期間中いつでも本関数を呼びうる（`allocate`/
/// `allocate_uninitialized`/`free` の呼び出し順序に制約は無い）ため、
/// `&BoundedArrayBufferAllocatorState` として参照する操作は常に安全。
/// パニックしうる処理は含まない（`extern "C"` 境界を越えて巻き戻ると
/// 未定義動作になるため）。
unsafe extern "C" fn bounded_array_buffer_allocate(
    handle: &BoundedArrayBufferAllocatorState,
    len: usize,
) -> *mut c_void {
    bounded_alloc(handle, len, true)
}

/// [`v8::RustAllocatorVtable::allocate_uninitialized`] の実体:
/// ゼロ初期化しない `len` バイトを確保する。
///
/// # SAFETY
/// [`bounded_array_buffer_allocate`] の SAFETY コメントと同じ。
unsafe extern "C" fn bounded_array_buffer_allocate_uninitialized(
    handle: &BoundedArrayBufferAllocatorState,
    len: usize,
) -> *mut c_void {
    bounded_alloc(handle, len, false)
}

/// [`v8::RustAllocatorVtable::free`] の実体: `bounded_array_buffer_allocate`／
/// `bounded_array_buffer_allocate_uninitialized` が返したポインタを解放し、
/// カウンタから `len` を減算する。
///
/// # SAFETY
/// V8 の `Allocator` 契約により、`data`・`len` は同じアロケータの
/// `Allocate`／`AllocateUninitialized` が過去に返した・受け取ったポインタ
/// と長さの組が、変更されずにそのまま渡される（`BackingStore` が
/// 内部で保持する）。したがって `Layout::from_size_align(len,
/// ARRAY_BUFFER_ALLOCATION_ALIGN)` は確保時と同一の `Layout` を再現し、
/// `dealloc` は確保時と対になる呼び出しになる。`len == 0` の場合は
/// `bounded_array_buffer_allocate{,_uninitialized}` が実際には確保して
/// いない（[`zero_length_allocation`] 参照）ため、対応してここでも
/// `dealloc` を呼ばない。`handle` の有効性は
/// [`bounded_array_buffer_allocate`] の SAFETY コメントと同じ。
unsafe extern "C" fn bounded_array_buffer_free(
    handle: &BoundedArrayBufferAllocatorState,
    data: *mut c_void,
    len: usize,
) {
    if len == 0 {
        return;
    }
    let Ok(layout) = Layout::from_size_align(len, ARRAY_BUFFER_ALLOCATION_ALIGN) else {
        // 確保時に同じ計算が成功しているはずであり、原理的に到達しない
        // （`bounded_alloc` は `Layout` 生成に失敗したら確保自体を行わない
        // ため、その場合は `free` が呼ばれることも無い）。パニックせず、
        // 何もせずに返す（`extern "C"` 境界内でパニックしないため）。
        return;
    };
    // SAFETY: 上記のとおり、`data` は同じ `len` で行われた確保の戻り値
    // そのものであり、`layout` はその確保時と同一である。
    unsafe { dealloc(data as *mut u8, layout) };
    release_reservation(&handle.allocated_bytes, len);
}

/// [`v8::RustAllocatorVtable::drop`] の実体: `handle` の所有権を取り戻して
/// 解放する。
///
/// # SAFETY
/// `handle` は [`new_bounded_array_buffer_allocator`] が `Arc::into_raw`
/// で生成した生ポインタであり、V8 はこの `drop` をちょうど 1 回、
/// `Allocator`（延いてはこの `handle`）を破棄するときにだけ呼ぶ
/// （`v8::new_rust_allocator` の契約）。`Arc::from_raw` は「`into_raw` で
/// 得たポインタを、対応する 1 回だけ `from_raw` に渡す」契約を満たす
/// ことが呼び出し元の責務であり、本関数がその唯一の呼び出し箇所である
/// ため、二重解放・use-after-free は起こらない。
unsafe extern "C" fn bounded_array_buffer_drop(handle: *const BoundedArrayBufferAllocatorState) {
    // SAFETY: 上記のとおり。
    let state = unsafe { Arc::from_raw(handle) };
    drop(state);
}

/// [`bounded_array_buffer_allocate`]・[`bounded_array_buffer_allocate_uninitialized`]・
/// [`bounded_array_buffer_free`]・[`bounded_array_buffer_drop`] をまとめた
/// vtable。`'static` な単一のインスタンスを全 Isolate で共有する
/// （`handle` 側だけが Isolate ごとに異なる）。
static BOUNDED_ARRAY_BUFFER_ALLOCATOR_VTABLE: v8::RustAllocatorVtable<
    BoundedArrayBufferAllocatorState,
> = v8::RustAllocatorVtable {
    allocate: bounded_array_buffer_allocate,
    allocate_uninitialized: bounded_array_buffer_allocate_uninitialized,
    free: bounded_array_buffer_free,
    drop: bounded_array_buffer_drop,
};

/// `ArrayBuffer` backing store の確保量を [`MAX_ARRAY_BUFFER_ALLOCATION_BYTES`]
/// までに制限する `v8::Allocator` を新規生成する（codex レビュー指摘
/// #503 P0「macOS の JS ワーカーに強制的なメモリ上限がない」対応。
/// ユーザー承認 2026-09-28。`v8::CreateParams::array_buffer_allocator`
/// へ渡す契約。呼び出し元: `super::v8_engine::V8Engine::new_with_heap_limit`）。
///
/// 承認された `unsafe` はここでの `v8::new_rust_allocator` 呼び出し
/// 1 か所と、上記 4 つの vtable 関数の内部（`std::alloc` の
/// `alloc`/`alloc_zeroed`/`dealloc`・`Arc::from_raw` による handle の
/// 復元）に限定される。
pub(crate) fn new_bounded_array_buffer_allocator() -> v8::UniqueRef<v8::Allocator> {
    let state = Arc::new(BoundedArrayBufferAllocatorState {
        allocated_bytes: AtomicUsize::new(0),
        limit_bytes: MAX_ARRAY_BUFFER_ALLOCATION_BYTES,
    });
    // SAFETY: `Arc::into_raw(state)` produces a pointer that stays valid
    // (the `Arc`'s heap allocation is not freed) until exactly one
    // matching `Arc::from_raw` call reclaims it. That call is
    // `bounded_array_buffer_drop`, which V8 guarantees to call exactly
    // once when the returned `Allocator` is destroyed (see its own SAFETY
    // comment). `BOUNDED_ARRAY_BUFFER_ALLOCATOR_VTABLE` is `'static` and
    // its four functions match the handle type
    // (`BoundedArrayBufferAllocatorState`) exactly, satisfying
    // `new_rust_allocator`'s documented contract ("the caller must ensure
    // that `handle` is valid and matches what `vtable` expects").
    unsafe { v8::new_rust_allocator(Arc::into_raw(state), &BOUNDED_ARRAY_BUFFER_ALLOCATOR_VTABLE) }
}

/// [`read_child_rss_bytes`] が受け取る、子プロセスを指し示す値。
///
/// Windows だけ Linux/macOS と異なり pid ではなく `HANDLE` を使う
/// （ユーザー承認 2026-09-28「ハンドルは std の `Child` から
/// `AsRawHandle` で得る」）。`GetProcessMemoryInfo` は pid ではなく
/// ハンドルを要求する Win32 API であり、`OpenProcess` で pid から
/// ハンドルを作り直す（追加の unsafe 呼び出しが要る）よりも、
/// 呼び出し元が既に持っている `std::process::Child` のハンドルを
/// そのまま使うほうが単純で、承認された unsafe の範囲にも収まる。
#[cfg(target_os = "windows")]
pub(crate) type ChildMemoryProbe = windows_sys::Win32::Foundation::HANDLE;
#[cfg(not(target_os = "windows"))]
pub(crate) type ChildMemoryProbe = u32;

/// 子プロセスのメモリ使用量（バイト）を取得する（親側の監視。
/// `super::process_engine::send_evaluate_and_await` から評価の応答待ちの
/// あいだ定期的に呼ばれる）。Linux は RSS（`/proc/<pid>/status` の
/// `VmRSS`）、macOS は RSS（`ps` の `rss`）、**Windows はコミット量
/// （`GetProcessMemoryInfo` の `PrivateUsage`）**を見る（codex レビュー
/// 指摘 #503 P0「Windows では working set の上限と監視だけではコミット
/// 量を制限できない」対応。working set ベースの `WorkingSet64` から
/// 変更した）。
///
/// 取得に失敗した場合は `None` を返す（呼び出し元の単発の判断としては
/// fail-open。理由: それでも取得失敗のたびに子を kill すると、`/proc` や
/// `ps`・`GetProcessMemoryInfo` が一時的に応答しないだけで正常な評価まで
/// 巻き込んで打ち切ってしまう）。ただし呼び出し元
/// （`super::process_engine::MemoryMonitor`）は連続失敗の回数を数えて
/// おり、[`super::process_engine::MAX_CONSECUTIVE_PROBE_FAILURES`] 回
/// 連続で `None` が続いた場合は fail-closed に切り替える（codex レビュー
/// 指摘 #503 P0「`ps` を起動できない場合、監視が `None` を返し続ける
/// だけで上限が事実上なくなる」対応。単発の判断は fail-open、繰り返しの
/// 判断は fail-closed という 2 段構えになっている）。
pub(crate) fn read_child_rss_bytes(probe: ChildMemoryProbe) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        read_rss_linux(probe)
    }
    #[cfg(target_os = "macos")]
    {
        read_rss_macos(probe)
    }
    #[cfg(target_os = "windows")]
    {
        read_rss_windows(probe)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = probe;
        None
    }
}

/// Linux: `/proc/<pid>/status` の `VmRSS` 行（キロバイト単位）を読む。
#[cfg(target_os = "linux")]
fn read_rss_linux(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // 例: "VmRSS:	   10384 kB"
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib.saturating_mul(1024));
        }
    }
    None
}

/// `ps` の起動から終了まで待つ上限時間（codex レビュー指摘 #503 P1・
/// Cursor Bugbot レビュー指摘「RSS helper can block worker Drop」対応）。
/// 通常の `ps -o rss= -p <pid>` は数十ミリ秒で完了するが、期限を設けず
/// `Child::wait`（`output()` 相当）で待つと、システムが極端に高負荷な
/// 状況で `ps` 自体がハングした場合に監視スレッドが無期限にブロックし、
/// [`super::process_engine::MemoryMonitor::stop_and_join`] の期限付き
/// 待ちだけでは detach された監視スレッドがいつまでも残り続ける（軽微だが
/// 望ましくない）。500 ミリ秒は通常の `ps` 実行時間に対して十分な余裕を
/// 持たせつつ、ハング時の影響を短く抑える値である。
#[cfg(target_os = "macos")]
const PS_TIMEOUT: Duration = Duration::from_millis(500);

/// `ps` の絶対パス（codex レビュー指摘 #503 P0「`ps` を PATH に依存させず
/// 絶対パスで起動する」対応）。`PATH` に `ps` が無い・別の実行ファイルが
/// 先に見つかる等の環境差に左右されないよう、macOS が標準で `ps` を
/// 置く場所を直接指定する。万一この経路に `ps` が存在しない環境（通常の
/// macOS では起こらない）では `spawn()` 自体が失敗し、
/// [`read_rss_macos`] は `None` を返す（呼び出し元の
/// [`super::process_engine::MemoryMonitor`] が連続失敗を数え、
/// fail-closed に倒す。同モジュールのドキュメントコメント参照）。
#[cfg(target_os = "macos")]
const PS_ABSOLUTE_PATH: &str = "/bin/ps";

/// macOS: `ps -o rss= -p <pid>` の出力（キロバイト単位）を読む。macOS には
/// Linux の `/proc` に相当する軽量な読み取り経路が無いため、外部コマンド
/// を都度起動する（モジュール冒頭の表が説明するとおり、macOS では本関数
/// が唯一の防衛線であるため、コストより確実性を優先する）。`ps` は
/// [`PS_ABSOLUTE_PATH`]（絶対パス）で起動し、`PATH` の設定には依存しない。
///
/// `ps` の起動から終了までを [`PS_TIMEOUT`] の期限付きで待つ（期限なしの
/// `Command::output` は使わない。理由は [`PS_TIMEOUT`] のドキュメント
/// コメント参照）。期限を過ぎたら `ps` プロセスを `kill` してから `wait`
/// し、`None` を返す（fail-open。呼び出し元のドキュメントコメント参照）。
#[cfg(target_os = "macos")]
fn read_rss_macos(pid: u32) -> Option<u64> {
    let mut child = std::process::Command::new(PS_ABSOLUTE_PATH)
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + PS_TIMEOUT;
    let mut exit_status = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let Some(exit_status) = exit_status else {
        // 期限内に終わらなかった。`ps` を待たずに諦める（fail-open）が、
        // プロセスは残さない。
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    if !exit_status.success() {
        return None;
    }

    let mut stdout = child.stdout.take()?;
    let mut text = String::new();
    stdout.read_to_string(&mut text).ok()?;
    let kib: u64 = text.trim().parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// Windows: `GetProcessMemoryInfo`（`psapi.dll`）で `PrivateUsage`
/// （コミット量。バイト単位）を読む。以前は PowerShell の `Get-Process`
/// （`WorkingSet64`）を都度起動していたが、Win32 API を直接呼ぶことで
/// プロセス起動コストが無くなり、かつ子側の OS 強制
/// （[`WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`]。コミット量の上限）と同じ
/// 指標を見られるようになった（codex レビュー指摘 #503 P0 対応）。
///
/// `probe` は `std::process::Child` から
/// `std::os::windows::io::AsRawHandle::as_raw_handle` で得たハンドルを
/// 想定する（ユーザー承認 2026-09-28）。呼び出し元（`super::process_engine`）
/// が `Child` の生存を保証する。
#[cfg(target_os = "windows")]
fn read_rss_windows(probe: ChildMemoryProbe) -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };

    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };

    // SAFETY: `probe` は呼び出し元が保証する、生きている（または直前まで
    // 生きていた）子プロセスの有効なハンドル。`GetProcessMemoryInfo` は
    // 拡張版（`_EX`）フィールドを埋めるために `cb` に拡張構造体の
    // サイズを渡す契約であり（Win32 の標準的な作法）、`counters` は
    // その分の領域を持つローカル変数へのポインタとして、この呼び出しの
    // 間だけ有効な可変参照を渡す。
    let ok = unsafe {
        GetProcessMemoryInfo(
            probe,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(counters.PrivateUsage as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// codex レビュー指摘 #503 P0: RSS しきい値（320 MiB）・
    /// `ArrayBuffer` 確保の合計上限（128 MiB）が、ドキュメントコメントが
    /// 説明する根拠どおりの具体値であること。ヒープ上限＋本上限が
    /// RSS しきい値を 64 MiB 下回ることも確認する（`MAX_ARRAY_BUFFER_ALLOCATION_BYTES`
    /// のドキュメントコメントが説明する予算配分）。
    #[test]
    fn js_1_memory_budget_constants_match_documented_values() {
        assert_eq!(MAX_CHILD_RSS_BYTES, 320 * 1024 * 1024);
        assert_eq!(MAX_ARRAY_BUFFER_ALLOCATION_BYTES, 128 * 1024 * 1024);
        assert_eq!(WASM_MAX_MEM_PAGES, 2048);
        let heap_plus_array_buffer =
            super::super::v8_engine::MAX_ISOLATE_HEAP_BYTES + MAX_ARRAY_BUFFER_ALLOCATION_BYTES;
        assert_eq!(
            heap_plus_array_buffer as u64,
            MAX_CHILD_RSS_BYTES - 64 * 1024 * 1024
        );
    }

    /// テスト専用の [`BoundedArrayBufferAllocatorState`] を作る（`Arc` へは
    /// 包まない。単体テストは vtable 関数を直接呼ぶだけであり、
    /// `v8::new_rust_allocator` を経由しないため `Arc::into_raw`／
    /// `Arc::from_raw` の対応関係を気にする必要が無い）。
    fn test_allocator_state(limit_bytes: usize) -> BoundedArrayBufferAllocatorState {
        BoundedArrayBufferAllocatorState {
            allocated_bytes: AtomicUsize::new(0),
            limit_bytes,
        }
    }

    /// codex レビュー指摘 #503 P0 の単体テスト: 上限以下の確保は成功し、
    /// カウンタが確保量ぶん増え、`free` で確保量ぶん減ることを確認する。
    #[test]
    fn js_1_bounded_array_buffer_allocator_tracks_allocate_and_free() {
        let state = test_allocator_state(1024);

        // SAFETY: `state` は本テスト内でのみ参照され、`allocate`／`free`
        // の呼び出しは他のどのスレッドとも競合しない。`len`（64）は
        // `state.limit_bytes`（1024）を超えない。
        let ptr = unsafe { bounded_array_buffer_allocate(&state, 64) };
        assert!(!ptr.is_null(), "allocation within the limit must succeed");
        assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 64);

        // SAFETY: `ptr` は直前の `bounded_array_buffer_allocate(&state, 64)`
        // の戻り値そのものであり、`len` も同じ 64 を渡している。
        unsafe { bounded_array_buffer_free(&state, ptr, 64) };
        assert_eq!(
            state.allocated_bytes.load(Ordering::SeqCst),
            0,
            "free must return the reserved bytes to the counter"
        );
    }

    /// codex レビュー指摘 #503 P0 の単体テスト: 上限をちょうど使い切る
    /// 確保は成功し、さらに 1 バイトでも超える確保は拒否される（境界値）。
    /// 拒否された確保はカウンタを変化させないことも確認する。
    #[test]
    fn js_1_bounded_array_buffer_allocator_rejects_allocation_exceeding_the_limit() {
        let state = test_allocator_state(128);

        // SAFETY: 上記テストと同様、単一スレッドからの呼び出しであり
        // `len` は `state.limit_bytes` と一致する（境界値）。
        let ptr = unsafe { bounded_array_buffer_allocate_uninitialized(&state, 128) };
        assert!(
            !ptr.is_null(),
            "an allocation exactly at the limit must succeed"
        );
        assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 128);

        // SAFETY: 直前の確保の戻り値・長さと対応する解放。
        unsafe { bounded_array_buffer_free(&state, ptr, 128) };
        assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 0);

        // SAFETY: `len`（129）は解放されない（`null` が返るため）。
        let rejected = unsafe { bounded_array_buffer_allocate(&state, 129) };
        assert!(
            rejected.is_null(),
            "an allocation exceeding the limit by even 1 byte must be rejected"
        );
        assert_eq!(
            state.allocated_bytes.load(Ordering::SeqCst),
            0,
            "a rejected allocation must not change the counter"
        );
    }

    /// codex レビュー指摘 #503 P0 の単体テスト: `len == 0` の確保は
    /// カウンタを変化させず、非 null のポインタを返す（`std::alloc` の
    /// サイズ 0 `Layout` を避ける分岐。`free` に `len == 0` で渡しても
    /// panic せず、カウンタも変化しないこと）。
    #[test]
    fn js_1_bounded_array_buffer_allocator_handles_zero_length_without_counting() {
        let state = test_allocator_state(0);

        // SAFETY: `len == 0` は実際の確保を行わない特別扱いであり、
        // `state.limit_bytes`（0）にも抵触しない。
        let ptr = unsafe { bounded_array_buffer_allocate(&state, 0) };
        assert!(
            !ptr.is_null(),
            "a zero-length allocation must return a non-null sentinel pointer"
        );
        assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 0);

        // SAFETY: `len == 0` で確保したポインタを、同じ `len == 0` で
        // 解放する。
        unsafe { bounded_array_buffer_free(&state, ptr, 0) };
        assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 0);
    }

    /// codex レビュー指摘 #503 P0 の単体テスト: 上限に達した後でも、
    /// 解放してから再度確保すれば成功する（カウンタが漏れなく戻ることの
    /// 確認。`tests/v8_worker.rs` の結合テストが確認する「GC を挟んだ
    /// 繰り返し確保」の土台となる単体レベルの保証）。
    #[test]
    fn js_1_bounded_array_buffer_allocator_can_reallocate_after_freeing() {
        let state = test_allocator_state(64);

        for _ in 0..5 {
            // SAFETY: 各イテレーションで直前の確保を解放してから次を
            // 行うため、常に高々 1 つの生存中の確保しか無い。
            let ptr = unsafe { bounded_array_buffer_allocate(&state, 64) };
            assert!(
                !ptr.is_null(),
                "re-allocation after freeing must succeed repeatedly, not just once"
            );
            assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 64);

            // SAFETY: 直前の確保の戻り値・長さと対応する解放。
            unsafe { bounded_array_buffer_free(&state, ptr, 64) };
            assert_eq!(state.allocated_bytes.load(Ordering::SeqCst), 0);
        }
    }

    /// advisor レビュー指摘（advisor がレビューした codex P0 対応の一部）:
    /// Windows の `ProcessMemoryLimit` は `MAX_CHILD_RSS_BYTES`（320 MiB）
    /// と**同じ値にしてはならない**（`JOB_OBJECT_LIMIT_PROCESS_MEMORY` は
    /// 上限到達時にプロセスを終了させず確保を失敗させるだけのため、
    /// 同じ値だと親側の監視が `>` 判定を満たせず検出できない回帰が
    /// 起こる。[`WINDOWS_PROCESS_MEMORY_LIMIT_BYTES`] のドキュメント
    /// コメント参照）。64 MiB の余裕を載せた 384 MiB であることを固定
    /// する。
    #[cfg(target_os = "windows")]
    #[test]
    fn js_1_windows_process_memory_limit_has_margin_over_the_monitoring_threshold() {
        assert_eq!(WINDOWS_PROCESS_MEMORY_LIMIT_BYTES, 384 * 1024 * 1024);
        assert!(WINDOWS_PROCESS_MEMORY_LIMIT_BYTES as u64 > MAX_CHILD_RSS_BYTES);
    }

    /// codex レビュー指摘 #503 P0: Linux の `RLIMIT_DATA` 上限が実測に
    /// 基づく 2 GiB であること（本モジュールのドキュメントコメント
    /// 「OS ごとの強制の強さ」節の実測値の約 2.7 倍の安全マージン）。
    #[cfg(target_os = "linux")]
    #[test]
    fn js_1_linux_rlimit_data_ceiling_matches_the_documented_value() {
        assert_eq!(LINUX_RLIMIT_DATA_CEILING_BYTES, 2 * 1024 * 1024 * 1024);
    }

    /// codex レビュー指摘 #503 P1「既存の RLIMIT_DATA を超える hard limit
    /// を設定しない」の単体テスト: `compute_rlimit_data_target` が、
    /// 既存の hard limit の 3 パターン（無制限・上限より大きい・上限より
    /// 小さい）それぞれで、期待どおりの具体値を返すこと。
    #[cfg(target_os = "linux")]
    #[test]
    fn js_1_compute_rlimit_data_target_never_exceeds_the_existing_hard_limit() {
        // 既存の hard limit が無制限（`None`）: 本来の上限（2 GiB）を
        // そのまま使う。
        assert_eq!(
            compute_rlimit_data_target(None),
            LINUX_RLIMIT_DATA_CEILING_BYTES
        );

        // 既存の hard limit が本来の上限より大きい（例: 4 GiB）:
        // 本来の上限（2 GiB）まで下げる。既存の hard limit をそのまま
        // 使うと、本モジュールが前提とする「際限のない膨張だけは防ぐ」
        // という設計より緩い上限になってしまうため。
        let larger_hard_limit = 4 * 1024 * 1024 * 1024;
        assert_eq!(
            compute_rlimit_data_target(Some(larger_hard_limit)),
            LINUX_RLIMIT_DATA_CEILING_BYTES
        );

        // 既存の hard limit が本来の上限より小さい（例: 512 MiB。
        // コンテナ・systemd 等で既により厳しい制限がある環境を模す）:
        // 既存の hard limit をそのまま使う（引き上げない。非特権
        // プロセスでも `setrlimit` が確実に成功することを優先する）。
        let smaller_hard_limit = 512 * 1024 * 1024;
        assert_eq!(
            compute_rlimit_data_target(Some(smaller_hard_limit)),
            smaller_hard_limit
        );

        // 既存の hard limit がちょうど本来の上限と同じ場合: そのまま
        // 使う（境界値）。
        assert_eq!(
            compute_rlimit_data_target(Some(LINUX_RLIMIT_DATA_CEILING_BYTES)),
            LINUX_RLIMIT_DATA_CEILING_BYTES
        );
    }

    /// codex レビュー指摘 #503 P0: 現在のプロセス自身の RSS を取得できる
    /// こと（`read_child_rss_bytes` が実プラットフォームで実際に機能する
    /// ことの最小限の確認。子プロセスを起動する結合テストは
    /// `tests/v8_worker.rs` 側で行う）。
    ///
    /// Windows は `ChildMemoryProbe` が pid ではなく `HANDLE` であり、
    /// 本テストのために新たな `unsafe` 呼び出し（例:
    /// `GetCurrentProcess`）を追加することは承認された unsafe の範囲外
    /// になるため、本テストは Windows では実行しない。Windows での
    /// `read_rss_windows`（`GetProcessMemoryInfo`）の確認は、実際に
    /// 子プロセスを起動する `tests/v8_worker.rs` の結合テスト
    /// （既存の生産コード経路である `child_memory_probe` を通す）で行う。
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn js_1_read_child_rss_bytes_reports_a_nonzero_value_for_the_current_process() {
        let pid = std::process::id();
        let rss = read_child_rss_bytes(pid);
        assert!(
            rss.is_some_and(|bytes| bytes > 0),
            "expected a nonzero RSS for the current process, got: {rss:?}"
        );
    }

    /// codex レビュー指摘 #503 P1・Cursor Bugbot レビュー指摘「RSS helper
    /// can block worker Drop」の回帰テスト（可能な範囲で）: macOS の
    /// `ps` 呼び出しが、通常のケース（現在のプロセス自身。必ず存在する
    /// pid）で [`PS_TIMEOUT`]（500 ミリ秒）に対して十分な余裕を持って
    /// 完了すること。`ps` がハングする状況そのものは決定的に再現でき
    /// ないため、通常時に期限内で戻ることの確認に留める。
    #[cfg(target_os = "macos")]
    #[test]
    fn js_1_read_rss_macos_returns_well_within_the_timeout_for_a_live_process() {
        let pid = std::process::id();
        let started = Instant::now();
        let rss = read_rss_macos(pid);
        let elapsed = started.elapsed();
        assert!(
            rss.is_some_and(|bytes| bytes > 0),
            "expected a nonzero RSS for the current process, got: {rss:?}"
        );
        assert!(
            elapsed < PS_TIMEOUT,
            "expected `ps` to complete well within PS_TIMEOUT ({PS_TIMEOUT:?}) for a live \
             process, took: {elapsed:?}"
        );
    }
}

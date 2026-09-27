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
//! - **子側（V8 初期化前）**: wasm の単一メモリインスタンスに上限を掛ける
//!   （[`configure_wasm_memory_flag`]）
//! - **親側（評価の応答待ちのあいだ）**: 子の RSS を定期的に監視し、
//!   上限を超えたら kill する（[`read_child_rss_bytes`]。
//!   `super::process_engine::send_evaluate_and_await` から呼ぶ）。**すべての
//!   OS でこれが主たる防衛線**であり、下記の子側の OS 別強制はその補助
//!   （効けば早期に検出できるが、効かなくても親の監視が最終的に捕まえる）
//!   にすぎない（実測に基づく判断。理由は次節）
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
//! | Linux | `RLIMIT_DATA`（[`enforce_child_memory_limit`]。`rustix::process::setrlimit`。上限 [`LINUX_RLIMIT_DATA_CEILING_BYTES`]＝2 GiB） | **OS がある程度強制するが、厳密な上限としては機能しない**。`RLIMIT_DATA` は匿名 `MAP_PRIVATE` の `mmap`（大きな `ArrayBuffer` の backing store が実際に使う経路。glibc malloc は既定のしきい値 128 KiB を超える確保を `mmap` に回す）にも、実際に触れていない仮想予約にも適用される（`setrlimit(2)` の「data segment のみ」という古い説明は、匿名 mmap を会計に含めない実装を前提にしており、本 crate が対象とする現行 Linux カーネルの挙動とは異なる）。しかし V8 自身の `CodeRange` 予約だけで数百 MiB（アーキテクチャ依存。実測は上記のとおり）を消費するため、[`HEAP_EXTERNAL_ALLOWANCE_BYTES`] 相当の小さい値には設定できず、実質的な防御は親側の RSS 監視に委ねている |
//! | Windows | Job Object（[`enforce_child_memory_limit`]。`win32job` の `limit_working_memory`。`JOB_OBJECT_LIMIT_WORKINGSET`。上限 [`WINDOWS_WORKING_SET_MAX_BYTES`]） | **OS の関与はあるが部分的（未実機検証）**。承認済みの `win32job =2.0.3` が公開する安全な API には、コミットチャージの上限を超えたらプロセスを強制終了する本来の `ProcessMemoryLimit` 相当（`JOB_OBJECT_LIMIT_PROCESS_MEMORY`/`JOB_OBJECT_LIMIT_JOB_MEMORY`）を設定する手段が無い（`ExtendedLimitInfo` が内部に持つ `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` は `pub(crate)` で本 crate からは触れず、`unsafe` を追加してまで構造体を直接書き換えることは本対応の制約「`unsafe` の追加は都度承認」に反するため行わない）。`JOB_OBJECT_LIMIT_WORKINGSET` は物理メモリの常駐量（working set）を trim させるだけで、コミットチャージそのものを止めたりプロセスを終了させたりしない。したがって Windows も実質的に親側の RSS 監視が主たる防衛線であり、Job 自体は working set を trim させる補助的な効果に留まる。真の `ProcessMemoryLimit` を得るには `win32job` の対応バージョンへの更新（ユーザー承認が必要）か `unsafe` な直接呼び出し（同様に承認が必要）のいずれかが要る。Windows 版 3 OS CI（`js-v8` feature 込み）は Job の作成・`assign_current_process`・子プロセスの起動を含めて通っている（2026-09-28 時点）が、これは「経路がエラーにならず動く」ことの確認であり、working set 上限が実際に機能してメモリを trim させる（＝限界まで確保して trim を観測する）ところまでは検証できていない。開発環境が Windows ではないため、本 crate 側でその種の実機検証はできていない |
//! | macOS | 無し | **親側の RSS 監視のみ**。本実装時にこの macOS 環境で実機検証したところ、`RLIMIT_DATA`・`RLIMIT_AS`・`RLIMIT_RSS` はいずれも `setrlimit(2)` の呼び出し自体が `EINVAL` で失敗した（「上限をかけたが効かない」ではなく「そもそも設定できない」）。Job Object 相当の OS 機構も無い。したがって [`enforce_child_memory_limit`] は macOS では何もせず常に成功を返し（呼び出そうとしても確実に失敗するため、fail-closed にすると macOS 上で子プロセスが常に起動できなくなってしまう）、実際の防御は親側の RSS 監視だけに委ねる |
//!
//! # 既知の制限（実装済みを装わない。REPAIR-3）
//!
//! - **主たる防御は 3 OS 共通で親側の RSS 監視であり、子側の OS 別強制は
//!   いずれも補助に留まる**（Linux は `CodeRange` 予約のため厳密な上限に
//!   できない。Windows は working set の trim にしかならない。macOS は
//!   OS 側の手段が無い）
//! - 親側の RSS 監視は [`RSS_POLL_INTERVAL`] の分だけ後追いになる。1 回の
//!   JS 実行が割り込みチェックを挟まずにポーリング間隔内で大量に確保
//!   すると、実際のピークメモリは一時的に [`MAX_CHILD_RSS_BYTES`] を
//!   超えてから検出・終了する
//! - RSS 取得（`/proc/<pid>/status`・`ps`・PowerShell の `Get-Process`）に
//!   失敗した場合は監視を諦めてそのポーリング回だけスキップする
//!   （fail-open。[`read_child_rss_bytes`] のドキュメントコメント参照）。
//!   [`enforce_child_memory_limit`] 自体の失敗は fail-closed（評価を
//!   始めずに終了）であり、起動時強制と監視とで fail-open/fail-closed の
//!   扱いが異なることに注意
//! - Windows 側の working set 上限の実効性（trim が実際に発動するか）は
//!   未検証。CI では経路（Job の作成・割り当て・子プロセスの起動）が
//!   エラーなく動くことまでは確認済み（上表参照）

use std::time::Duration;

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
/// 64 MiB を上乗せする（合計 320 MiB）。**3 OS 共通で主たる防衛線**
/// （モジュール冒頭のドキュメントコメント参照）であるため、
/// [`RSS_POLL_INTERVAL`] のドキュメントコメントが説明する「後追い」の
/// 限界とあわせて運用する。
///
/// [`LINUX_RLIMIT_DATA_CEILING_BYTES`]・[`WINDOWS_WORKING_SET_MAX_BYTES`]
/// とは意図的に値を分離している（本モジュールのドキュメントコメント
/// 「OS ごとの強制の強さ」節の実測結果を参照）。
pub(crate) const MAX_CHILD_RSS_BYTES: u64 = super::v8_engine::MAX_ISOLATE_HEAP_BYTES as u64
    + HEAP_EXTERNAL_ALLOWANCE_BYTES
    + 64 * 1024 * 1024;

/// 親が子の RSS をポーリングする間隔。50〜100 ミリ秒の範囲で、監視の
/// 追随性（短いほど検出が速い）と監視自体のコスト（`ps`・PowerShell の
/// 起動を伴う OS では特に、短すぎると監視自体が負荷になる）の折衷として
/// 75 ミリ秒を選んだ。
///
/// **Windows だけ 500 ミリ秒にしている**（[`read_rss_windows`] が
/// PowerShell プロセスを都度起動するため。PowerShell の起動コストは
/// 200〜500 ミリ秒程度かかることがあり、75 ミリ秒間隔で呼ぶと
/// 「評価の応答待ちスレッドが PowerShell の起動待ちで塞がれ、数十〜
/// 百ミリ秒程度で終わる通常のスクリプト評価まで大きく遅延させる」
/// 問題が生じる。設計書の指示（「Windows は監視を省略するか軽いものでよい」）
/// に従い、Windows では監視の追随性より評価のレイテンシを優先し、
/// ポーリング間隔を緩める（本 crate の開発環境が Windows ではないため
/// 実機で計測はできておらず、この値は PowerShell 起動コストの一般的な
/// 目安に基づく見積もりである）。
#[cfg(not(target_os = "windows"))]
pub(crate) const RSS_POLL_INTERVAL: Duration = Duration::from_millis(75);
#[cfg(target_os = "windows")]
pub(crate) const RSS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Linux の `RLIMIT_DATA`（soft/hard 両方）に設定する上限（バイト。
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
#[cfg(target_os = "linux")]
const LINUX_RLIMIT_DATA_CEILING_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Windows の Job Object working set 上限（バイト。512 MiB）。working set
/// の trim にしか効かない（モジュール冒頭の表参照）ため、
/// [`MAX_CHILD_RSS_BYTES`] より緩めの値にして、正常なスクリプト評価が
/// working set の trim に巻き込まれて過度に遅くなることを避ける。実際の
/// メモリ上限としての強制は親側の RSS 監視が担う。
#[cfg(target_os = "windows")]
const WINDOWS_WORKING_SET_MAX_BYTES: usize = 512 * 1024 * 1024;

/// wasm の 1 ページのバイト数（V8 の仕様で固定。64 KiB）。
const WASM_MEMORY_PAGE_BYTES: u64 = 64 * 1024;

/// wasm の単一メモリインスタンスに許す最大ページ数。
/// [`HEAP_EXTERNAL_ALLOWANCE_BYTES`]（128 MiB）と同じ予算を割り当てる
/// （wasm のメモリも V8 のヒープ外に確保されるため。wasm のメモリは V8 の
/// `CodeRange` 予約とは別の会計であり、Linux の `RLIMIT_DATA` の実効値を
/// 左右する問題とは無関係に、この値をそのまま使える）。
/// `HEAP_EXTERNAL_ALLOWANCE_BYTES / WASM_MEMORY_PAGE_BYTES` = 2048 ページ。
const WASM_MAX_MEM_PAGES: u64 = HEAP_EXTERNAL_ALLOWANCE_BYTES / WASM_MEMORY_PAGE_BYTES;

/// [`enforce_child_memory_limit`] が成功した際に返すガード（codex
/// レビュー指摘 #503 P1「`enforce_via_job_object` が Job をローカル変数の
/// まま返しているため、関数を抜けた時点でハンドルが閉じてしまう」
/// 対応）。
///
/// Windows では Win32 の Job Object はハンドルへの参照が無くなると
/// 閉じられ、`assign_current_process` で設定した working set 上限が
/// 維持される保証が無くなる。呼び出し元（`super::worker::worker_main`）
/// は、このガードを子プロセスの寿命いっぱい（`worker_main` 関数の
/// スコープの終わりまで）保持しなければならない。
///
/// Linux（`setrlimit`）・macOS（何もしない）では追加のハンドルを必要と
/// しないため、このガードは中身を持たないユニット型として振る舞う。
#[must_use = "drop するとメモリ上限が失われる場合がある（Windows）。プロセスの寿命いっぱい保持すること"]
pub(crate) struct ChildMemoryLimitGuard {
    #[cfg(target_os = "windows")]
    _job: win32job::Job,
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
        let job = enforce_via_job_object()?;
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

/// Linux: `RLIMIT_DATA` の soft/hard 両方を [`LINUX_RLIMIT_DATA_CEILING_BYTES`]
/// に設定する。`rustix::process::setrlimit` は safe API のため `unsafe` を
/// 追加しない。
#[cfg(target_os = "linux")]
fn enforce_via_rlimit_data() -> Result<(), String> {
    use rustix::process::{Resource, Rlimit, setrlimit};

    let limit = Rlimit {
        current: Some(LINUX_RLIMIT_DATA_CEILING_BYTES),
        maximum: Some(LINUX_RLIMIT_DATA_CEILING_BYTES),
    };
    setrlimit(Resource::Data, limit).map_err(|err| {
        format!("failed to set RLIMIT_DATA to {LINUX_RLIMIT_DATA_CEILING_BYTES} bytes: {err}")
    })
}

/// Windows: Job Object を作成し、working set 上限を
/// [`WINDOWS_WORKING_SET_MAX_BYTES`] に設定したうえで自分自身（呼び出し
/// プロセス）を割り当て、その `Job` を呼び出し元へ返す（codex レビュー
/// 指摘 #503 P1 対応。呼び出し元がこの `Job` を保持し続けないと、
/// ハンドルが閉じて上限が失われうる。[`ChildMemoryLimitGuard`] 参照）。
/// `win32job` は Win32 API を隠蔽した safe ラッパのため `unsafe` を
/// 追加しない。
///
/// working set 上限が実際にはコミットチャージの上限にならないことは
/// モジュール冒頭の表を参照（既知の制限。実装済みを装わない。REPAIR-3）。
/// working set 上限の実効性（trim の発動）は未検証である（同表参照。
/// Windows 版 3 OS CI ではこの関数がエラーなく完走することは確認済み）。
#[cfg(target_os = "windows")]
fn enforce_via_job_object() -> Result<win32job::Job, String> {
    use win32job::{ExtendedLimitInfo, Job};

    let mut info = ExtendedLimitInfo::new();
    // working set の下限は、通常のトリビアルなスクリプト評価に必要な
    // 常駐メモリを OS がすぐ再確保しなくて済む程度の値（16 MiB）にする。
    const MIN_WORKING_SET_BYTES: usize = 16 * 1024 * 1024;
    info.limit_working_memory(MIN_WORKING_SET_BYTES, WINDOWS_WORKING_SET_MAX_BYTES);

    let job = Job::create_with_limit_info(&info)
        .map_err(|err| format!("failed to create a Windows Job Object: {err}"))?;
    job.assign_current_process()
        .map_err(|err| format!("failed to assign this process to the Job Object: {err}"))?;
    Ok(job)
}

/// wasm の単一メモリインスタンスに [`WASM_MAX_MEM_PAGES`] を上限として
/// 設定する（V8 152.2.0 の `v8/src/flags/flag-definitions.h` に定義された
/// `wasm_max_mem_pages` フラグに対応。コマンドライン形式は
/// `--wasm-max-mem-pages=<N>`）。
///
/// **`v8::V8::initialize_platform` より前に呼ぶ契約**（フラグは V8 の
/// 初期化前にしか反映されない。`super::v8_engine::ensure_v8_initialized`
/// 参照）。
pub(crate) fn configure_wasm_memory_flag() {
    v8::V8::set_flags_from_string(&format!("--wasm-max-mem-pages={WASM_MAX_MEM_PAGES}"));
}

/// 子プロセスの RSS（常駐メモリ量。バイト）を取得する（親側の監視。
/// `super::process_engine::send_evaluate_and_await` から評価の応答待ちの
/// あいだ定期的に呼ばれる）。
///
/// 取得に失敗した場合は `None` を返す（fail-open。理由: 親側の RSS 監視は
/// 3 OS 共通の主たる防衛線であるが、それでも取得失敗のたびに子を kill
/// すると、`/proc` や `ps`・PowerShell が一時的に応答しないだけで正常な
/// 評価まで巻き込んで打ち切ってしまう。呼び出し元は `None` を「そのポー
/// リング回は監視できなかった」として扱い、次のポーリングで再試行する。
/// 全体の期限（`super::process_engine::EVALUATE_RECV_TIMEOUT`）は別途
/// 効いているため、監視が効かない期間が無限に続くことはない）。
pub(crate) fn read_child_rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        read_rss_linux(pid)
    }
    #[cfg(target_os = "macos")]
    {
        read_rss_macos(pid)
    }
    #[cfg(target_os = "windows")]
    {
        read_rss_windows(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = pid;
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

/// macOS: `ps -o rss= -p <pid>` の出力（キロバイト単位）を読む。macOS には
/// Linux の `/proc` に相当する軽量な読み取り経路が無いため、外部コマンド
/// を都度起動する（モジュール冒頭の表が説明するとおり、macOS では本関数
/// が唯一の防衛線であるため、コストより確実性を優先する）。
#[cfg(target_os = "macos")]
fn read_rss_macos(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let kib: u64 = text.trim().parse().ok()?;
    Some(kib.saturating_mul(1024))
}

/// Windows: PowerShell の `Get-Process` で `WorkingSet64`（バイト単位）を
/// 読む。Job の working set 上限（[`enforce_via_job_object`]）が実際には
/// コミットチャージを止めないため（モジュール冒頭の表参照）、Windows でも
/// 本関数による監視を有効にする。
#[cfg(target_os = "windows")]
fn read_rss_windows(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-Process -Id {pid}).WorkingSet64"),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// codex レビュー指摘 #503 P0: RSS しきい値（320 MiB）・wasm ページ数
    /// （2048）が、ドキュメントコメントが説明する根拠どおりの具体値で
    /// あること。
    #[test]
    fn js_1_memory_budget_constants_match_documented_values() {
        assert_eq!(MAX_CHILD_RSS_BYTES, 320 * 1024 * 1024);
        assert_eq!(WASM_MAX_MEM_PAGES, 2048);
    }

    /// codex レビュー指摘 #503 P0: Linux の `RLIMIT_DATA` 上限が実測に
    /// 基づく 2 GiB であること（本モジュールのドキュメントコメント
    /// 「OS ごとの強制の強さ」節の実測値の約 2.7 倍の安全マージン）。
    #[cfg(target_os = "linux")]
    #[test]
    fn js_1_linux_rlimit_data_ceiling_matches_the_documented_value() {
        assert_eq!(LINUX_RLIMIT_DATA_CEILING_BYTES, 2 * 1024 * 1024 * 1024);
    }

    /// codex レビュー指摘 #503 P0: 現在のプロセス自身の RSS を取得できる
    /// こと（`read_child_rss_bytes` が実プラットフォームで実際に機能する
    /// ことの最小限の確認。子プロセスを起動する結合テストは
    /// `tests/v8_worker.rs` 側で行う）。
    #[test]
    fn js_1_read_child_rss_bytes_reports_a_nonzero_value_for_the_current_process() {
        let pid = std::process::id();
        let rss = read_child_rss_bytes(pid);
        assert!(
            rss.is_some_and(|bytes| bytes > 0),
            "expected a nonzero RSS for the current process, got: {rss:?}"
        );
    }
}

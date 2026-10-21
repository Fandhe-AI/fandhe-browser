//! 確定文書の上で保持する評価コンテキスト: `Send + Sync` なハンドルへ「キー + 確定済みの文書 +
//! ソース」を渡すと、専用スレッドが保持する `JsRuntime` + `DomBridge` の上でソースを評価し、
//! 結果文字列（JS 評価結果）を async で返す（TASK-44.5・MS-4・Issue #1216・ビヘイビア
//! `CDP-2`・`JS-1`・`JS-6`・`PERF-7`・`REPAIR-3`）。
//!
//! # 役割と呼び出し元
//!
//! Playwright の utility world が成立せず `selector` 段階が `StageTimeout` になる問題（#1062）に
//! 対し、cdp の `Runtime.evaluate` 等（#1217〜#1219）は「確定したページの DOM の上で JS を評価し
//! 続けられる実体」を要する。依存方向（cdp → js を直接作らない）により、その実体を core に置く。
//! `AppState::script_contexts()`（`CdpState::app_state()` 経由）で到達できる。`page_actor` は遷移中に
//! `<script>` を 1 回実行して破棄する用途で、評価後もランタイムを保持できないため別アクターとした。
//! cli が `[js] engine` から構築したハンドルを `AppState` へ渡す（配線は #1220）。本モジュールだけでは
//! 既定の挙動は変わらない（`AppState` の既定は無効）。
//!
//! # 契約
//!
//! - **キー**: [`ScriptContextKey`] 1 つ = `JsRuntime` 1 つ + `DomBridge` 1 つ（V8 では子プロセス
//!   1 つ）。`world` には cdp が払い出す contextId（`u64`）を渡す。world 名などの文字列は持ち込まない。
//! - **HTML の受け取り方（Issue の API 案との差分）**: cdp はターゲットごとに別の `NavigationState`
//!   を持ち、core 側からは辿れない。そのため [`ScriptContextHandle::evaluate`] は文書を
//!   `Arc<NavigationResult>`（cdp が `NavigationState::latest()` から得る。HTML はコピーしない）で
//!   受け取る。ワーカーはキーが未作成のときだけこれを使い、既存のキーでは無視する。
//! - **遅延起動（`PERF-7`）**: ハンドルの生成・[`begin_generation`](ScriptContextHandle::begin_generation)・
//!   [`release_scope`](ScriptContextHandle::release_scope) ではスレッドも子プロセスも作らない。
//!   最初の `evaluate` で初めてワーカーを起動し、キーの初回評価でコンテキストを作る。ワーカーは
//!   「待ち行列が空 かつ コンテキストが 0」になったら自分で終了する（`JsRuntime` は `!Send` のため
//!   コンテキストが残る間はワーカーが生き続ける）。
//! - **有効化**: `JsConfig::engine()` が `Some` で、構築時の `JsRuntime::from_config` が `Ok` のとき
//!   だけ有効（`create_engine` は子プロセスを起動しない）。macOS の boa のように生成が失敗する構成は
//!   無効（`is_enabled() == false`）になり、評価は [`ScriptContextError::Unavailable`]。
//!   `[js] execute_page_scripts` はこのハンドルを左右しない。
//! - **世代（`CDP-2`）**: scope ごとに「見た中で最新の世代」を記録する。`begin_generation(scope, g)` は
//!   最新を `g` に更新し、その scope の古い世代のコンテキストを破棄する。`evaluate` で `key.generation`
//!   が最新より古ければ [`ScriptContextError::NotFound`]、新しければ古い世代を破棄して更新する。
//!   同じ世代で未作成のキーは上限内で新規作成する。`release_scope` は scope のコンテキストをすべて破棄し
//!   記録も消す。release 後に同じ scope へ評価を送らないのは呼び出し側の契約（送られた場合は上限内で
//!   新規作成になる）。通知と評価は同じ FIFO を通るため、送った順に処理される。
//! - **上限（`JS-6`）**: 生きているコンテキスト数は全体 [`DEFAULT_MAX_LIVE_SCRIPT_CONTEXTS`]・scope
//!   あたり [`DEFAULT_MAX_SCRIPT_CONTEXTS_PER_SCOPE`]。待ち行列は [`DEFAULT_SCRIPT_CONTEXT_QUEUE_CAPACITY`]
//!   件（評価のみ数える。実行中を除く）で、超過は待たせず `LimitExceeded`。ソースは
//!   [`MAX_SCRIPT_CONTEXT_SOURCE_BYTES`]、結果は [`DEFAULT_MAX_SCRIPT_RESULT_BYTES`]、タイムアウトは既定
//!   [`DEFAULT_SCRIPT_EVAL_TIMEOUT`]・最大 [`MAX_SCRIPT_EVAL_TIMEOUT`]。DOM 操作回数・ノード数は
//!   [`DomBridgeLimits`] と `DomLimits` に従う。
//! - **エンジンの再起動を隠さない**: 子プロセスの異常終了・親による kill（"context was discarded"
//!   を含む `Timeout` / `ResourceLimitExceeded`）は [`ScriptContextError::EngineRestarted`] で返し、そのキーの
//!   コンテキストを破棄する。同じキーの次の `evaluate` は新しいコンテキストを作る。cdp は受け取ったら
//!   ヘルパー JS を注入し直す必要がある（#1218 への申し送り）。V8 の watchdog による単純な `Timeout`・
//!   `ResourceLimit` ではコンテキストが残る。
//!   "context was discarded" の文言照合は、js crate の `ensure_discarded_phrase`（Issue #527）が
//!   「すべての破棄経路に付ける」ことを保証した事後条件に限って認める。一方、結果サイズ超過の
//!   メッセージは各エンジンが個別に組み立てる文言で契約ではないため照合しない。エンジンへは天井
//!   （3 MiB）を渡し、返った文字列を core がコンテキストの上限と比べる。
//!
//! # スタブ・簡易実装・既知の制限（REPAIR-3）
//!
//! - **書き戻さない**: コンテキスト内の DOM 変更は `NavigationResult` に書き戻さない（AI snapshot・
//!   query・シリアライズは確定時の HTML のまま。変更後の DOM が見えるのは評価だけ）。
//! - **イベントループを回さない**: shim の `fetch` / XHR は要求を積むだけで取得されず、`setTimeout` /
//!   `setInterval` は発火しない。microtask checkpoint も自動では回さない（`awaitPromise` は cdp が
//!   `__fandheRunMicrotasks()` を別の評価で呼ぶ想定。#1219）。
//! - **ページの `<script>` は再実行しない**: 確定 HTML を再パースするだけで、ページ JS が作った
//!   グローバルは存在しない。同じ文書でも world（キー）ごとに別の `Document` を持ち、互いの変更は見えない。
//! - **DOM 操作回数の上限は生存期間あたり**: `DomBridgeLimits::max_ops_per_page` は `attach` 単位で
//!   数えるため、コンテキストでは生存期間あたりの上限になり、超過後は同じコンテキストで拒否され続ける
//!   （次の世代で新しいコンテキストになる）。shim の注入自体も操作を消費する。
//! - **結果サイズの分類**: 既定（3 MiB = エンジンの天井）では core の検査は働かず、天井超過（3 MiB 超、
//!   または V8・boa 共通の 1M UTF-16 単位超）は [`ScriptContextError::EvaluationFailed`] として返る。
//! - 全ターゲットの評価は 1 本のスレッドで直列に処理する。待ちは非同期なので他の CDP 呼び出しは塞がない。
//! - release ビルドの panic は `panic = "abort"` でプロセスごと終了する（回復経路が働くのは unwind する
//!   ビルドと panic 以外の失敗）。
//! - 上限値の設定ファイルからの読み込みは未対応（定数とオプション型で持つ。新しい設定キーは足さない）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use fandhe_browser_js::{EvaluateOptions, JsEngineError, JsValue};

use crate::config::JsConfig;
use crate::dom_bridge::{DocumentReadyState, DomBridge, DomBridgeLimits, truncate_utf8};
use crate::js_stub::JsRuntime;
use crate::page_runner::MAX_SCRIPT_ERROR_MESSAGE_BYTES;
use crate::parse::{ParseOptions, parse_document};
use crate::state::{NavigationGeneration, NavigationResult, NavigationScope};

#[cfg(test)]
mod tests;

/// 生きているコンテキスト数の既定上限（全体。`JS-6`）。
pub const DEFAULT_MAX_LIVE_SCRIPT_CONTEXTS: usize = 16;

/// 生きているコンテキスト数の既定上限（scope = ターゲットあたり。`JS-6`）。
pub const DEFAULT_MAX_SCRIPT_CONTEXTS_PER_SCOPE: usize = 4;

/// 待ち行列の既定上限（評価のみ数える。実行中を除く。`JS-6`）。
pub const DEFAULT_SCRIPT_CONTEXT_QUEUE_CAPACITY: usize = 64;

/// 1 回の評価の既定タイムアウト（#1218 の既定 5 秒と揃える）。
pub const DEFAULT_SCRIPT_EVAL_TIMEOUT: Duration = Duration::from_secs(5);

/// 1 回の評価のタイムアウトの最大値。[`ScriptEvalOptions::with_timeout`] はこれで頭打ちにする。
pub const MAX_SCRIPT_EVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// 結果サイズの既定上限（= エンジンの天井 3 MiB）。
pub const DEFAULT_MAX_SCRIPT_RESULT_BYTES: usize = EvaluateOptions::DEFAULT_MAX_RESULT_BYTES;

/// 評価ソースの最大バイト数（エンジンの `MAX_SCRIPT_SOURCE_BYTES` と同値。エンジンへ渡す前に拒否する）。
pub const MAX_SCRIPT_CONTEXT_SOURCE_BYTES: usize = 1_048_576;

/// ワーカースレッドの名前（診断用）。
const WORKER_THREAD_NAME: &str = "fandhe-browser-script-ctx";

/// 追跡する scope の記録の上限（超えたら生きていない scope の記録から捨てる）。
const MAX_TRACKED_SCOPES: usize = 1024;

/// js crate が全破棄経路に付けることを保証する文言（`ensure_discarded_phrase`・Issue #527）。
const DISCARDED_PHRASE: &str = "context was discarded";

/// 評価コンテキストの識別子。1 キー = 1 `JsRuntime` + 1 `DomBridge`（V8 では子プロセス 1 つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScriptContextKey {
    scope: NavigationScope,
    generation: NavigationGeneration,
    world: u64,
}

impl ScriptContextKey {
    /// キーを構築する。`scope`・`generation` は `NavigationState::scope()` / `begin_navigation()` の値、
    /// `world` は cdp が払い出す contextId を渡す。
    pub fn new(scope: NavigationScope, generation: NavigationGeneration, world: u64) -> Self {
        Self {
            scope,
            generation,
            world,
        }
    }

    /// 遷移系列（ターゲット）の識別子を返す。
    pub fn scope(&self) -> NavigationScope {
        self.scope
    }

    /// 世代を返す。
    pub fn generation(&self) -> NavigationGeneration {
        self.generation
    }

    /// world（cdp の contextId）を返す。
    pub fn world(&self) -> u64 {
        self.world
    }
}

/// ハンドルのオプション（上限値）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ScriptContextOptions {
    max_live: usize,
    max_per_scope: usize,
    queue_capacity: usize,
    bridge_limits: DomBridgeLimits,
}

impl Default for ScriptContextOptions {
    fn default() -> Self {
        Self {
            max_live: DEFAULT_MAX_LIVE_SCRIPT_CONTEXTS,
            max_per_scope: DEFAULT_MAX_SCRIPT_CONTEXTS_PER_SCOPE,
            queue_capacity: DEFAULT_SCRIPT_CONTEXT_QUEUE_CAPACITY,
            bridge_limits: DomBridgeLimits::default(),
        }
    }
}

impl ScriptContextOptions {
    /// 全体のコンテキスト数の上限を設定する。0 は 1 に切り上げる。
    #[must_use]
    pub fn with_max_live_contexts(mut self, max: usize) -> Self {
        self.max_live = max.max(1);
        self
    }

    /// scope あたりのコンテキスト数の上限を設定する。0 は 1 に切り上げる。
    #[must_use]
    pub fn with_max_contexts_per_scope(mut self, max: usize) -> Self {
        self.max_per_scope = max.max(1);
        self
    }

    /// 待ち行列の上限（評価のみ。実行中を除く）を設定する。0 は 1 に切り上げる。
    #[must_use]
    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    /// `DomBridge` の上限（DOM 操作回数など）を設定する。
    #[must_use]
    pub fn with_bridge_limits(mut self, limits: DomBridgeLimits) -> Self {
        self.bridge_limits = limits;
        self
    }

    /// 全体のコンテキスト数の上限を返す。
    pub fn max_live_contexts(&self) -> usize {
        self.max_live
    }

    /// scope あたりのコンテキスト数の上限を返す。
    pub fn max_contexts_per_scope(&self) -> usize {
        self.max_per_scope
    }

    /// 待ち行列の上限を返す。
    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }
}

/// 1 回の評価のオプション。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptEvalOptions {
    timeout: Duration,
    max_result_bytes: usize,
}

impl Default for ScriptEvalOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_SCRIPT_EVAL_TIMEOUT,
            max_result_bytes: DEFAULT_MAX_SCRIPT_RESULT_BYTES,
        }
    }
}

impl ScriptEvalOptions {
    /// タイムアウトを設定する。[`MAX_SCRIPT_EVAL_TIMEOUT`] で頭打ちにする。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.min(MAX_SCRIPT_EVAL_TIMEOUT);
        self
    }

    /// 結果サイズの上限（UTF-8 バイト）を設定する。[`DEFAULT_MAX_SCRIPT_RESULT_BYTES`] を超える値は
    /// その値に丸める（エンジンの天井を超えて広げない）。
    #[must_use]
    pub fn with_max_result_bytes(mut self, bytes: usize) -> Self {
        self.max_result_bytes = bytes.min(DEFAULT_MAX_SCRIPT_RESULT_BYTES);
        self
    }

    /// タイムアウトを返す。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 結果サイズの上限を返す。
    pub fn max_result_bytes(&self) -> usize {
        self.max_result_bytes
    }
}

/// 評価コンテキストが使えない理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnavailableReason {
    /// ハンドルが無効（エンジンなし・生成不能・[`ScriptContextHandle::disabled`]）。
    Disabled,
    /// コンテキストの `JsRuntime` を生成できなかった。
    EngineCreation,
    /// コンテキストの初期化（文書のパース・attach・shim 注入）に失敗した。
    ContextInit,
    /// ワーカースレッドを生成できなかった。
    SpawnFailed,
    /// ワーカーが結果を返す前に終了した（panic・ハンドルの破棄）。次の要求で再起動を試みられる。
    Terminated,
}

/// 上限超過の種別（具体値を含む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptContextLimit {
    /// 生きているコンテキスト数（全体）。
    LiveContexts {
        /// 上限。
        limit: usize,
    },
    /// scope あたりのコンテキスト数。
    ContextsPerScope {
        /// 上限。
        limit: usize,
    },
    /// 待ち行列の件数。
    Queue {
        /// 上限。
        capacity: usize,
    },
    /// ソースのバイト数。
    SourceBytes {
        /// 渡されたバイト数。
        len: usize,
        /// 上限。
        limit: usize,
    },
    /// 結果のバイト数（切り詰めずに拒否する）。
    ResultBytes {
        /// 結果のバイト数。
        len: usize,
        /// 上限。
        limit: usize,
    },
}

/// エンジンが再起動（コンテキストが破棄）された原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestartCause {
    /// 子プロセスの異常終了・通信断。
    Crashed,
    /// タイムアウトで親が子を終了した（boa・V8 の猶予超過）。
    Timeout,
    /// ヒープ上限などで子が終了した。
    ResourceLimit,
}

/// 評価が結果を返せなかった理由。`crate::Error` には統合しない（`PageScriptError` と同じ方針）。
///
/// `Display`・`Debug` はソース・HTML・URL を出さない（`EvaluationFailed` のメッセージは長さだけ）。
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptContextError {
    /// 評価コンテキストが使えない。
    Unavailable {
        /// 理由。
        reason: UnavailableReason,
    },
    /// キーの世代が古い（より新しい世代を受け付け済み）。
    NotFound,
    /// 上限を超えた。
    LimitExceeded {
        /// 超えた上限。
        limit: ScriptContextLimit,
    },
    /// 評価がタイムアウトした。コンテキストは残っている。
    Timeout {
        /// 適用したタイムアウト。
        limit: Duration,
    },
    /// ヒープ上限に達した。コンテキストは残っている。
    ResourceLimit,
    /// エンジンが再起動し、そのキーのコンテキストの状態は失われた。
    EngineRestarted {
        /// 原因。
        cause: RestartCause,
    },
    /// 評価が失敗した（捕捉されない例外・構文エラー・エンジンの上限超過・文字列でない完了値）。
    /// コンテキストは残る。
    EvaluationFailed {
        /// JS 由来の（untrusted な）メッセージ。[`MAX_SCRIPT_ERROR_MESSAGE_BYTES`] で切り詰め済み。
        message: String,
    },
}

impl fmt::Debug for ScriptContextError {
    /// `EvaluationFailed` のメッセージは長さだけを出す。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason } => f
                .debug_struct("Unavailable")
                .field("reason", reason)
                .finish(),
            Self::NotFound => f.write_str("NotFound"),
            Self::LimitExceeded { limit } => f
                .debug_struct("LimitExceeded")
                .field("limit", limit)
                .finish(),
            Self::Timeout { limit } => f.debug_struct("Timeout").field("limit", limit).finish(),
            Self::ResourceLimit => f.write_str("ResourceLimit"),
            Self::EngineRestarted { cause } => f
                .debug_struct("EngineRestarted")
                .field("cause", cause)
                .finish(),
            Self::EvaluationFailed { message } => f
                .debug_struct("EvaluationFailed")
                .field("message_len", &message.len())
                .finish(),
        }
    }
}

impl fmt::Display for ScriptContextError {
    /// 固定の英語文言（ソース・メッセージを反響しない）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason } => {
                let why = match reason {
                    UnavailableReason::Disabled => "script contexts are disabled",
                    UnavailableReason::EngineCreation => "JS engine could not be created",
                    UnavailableReason::ContextInit => "script context could not be initialized",
                    UnavailableReason::SpawnFailed => "worker thread could not be spawned",
                    UnavailableReason::Terminated => "worker terminated before finishing",
                };
                write!(f, "script context unavailable: {why}")
            }
            Self::NotFound => f.write_str("script context not found (stale generation)"),
            Self::LimitExceeded { limit } => match limit {
                ScriptContextLimit::LiveContexts { limit } => {
                    write!(f, "too many live script contexts (limit {limit})")
                }
                ScriptContextLimit::ContextsPerScope { limit } => {
                    write!(f, "too many script contexts for one target (limit {limit})")
                }
                ScriptContextLimit::Queue { capacity } => {
                    write!(f, "script context queue is full (capacity {capacity})")
                }
                ScriptContextLimit::SourceBytes { len, limit } => {
                    write!(f, "script source is too large ({len} bytes, limit {limit})")
                }
                ScriptContextLimit::ResultBytes { len, limit } => {
                    write!(f, "script result is too large ({len} bytes, limit {limit})")
                }
            },
            Self::Timeout { limit } => {
                write!(f, "script evaluation timed out after {limit:?}")
            }
            Self::ResourceLimit => f.write_str("script evaluation hit the resource limit"),
            Self::EngineRestarted { cause } => {
                let why = match cause {
                    RestartCause::Crashed => "engine crashed",
                    RestartCause::Timeout => "engine was terminated after a timeout",
                    RestartCause::ResourceLimit => "engine was terminated at the resource limit",
                };
                write!(f, "script context was discarded: {why}")
            }
            Self::EvaluationFailed { .. } => f.write_str("script evaluation failed"),
        }
    }
}

impl std::error::Error for ScriptContextError {}

/// ハンドルの状態の観測点（OS 非依存。起動前後の比較に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptContextStats {
    worker_running: bool,
    worker_spawns: u64,
    live_contexts: usize,
    queued: usize,
}

impl ScriptContextStats {
    /// ワーカースレッドが動作中か。
    pub fn worker_running(&self) -> bool {
        self.worker_running
    }

    /// これまでにワーカースレッドを起動した回数。
    pub fn worker_spawns(&self) -> u64 {
        self.worker_spawns
    }

    /// 生きているコンテキスト数。
    pub fn live_contexts(&self) -> usize {
        self.live_contexts
    }

    /// 待ち行列の評価の件数（実行中を除く）。
    pub fn queued(&self) -> usize {
        self.queued
    }
}

/// 評価の成功結果。`ScriptEvalFuture` の `Output` の成功側。
///
/// 現時点の中身は評価結果の文字列だけだが、cdp の Runtime ドメイン（#1217〜#1219）が必要とする
/// 付随情報（値の型・例外詳細など）を後から足せるよう、フラットな文字列ではなく構造体で返す
/// （`REPAIR-4`・`CDP-2`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptEvalOutput {
    value: String,
}

impl ScriptEvalOutput {
    fn new(value: String) -> Self {
        Self { value }
    }

    /// 評価結果の文字列（結果サイズの上限検査済み）。
    pub fn value(&self) -> &str {
        &self.value
    }

    /// 評価結果の文字列を取り出す。
    pub fn into_value(self) -> String {
        self.value
    }
}

type EvalResult = Result<ScriptEvalOutput, ScriptContextError>;

/// 要求 1 件の結果スロット。呼び出し側の future・待ち行列・ワーカーが共有する。
///
/// 結果と waker を同じ `Mutex` の下に置いて wake の取りこぼしを防ぐ（std のみの oneshot。
/// `page_actor` の `JobState` と同じ形。評価は途中で打ち切れないためワーカー側の waker は持たない）。
struct JobState {
    slot: Mutex<Slot>,
    /// 呼び出し側が future を破棄した。ワーカーは未着手の放棄済み要求をスキップする。
    abandoned: AtomicBool,
}

struct Slot {
    result: Option<EvalResult>,
    /// 結果を書き込み済み（取り出し済みを含む）。先勝ちの判定に使う。
    completed: bool,
    caller_waker: Option<Waker>,
}

impl JobState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot {
                result: None,
                completed: false,
                caller_waker: None,
            }),
            abandoned: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 結果が未設定のときだけ書き込み、呼び出し側を起こす（先勝ち）。
    fn complete(&self, result: EvalResult) {
        let waker = {
            let mut slot = self.lock();
            if slot.completed {
                return;
            }
            slot.completed = true;
            slot.result = Some(result);
            slot.caller_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::SeqCst)
    }
}

/// ワーカー側が 1 つだけ持つ券。どこで捨てられても（unwind・ハンドルの破棄）呼び出し側が永久に
/// 待たないよう、`Drop` で `Terminated` を書く（先勝ちのため正常完了後は無害）。
struct JobTicket {
    state: Arc<JobState>,
}

impl Drop for JobTicket {
    fn drop(&mut self) {
        self.state.complete(Err(ScriptContextError::Unavailable {
            reason: UnavailableReason::Terminated,
        }));
    }
}

/// 待ち行列の要素。
enum Task {
    Evaluate(EvalJob),
    BeginGeneration {
        scope: NavigationScope,
        generation: NavigationGeneration,
    },
    ReleaseScope {
        scope: NavigationScope,
    },
}

struct EvalJob {
    id: u64,
    key: ScriptContextKey,
    document: Arc<NavigationResult>,
    source: String,
    opts: ScriptEvalOptions,
    state: Arc<JobState>,
    ticket: JobTicket,
}

/// 待ち行列とワーカー生存・世代記録を 1 つのロックで一貫して見る状態。
///
/// `JsRuntime` は `!Send` のため、キー → ランタイムの表はここに置かずワーカースレッドのスタックに
/// 置く（置くと `Inner` が `Send + Sync` でなくなる）。ここにあるのは `Send` な写しと記録だけ。
struct State {
    queue: VecDeque<Task>,
    worker_alive: bool,
    worker_spawns: u64,
    /// ハンドルが破棄された。ワーカーは次のループで終了する。
    shutdown: bool,
    next_id: u64,
    /// 生きているコンテキスト数の写し（`stats()` 用。ワーカーが更新する）。
    live_contexts: usize,
    /// scope ごとに見た最新の世代。ワーカーがアイドル終了・panic で消えても失われないよう共有側に置く。
    latest: HashMap<NavigationScope, NavigationGeneration>,
    /// 待ち行列が満杯で積めなかった世代通知（scope → 大きい方の世代と、保留した時点の評価 id 採番位置）。
    /// ワーカーが必ず消化する。
    pending_begin: HashMap<NavigationScope, (NavigationGeneration, u64)>,
    /// 待ち行列が満杯で積めなかった解放要求（scope → 保留した時点の評価 id 採番位置）。
    /// 件数が上限を超えたら `pending_release_all` に倒す（解放要求を失わない）。
    pending_release: HashMap<NavigationScope, u64>,
    /// 保留の解放が上限を超えた（保留した時点の評価 id 採番位置）。先に積まれた評価が無くなった時点で
    /// 全コンテキストを破棄する（`CDP-2`）。
    pending_release_all: Option<u64>,
}

/// ワーカーが消化する保留中の通知（`State::take_ready_pending` が返す）。
struct PendingWork {
    begins: Vec<(NavigationScope, NavigationGeneration)>,
    releases: Vec<NavigationScope>,
    release_all: bool,
}

impl State {
    fn has_pending(&self) -> bool {
        !self.pending_begin.is_empty()
            || !self.pending_release.is_empty()
            || self.pending_release_all.is_some()
    }

    /// 保留のうち、順序境界を越えずに適用できるものを取り出す。
    /// 評価は通知と送った順に処理する契約のため、保留した時点より前に積まれた評価（id が採番位置未満。
    /// 世代通知は同じ scope かつ古い世代のもの、解放は同じ scope のもの）が積み済みの間は保留のまま残す。
    /// 保留より後に送られた評価は境界にならない。待ち行列に残る評価は有限なので必ず消化される。
    fn take_ready_pending(&mut self) -> Option<PendingWork> {
        if !self.has_pending() {
            return None;
        }
        let queue = &self.queue;
        let blocked =
            |scope: Option<NavigationScope>, below: Option<NavigationGeneration>, seq: u64| {
                queue.iter().any(|t| {
                    matches!(t, Task::Evaluate(j)
                    if j.id < seq
                        && scope.is_none_or(|s| j.key.scope == s)
                        && below.is_none_or(|g| j.key.generation < g))
                })
            };
        let begins: Vec<_> = self
            .pending_begin
            .iter()
            .filter(|(s, (g, q))| !blocked(Some(**s), Some(*g), *q))
            .map(|(s, (g, _))| (*s, *g))
            .collect();
        let releases: Vec<_> = self
            .pending_release
            .iter()
            .filter(|(s, q)| !blocked(Some(**s), None, **q))
            .map(|(s, _)| *s)
            .collect();
        let release_all = self
            .pending_release_all
            .is_some_and(|q| !blocked(None, None, q));
        if begins.is_empty() && releases.is_empty() && !release_all {
            return None;
        }
        for (s, _) in &begins {
            self.pending_begin.remove(s);
        }
        for s in &releases {
            self.pending_release.remove(s);
            self.pending_begin.remove(s);
        }
        if release_all {
            self.pending_release_all = None;
            self.pending_release.clear();
            self.pending_begin.clear();
        }
        Some(PendingWork {
            begins,
            releases,
            release_all,
        })
    }

    /// 満杯時の世代通知を保留へ積む（同一 scope は大きい世代へ。記録が上限なら新しい scope は捨てる）。
    fn defer_begin(&mut self, scope: NavigationScope, generation: NavigationGeneration) {
        if self.pending_begin.len() >= MAX_TRACKED_SCOPES
            && !self.pending_begin.contains_key(&scope)
        {
            return;
        }
        let seq = self.next_id;
        let entry = self.pending_begin.entry(scope).or_insert((generation, seq));
        if entry.0 < generation {
            entry.0 = generation;
        }
        entry.1 = seq;
    }

    /// 満杯時の解放要求を保留へ積む。上限を超える分は全破棄へ倒す（解放要求を失わない）。
    fn defer_release(&mut self, scope: NavigationScope) {
        let seq = self.next_id;
        if self.pending_release.len() >= MAX_TRACKED_SCOPES
            && !self.pending_release.contains_key(&scope)
        {
            self.pending_release_all = Some(seq);
            return;
        }
        self.pending_release.insert(scope, seq);
    }

    fn queued_evals(&self) -> usize {
        self.queue
            .iter()
            .filter(|t| matches!(t, Task::Evaluate(_)))
            .count()
    }

    /// 待ち行列にある通知（世代変更・解放）の件数。
    fn queued_notifications(&self) -> usize {
        self.queue.len() - self.queued_evals()
    }

    /// 待ち行列の末尾が同一 scope の世代通知のときだけ、その世代を大きい方へ更新して真を返す。
    /// 評価など順序境界を越えて前方の通知を書き換えない（送った順に処理する契約）。
    fn coalesce_begin(&mut self, scope: NavigationScope, generation: NavigationGeneration) -> bool {
        if let Some(Task::BeginGeneration {
            scope: s,
            generation: g,
        }) = self.queue.back_mut()
            && *s == scope
        {
            if *g < generation {
                *g = generation;
            }
            return true;
        }
        false
    }

    /// `scope` の最新世代を `generation` に更新する（より古い値では下げない）。
    /// 記録が上限に達していて新しい scope なら、`live` に無い scope の記録を捨ててから入れる。
    fn note_latest(
        &mut self,
        scope: NavigationScope,
        generation: NavigationGeneration,
        live: &HashSet<NavigationScope>,
    ) {
        if self.latest.len() >= MAX_TRACKED_SCOPES && !self.latest.contains_key(&scope) {
            self.latest.retain(|k, _| live.contains(k));
        }
        let entry = self.latest.entry(scope).or_insert(generation);
        if *entry < generation {
            *entry = generation;
        }
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // 書き込みは常に完結した状態遷移のため、poison は回復して使う。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// ランタイム生成の継ぎ目（本番は `JsRuntime::from_config`。単体テストで偽エンジンに差し替える）。
type RuntimeFactory = dyn Fn() -> crate::Result<JsRuntime> + Send + Sync;

/// ワーカースレッドが掴む設定の束（`Arc<Inner>` は掴まない。掴むと最後のハンドルの破棄を検出できない）。
struct WorkerConfig {
    factory: Arc<RuntimeFactory>,
    bridge_limits: DomBridgeLimits,
    max_live: usize,
    max_per_scope: usize,
}

struct Inner {
    config: Arc<WorkerConfig>,
    queue_capacity: usize,
    shared: Arc<Shared>,
}

impl Drop for Inner {
    /// 最後のハンドルが消えたとき、待ちを捨ててワーカーへ終了を通知する。join はしない
    /// （非同期文脈でブロックしない）。ランタイムはワーカースレッド上で drop され、子プロセスは
    /// drop で kill / wait される。
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        st.queue.clear();
        st.shutdown = true;
        drop(st);
        self.shared.wake.notify_all();
    }
}

/// 評価コンテキストのハンドル（`Send + Sync`・`Clone`）。
///
/// 生成してもスレッドは起動しない（遅延起動。モジュール doc）。`AppState` が保持する。
#[derive(Clone)]
pub struct ScriptContextHandle {
    inner: Option<Arc<Inner>>,
}

impl fmt::Debug for ScriptContextHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptContextHandle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl ScriptContextHandle {
    /// 常に [`UnavailableReason::Disabled`] を返す無効なハンドル。スレッドも子プロセスも持たない。
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// 既定のオプションで構築する。`js` にエンジンが無い・生成できない場合は [`Self::disabled`] と
    /// 同じ挙動になる。
    pub fn new(js: &JsConfig) -> Self {
        Self::with_options(js, ScriptContextOptions::default())
    }

    /// オプション付きで構築する。有効化の条件はモジュール doc を参照（試し生成したランタイムは
    /// 直ちに破棄する）。
    pub fn with_options(js: &JsConfig, options: ScriptContextOptions) -> Self {
        if js.engine().is_none() || JsRuntime::from_config(js).is_err() {
            return Self::disabled();
        }
        let js = *js;
        Self::with_factory(Arc::new(move || JsRuntime::from_config(&js)), options)
    }

    fn with_factory(factory: Arc<RuntimeFactory>, options: ScriptContextOptions) -> Self {
        let config = Arc::new(WorkerConfig {
            factory,
            bridge_limits: options.bridge_limits,
            max_live: options.max_live,
            max_per_scope: options.max_per_scope,
        });
        Self {
            inner: Some(Arc::new(Inner {
                config,
                queue_capacity: options.queue_capacity,
                shared: Arc::new(Shared {
                    state: Mutex::new(State {
                        queue: VecDeque::new(),
                        worker_alive: false,
                        worker_spawns: 0,
                        shutdown: false,
                        next_id: 0,
                        live_contexts: 0,
                        latest: HashMap::new(),
                        pending_begin: HashMap::new(),
                        pending_release: HashMap::new(),
                        pending_release_all: None,
                    }),
                    wake: Condvar::new(),
                }),
            })),
        }
    }

    /// 単体テスト用: ランタイム生成を偽エンジンに差し替えて有効なハンドルを作る。
    #[cfg(test)]
    fn with_factory_for_test(factory: Arc<RuntimeFactory>, options: ScriptContextOptions) -> Self {
        Self::with_factory(factory, options)
    }

    /// 評価が有効か。
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// 現在の状態を返す（起動前後の比較用）。無効なハンドルは全て 0 / false。
    pub fn stats(&self) -> ScriptContextStats {
        let Some(inner) = self.inner.as_ref() else {
            return ScriptContextStats {
                worker_running: false,
                worker_spawns: 0,
                live_contexts: 0,
                queued: 0,
            };
        };
        let st = inner.shared.lock();
        ScriptContextStats {
            worker_running: st.worker_alive,
            worker_spawns: st.worker_spawns,
            live_contexts: st.live_contexts,
            queued: st.queued_evals(),
        }
    }

    /// 遷移の開始を通知する（非同期）。`scope` の最新世代を `generation` に更新し、その scope の
    /// 古い世代のコンテキストを破棄する（cdp の遷移経路〔#1217〕が世代の払い出し直後に呼ぶ。
    /// `PageScriptHandle::begin_generation` と同じ名前）。ワーカーが居なければコンテキストも無いため、
    /// スレッドは起動せず記録だけ更新する（`PERF-7`）。
    pub fn begin_generation(&self, scope: NavigationScope, generation: NavigationGeneration) {
        self.notify(Task::BeginGeneration { scope, generation });
    }

    /// `scope` のコンテキストをすべて破棄し、世代の記録も消す（非同期。ターゲットのクローズ時に
    /// cdp が呼ぶ）。release 後に同じ scope へ評価を送らないのは呼び出し側の契約で、送られた場合は
    /// 上限内で新規作成になる。
    pub fn release_scope(&self, scope: NavigationScope) {
        self.notify(Task::ReleaseScope { scope });
    }

    fn notify(&self, task: Task) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let mut st = inner.shared.lock();
        if st.worker_alive {
            // 通知にも評価と同じ上限を課す（`JS-6`。評価実行中に通知を連投されても無制限に増やさない）。
            // 上限内なら FIFO で積む。上限に達したら、末尾が同一 scope の世代通知なら畳み込み
            // （世代は大きい方を採る）、それ以外は有界な保留状態へ積む（ワーカーが順序境界を守って必ず消化する）。
            if st.queued_notifications() < inner.queue_capacity {
                st.queue.push_back(task);
                drop(st);
                inner.shared.wake.notify_all();
                return;
            }
            let task = match task {
                Task::BeginGeneration { scope, generation } => {
                    if st.coalesce_begin(scope, generation) {
                        return;
                    }
                    Task::BeginGeneration { scope, generation }
                }
                other => other,
            };
            match task {
                Task::BeginGeneration { scope, generation } => st.defer_begin(scope, generation),
                Task::ReleaseScope { scope } => st.defer_release(scope),
                Task::Evaluate(_) => {}
            }
            drop(st);
            inner.shared.wake.notify_all();
            return;
        }
        // ワーカーが居ない間はコンテキストも無い（ガードが写しを 0 に戻す）。記録だけ直接更新する。
        match task {
            Task::BeginGeneration { scope, generation } => {
                st.note_latest(scope, generation, &HashSet::new());
            }
            Task::ReleaseScope { scope } => {
                st.latest.remove(&scope);
            }
            Task::Evaluate(_) => {}
        }
    }

    /// `document` の上で `source` を評価する要求を出す。結果は [`ScriptEvalOutput`]（JS 評価結果の文字列を持つ）。
    ///
    /// `document` は `key` が未作成のときだけ使う（既存のキーでは無視する。モジュール doc）。
    /// 即時に決まる結果（無効・ソース超過・待ち行列の満杯・スレッド生成失敗）は呼び出した時点で
    /// 同期に決め、初回 poll で必ず Ready になる。`source` は加工せずエンジンへ渡す。
    pub fn evaluate(
        &self,
        key: ScriptContextKey,
        document: Arc<NavigationResult>,
        source: impl Into<String>,
        opts: &ScriptEvalOptions,
    ) -> ScriptEvalFuture {
        let Some(inner) = self.inner.as_ref() else {
            return ScriptEvalFuture::ready(Err(ScriptContextError::Unavailable {
                reason: UnavailableReason::Disabled,
            }));
        };
        let source = source.into();
        if source.len() > MAX_SCRIPT_CONTEXT_SOURCE_BYTES {
            return ScriptEvalFuture::ready(Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::SourceBytes {
                    len: source.len(),
                    limit: MAX_SCRIPT_CONTEXT_SOURCE_BYTES,
                },
            }));
        }
        let shared = &inner.shared;
        let mut st = shared.lock();
        if st.queued_evals() >= inner.queue_capacity {
            return ScriptEvalFuture::ready(Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::Queue {
                    capacity: inner.queue_capacity,
                },
            }));
        }
        let id = st.next_id;
        st.next_id = st.next_id.wrapping_add(1);
        let state = JobState::new();
        st.queue.push_back(Task::Evaluate(EvalJob {
            id,
            key,
            document,
            source,
            opts: *opts,
            state: Arc::clone(&state),
            ticket: JobTicket {
                state: Arc::clone(&state),
            },
        }));

        if !st.worker_alive {
            if spawn_worker(
                Arc::clone(shared),
                Arc::clone(&inner.config),
                st.worker_spawns.saturating_add(1),
            )
            .is_err()
            {
                // 積んだ要求を取り消す（券の Drop が書く Terminated ではなく失敗理由を返す）。
                st.queue.pop_back();
                return ScriptEvalFuture::ready(Err(ScriptContextError::Unavailable {
                    reason: UnavailableReason::SpawnFailed,
                }));
            }
            st.worker_alive = true;
            st.worker_spawns = st.worker_spawns.saturating_add(1);
        } else {
            shared.wake.notify_all();
        }
        drop(st);

        ScriptEvalFuture {
            kind: FutureKind::Pending {
                state,
                shared: Arc::clone(shared),
                id,
            },
            finished: false,
        }
    }
}

/// [`ScriptContextHandle::evaluate`] が返す future（`Send`）。
///
/// 完了前に drop すると、その要求を待ち行列から外す（実行中の評価は途中で打ち切れないが、結果は捨てる）。
pub struct ScriptEvalFuture {
    kind: FutureKind,
    finished: bool,
}

enum FutureKind {
    Ready(Option<EvalResult>),
    Pending {
        state: Arc<JobState>,
        shared: Arc<Shared>,
        id: u64,
    },
}

impl ScriptEvalFuture {
    fn ready(result: EvalResult) -> Self {
        Self {
            kind: FutureKind::Ready(Some(result)),
            finished: false,
        }
    }
}

impl Future for ScriptEvalFuture {
    type Output = EvalResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<EvalResult> {
        let this = self.get_mut();
        let out = match &mut this.kind {
            FutureKind::Ready(slot) => slot.take().unwrap_or(
                // poll 済みの future を再度 poll された場合（契約違反）も panic しない。
                Err(ScriptContextError::Unavailable {
                    reason: UnavailableReason::Terminated,
                }),
            ),
            FutureKind::Pending { state, .. } => {
                let mut slot = state.lock();
                match slot.result.take() {
                    Some(r) => r,
                    None => {
                        slot.caller_waker = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                }
            }
        };
        this.finished = true;
        Poll::Ready(out)
    }
}

impl Drop for ScriptEvalFuture {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let FutureKind::Pending { state, shared, id } = &self.kind {
            state.abandoned.store(true, Ordering::SeqCst);
            let mut st = shared.lock();
            st.queue
                .retain(|t| !matches!(t, Task::Evaluate(j) if j.id == *id));
        }
    }
}

/// ワーカースレッドを起動する。呼び出し元は `State` のロックを保持している。
///
/// `generation` はこのワーカーの世代番号（起動後の `worker_spawns` と一致する値）。
fn spawn_worker(
    shared: Arc<Shared>,
    config: Arc<WorkerConfig>,
    generation: u64,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(WORKER_THREAD_NAME.to_string())
        .spawn(move || worker_main(&shared, &config, generation))
        .map(|_| ())
}

/// ワーカー終了時の後始末（正常終了でも unwind でも）。写しを 0 に戻し、停止状態にし、
/// 待っている要求を `Terminated` で解決する。戻さないと panic 後に幻の件数が残り、以後の評価が
/// すべて上限超過になる。`Drop` 内では panic し得る操作を使わない。
///
/// アイドル終了の `worker_alive = false` からこの Drop までの隙間に別スレッドが新ワーカーを起動し得る。
/// そのため自分の世代（`generation`）が最新のときだけ後始末し、新しい世代が居れば何もしない
/// （新ワーカーの待ち行列・生存フラグ・件数を壊さない）。
struct WorkerGuard {
    shared: Arc<Shared>,
    generation: u64,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        if st.worker_spawns != self.generation {
            return;
        }
        st.worker_alive = false;
        st.live_contexts = 0;
        // コンテキストはこのワーカーと共に破棄されるため、保留の通知は不要になる。
        st.pending_begin.clear();
        st.pending_release.clear();
        st.pending_release_all = None;
        // 券の Drop が Terminated を書く。
        st.queue.clear();
    }
}

/// 生きているコンテキスト 1 つ。`runtime` を先に drop する（子プロセスの kill / wait を先に行う）。
struct Ctx {
    runtime: JsRuntime,
    _bridge: DomBridge,
}

type CtxTable = HashMap<ScriptContextKey, Ctx>;

fn sync_live(shared: &Shared, contexts: &CtxTable) {
    shared.lock().live_contexts = contexts.len();
}

fn worker_main(shared: &Arc<Shared>, config: &WorkerConfig, generation: u64) {
    // 宣言順の逆に drop される: `contexts`（子プロセスの破棄）→ `guard`（写しのリセット）。
    let _guard = WorkerGuard {
        shared: Arc::clone(shared),
        generation,
    };
    let mut contexts: CtxTable = HashMap::new();
    loop {
        let task = {
            let mut st = shared.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if let Some(work) = st.take_ready_pending() {
                    break Work::Pending(work);
                }
                if let Some(t) = st.queue.pop_front() {
                    break Work::Task(t);
                }
                if contexts.is_empty() && !st.has_pending() {
                    // 同じロック内で停止状態にする（取りこぼし防止）。
                    st.worker_alive = false;
                    return;
                }
                st = shared.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let task = match task {
            Work::Task(t) => t,
            Work::Pending(work) => {
                apply_pending(shared, &mut contexts, work);
                continue;
            }
        };
        match task {
            Task::BeginGeneration { scope, generation } => {
                let live = live_scopes(&contexts);
                shared.lock().note_latest(scope, generation, &live);
                contexts.retain(|k, _| k.scope != scope || k.generation >= generation);
                sync_live(shared, &contexts);
            }
            Task::ReleaseScope { scope } => {
                contexts.retain(|k, _| k.scope != scope);
                let mut st = shared.lock();
                st.latest.remove(&scope);
                st.live_contexts = contexts.len();
            }
            Task::Evaluate(job) => {
                let EvalJob {
                    key,
                    document,
                    source,
                    opts,
                    state,
                    ticket,
                    ..
                } = job;
                if !state.is_abandoned() {
                    let result = handle_evaluate(
                        shared,
                        config,
                        &mut contexts,
                        key,
                        &document,
                        &source,
                        &opts,
                    );
                    state.complete(result);
                }
                drop(ticket);
            }
        }
    }
}

/// ワーカーが取り出す次の仕事（待ち行列の要素か、保留中の通知）。
enum Work {
    Task(Task),
    Pending(PendingWork),
}

/// 保留中の世代通知・解放要求を適用する（ワーカースレッド上）。
fn apply_pending(shared: &Shared, contexts: &mut CtxTable, work: PendingWork) {
    for (scope, generation) in work.begins {
        let live = live_scopes(contexts);
        shared.lock().note_latest(scope, generation, &live);
        contexts.retain(|k, _| k.scope != scope || k.generation >= generation);
    }
    for scope in work.releases {
        contexts.retain(|k, _| k.scope != scope);
        shared.lock().latest.remove(&scope);
    }
    if work.release_all {
        contexts.clear();
    }
    sync_live(shared, contexts);
}

fn live_scopes(contexts: &CtxTable) -> HashSet<NavigationScope> {
    contexts.keys().map(|k| k.scope).collect()
}

/// 世代の判定・コンテキストの作成・評価を行う（ワーカースレッド上）。
fn handle_evaluate(
    shared: &Shared,
    config: &WorkerConfig,
    contexts: &mut CtxTable,
    key: ScriptContextKey,
    document: &NavigationResult,
    source: &str,
    opts: &ScriptEvalOptions,
) -> EvalResult {
    // 世代の判定。FIFO で取り出した時点で行うので、通知との順序が保たれる。
    let live = live_scopes(contexts);
    let stale = {
        let mut st = shared.lock();
        match st.latest.get(&key.scope).copied() {
            Some(g) if g > key.generation => true,
            _ => {
                st.note_latest(key.scope, key.generation, &live);
                false
            }
        }
    };
    if stale {
        return Err(ScriptContextError::NotFound);
    }
    let before = contexts.len();
    contexts.retain(|k, _| k.scope != key.scope || k.generation >= key.generation);
    if contexts.len() != before {
        sync_live(shared, contexts);
    }

    if !contexts.contains_key(&key) {
        if contexts.len() >= config.max_live {
            return Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::LiveContexts {
                    limit: config.max_live,
                },
            });
        }
        let in_scope = contexts.keys().filter(|k| k.scope == key.scope).count();
        if in_scope >= config.max_per_scope {
            return Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::ContextsPerScope {
                    limit: config.max_per_scope,
                },
            });
        }
        let ctx = build_context(config, document, opts)?;
        contexts.insert(key, ctx);
        sync_live(shared, contexts);
    }

    let Some(ctx) = contexts.get_mut(&key) else {
        return Err(ScriptContextError::NotFound);
    };
    // エンジンへは天井を渡し、返った文字列を core がコンテキストの上限と比べる（モジュール doc）。
    let engine_opts = EvaluateOptions::default()
        .with_timeout(opts.timeout)
        .with_max_result_bytes(DEFAULT_MAX_SCRIPT_RESULT_BYTES);
    let outcome = classify_evaluation(ctx.runtime.execute_with_options(source, &engine_opts), opts);
    if outcome.discard {
        contexts.remove(&key);
        sync_live(shared, contexts);
    }
    outcome.result
}

/// コンテキストを作る（ここで子プロセスが起動する）。
fn build_context(
    config: &WorkerConfig,
    document: &NavigationResult,
    opts: &ScriptEvalOptions,
) -> Result<Ctx, ScriptContextError> {
    let unavailable = |reason| ScriptContextError::Unavailable { reason };
    let mut runtime =
        (config.factory)().map_err(|_| unavailable(UnavailableReason::EngineCreation))?;
    let bridge = DomBridge::new(config.bridge_limits.clone());
    let parsed = parse_document(document.html(), &ParseOptions::default())
        .map_err(|_| unavailable(UnavailableReason::ContextInit))?;
    bridge
        .attach(parsed.document)
        .map_err(|_| unavailable(UnavailableReason::ContextInit))?;
    // `about:blank` や http(s) 以外の URL は拒否されるので無視し、既定の `about:blank` のままにする。
    let _ = bridge.set_location(document.url());
    bridge
        .set_ready_state(DocumentReadyState::Complete)
        .map_err(|_| unavailable(UnavailableReason::ContextInit))?;
    let shim_opts = EvaluateOptions::default().with_timeout(opts.timeout);
    match runtime.install_dom_shim_with_options(&bridge, &shim_opts) {
        Ok(_) => Ok(Ctx {
            runtime,
            _bridge: bridge,
        }),
        Err(crate::Error::JsEvaluation(e)) => Err(match restart_cause(&e) {
            Some(cause) => ScriptContextError::EngineRestarted { cause },
            None => unavailable(UnavailableReason::ContextInit),
        }),
        Err(_) => Err(unavailable(UnavailableReason::ContextInit)),
    }
}

/// shim 注入の失敗がエンジンの破棄・再起動によるなら、その原因を返す。
fn restart_cause(e: &JsEngineError) -> Option<RestartCause> {
    match e {
        JsEngineError::Timeout(_) => Some(RestartCause::Timeout),
        JsEngineError::ResourceLimitExceeded(_) => Some(RestartCause::ResourceLimit),
        JsEngineError::EngineUnavailable(_) => Some(RestartCause::Crashed),
        _ => None,
    }
}

/// 評価結果の写像（モジュール doc の表）。`discard` が真ならそのキーのコンテキストを破棄する。
struct Classified {
    result: EvalResult,
    discard: bool,
}

/// メッセージが「コンテキストが破棄された」ことを示すか。js crate が全破棄経路に付ける文言に限る。
fn mentions_discard(message: &str) -> bool {
    message.contains(DISCARDED_PHRASE)
}

fn classify_evaluation(
    result: crate::Result<crate::js_stub::JsExecutionOutput>,
    opts: &ScriptEvalOptions,
) -> Classified {
    let keep = |result| Classified {
        result,
        discard: false,
    };
    let restart = |cause| Classified {
        result: Err(ScriptContextError::EngineRestarted { cause }),
        discard: true,
    };
    let failed = |message: String| {
        keep(Err(ScriptContextError::EvaluationFailed {
            message: truncate_utf8(&message, MAX_SCRIPT_ERROR_MESSAGE_BYTES)
                .0
                .to_string(),
        }))
    };
    match result {
        Ok(out) => match out.js_value {
            JsValue::String(s) if s.len() <= opts.max_result_bytes => {
                keep(Ok(ScriptEvalOutput::new(s)))
            }
            JsValue::String(s) => keep(Err(ScriptContextError::LimitExceeded {
                limit: ScriptContextLimit::ResultBytes {
                    len: s.len(),
                    limit: opts.max_result_bytes,
                },
            })),
            _ => failed("evaluation result is not a string".to_string()),
        },
        Err(crate::Error::JsEvaluation(e)) => match e {
            JsEngineError::Timeout(m) if mentions_discard(&m) => restart(RestartCause::Timeout),
            JsEngineError::Timeout(_) => keep(Err(ScriptContextError::Timeout {
                limit: opts.timeout,
            })),
            JsEngineError::ResourceLimitExceeded(m) if mentions_discard(&m) => {
                restart(RestartCause::ResourceLimit)
            }
            JsEngineError::ResourceLimitExceeded(_) => keep(Err(ScriptContextError::ResourceLimit)),
            JsEngineError::EngineUnavailable(_) => restart(RestartCause::Crashed),
            JsEngineError::EvaluationFailed(m) => failed(m),
            _ => failed("engine error".to_string()),
        },
        Err(crate::Error::JsExecutionUnavailable { .. }) => {
            keep(Err(ScriptContextError::Unavailable {
                reason: UnavailableReason::EngineCreation,
            }))
        }
        Err(_) => failed("evaluation failed".to_string()),
    }
}

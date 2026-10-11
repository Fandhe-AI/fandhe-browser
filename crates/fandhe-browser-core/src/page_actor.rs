//! ページスクリプト実行アクター: `Send + Sync` なハンドルへ要求を渡すと、専用スレッド上で
//! [`crate::page_runner::run_page_scripts`] が走り、結果を async で受け取れる経路
//! （TASK-112・MS-6・Issue #784・ビヘイビア `JS-4`・`JS-6`・`JS-1`・`PERF-7`）。
//!
//! # 役割と呼び出し元
//!
//! ランナーが返す future は `Send` ではない（`JsRuntime`・`DomBridge` を保持したままネットワークを
//! await するため。`page_runner` のモジュール doc）。cdp のハンドラ（`Send` な async）は直接
//! 呼べないので、本モジュールが専用スレッドでランナーを駆動し、ハンドルの
//! [`PageScriptHandle::run_page_scripts`] が結果を返す。`AppState::page_scripts()`
//! （`CdpState::app_state()` 経由）で到達でき、`navigate_gated` への接続は TASK-113（#785）が行う。
//! cli が設定 `[js] execute_page_scripts` からハンドルを構築して `AppState` へ渡す。
//!
//! # tokio を持たない理由と実行器
//!
//! core は tokio を dev-dependency でしか持たない（`Fetcher` の契約。ユーザー承認なしに通常依存へ
//! 昇格できない）。一方ランナーは `Fetcher::get` を await するため、tokio のリアクタが要る。
//! そこで「呼び出したスレッド上で `!Send` な future を完了まで駆動する」処理をホストが
//! [`PageScriptExecutor`] として注入する（cli は current_thread ランタイムで実装する）。
//!
//! # 契約
//!
//! - **遅延起動（`JS-1`・`PERF-7`）**: ハンドル生成時、設定無効時、`<script>` を含まないページでは
//!   スレッドも JS 子プロセスも作らない（[`PageScriptOutcome::Skipped`]）。最初の実行要求で初めて
//!   ワーカースレッドを起動する。ワーカーは待ち行列が空になった時点で自分で終了する
//!   （JS を使わない間はスレッドが残らない。代償として、仕事の切れ目ごとに実行器と `Fetcher` の
//!   接続プールを作り直す。ページごとの子プロセス起動に比べて小さい）。
//! - **キャンセル（`JS-4`）**: 同じ [`NavigationScope`]（遷移系列）でより新しい世代の要求が来たら、
//!   古い要求を打ち切り、古い側は [`PageScriptError::Cancelled`] を受け取る。scope が異なる要求は
//!   互いに干渉しない（cdp はターゲットごとに別の `NavigationState` を持つ）。古い結果が状態を
//!   上書きしないことの最終保証は従来どおり `NavigationState::commit_navigation` の
//!   `Superseded` で、本モジュールの打ち切りは無駄な実行を止める層。呼び出し側の future を
//!   drop した場合も、その要求を待ち行列から外し、実行中なら打ち切る。
//! - **待ち行列の上限（`JS-6`）**: 既定 [`DEFAULT_PAGE_SCRIPT_QUEUE_CAPACITY`] 件（実行中の 1 件を
//!   除く）。超過は待たせず [`PageScriptError::Busy`]。1 要求の時間はランナーの上限
//!   （[`PageRunOptions`]）に従う。
//! - **回復**: ワーカーの panic・実行器の失敗では、待っている呼び出し側は [`PageScriptError`] を
//!   受け取り（panic しない）、次の要求でワーカーを再起動できる。
//!
//! # スタブ・簡易実装・既知の制限（REPAIR-3）
//!
//! - **同期区間の打ち切り遅延**: 打ち切りが効くのは await 点（`src` 取得・ページ内取得の待ち）と
//!   要求の切れ目。評価中の JS は中断できず、同期区間はランナーのページ実時間上限で頭打ちになる。
//!   古い要求の呼び出し側は即座に `Cancelled` を受け取るが、新しい要求の開始はその分待つ
//!   （ランナーへの協調的な打ち切りは別 Issue 候補。`JS-6`）。
//! - **release の panic**: `panic = "abort"` のためプロセスごと終了する。回復経路が働くのは
//!   unwind するビルド（dev / test）と、panic 以外の失敗（実行器の失敗・スレッド生成失敗・
//!   子プロセスの異常終了）。
//! - アクターは 1 本で、複数ターゲットの実行は直列。待ちは非同期なので他の CDP 呼び出しは塞がない。
//! - 上限値（待ち行列・実時間）の設定ファイルからの読み込みは未対応（定数とオプション型で持つ）。

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

use crate::config::JsConfig;
use crate::fetch::{FetchOptions, Fetcher};
use crate::page_runner::{
    PageRunInput, PageRunOptions, PageRunOutput, page_script_fetch_options, run_page_scripts,
};
use crate::state::{NavigationGeneration, NavigationScope};

/// 待ち行列の既定上限（実行中の 1 件を除く。`JS-6`）。
pub const DEFAULT_PAGE_SCRIPT_QUEUE_CAPACITY: usize = 16;

/// ワーカースレッドの名前（診断用）。
const WORKER_THREAD_NAME: &str = "fandhe-browser-page-js";

/// 専用スレッド上で `!Send` な future を完了まで駆動する実行器。ホストが tokio 等で実装して
/// 注入する（core は tokio を通常依存に持たない。モジュール doc）。
pub trait PageScriptExecutor: Send + Sync + 'static {
    /// 呼び出したスレッド上で `future` を完了まで駆動する。`Fetcher` が要求するリアクタを
    /// この中で提供する。ランタイムの構築に失敗したら `Err` を返す（future は poll されない）。
    fn block_on<'a>(&self, future: Pin<Box<dyn Future<Output = ()> + 'a>>) -> std::io::Result<()>;
}

/// ランナーの呼び出しの継ぎ目（本番は [`run_page_scripts`] を呼ぶだけ。単体テストで差し替える）。
trait PageRunDriver: Send + Sync + 'static {
    fn run<'a>(
        &'a self,
        html: &'a str,
        url: &'a str,
        fetcher: &'a Fetcher,
    ) -> Pin<Box<dyn Future<Output = crate::Result<PageRunOutput>> + 'a>>;
}

/// 本番のドライバ。設定とランナーのオプションを保持し、ランナーへそのまま渡す。
struct RunnerDriver {
    js: JsConfig,
    run: PageRunOptions,
}

impl PageRunDriver for RunnerDriver {
    fn run<'a>(
        &'a self,
        html: &'a str,
        url: &'a str,
        fetcher: &'a Fetcher,
    ) -> Pin<Box<dyn Future<Output = crate::Result<PageRunOutput>> + 'a>> {
        Box::pin(async move {
            run_page_scripts(&PageRunInput::new(html, url), &self.js, &self.run, fetcher).await
        })
    }
}

/// アクターへの 1 要求（HTML・ベース URL・遷移系列と世代）。
///
/// HTML は `Arc<str>` で受け取る。`Skipped` と `Err` のとき呼び出し側は元の HTML で遷移を
/// 確定するため、`String` を move する形だと呼び出しのたびに HTML 全体の複製が要る。
#[derive(Clone)]
#[non_exhaustive]
pub struct PageRunRequest {
    html: Arc<str>,
    url: String,
    scope: NavigationScope,
    generation: NavigationGeneration,
}

impl PageRunRequest {
    /// 要求を構築する。`scope` と `generation` は `NavigationState::scope()` /
    /// `begin_navigation()` の値を渡す。
    pub fn new(
        html: Arc<str>,
        url: impl Into<String>,
        scope: NavigationScope,
        generation: NavigationGeneration,
    ) -> Self {
        Self {
            html,
            url: url.into(),
            scope,
            generation,
        }
    }

    /// 遷移系列の識別子を返す。
    pub fn scope(&self) -> NavigationScope {
        self.scope
    }

    /// 世代を返す。
    pub fn generation(&self) -> NavigationGeneration {
        self.generation
    }
}

impl fmt::Debug for PageRunRequest {
    /// HTML と URL（クエリにトークンを含み得る）をログへ漏らさないよう、長さだけを出す。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageRunRequest")
            .field("html_len", &self.html.len())
            .field("url_len", &self.url.len())
            .field("scope", &self.scope)
            .field("generation", &self.generation.get())
            .finish()
    }
}

/// 要求が実行されなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PageScriptSkipReason {
    /// 設定が無効（`[js] execute_page_scripts = false`、またはハンドルが [`PageScriptHandle::disabled`]）。
    Disabled,
    /// HTML に `<script` の開始タグが無く、実行する対象がない。
    NoScriptElement,
}

/// 要求の結果。
///
/// 実行しなかったときにランナーの結果（[`PageRunOutput`]）を捏造して実行したように装わず、
/// [`Self::Skipped`] で理由を返す（REPAIR-3・REPAIR-4）。呼び出し側は `Skipped` と `Err` のとき
/// 取得した HTML をそのまま使う。
#[derive(Debug)]
#[non_exhaustive]
pub enum PageScriptOutcome {
    /// ランナーが実行した（`<script>` が実行不能で子プロセスを起動しなかった場合も含む）。
    Executed(Box<PageRunOutput>),
    /// 実行しなかった。
    Skipped(PageScriptSkipReason),
}

/// アクターが使えなかった理由の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActorFailureKind {
    /// ワーカースレッドを生成できなかった。
    SpawnFailed,
    /// 実行器（ランタイム）が `Err` を返した。
    ExecutorFailed,
    /// ワーカーが結果を返す前に終了した（panic・ハンドルの破棄など）。
    Terminated,
}

/// 要求が結果を返せなかった理由。`crate::Error` には統合しない（`StateError` と同じ方針）。
#[derive(Debug)]
#[non_exhaustive]
pub enum PageScriptError {
    /// 待ち行列が上限に達していた。待たずに返す。
    Busy {
        /// 待ち行列の上限。
        capacity: usize,
    },
    /// 同じ遷移系列でより新しい世代の要求が来た、または呼び出し側が離脱したため打ち切った。
    Cancelled,
    /// アクターが使えなかった。次の要求で再起動を試みられる。
    ActorUnavailable {
        /// 失敗の種別。
        kind: ActorFailureKind,
    },
    /// ランナーが `Err` を返した（入力不正・エンジン生成失敗など）。
    Run(crate::Error),
}

impl fmt::Display for PageScriptError {
    /// 固定の英語文言（HTML・URL を含めない）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { capacity } => {
                write!(f, "page script queue is full (capacity {capacity})")
            }
            Self::Cancelled => f.write_str("page script run was cancelled by a newer navigation"),
            Self::ActorUnavailable { kind } => {
                let reason = match kind {
                    ActorFailureKind::SpawnFailed => "worker thread could not be spawned",
                    ActorFailureKind::ExecutorFailed => "executor failed",
                    ActorFailureKind::Terminated => "worker terminated before finishing",
                };
                write!(f, "page script actor unavailable: {reason}")
            }
            Self::Run(e) => write!(f, "page script run failed: {e}"),
        }
    }
}

impl std::error::Error for PageScriptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Run(e) => Some(e),
            _ => None,
        }
    }
}

/// アクターのオプション。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PageScriptActorOptions {
    queue_capacity: usize,
    run: PageRunOptions,
    fetch: FetchOptions,
}

impl Default for PageScriptActorOptions {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_PAGE_SCRIPT_QUEUE_CAPACITY,
            run: PageRunOptions::default(),
            // 内部アドレス拒否が既定の `src` 取得用設定（SSRF 対策。別経路の HTTP クライアントは作らない）。
            fetch: page_script_fetch_options(),
        }
    }
}

impl PageScriptActorOptions {
    /// 待ち行列の上限（実行中の 1 件を除く）を設定する。0 は 1 に切り上げる。
    #[must_use]
    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    /// ランナーのオプション（実時間上限など）を設定する。
    #[must_use]
    pub fn with_run_options(mut self, run: PageRunOptions) -> Self {
        self.run = run;
        self
    }

    /// `Fetcher` の設定を差し替える（テストが内部アドレスを許可するために使う）。
    #[must_use]
    pub fn with_fetch_options(mut self, fetch: FetchOptions) -> Self {
        self.fetch = fetch;
        self
    }

    /// 待ち行列の上限を返す。
    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }
}

/// アクターの状態の観測点（OS 非依存。起動前後の比較に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageScriptActorStats {
    worker_running: bool,
    worker_spawns: u64,
    queued: usize,
    running: bool,
}

impl PageScriptActorStats {
    /// ワーカースレッドが（終了処理中を除き）動作中か。
    pub fn worker_running(&self) -> bool {
        self.worker_running
    }

    /// これまでにワーカースレッドを起動した回数。
    pub fn worker_spawns(&self) -> u64 {
        self.worker_spawns
    }

    /// 待ち行列の件数（実行中を除く）。
    pub fn queued(&self) -> usize {
        self.queued
    }

    /// 実行中の要求があるか。
    pub fn running(&self) -> bool {
        self.running
    }
}

type RunResult = Result<PageScriptOutcome, PageScriptError>;

/// 要求 1 件の結果スロット。呼び出し側の future・待ち行列・ワーカーが共有する。
///
/// `fetch.rs` の `BlockingDnsFuture` と同じく、結果と waker を同じ `Mutex` の下に置いて
/// wake の取りこぼしを防ぐ（std のみの oneshot）。
struct JobState {
    slot: Mutex<Slot>,
    cancelled: AtomicBool,
}

struct Slot {
    result: Option<RunResult>,
    /// 結果を書き込み済み（取り出し済みを含む）。先勝ちの判定に使う。
    completed: bool,
    caller_waker: Option<Waker>,
    worker_waker: Option<Waker>,
}

impl JobState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(Slot {
                result: None,
                completed: false,
                caller_waker: None,
                worker_waker: None,
            }),
            cancelled: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        // 書き込みは常に完結した状態遷移のため、poison は回復して使う。
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 結果が未設定のときだけ書き込み、呼び出し側を起こす（先勝ち）。
    fn complete(&self, result: RunResult) {
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

    /// 打ち切る。呼び出し側へ `Cancelled` を返し（先に結果があればそれを優先）、ワーカーを起こす。
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.complete(Err(PageScriptError::Cancelled));
        let waker = self.lock().worker_waker.take();
        if let Some(w) = waker {
            w.wake();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn register_worker(&self, waker: &Waker) {
        self.lock().worker_waker = Some(waker.clone());
    }
}

/// ワーカー側が 1 つだけ持つ券。どこで捨てられても（unwind・実行器の失敗・ハンドルの破棄）
/// 呼び出し側が永久に待たないよう、`Drop` で `Terminated` を書く（先勝ちのため正常完了後は無害）。
struct JobTicket {
    state: Arc<JobState>,
}

impl Drop for JobTicket {
    fn drop(&mut self) {
        self.state.complete(Err(PageScriptError::ActorUnavailable {
            kind: ActorFailureKind::Terminated,
        }));
    }
}

struct Job {
    id: u64,
    request: PageRunRequest,
    state: Arc<JobState>,
    ticket: JobTicket,
}

struct Running {
    scope: NavigationScope,
    generation: NavigationGeneration,
    state: Arc<JobState>,
}

/// 待ち行列と実行中・ワーカー生存を 1 つのロックで一貫して見る状態。
///
/// ロック順は「`State` → `JobState`」に固定する（受付は `State` のロック内で `cancel()` を呼ぶ。
/// ワーカーは `JobState` を単独で取り、`State` と入れ子にしない）。
struct State {
    queue: VecDeque<Job>,
    running: Option<Running>,
    worker_alive: bool,
    worker_spawns: u64,
    next_id: u64,
    /// scope ごとに受け付けた最新の世代（完了・`Skipped` 後も残す。`JS-4`）。
    latest: HashMap<NavigationScope, NavigationGeneration>,
}

/// `State::latest` の保持上限。超えたら待ち・実行中でない scope の記録から捨てる
/// （捨てた scope の遅延要求は打ち切れないが、無制限の増加を避ける）。
const MAX_TRACKED_SCOPES: usize = 1024;

struct Shared {
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// ワーカースレッドが掴む設定の束（`Arc<Inner>` は掴まない。掴むと最後のハンドルの破棄を
/// 検出できない）。
struct WorkerConfig {
    fetch: FetchOptions,
    driver: Arc<dyn PageRunDriver>,
}

struct Inner {
    /// 有効なときだけ `Some`（無効ハンドルは実行器を持たない）。
    executor: Option<Arc<dyn PageScriptExecutor>>,
    config: Arc<WorkerConfig>,
    capacity: usize,
    shared: Arc<Shared>,
}

impl Drop for Inner {
    /// 最後のハンドルが消えたとき、待ちジョブを捨て実行中を打ち切る。join はしない
    /// （非同期文脈でブロックしない。ワーカーは自分で終了する）。
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        for job in st.queue.drain(..) {
            job.state.cancel();
        }
        if let Some(r) = st.running.as_ref() {
            r.state.cancel();
        }
    }
}

/// ページスクリプト実行アクターのハンドル（`Send + Sync`・`Clone`）。
///
/// 生成してもスレッドは起動しない（遅延起動。モジュール doc）。`AppState` が保持する。
#[derive(Clone)]
pub struct PageScriptHandle {
    inner: Arc<Inner>,
}

impl fmt::Debug for PageScriptHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageScriptHandle")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl PageScriptHandle {
    /// 常に [`PageScriptSkipReason::Disabled`] を返す無効なハンドル。実行器もスレッドも持たない。
    pub fn disabled() -> Self {
        Self::build(
            None,
            Arc::new(WorkerConfig {
                fetch: FetchOptions::new(),
                driver: Arc::new(RunnerDriver {
                    js: JsConfig::default(),
                    run: PageRunOptions::default(),
                }),
            }),
            DEFAULT_PAGE_SCRIPT_QUEUE_CAPACITY,
        )
    }

    /// 既定のオプションで構築する。`js` が無効（`execute_page_scripts = false`、またはエンジンなし）
    /// なら [`Self::disabled`] と同じ挙動になる。
    pub fn new(js: &JsConfig, executor: Arc<dyn PageScriptExecutor>) -> Self {
        Self::with_options(js, executor, PageScriptActorOptions::default())
    }

    /// オプション付きで構築する。
    pub fn with_options(
        js: &JsConfig,
        executor: Arc<dyn PageScriptExecutor>,
        options: PageScriptActorOptions,
    ) -> Self {
        if !(js.execute_page_scripts() && js.engine().is_some()) {
            return Self::disabled();
        }
        let driver = Arc::new(RunnerDriver {
            js: *js,
            run: options.run.clone(),
        });
        Self::with_driver(executor, options, driver)
    }

    fn with_driver(
        executor: Arc<dyn PageScriptExecutor>,
        options: PageScriptActorOptions,
        driver: Arc<dyn PageRunDriver>,
    ) -> Self {
        Self::build(
            Some(executor),
            Arc::new(WorkerConfig {
                fetch: options.fetch,
                driver,
            }),
            options.queue_capacity.max(1),
        )
    }

    /// 単体テスト用: ランナーを偽ドライバに差し替えて有効なハンドルを作る。
    #[cfg(test)]
    fn with_driver_for_test(
        executor: Arc<dyn PageScriptExecutor>,
        options: PageScriptActorOptions,
        driver: Arc<dyn PageRunDriver>,
    ) -> Self {
        Self::with_driver(executor, options, driver)
    }

    fn build(
        executor: Option<Arc<dyn PageScriptExecutor>>,
        config: Arc<WorkerConfig>,
        capacity: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                executor,
                config,
                capacity,
                shared: Arc::new(Shared {
                    state: Mutex::new(State {
                        queue: VecDeque::new(),
                        running: None,
                        worker_alive: false,
                        worker_spawns: 0,
                        next_id: 0,
                        latest: HashMap::new(),
                    }),
                }),
            }),
        }
    }

    /// 実行が有効か。
    pub fn is_enabled(&self) -> bool {
        self.inner.executor.is_some()
    }

    /// 現在の状態を返す（起動前後の比較用）。
    pub fn stats(&self) -> PageScriptActorStats {
        let st = self.inner.shared.lock();
        PageScriptActorStats {
            worker_running: st.worker_alive,
            worker_spawns: st.worker_spawns,
            queued: st.queue.len(),
            running: st.running.is_some(),
        }
    }

    /// ページのスクリプトを専用スレッドで実行する要求を出す。
    ///
    /// 受付（無効・世代の比較と古い要求の打ち切り・`<script>` なし・上限の判定・ワーカーの起動）は呼び出した時点で
    /// 同期に済ませ、即時に決まる結果は初回 poll で必ず Ready になる（待たせない）。
    pub fn run_page_scripts(&self, request: PageRunRequest) -> PageRunFuture {
        let Some(executor) = self.inner.executor.as_ref() else {
            return PageRunFuture::ready(Ok(PageScriptOutcome::Skipped(
                PageScriptSkipReason::Disabled,
            )));
        };
        let shared = &self.inner.shared;
        let mut st = shared.lock();
        let scope = request.scope;
        let generation = request.generation;

        // 同じ遷移系列で既により新しい世代を受け付けていれば（待ち・実行中・完了・Skipped を
        // 問わず）この要求は古い。
        if st.latest.get(&scope).is_some_and(|g| *g > generation) {
            return PageRunFuture::ready(Err(PageScriptError::Cancelled));
        }
        if st.latest.len() >= MAX_TRACKED_SCOPES && !st.latest.contains_key(&scope) {
            let live: Vec<NavigationScope> = st
                .queue
                .iter()
                .map(|j| j.request.scope)
                .chain(st.running.as_ref().map(|r| r.scope))
                .collect();
            st.latest.retain(|k, _| live.contains(k));
        }
        st.latest.insert(scope, generation);

        // より古い待ちジョブは取り除いてから打ち切る（待ち枠を即座に空け、不変条件
        // 「待ち行列に残るのは打ち切られていないジョブだけ」を保つ）。実行中のより古い
        // 要求は打ち切りを通知する。
        let mut kept = VecDeque::with_capacity(st.queue.len());
        for job in st.queue.drain(..) {
            if job.request.scope == scope && job.request.generation < generation {
                job.state.cancel();
            } else {
                kept.push_back(job);
            }
        }
        st.queue = kept;
        if let Some(r) = st
            .running
            .as_ref()
            .filter(|r| r.scope == scope && r.generation < generation)
        {
            r.state.cancel();
        }

        // `<script` が無い要求はスレッドを起動せず Skipped にするが、古い世代の打ち切り（JS-4）は
        // 上で済ませてから返す（取得・実行を続けさせない）。
        if !contains_script_start_tag(&request.html) {
            return PageRunFuture::ready(Ok(PageScriptOutcome::Skipped(
                PageScriptSkipReason::NoScriptElement,
            )));
        }

        if st.queue.len() >= self.inner.capacity {
            return PageRunFuture::ready(Err(PageScriptError::Busy {
                capacity: self.inner.capacity,
            }));
        }

        let id = st.next_id;
        st.next_id = st.next_id.wrapping_add(1);
        let state = JobState::new();
        st.queue.push_back(Job {
            id,
            request,
            state: Arc::clone(&state),
            ticket: JobTicket {
                state: Arc::clone(&state),
            },
        });

        if !st.worker_alive {
            let spawn = spawn_worker(
                Arc::clone(shared),
                Arc::clone(executor),
                Arc::clone(&self.inner.config),
            );
            if spawn.is_err() {
                // 積んだジョブを取り消す（`cancel` ではなく失敗理由を書いてから捨てる）。
                if let Some(job) = st.queue.pop_back() {
                    job.state.complete(Err(PageScriptError::ActorUnavailable {
                        kind: ActorFailureKind::SpawnFailed,
                    }));
                }
                return PageRunFuture::ready(Err(PageScriptError::ActorUnavailable {
                    kind: ActorFailureKind::SpawnFailed,
                }));
            }
            st.worker_alive = true;
            st.worker_spawns = st.worker_spawns.saturating_add(1);
        }
        drop(st);

        PageRunFuture {
            kind: FutureKind::Pending {
                state,
                shared: Arc::clone(shared),
                id,
            },
            finished: false,
        }
    }
}

/// [`PageScriptHandle::run_page_scripts`] が返す future（`Send`）。
///
/// 完了前に drop すると、その要求を待ち行列から外し、実行中なら打ち切る（接続断などで待つ人が
/// いなくなった実行を続けない）。
pub struct PageRunFuture {
    kind: FutureKind,
    finished: bool,
}

enum FutureKind {
    Ready(Option<RunResult>),
    Pending {
        state: Arc<JobState>,
        shared: Arc<Shared>,
        id: u64,
    },
}

impl PageRunFuture {
    fn ready(result: RunResult) -> Self {
        Self {
            kind: FutureKind::Ready(Some(result)),
            finished: false,
        }
    }
}

impl Future for PageRunFuture {
    type Output = RunResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<RunResult> {
        let this = self.get_mut();
        let out = match &mut this.kind {
            FutureKind::Ready(slot) => slot.take().unwrap_or(
                // poll 済みの future を再度 poll された場合（契約違反）も panic しない。
                Err(PageScriptError::ActorUnavailable {
                    kind: ActorFailureKind::Terminated,
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

impl Drop for PageRunFuture {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let FutureKind::Pending { state, shared, id } = &self.kind {
            // 打ち切りは「cancel → 待ち行列から除去」の順で `State` のロック内で行う。
            let mut st = shared.lock();
            state.cancel();
            st.queue.retain(|j| j.id != *id);
        }
    }
}

/// ワーカースレッドを起動する。呼び出し元は `State` のロックを保持している。
fn spawn_worker(
    shared: Arc<Shared>,
    executor: Arc<dyn PageScriptExecutor>,
    config: Arc<WorkerConfig>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(WORKER_THREAD_NAME.to_string())
        .spawn(move || worker_main(&shared, executor.as_ref(), &config))
        .map(|_| ())
}

/// ワーカースレッドの本体。ガードがスレッド死亡（unwind・実行器の失敗）を検出する。
fn worker_main(shared: &Arc<Shared>, executor: &dyn PageScriptExecutor, config: &WorkerConfig) {
    let guard = WorkerGuard {
        shared: Arc::clone(shared),
        clean: Cell::new(false),
        failure: Cell::new(ActorFailureKind::Terminated),
    };
    if executor
        .block_on(Box::pin(worker_loop(shared, config, &guard)))
        .is_err()
    {
        guard.failure.set(ActorFailureKind::ExecutorFailed);
    }
}

/// 正常終了時は `clean` を立てて解除する。立っていなければ（unwind か実行器の失敗）、
/// 実行中と待ちのジョブを失敗として解決し、`worker_alive` を下ろして次の要求で再起動できるようにする。
/// `Drop` 内では panic し得る操作を使わない。
struct WorkerGuard {
    shared: Arc<Shared>,
    clean: Cell<bool>,
    failure: Cell<ActorFailureKind>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if self.clean.get() {
            return;
        }
        let kind = self.failure.get();
        let fail = || Err(PageScriptError::ActorUnavailable { kind });
        let mut st = self.shared.lock();
        st.worker_alive = false;
        if let Some(r) = st.running.take() {
            r.state.complete(fail());
        }
        for job in st.queue.drain(..) {
            job.state.complete(fail());
        }
    }
}

/// 待ち行列が空になるまでジョブを 1 件ずつ実行する。空になったら同じロック内で
/// `worker_alive` を下ろしてスレッドを終了させる（常駐させない）。
async fn worker_loop(shared: &Shared, config: &WorkerConfig, guard: &WorkerGuard) {
    let mut fetcher: Option<Fetcher> = None;
    loop {
        let job = {
            let mut st = shared.lock();
            match st.queue.pop_front() {
                None => {
                    st.worker_alive = false;
                    guard.clean.set(true);
                    return;
                }
                Some(job) => {
                    st.running = Some(Running {
                        scope: job.request.scope,
                        generation: job.request.generation,
                        state: Arc::clone(&job.state),
                    });
                    job
                }
            }
        };
        let Job {
            request,
            state,
            ticket,
            ..
        } = job;

        if !state.is_cancelled() {
            if fetcher.is_none() {
                match Fetcher::new(config.fetch.clone()) {
                    Ok(f) => fetcher = Some(f),
                    Err(e) => state.complete(Err(PageScriptError::Run(e))),
                }
            }
            if let Some(f) = fetcher.as_ref() {
                let run = config.driver.run(&request.html, &request.url, f);
                let outcome = CancelOnSignal {
                    inner: run,
                    state: &state,
                }
                .await;
                // 打ち切り済みなら先勝ちにより無視される。
                match outcome {
                    Some(Ok(out)) => state.complete(Ok(PageScriptOutcome::Executed(Box::new(out)))),
                    Some(Err(e)) => state.complete(Err(PageScriptError::Run(e))),
                    None => {}
                }
            }
        }
        shared.lock().running = None;
        drop(ticket);
    }
}

/// 打ち切りの合図が立ったら内側の future を poll せずに `None` で終わるラッパー。
/// 内側の future が drop されると `JsRuntime` が drop され、子プロセスは kill / wait される。
struct CancelOnSignal<'a, T> {
    inner: Pin<Box<dyn Future<Output = T> + 'a>>,
    state: &'a JobState,
}

impl<T> Future for CancelOnSignal<'_, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        // `inner` は `Pin<Box<..>>` で `Unpin` のため、`get_mut` で取り出せる。
        let this = self.get_mut();
        this.state.register_worker(cx.waker());
        if this.state.is_cancelled() {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll(cx) {
            Poll::Ready(v) => Poll::Ready(Some(v)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// HTML に `<script` の開始タグ（ASCII 大文字小文字無視）が含まれるか。
///
/// 健全性: HTML パーサーが `script` 要素を作るのは名前が ASCII 大文字小文字無視で `script` の
/// 開始タグだけで、開始タグは必ず `<` の直後にタグ名が来る。よって偽なら
/// `collect_page_scripts` の結果は空で、ランナーはランタイムを作らない。
/// 偽陽性（コメント・文字列中の `<script`）ではスレッドは起動するが、子プロセスはランナー側の
/// 判定で起動しない。
fn contains_script_start_tag(html: &str) -> bool {
    html.as_bytes()
        .windows(7)
        .any(|w| w.eq_ignore_ascii_case(b"<script"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::NavigationState;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}

    /// 現在スレッドの tokio ランタイムで駆動する実行器（テスト用）。
    struct TestExecutor {
        fail_first: AtomicBool,
    }

    impl TestExecutor {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                fail_first: AtomicBool::new(false),
            })
        }

        fn failing_once() -> Arc<Self> {
            Arc::new(Self {
                fail_first: AtomicBool::new(true),
            })
        }
    }

    impl PageScriptExecutor for TestExecutor {
        fn block_on<'a>(
            &self,
            future: Pin<Box<dyn Future<Output = ()> + 'a>>,
        ) -> std::io::Result<()> {
            if self.fail_first.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other("test executor failure"));
            }
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(future);
            Ok(())
        }
    }

    /// 要求の HTML に埋めた目印で挙動を変える偽ドライバ。
    /// HTML は `<script` を含むが、コメント内なのでランナーは実行対象を持たない。
    #[derive(Default)]
    struct FakeDriver {
        /// `hang` を含む要求が drop されて（future が捨てられて）数えた回数。
        dropped: Arc<AtomicUsize>,
        /// `gate` を含む要求が待つ門。立つまで yield し続ける。
        gate_open: Arc<AtomicBool>,
        /// `gate` を含む要求が走り始めた回数。
        started: Arc<AtomicUsize>,
        /// `panic-once` を含む要求が最初の 1 回だけ panic する。
        panicked: AtomicBool,
    }

    struct DropCount(Arc<AtomicUsize>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl PageRunDriver for FakeDriver {
        fn run<'a>(
            &'a self,
            html: &'a str,
            url: &'a str,
            fetcher: &'a Fetcher,
        ) -> Pin<Box<dyn Future<Output = crate::Result<PageRunOutput>> + 'a>> {
            Box::pin(async move {
                if html.contains("hang") {
                    let _guard = DropCount(Arc::clone(&self.dropped));
                    self.started.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
                if html.contains("gate") {
                    self.started.fetch_add(1, Ordering::SeqCst);
                    while !self.gate_open.load(Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                }
                if html.contains("panic-once") && !self.panicked.swap(true, Ordering::SeqCst) {
                    panic!("fake driver panic (expected in test)");
                }
                run_page_scripts(
                    &PageRunInput::new(html, url),
                    &JsConfig::default(),
                    &PageRunOptions::default(),
                    fetcher,
                )
                .await
            })
        }
    }

    fn html(mark: &str) -> Arc<str> {
        Arc::from(format!("<p>{mark}</p><!-- <script -->"))
    }

    fn req(mark: &str, state: &NavigationState, generation: u64) -> PageRunRequest {
        PageRunRequest::new(
            html(mark),
            "https://example.com/",
            state.scope(),
            // 世代は `begin_navigation` の払い出しを使う（単調増加）。
            nth_generation(state, generation),
        )
    }

    /// `state` から `n` 番目の世代を払い出す（以前に払い出した分は進めない）。
    fn nth_generation(state: &NavigationState, n: u64) -> NavigationGeneration {
        while state.current_generation().get() < n {
            state.begin_navigation().expect("世代を払い出せる");
        }
        // 既に進んでいる場合は現在値から作れないため、n に一致する世代を得るには
        // 呼び出し側が昇順で呼ぶこと。
        assert_eq!(state.current_generation().get(), n, "世代は昇順で要求する");
        state.current_generation()
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("テスト用ランタイム")
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timeout waiting for: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn handle(driver: Arc<FakeDriver>) -> PageScriptHandle {
        PageScriptHandle::with_driver_for_test(
            TestExecutor::new(),
            PageScriptActorOptions::default(),
            driver,
        )
    }

    fn executed_html(r: RunResult) -> String {
        match r.expect("実行された") {
            PageScriptOutcome::Executed(out) => out.html().to_string(),
            other => panic!("Executed を期待: {other:?}"),
        }
    }

    #[test]
    fn js_1_static_checks_send_sync() {
        assert_send_sync::<PageScriptHandle>();
        assert_send_sync::<PageScriptActorOptions>();
        assert_send_sync::<WorkerConfig>();
        assert_send::<PageRunFuture>();
        assert_send::<PageRunRequest>();
        assert_send::<PageScriptOutcome>();
        assert_send::<PageScriptError>();
        assert_send::<PageRunOutput>();
    }

    /// `JS-1`・`PERF-7`: 生成直後・無効ハンドル・`<script>` なしではスレッドを起動しない。
    #[test]
    fn js_1_lazy_start_no_worker_until_first_script_request() {
        let handle = handle(Arc::new(FakeDriver::default()));
        assert_eq!(handle.stats().worker_spawns(), 0);
        assert!(!handle.stats().worker_running());

        let state = NavigationState::new();
        let rt = rt();
        let no_script = PageRunRequest::new(
            Arc::from("<p>plain</p>"),
            "https://example.com/",
            state.scope(),
            nth_generation(&state, 1),
        );
        let out = rt.block_on(handle.run_page_scripts(no_script));
        assert!(matches!(
            out,
            Ok(PageScriptOutcome::Skipped(
                PageScriptSkipReason::NoScriptElement
            ))
        ));
        assert_eq!(handle.stats().worker_spawns(), 0);

        let disabled = PageScriptHandle::disabled();
        assert!(!disabled.is_enabled());
        let out = rt.block_on(disabled.run_page_scripts(req("x", &state, 1)));
        assert!(matches!(
            out,
            Ok(PageScriptOutcome::Skipped(PageScriptSkipReason::Disabled))
        ));
        assert_eq!(disabled.stats().worker_spawns(), 0);

        // 最初の実行要求で初めて起動し、完了後はスレッドが終了して、次の要求で再起動する。
        let out = rt.block_on(handle.run_page_scripts(req("first", &state, 1)));
        assert!(executed_html(out).contains("first"));
        assert_eq!(handle.stats().worker_spawns(), 1);
        wait_until("worker exit", || !handle.stats().worker_running());
        let out = rt.block_on(handle.run_page_scripts(req("second", &state, 1)));
        assert!(executed_html(out).contains("second"));
        assert_eq!(handle.stats().worker_spawns(), 2);
    }

    /// `JS-4`: 実行中の古い世代は打ち切られ（`Cancelled`・内側 future が drop される）、
    /// 新しい世代だけが結果になる。呼び出し側が `Ok` のときだけ commit する想定でも古い HTML は残らない。
    #[test]
    fn js_4_running_older_generation_is_cancelled() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let state = NavigationState::new();
        let rt = rt();

        let first = handle.run_page_scripts(req("hang-gen-1", &state, 1));
        wait_until("first started", || {
            driver.started.load(Ordering::SeqCst) == 1
        });
        let second = handle.run_page_scripts(req("gen-2", &state, 2));
        let (r1, r2) = rt.block_on(async { tokio::join!(first, second) });
        assert!(matches!(r1, Err(PageScriptError::Cancelled)), "{r1:?}");
        let html2 = executed_html(r2);
        assert!(html2.contains("gen-2"), "{html2}");
        wait_until("inner dropped", || {
            driver.dropped.load(Ordering::SeqCst) == 1
        });

        // 呼び出し側の模擬: Ok のときだけ commit する。
        let gen2 = state.current_generation();
        state
            .commit_navigation(
                gen2,
                crate::state::NavigationResult::new("https://example.com/", html2),
            )
            .expect("commit");
        let latest = state.latest().expect("結果あり");
        assert!(latest.html().contains("gen-2"));
    }

    /// `JS-4`: `<script` の無い新しい要求でも、同じ scope の実行中の古い要求は `Cancelled` になる。
    #[test]
    fn js_4_scriptless_newer_request_cancels_running_older() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let state = NavigationState::new();
        let rt = rt();

        let first = handle.run_page_scripts(req("hang-gen-1", &state, 1));
        wait_until("first started", || {
            driver.started.load(Ordering::SeqCst) == 1
        });
        let second = handle.run_page_scripts(PageRunRequest::new(
            Arc::from("<p>no scripts</p>"),
            "https://example.com/",
            state.scope(),
            nth_generation(&state, 2),
        ));
        let (r1, r2) = rt.block_on(async { tokio::join!(first, second) });
        assert!(matches!(r1, Err(PageScriptError::Cancelled)), "{r1:?}");
        assert!(
            matches!(
                r2,
                Ok(PageScriptOutcome::Skipped(
                    PageScriptSkipReason::NoScriptElement
                ))
            ),
            "{r2:?}"
        );
    }

    /// `JS-4`: 待ち中の古い世代は、実行中のジョブが終わる前に打ち切られ、待ち枠が空く。
    #[test]
    fn js_4_queued_older_generation_is_cancelled_and_removed() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let other = NavigationState::new();
        let state = NavigationState::new();
        let rt = rt();

        let blocker = handle.run_page_scripts(req("gate-blocker", &other, 1));
        wait_until("blocker started", || {
            driver.started.load(Ordering::SeqCst) == 1
        });
        let older = handle.run_page_scripts(req("gen-1", &state, 1));
        assert_eq!(handle.stats().queued(), 1);
        let newer = handle.run_page_scripts(req("gen-2", &state, 2));
        assert_eq!(handle.stats().queued(), 1, "古い待ちジョブは取り除かれる");

        let r_older = rt.block_on(older);
        assert!(matches!(r_older, Err(PageScriptError::Cancelled)));
        driver.gate_open.store(true, Ordering::SeqCst);
        let (rb, rn) = rt.block_on(async { tokio::join!(blocker, newer) });
        assert!(executed_html(rb).contains("gate-blocker"));
        assert!(executed_html(rn).contains("gen-2"));
    }

    /// `JS-4`: 遅れて届いた古い世代は即 `Cancelled`。
    #[test]
    fn js_4_late_older_generation_is_rejected_immediately() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let state = NavigationState::new();
        let rt = rt();

        let older_req = req("gen-1", &state, 1);
        let newer = handle.run_page_scripts(req("gate-gen-2", &state, 2));
        wait_until("newer started", || {
            driver.started.load(Ordering::SeqCst) == 1
        });
        let late = handle.run_page_scripts(older_req);
        assert!(matches!(rt.block_on(late), Err(PageScriptError::Cancelled)));
        driver.gate_open.store(true, Ordering::SeqCst);
        assert!(executed_html(rt.block_on(newer)).contains("gate-gen-2"));
    }

    /// `JS-4`: 新しい世代が完了した後に届いた古い世代も `Cancelled`（実行しない）。
    #[test]
    fn js_4_older_generation_after_newer_completed_is_cancelled() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let state = NavigationState::new();
        let rt = rt();

        let older_req = req("gen-1", &state, 1);
        let newer = handle.run_page_scripts(req("gen-2", &state, 2));
        assert!(executed_html(rt.block_on(newer)).contains("gen-2"));
        let started = driver.started.load(Ordering::SeqCst);

        let late = handle.run_page_scripts(older_req);
        let r = rt.block_on(late);
        assert!(matches!(r, Err(PageScriptError::Cancelled)), "{r:?}");
        assert_eq!(
            driver.started.load(Ordering::SeqCst),
            started,
            "実行されない"
        );
    }

    /// `JS-4`: `<script` の無い新しい世代が `Skipped` で終わった後に届いた古い世代も `Cancelled`。
    #[test]
    fn js_4_older_generation_after_newer_skipped_is_cancelled() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let state = NavigationState::new();
        let rt = rt();

        let older_req = req("gen-1", &state, 1);
        let newer = handle.run_page_scripts(PageRunRequest::new(
            Arc::from("<p>no scripts</p>"),
            "https://example.com/",
            state.scope(),
            nth_generation(&state, 2),
        ));
        let r2 = rt.block_on(newer);
        assert!(
            matches!(
                r2,
                Ok(PageScriptOutcome::Skipped(
                    PageScriptSkipReason::NoScriptElement
                ))
            ),
            "{r2:?}"
        );

        let late = handle.run_page_scripts(older_req);
        let r = rt.block_on(late);
        assert!(matches!(r, Err(PageScriptError::Cancelled)), "{r:?}");
        assert_eq!(driver.started.load(Ordering::SeqCst), 0, "実行されない");
    }

    /// 別の遷移系列（ターゲット）は同じ世代番号でも互いを打ち切らない。
    #[test]
    fn js_4_different_scope_does_not_cancel() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let a = NavigationState::new();
        let b = NavigationState::new();
        assert_ne!(a.scope(), b.scope());
        let rt = rt();

        let first = handle.run_page_scripts(req("gate-a", &a, 1));
        wait_until("a started", || driver.started.load(Ordering::SeqCst) == 1);
        let second = handle.run_page_scripts(req("b-gen-2", &b, 1));
        driver.gate_open.store(true, Ordering::SeqCst);
        let (r1, r2) = rt.block_on(async { tokio::join!(first, second) });
        assert!(executed_html(r1).contains("gate-a"));
        assert!(executed_html(r2).contains("b-gen-2"));
    }

    /// `JS-6`: 上限（既定 16）を超える要求は待たせず `Busy`。止めを外すと積んだ分は全て実行される。
    #[test]
    fn js_6_queue_overflow_is_busy_without_waiting() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let rt = rt();

        let states: Vec<NavigationState> = (0..18).map(|_| NavigationState::new()).collect();
        let mut futures = Vec::new();
        let blocker = handle.run_page_scripts(req("gate-blocker", &states[0], 1));
        wait_until("blocker started", || {
            driver.started.load(Ordering::SeqCst) == 1
        });
        for (i, st) in states.iter().enumerate().skip(1).take(16) {
            futures.push(handle.run_page_scripts(req(&format!("q{i}"), st, 1)));
        }
        assert_eq!(handle.stats().queued(), 16);
        let overflow = handle.run_page_scripts(req("overflow", &states[17], 1));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut overflow = Box::pin(overflow);
        match overflow.as_mut().poll(&mut cx) {
            Poll::Ready(Err(PageScriptError::Busy { capacity })) => assert_eq!(capacity, 16),
            other => panic!("初回 poll で Busy を期待: {other:?}"),
        }

        driver.gate_open.store(true, Ordering::SeqCst);
        let out = rt.block_on(async {
            let mut outs = vec![blocker.await];
            for f in futures {
                outs.push(f.await);
            }
            outs
        });
        assert_eq!(out.len(), 17);
        for r in out {
            assert!(matches!(r, Ok(PageScriptOutcome::Executed(_))), "{r:?}");
        }
    }

    /// 待ち中の future を drop すると待ち行列から消え、枠が空く。
    #[test]
    fn js_6_dropping_queued_future_frees_slot() {
        let driver = Arc::new(FakeDriver::default());
        let handle = PageScriptHandle::with_driver_for_test(
            TestExecutor::new(),
            PageScriptActorOptions::default().with_queue_capacity(1),
            driver.clone(),
        );
        let a = NavigationState::new();
        let b = NavigationState::new();
        let c = NavigationState::new();
        let rt = rt();

        let blocker = handle.run_page_scripts(req("gate-blocker", &a, 1));
        wait_until("started", || driver.started.load(Ordering::SeqCst) == 1);
        let queued = handle.run_page_scripts(req("queued", &b, 1));
        assert_eq!(handle.stats().queued(), 1);
        let busy = handle.run_page_scripts(req("busy", &c, 1));
        assert!(matches!(
            rt.block_on(busy),
            Err(PageScriptError::Busy { capacity: 1 })
        ));
        drop(queued);
        assert_eq!(handle.stats().queued(), 0);
        let again = handle.run_page_scripts(req("again", &c, 1));
        assert_eq!(handle.stats().queued(), 1);
        driver.gate_open.store(true, Ordering::SeqCst);
        let (r1, r2) = rt.block_on(async { tokio::join!(blocker, again) });
        assert!(executed_html(r1).contains("gate-blocker"));
        assert!(executed_html(r2).contains("again"));
    }

    /// 実行中の future を drop するとそのジョブが打ち切られる。
    #[test]
    fn js_4_dropping_running_future_cancels_job() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let state = NavigationState::new();
        let fut = handle.run_page_scripts(req("hang", &state, 1));
        wait_until("started", || driver.started.load(Ordering::SeqCst) == 1);
        drop(fut);
        wait_until("inner dropped", || {
            driver.dropped.load(Ordering::SeqCst) == 1
        });
        wait_until("worker exit", || !handle.stats().worker_running());
        assert!(!handle.stats().running());
    }

    /// 最後のハンドルを drop すると待ちジョブが `Err` で解決し、実行中は打ち切られる。
    #[test]
    fn js_4_dropping_handle_resolves_waiters() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let a = NavigationState::new();
        let b = NavigationState::new();
        let running = handle.run_page_scripts(req("hang", &a, 1));
        wait_until("started", || driver.started.load(Ordering::SeqCst) == 1);
        let queued = handle.run_page_scripts(req("queued", &b, 1));
        drop(handle);
        let rt = rt();
        assert!(matches!(
            rt.block_on(running),
            Err(PageScriptError::Cancelled)
        ));
        assert!(matches!(
            rt.block_on(queued),
            Err(PageScriptError::Cancelled)
        ));
        wait_until("inner dropped", || {
            driver.dropped.load(Ordering::SeqCst) == 1
        });
    }

    /// ワーカーが panic しても呼び出し側は panic せず `Err` を受け取り、次の要求で再起動できる。
    #[test]
    fn js_6_worker_panic_is_reported_and_actor_restarts() {
        let driver = Arc::new(FakeDriver::default());
        let handle = handle(Arc::clone(&driver));
        let a = NavigationState::new();
        let b = NavigationState::new();
        let rt = rt();

        let panicking = handle.run_page_scripts(req("panic-once", &a, 1));
        let queued = handle.run_page_scripts(req("queued", &b, 1));
        let r1 = rt.block_on(panicking);
        assert!(
            matches!(
                r1,
                Err(PageScriptError::ActorUnavailable {
                    kind: ActorFailureKind::Terminated
                })
            ),
            "{r1:?}"
        );
        let r2 = rt.block_on(queued);
        assert!(
            matches!(
                r2,
                Err(PageScriptError::ActorUnavailable {
                    kind: ActorFailureKind::Terminated
                })
            ),
            "{r2:?}"
        );
        wait_until("worker dead", || !handle.stats().worker_running());
        let again = rt.block_on(handle.run_page_scripts(req("recovered", &a, 2)));
        assert!(executed_html(again).contains("recovered"));
        assert_eq!(handle.stats().worker_spawns(), 2);
    }

    /// 実行器が `Err` を返しても `ExecutorFailed` で解決し、次の要求で再起動できる。
    #[test]
    fn js_6_executor_failure_is_reported_and_actor_restarts() {
        let handle = PageScriptHandle::with_driver_for_test(
            TestExecutor::failing_once(),
            PageScriptActorOptions::default(),
            Arc::new(FakeDriver::default()),
        );
        let state = NavigationState::new();
        let rt = rt();
        let r1 = rt.block_on(handle.run_page_scripts(req("fail", &state, 1)));
        assert!(
            matches!(
                r1,
                Err(PageScriptError::ActorUnavailable {
                    kind: ActorFailureKind::ExecutorFailed
                })
            ),
            "{r1:?}"
        );
        wait_until("worker dead", || !handle.stats().worker_running());
        let r2 = rt.block_on(handle.run_page_scripts(req("ok", &state, 1)));
        assert!(executed_html(r2).contains("ok"));
        assert_eq!(handle.stats().worker_spawns(), 2);
    }

    #[test]
    fn js_1_script_start_tag_detection_matches_collector() {
        let cases: [(&str, bool); 9] = [
            ("", false),
            ("<p>hi</p>", false),
            ("<script>1</script>", true),
            ("<SCRIPT>1</SCRIPT>", true),
            ("<ScRiPt src=a>", true),
            ("<scr ipt>", false),
            ("<!-- <script> -->", true),
            ("あ<script>い", true),
            ("<scrip", false),
        ];
        for (html, expected) in cases {
            assert_eq!(contains_script_start_tag(html), expected, "{html:?}");
            if !expected {
                let doc =
                    crate::parse::parse_document(html, &crate::parse::ParseOptions::default())
                        .expect("パースできる")
                        .document;
                let collected = crate::page_script::collect_page_scripts(
                    &doc,
                    &crate::page_script::ScriptCollectionOptions::default(),
                );
                assert!(collected.entries().is_empty(), "{html:?}");
            }
        }
    }

    #[test]
    fn js_4_request_debug_hides_html_and_url() {
        let state = NavigationState::new();
        let r = PageRunRequest::new(
            Arc::from("<script>secret-html</script>"),
            "https://example.com/?token=secret-token",
            state.scope(),
            nth_generation(&state, 1),
        );
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("secret"), "{dbg}");
        assert!(dbg.contains("html_len: 28"), "{dbg}");
    }

    #[test]
    fn js_6_error_display_is_fixed_english() {
        assert_eq!(
            PageScriptError::Busy { capacity: 16 }.to_string(),
            "page script queue is full (capacity 16)"
        );
        assert_eq!(
            PageScriptError::Cancelled.to_string(),
            "page script run was cancelled by a newer navigation"
        );
    }
}

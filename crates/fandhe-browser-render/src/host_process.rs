//! レンダリングホスト（Servo ホスト）の子プロセス管理（RENDER-3・RENDER-4・PLUG-1。
//! TASK-38・#994・設計 `docs/design/servo-host-design.md` §2.4・§2.6・§2.7）。
//!
//! [`HostProcess`] はプロファイル 1 つにつき 1 つ作り、最初の描画要求で遅延起動する
//! （static・プールは置かない。PROF 境界）。後続の `HostRenderer`（#235）が core の
//! `Renderer` として本型を包み、cli（#236）が設定 `render.host_path` を
//! [`resolve_host_binary_from_process`] へ渡す。偽ホストを使う結合テストは
//! `tests/host_process.rs`。
//!
//! # 方針
//!
//! - シェルを介さず固定の引数 [`HOST_ARGS`] で起動し、`env_clear` のうえ許可リスト
//!   （[`env`] の定数）だけを渡す。作業ディレクトリは専用の一時ディレクトリ。
//! - クラッシュ・タイムアウト・プロトコル違反・RSS 超過は失敗として返し、成功を装わない。
//!   ホストは破棄し（タイムアウト時は遅れて届くフレームで同期がずれないよう kill）、
//!   次の呼び出しで再起動する。60 秒内に 3 回失敗したらサーキットブレーカーが作動する。
//! - ホスト起動世代とナビゲーション世代を保持し、どちらかが変われば再ロードしてから描画する。
//!
//! # `RenderError` との対応（写像の実装は core に依存する #235 で行う）
//!
//! | [`HostProcessError`] | core の `RenderError` |
//! | -------------------- | --------------------- |
//! | `Disabled`・`InvalidHostBinary` | `RenderingDisabled` |
//! | `SpawnFailed`・`WorkDirFailed`・`Crashed`・`ResourceLimitExceeded`・`CircuitOpen` | `HostUnavailable` |
//! | `Timeout` | `Timeout` |
//! | `ProtocolMismatch`・`ProtocolViolation` | `ProtocolMismatch` |
//! | `Host`（`not_loaded`） | `PageNotLoaded` |
//! | `Host`（`not_found`） | `ElementNotFound` |
//! | `Host`（`timeout`） | `Timeout` |
//! | `Host`（その他）・`InvalidRequest` | `Internal` |
//!
//! # 未実装・暫定
//!
//! - Windows の RSS 監視と Job Object は未実装（新規 `unsafe` と `windows-sys` が要るため承認事項。
//!   REPAIR-3）。プローブが無い OS では監視しない。
//! - [`DEFAULT_RSS_LIMIT_BYTES`]・[`BREAKER_COOLDOWN`]・[`HostTimeouts::load_grace`] は暫定値
//!   （Servo の実測は TASK-82）。

mod breaker;
mod env;
mod resolve;
mod rss;
mod stderr_tail;
mod workdir;

use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use breaker::{BREAKER_COOLDOWN, BREAKER_THRESHOLD, BREAKER_WINDOW};
pub use env::{FORCED_ENV_NAMES, PASSTHROUGH_ENV_NAMES};
pub use resolve::{
    HOST_BINARY_ENV, HOST_BINARY_STEM, HostBinary, resolve_host_binary,
    resolve_host_binary_from_process,
};
pub use stderr_tail::STDERR_TAIL_BYTES;

use crate::codec::{
    MAX_HELLO_FRAME_BYTES, MAX_REQUEST_FRAME_BYTES, decode_hello, decode_response, effective_limit,
    encode_request, read_frame, response_frame_limit, write_frame,
};
use crate::protocol::{
    Hello, HelloLimits, HostError, HostErrorKind, LoadParams, LoadResult, MAX_LOAD_TIMEOUT_MS,
    MAX_SELECTOR_BYTES, MAX_URL_BYTES, Method, MethodResult, PROTOCOL_VERSION, ProtocolError,
    Request, RequestId,
};
use breaker::CircuitBreaker;
use rss::RssMonitor;
use stderr_tail::StderrTail;
use workdir::WorkDir;

/// ホストへ渡す固定の起動引数。stdio サーバーモードに入る契約（#993 が合わせる）。
pub const HOST_ARGS: &[&str] = &["--stdio"];
/// 子プロセスの RSS 上限の既定値（2 GiB。暫定。Servo の実測は TASK-82）。
pub const DEFAULT_RSS_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// ホストバイナリが不正だった理由（診断用。利用者向けの文言にパスを含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidBinaryReason {
    /// 空文字列。
    Empty,
    /// 相対パス。
    NotAbsolute,
    /// 存在しない・canonicalize できない。
    NotFound,
    /// 通常ファイルでない。
    NotRegularFile,
    /// 実行ビットが無い（Unix）。
    NotExecutable,
    /// 他者書き込み可（Unix）。
    WorldWritable,
}

/// タイムアウトした段階。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimeoutPhase {
    /// 起動直後の `hello` 待ち。
    Handshake,
    /// `load`。
    Load,
    /// `screenshot`。
    Screenshot,
    /// `visibility`・`bounding_box`。
    ElementQuery,
    /// `shutdown`。
    Shutdown,
}

/// ホスト子プロセス管理のエラー。`Display` は固定の英語文言で、URL・パス・stderr を含めない。
#[derive(Clone)]
#[non_exhaustive]
pub enum HostProcessError {
    /// ホストパスが未設定（レンダリング層が無効）。
    Disabled,
    /// ホストバイナリが検証に失敗した。
    InvalidHostBinary(InvalidBinaryReason),
    /// 子プロセスを起動できなかった。
    SpawnFailed,
    /// 専用の一時ディレクトリを作れなかった。
    WorkDirFailed,
    /// 期限内に応答が無かった（ホストは kill して破棄済み）。
    Timeout(TimeoutPhase),
    /// ホストが終了した・パイプが壊れた。
    Crashed,
    /// RSS 上限超過（または監視の連続失敗）で kill した。
    ResourceLimitExceeded,
    /// サーキットブレーカー作動中のため起動しなかった。
    CircuitOpen,
    /// ホストの `protocol` 範囲に本クライアントの版が含まれない。
    ProtocolMismatch,
    /// フレーム・JSON・id の不整合などプロトコル違反。
    ProtocolViolation(ProtocolError),
    /// ホストの `error` 応答（正常な応答のためホストは維持する）。
    Host(HostError),
    /// `call_on_page` に `Load`・`Shutdown` が渡された、または要求を符号化できなかった。
    InvalidRequest,
}

impl fmt::Display for HostProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disabled => "render host is not configured",
            Self::InvalidHostBinary(_) => "render host binary is invalid",
            Self::SpawnFailed => "failed to start render host",
            Self::WorkDirFailed => "failed to create render host work directory",
            Self::Timeout(_) => "render host timed out",
            Self::Crashed => "render host exited unexpectedly",
            Self::ResourceLimitExceeded => "render host exceeded its memory limit",
            Self::CircuitOpen => "render host restarts are suspended after repeated failures",
            Self::ProtocolMismatch => "render host protocol version is not supported",
            Self::ProtocolViolation(_) => "render host violated the protocol",
            Self::Host(_) => "render host reported an error",
            Self::InvalidRequest => "invalid render host request",
        })
    }
}

/// ホストの message（untrusted・URL を含みうる）を出さないよう手書きにしている。
impl fmt::Debug for HostProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHostBinary(r) => f.debug_tuple("InvalidHostBinary").field(r).finish(),
            Self::Timeout(p) => f.debug_tuple("Timeout").field(p).finish(),
            Self::ProtocolViolation(e) => f.debug_tuple("ProtocolViolation").field(e).finish(),
            Self::Host(e) => f.debug_tuple("Host").field(&e.kind()).finish(),
            Self::Disabled => f.write_str("Disabled"),
            Self::SpawnFailed => f.write_str("SpawnFailed"),
            Self::WorkDirFailed => f.write_str("WorkDirFailed"),
            Self::Crashed => f.write_str("Crashed"),
            Self::ResourceLimitExceeded => f.write_str("ResourceLimitExceeded"),
            Self::CircuitOpen => f.write_str("CircuitOpen"),
            Self::ProtocolMismatch => f.write_str("ProtocolMismatch"),
            Self::InvalidRequest => f.write_str("InvalidRequest"),
        }
    }
}

impl std::error::Error for HostProcessError {}

impl HostProcessError {
    /// ホストを破棄し、ブレーカーに失敗として数えるか。
    fn counts_as_failure(&self) -> bool {
        matches!(
            self,
            Self::SpawnFailed
                | Self::Timeout(_)
                | Self::Crashed
                | Self::ResourceLimitExceeded
                | Self::ProtocolMismatch
                | Self::ProtocolViolation(_)
        )
    }
}

/// 段階別のタイムアウト（設計 §2.4）。短縮値はテスト・設定用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostTimeouts {
    /// `hello` 待ち（既定 10 s）。
    pub handshake: Duration,
    /// `load` の既定（30 s）。
    pub load_default: Duration,
    /// `load` の上限（60 s。[`MAX_LOAD_TIMEOUT_MS`] を超えない）。
    pub load_max: Duration,
    /// `screenshot`（20 s）。
    pub screenshot: Duration,
    /// `visibility`・`bounding_box`（5 s）。
    pub element_query: Duration,
    /// `shutdown` の猶予（3 s。超えたら kill）。
    pub shutdown_grace: Duration,
    /// `load` でホスト自身の `timeout` 応答が先に届くための上乗せ（暫定 2 s）。
    pub load_grace: Duration,
}

impl Default for HostTimeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(10),
            load_default: Duration::from_secs(30),
            load_max: Duration::from_secs(60),
            screenshot: Duration::from_secs(20),
            element_query: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(3),
            load_grace: Duration::from_secs(2),
        }
    }
}

impl HostTimeouts {
    /// `hello` 待ちを変える。
    pub fn with_handshake(mut self, d: Duration) -> Self {
        self.handshake = d;
        self
    }
    /// `load` の既定を変える。
    pub fn with_load_default(mut self, d: Duration) -> Self {
        self.load_default = d;
        self
    }
    /// `load` の上限を変える（[`MAX_LOAD_TIMEOUT_MS`] を超える値は丸める）。
    pub fn with_load_max(mut self, d: Duration) -> Self {
        self.load_max = d.min(Duration::from_millis(u64::from(MAX_LOAD_TIMEOUT_MS)));
        self
    }
    /// `screenshot` を変える。
    pub fn with_screenshot(mut self, d: Duration) -> Self {
        self.screenshot = d;
        self
    }
    /// 要素クエリを変える。
    pub fn with_element_query(mut self, d: Duration) -> Self {
        self.element_query = d;
        self
    }
    /// `shutdown` の猶予を変える。
    pub fn with_shutdown_grace(mut self, d: Duration) -> Self {
        self.shutdown_grace = d;
        self
    }
    /// `load` の上乗せを変える。
    pub fn with_load_grace(mut self, d: Duration) -> Self {
        self.load_grace = d;
        self
    }

    /// 要求された `load` の `timeout_ms` を既定・上限で丸める（最小 1 ms）。
    pub fn load_timeout_ms(&self, requested: Option<u32>) -> u32 {
        let to_ms = |d: Duration| u32::try_from(d.as_millis()).unwrap_or(u32::MAX);
        let max = to_ms(self.load_max).min(MAX_LOAD_TIMEOUT_MS);
        requested
            .unwrap_or_else(|| to_ms(self.load_default))
            .clamp(1, max.max(1))
    }
}

/// [`HostProcess`] の設定。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostProcessConfig {
    /// 検証済みのホストバイナリ。
    pub binary: HostBinary,
    /// 段階別のタイムアウト。
    pub timeouts: HostTimeouts,
    /// 一時ディレクトリの親（絶対パス必須）。
    pub temp_root: PathBuf,
    /// RSS 上限（バイト）。
    pub rss_limit_bytes: u64,
    /// RSS のプローブ間隔。
    pub rss_poll_interval: Duration,
}

impl HostProcessConfig {
    /// 既定値で作る（`temp_root` は OS の一時ディレクトリ）。
    pub fn new(binary: HostBinary) -> Self {
        Self {
            binary,
            timeouts: HostTimeouts::default(),
            temp_root: std::env::temp_dir(),
            rss_limit_bytes: DEFAULT_RSS_LIMIT_BYTES,
            rss_poll_interval: rss::RSS_POLL_INTERVAL,
        }
    }
    /// タイムアウトを差し替える。
    pub fn with_timeouts(mut self, t: HostTimeouts) -> Self {
        self.timeouts = t;
        self
    }
    /// 一時ディレクトリの親を差し替える。
    pub fn with_temp_root(mut self, p: PathBuf) -> Self {
        self.temp_root = p;
        self
    }
    /// RSS 上限を差し替える。
    pub fn with_rss_limit_bytes(mut self, n: u64) -> Self {
        self.rss_limit_bytes = n;
        self
    }
    /// RSS のプローブ間隔を差し替える。
    pub fn with_rss_poll_interval(mut self, d: Duration) -> Self {
        self.rss_poll_interval = d;
        self
    }
}

/// 診断用の情報。ログ専用で、エラー型には載せない。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostDiagnostics {
    /// 直近のホストの stderr 末尾（8 KiB 以内。クエリと userinfo は伏字化済み）。
    pub stderr_tail: String,
}

/// 現在のホストにロード済みのページ。
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoadedPage {
    url: String,
    nav_generation: u64,
    host_generation: u64,
}

/// 再ロードが必要か（設計 §2.7）。未ロード、または URL・ナビゲーション世代・
/// ホスト起動世代のどれかが異なれば true。
fn needs_reload(
    loaded: Option<&LoadedPage>,
    url: &str,
    nav_generation: u64,
    host_generation: u64,
) -> bool {
    match loaded {
        None => true,
        Some(l) => {
            l.url != url
                || l.nav_generation != nav_generation
                || l.host_generation != host_generation
        }
    }
}

type FrameResult = Result<Vec<u8>, ProtocolError>;

/// 起動中の子プロセスとその付属スレッド・資源。
struct Running {
    child: Arc<Mutex<Child>>,
    pid: u32,
    write_tx: Sender<Vec<u8>>,
    ack_rx: Receiver<Result<(), ProtocolError>>,
    read_cmd_tx: Sender<usize>,
    frame_rx: Receiver<FrameResult>,
    stderr_thread: Option<JoinHandle<()>>,
    monitor: Option<RssMonitor>,
    exceeded: Arc<AtomicBool>,
    hello: Option<Hello>,
    workdir: WorkDir,
}

/// プロファイル 1 つ分のレンダリングホスト子プロセス。
pub struct HostProcess {
    config: HostProcessConfig,
    running: Option<Running>,
    breaker: CircuitBreaker,
    host_generation: u64,
    loaded: Option<LoadedPage>,
    next_id: u64,
    stderr_tail: Arc<Mutex<StderrTail>>,
}

impl fmt::Debug for HostProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostProcess")
            .field("host_generation", &self.host_generation)
            .field("pid", &self.pid())
            // URL は出さず長さだけにする（機密の露出対策）。
            .field("loaded_url_len", &self.loaded.as_ref().map(|l| l.url.len()))
            .finish_non_exhaustive()
    }
}

/// 要求フレーム長と URL・セレクタ長が、固定上限とホスト申告値の小さい方（実効上限）に
/// 収まるか。`limits` が無い（hello 前）場合は固定上限だけで判定する。
fn request_within_limits(
    limits: Option<&HelloLimits>,
    request: &Request,
    frame_len: usize,
) -> bool {
    let frame_max = effective_limit(
        MAX_REQUEST_FRAME_BYTES,
        limits.map(|l| l.request_frame_bytes),
    );
    let url_max = effective_limit(MAX_URL_BYTES, limits.map(|l| l.url_bytes));
    let selector_max = effective_limit(MAX_SELECTOR_BYTES, limits.map(|l| l.selector_bytes));
    let param_ok = match request {
        Request::Load(p) => p.url().len() <= url_max,
        Request::Visibility(p) => p.selector().len() <= selector_max,
        Request::BoundingBox(p) => p.selector().len() <= selector_max,
        Request::Screenshot(_) | Request::Shutdown => true,
    };
    param_ok && frame_len <= frame_max
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn classify_read(
    r: Result<FrameResult, RecvTimeoutError>,
    phase: TimeoutPhase,
) -> Result<Vec<u8>, HostProcessError> {
    match r {
        Err(RecvTimeoutError::Timeout) => Err(HostProcessError::Timeout(phase)),
        Err(RecvTimeoutError::Disconnected) => Err(HostProcessError::Crashed),
        Ok(Err(ProtocolError::Closed | ProtocolError::Truncated | ProtocolError::Io(_))) => {
            Err(HostProcessError::Crashed)
        }
        Ok(Err(e)) => Err(HostProcessError::ProtocolViolation(e)),
        Ok(Ok(bytes)) => Ok(bytes),
    }
}

/// 1 往復分の送受信（読み取りの上限通知 → 書き込み完了待ち → 応答フレーム待ち）。
fn roundtrip(
    r: &Running,
    payload: Vec<u8>,
    read_limit: usize,
    deadline: Instant,
    phase: TimeoutPhase,
) -> Result<Vec<u8>, HostProcessError> {
    r.read_cmd_tx
        .send(read_limit)
        .map_err(|_| HostProcessError::Crashed)?;
    r.write_tx
        .send(payload)
        .map_err(|_| HostProcessError::Crashed)?;
    match r.ack_rx.recv_timeout(remaining(deadline)) {
        Ok(Ok(())) => {}
        Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {
            return Err(HostProcessError::Crashed);
        }
        Err(RecvTimeoutError::Timeout) => return Err(HostProcessError::Timeout(phase)),
    }
    classify_read(r.frame_rx.recv_timeout(remaining(deadline)), phase)
}

impl HostProcess {
    /// 作る。ここでは起動しない（遅延起動）。
    pub fn new(config: HostProcessConfig) -> Self {
        Self {
            config,
            running: None,
            breaker: CircuitBreaker::new(),
            host_generation: 0,
            loaded: None,
            next_id: 1,
            stderr_tail: Arc::new(Mutex::new(StderrTail::new())),
        }
    }

    /// ハンドシェイクに成功するたびに +1 されるホスト起動世代。
    pub fn host_generation(&self) -> u64 {
        self.host_generation
    }

    /// 起動中の子の pid（診断・テスト用）。
    pub fn pid(&self) -> Option<u32> {
        self.running.as_ref().map(|r| r.pid)
    }

    /// 起動中のホストの作業ディレクトリ（テスト用）。
    pub fn workdir_path(&self) -> Option<PathBuf> {
        self.running
            .as_ref()
            .map(|r| r.workdir.path().to_path_buf())
    }

    /// 起動中のホストの `hello`。
    pub fn hello(&self) -> Option<&Hello> {
        self.running.as_ref().and_then(|r| r.hello.as_ref())
    }

    /// 直近のホストの診断情報（ログ専用）。
    pub fn diagnostics(&self) -> HostDiagnostics {
        let stderr_tail = self
            .stderr_tail
            .lock()
            .map(|t| t.redacted())
            .unwrap_or_default();
        HostDiagnostics { stderr_tail }
    }

    /// ページを読み込む。必要ならホストを起動する。
    pub fn load(
        &mut self,
        page: &LoadParams,
        nav_generation: u64,
    ) -> Result<LoadResult, HostProcessError> {
        self.ensure_running()?;
        self.load_inner(page, nav_generation)
    }

    /// 現在のページに対する要求（`screenshot`・`visibility`・`bounding_box`）を送る。
    /// ホストが無ければ起動し、ロード済みページが現在の世代と一致しなければ再ロードしてから送る。
    pub fn call_on_page(
        &mut self,
        page: &LoadParams,
        nav_generation: u64,
        request: Request,
    ) -> Result<MethodResult, HostProcessError> {
        if matches!(request, Request::Load(_) | Request::Shutdown) {
            return Err(HostProcessError::InvalidRequest);
        }
        self.ensure_running()?;
        if needs_reload(
            self.loaded.as_ref(),
            page.url(),
            nav_generation,
            self.host_generation,
        ) {
            self.load_inner(page, nav_generation)?;
        }
        let (wait, phase) = self.wait_for(&request);
        self.exchange(&request, wait, phase)
    }

    /// `shutdown` 要求 → 猶予 → kill → wait → 一時ディレクトリ削除。何度呼んでもよい。
    pub fn shutdown(&mut self) {
        if let Some(r) = self.running.as_ref() {
            let deadline = Instant::now() + self.config.timeouts.shutdown_grace;
            if let Ok(id) = RequestId::new(self.next_id) {
                self.next_id += 1;
                if let Ok(bytes) = encode_request(id, &Request::Shutdown)
                    && request_within_limits(
                        r.hello.as_ref().map(|h| &h.limits),
                        &Request::Shutdown,
                        bytes.len(),
                    )
                {
                    let limit = response_frame_limit(Method::Shutdown);
                    // 応答の中身は見ない。受け取れても受け取れなくても、猶予の間に終了を待つ。
                    let _ = roundtrip(r, bytes, limit, deadline, TimeoutPhase::Shutdown);
                }
            }
            while Instant::now() < deadline {
                let exited = r
                    .child
                    .lock()
                    .map(|mut c| !matches!(c.try_wait(), Ok(None)))
                    .unwrap_or(true);
                if exited {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.teardown();
        self.loaded = None;
    }

    fn wait_for(&self, request: &Request) -> (Duration, TimeoutPhase) {
        let t = &self.config.timeouts;
        match request {
            Request::Screenshot(_) => (t.screenshot, TimeoutPhase::Screenshot),
            Request::Load(p) => (
                Duration::from_millis(u64::from(t.load_timeout_ms(p.timeout_ms()))) + t.load_grace,
                TimeoutPhase::Load,
            ),
            Request::Shutdown => (t.shutdown_grace, TimeoutPhase::Shutdown),
            _ => (t.element_query, TimeoutPhase::ElementQuery),
        }
    }

    fn load_inner(
        &mut self,
        page: &LoadParams,
        nav_generation: u64,
    ) -> Result<LoadResult, HostProcessError> {
        let ms = self.config.timeouts.load_timeout_ms(page.timeout_ms());
        let params =
            LoadParams::new(page.url(), Some(ms)).map_err(|_| HostProcessError::InvalidRequest)?;
        let request = Request::Load(params);
        let (wait, phase) = self.wait_for(&request);
        self.loaded = None;
        match self.exchange(&request, wait, phase)? {
            MethodResult::Load(l) => {
                self.loaded = Some(LoadedPage {
                    url: page.url().to_string(),
                    nav_generation,
                    host_generation: self.host_generation,
                });
                Ok(l)
            }
            _ => Err(self.discard(HostProcessError::ProtocolViolation(
                ProtocolError::InvalidMessage("unexpected result type"),
            ))),
        }
    }

    /// 1 要求を送って応答を待つ。失敗はホストを破棄してブレーカーに記録する。
    fn exchange(
        &mut self,
        request: &Request,
        wait: Duration,
        phase: TimeoutPhase,
    ) -> Result<MethodResult, HostProcessError> {
        let id = RequestId::new(self.next_id).map_err(|_| HostProcessError::InvalidRequest)?;
        self.next_id += 1;
        let payload = encode_request(id, request).map_err(|_| HostProcessError::InvalidRequest)?;
        let method = request.method();
        let outcome = {
            let Some(r) = self.running.as_ref() else {
                return Err(HostProcessError::Crashed);
            };
            // ホストが申告した上限（固定上限との min）を超える要求は送らない。
            // 呼び出し側の入力の問題でありホストの失敗ではないため、破棄もブレーカー記録もしない。
            if !request_within_limits(r.hello.as_ref().map(|h| &h.limits), request, payload.len()) {
                return Err(HostProcessError::InvalidRequest);
            }
            let declared = r.hello.as_ref().map(|h| {
                if method == Method::Screenshot {
                    h.limits.screenshot_frame_bytes
                } else {
                    h.limits.response_frame_bytes
                }
            });
            let limit = effective_limit(response_frame_limit(method), declared);
            roundtrip(r, payload, limit, Instant::now() + wait, phase)
        };
        let bytes = match outcome {
            Ok(b) => b,
            Err(e) => return Err(self.discard(e)),
        };
        let response = match decode_response(&bytes, method) {
            Ok(r) => r,
            Err(e) => return Err(self.discard(HostProcessError::ProtocolViolation(e))),
        };
        if response.id != id {
            return Err(self.discard(HostProcessError::ProtocolViolation(
                ProtocolError::InvalidMessage("response id mismatch"),
            )));
        }
        self.breaker.record_success();
        match response.outcome {
            Ok(m) => Ok(m),
            Err(host_err) => {
                if host_err.kind() == HostErrorKind::NotLoaded {
                    self.loaded = None;
                }
                Err(HostProcessError::Host(host_err))
            }
        }
    }

    /// ホストを破棄し、失敗ならブレーカーへ記録して最終的なエラーを返す。
    fn discard(&mut self, err: HostProcessError) -> HostProcessError {
        let exceeded = self
            .running
            .as_ref()
            .is_some_and(|r| r.exceeded.load(Ordering::SeqCst));
        let err = if exceeded
            && matches!(
                err,
                HostProcessError::Crashed | HostProcessError::Timeout(_)
            ) {
            HostProcessError::ResourceLimitExceeded
        } else {
            err
        };
        self.teardown();
        self.loaded = None;
        if err.counts_as_failure() {
            self.breaker.record_failure(Instant::now());
        }
        err
    }

    /// 子を kill して wait し、付属スレッドの終了を待ち、一時ディレクトリを削除する。
    fn teardown(&mut self) {
        let Some(mut r) = self.running.take() else {
            return;
        };
        drop(r.monitor.take());
        if let Ok(mut c) = r.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // stderr は子の終了で EOF になる。診断に取り込むため少しだけ待つ（上限付き）。
        if let Some(h) = r.stderr_thread.take() {
            let deadline = Instant::now() + Duration::from_millis(500);
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        // child を wait した後に workdir を落とす（Windows は cwd 使用中だと削除できない）。
        drop(r);
    }

    fn ensure_running(&mut self) -> Result<(), HostProcessError> {
        if let Some(r) = self.running.as_ref() {
            let alive = r
                .child
                .lock()
                .map(|mut c| matches!(c.try_wait(), Ok(None)))
                .unwrap_or(false);
            if alive {
                return Ok(());
            }
            // 呼び出しの合間にホストが死んでいた。失敗として数えて破棄し、再起動へ進む。
            let _ = self.discard(HostProcessError::Crashed);
        }
        self.breaker.check(Instant::now())?;
        self.spawn()
    }

    fn spawn(&mut self) -> Result<(), HostProcessError> {
        let workdir = WorkDir::create(&self.config.temp_root)?;
        let mut cmd = Command::new(self.config.binary.path());
        cmd.args(HOST_ARGS)
            .env_clear()
            .current_dir(workdir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        env::apply(&mut cmd, workdir.path(), &workdir.home(), &workdir.tmp());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return Err(self.fail_spawn()),
        };
        let pid = child.id();
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(self.fail_spawn());
        };

        // 新しいホストの stderr は新しいバッファへ（直前のホストの診断を上書きしない）。
        let tail = Arc::new(Mutex::new(StderrTail::new()));
        self.stderr_tail = Arc::clone(&tail);

        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>();
        let (ack_tx, ack_rx) = mpsc::channel::<Result<(), ProtocolError>>();
        let (read_cmd_tx, read_cmd_rx) = mpsc::channel::<usize>();
        let (frame_tx, frame_rx) = mpsc::channel::<FrameResult>();

        let threads = (|| -> std::io::Result<JoinHandle<()>> {
            std::thread::Builder::new()
                .name("render-host-writer".into())
                .spawn(move || {
                    for payload in write_rx {
                        let r = write_frame(&mut stdin, &payload, MAX_REQUEST_FRAME_BYTES);
                        let failed = r.is_err();
                        if ack_tx.send(r).is_err() || failed {
                            break;
                        }
                    }
                })?;
            std::thread::Builder::new()
                .name("render-host-reader".into())
                .spawn(move || {
                    for limit in read_cmd_rx {
                        let r = read_frame(&mut stdout, limit);
                        let failed = r.is_err();
                        if frame_tx.send(r).is_err() || failed {
                            break;
                        }
                    }
                })?;
            std::thread::Builder::new()
                .name("render-host-stderr".into())
                .spawn(move || {
                    let mut buf = [0u8; 4096];
                    loop {
                        match stderr.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if let (Ok(mut t), Some(chunk)) = (tail.lock(), buf.get(..n)) {
                                    t.push(chunk);
                                }
                            }
                        }
                    }
                })
        })();
        let stderr_thread = match threads {
            Ok(h) => h,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(self.fail_spawn());
            }
        };

        let child = Arc::new(Mutex::new(child));
        let exceeded = Arc::new(AtomicBool::new(false));
        let monitor = match RssMonitor::start(
            pid,
            self.config.rss_limit_bytes,
            self.config.rss_poll_interval,
            Arc::clone(&child),
            Arc::clone(&exceeded),
        ) {
            Ok(m) => m,
            Err(_) => {
                // 監視できないまま走らせない（RSS 上限が黙って無効になるのを防ぐ）。
                if let Ok(mut c) = child.lock() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                return Err(self.fail_spawn());
            }
        };
        self.running = Some(Running {
            child,
            pid,
            write_tx,
            ack_rx,
            read_cmd_tx,
            frame_rx,
            stderr_thread: Some(stderr_thread),
            monitor,
            exceeded,
            hello: None,
            workdir,
        });
        self.handshake()
    }

    fn fail_spawn(&mut self) -> HostProcessError {
        self.breaker.record_failure(Instant::now());
        HostProcessError::SpawnFailed
    }

    fn handshake(&mut self) -> Result<(), HostProcessError> {
        let deadline = Instant::now() + self.config.timeouts.handshake;
        let outcome = {
            let Some(r) = self.running.as_ref() else {
                return Err(HostProcessError::Crashed);
            };
            if r.read_cmd_tx.send(MAX_HELLO_FRAME_BYTES).is_err() {
                Err(HostProcessError::Crashed)
            } else {
                classify_read(
                    r.frame_rx.recv_timeout(remaining(deadline)),
                    TimeoutPhase::Handshake,
                )
            }
        };
        let hello = match outcome.and_then(|bytes| {
            decode_hello(&bytes).map_err(|e| match e {
                ProtocolError::ProtocolMismatch { .. } => HostProcessError::ProtocolMismatch,
                other => HostProcessError::ProtocolViolation(other),
            })
        }) {
            Ok(h) => h,
            Err(e) => return Err(self.discard(e)),
        };
        let supported = u64::from(PROTOCOL_VERSION);
        if !(hello.protocol.min..=hello.protocol.max).contains(&supported) {
            return Err(self.discard(HostProcessError::ProtocolMismatch));
        }
        if let Some(r) = self.running.as_mut() {
            r.hello = Some(hello);
        }
        self.host_generation += 1;
        Ok(())
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VisibilityParams;

    fn loaded() -> LoadedPage {
        LoadedPage {
            url: "https://example.com/".into(),
            nav_generation: 3,
            host_generation: 2,
        }
    }

    #[test]
    fn render4_needs_reload_truth_table() {
        let l = loaded();
        let u = "https://example.com/";
        assert!(needs_reload(None, u, 3, 2));
        assert!(!needs_reload(Some(&l), u, 3, 2));
        assert!(needs_reload(Some(&l), "https://example.org/", 3, 2));
        assert!(needs_reload(Some(&l), u, 4, 2));
        assert!(needs_reload(Some(&l), u, 3, 3));
    }

    #[test]
    fn render4_default_timeouts_match_design() {
        let t = HostTimeouts::default();
        assert_eq!(t.handshake, Duration::from_secs(10));
        assert_eq!(t.load_default, Duration::from_secs(30));
        assert_eq!(t.load_max, Duration::from_secs(60));
        assert_eq!(t.screenshot, Duration::from_secs(20));
        assert_eq!(t.element_query, Duration::from_secs(5));
        assert_eq!(t.shutdown_grace, Duration::from_secs(3));
    }

    #[test]
    fn render4_load_timeout_is_clamped() {
        let t = HostTimeouts::default();
        assert_eq!(t.load_timeout_ms(None), 30_000);
        assert_eq!(t.load_timeout_ms(Some(45_000)), 45_000);
        assert_eq!(t.load_timeout_ms(Some(90_000)), 60_000);
        let over = HostTimeouts::default().with_load_max(Duration::from_secs(600));
        assert_eq!(over.load_max, Duration::from_secs(60));
    }

    #[test]
    fn render3_error_display_and_debug_hide_host_message() {
        let e = HostProcessError::InvalidHostBinary(InvalidBinaryReason::NotFound);
        assert_eq!(e.to_string(), "render host binary is invalid");
        assert_eq!(HostProcessError::CircuitOpen.to_string(), {
            "render host restarts are suspended after repeated failures"
        });
    }

    fn limits(request: u64, url: u64, selector: u64) -> HelloLimits {
        HelloLimits {
            request_frame_bytes: request,
            response_frame_bytes: 1024,
            hello_frame_bytes: 1024,
            screenshot_frame_bytes: 1024,
            url_bytes: url,
            selector_bytes: selector,
            error_message_bytes: 512,
            max_in_flight: 1,
        }
    }

    #[test]
    fn render3_request_limits_use_min_of_fixed_and_declared() {
        let url = "https://example.com/aaaaaaaaaa"; // 30 バイト
        let load = Request::Load(LoadParams::new(url, None).unwrap());
        // 申告 url_bytes 29 < 30 は拒否、30 は許可。
        assert!(!request_within_limits(
            Some(&limits(65536, 29, 1024)),
            &load,
            100
        ));
        assert!(request_within_limits(
            Some(&limits(65536, 30, 1024)),
            &load,
            100
        ));
        // 要求フレーム上限 99 < 100 は拒否。
        assert!(!request_within_limits(
            Some(&limits(99, 2048, 1024)),
            &load,
            100
        ));
        // 固定上限（64 KiB）を申告値が引き上げても越えられない。
        assert!(!request_within_limits(
            Some(&limits(u64::MAX, 2048, 1024)),
            &load,
            MAX_REQUEST_FRAME_BYTES + 1
        ));
        // セレクタ。
        let sel = Request::Visibility(VisibilityParams::new("div.item").unwrap()); // 8 バイト
        assert!(!request_within_limits(
            Some(&limits(65536, 2048, 7)),
            &sel,
            50
        ));
        assert!(request_within_limits(
            Some(&limits(65536, 2048, 8)),
            &sel,
            50
        ));
        // shutdown はフレーム上限のみ。
        assert!(!request_within_limits(
            Some(&limits(10, 2048, 1024)),
            &Request::Shutdown,
            11
        ));
        assert!(request_within_limits(None, &Request::Shutdown, 11));
    }
}

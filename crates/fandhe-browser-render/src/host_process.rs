//! レンダリングホスト（Servo ホスト）の子プロセス管理（RENDER-3・RENDER-4・PLUG-1。
//! TASK-38・#994・設計 `docs/design/servo-host-design.md` §2.4・§2.6・§2.7）。
//!
//! [`HostProcess`] はプロファイル 1 つにつき 1 つ作り、最初の描画要求で遅延起動する
//! （static・プールは置かない。PROF 境界）。後続の `HostRenderer`（#235）が core の
//! `Renderer` として本型を包み、cli（#236）が設定 `render.host_path` を
//! [`resolve_host_binary_from_process`] へ渡す。偽ホストを使う結合テストは
//! `tests/host_process.rs`（HostProcess 層）・`tests/basic_api.rs`（Renderer 層。TASK-38.8a）。
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
//! # `RenderError` との対応（写像は `host_renderer` で実装済み。#235）
//!
//! | [`HostProcessError`] | core の `RenderError` |
//! | -------------------- | --------------------- |
//! | `Disabled`・`InvalidHostBinary` | `RenderingDisabled` |
//! | `ExitedBeforeHello` | `HostExitedBeforeHello` |
//! | `SpawnFailed`・`WorkDirFailed`・`Crashed`・`ResourceLimitExceeded`・`CircuitOpen`・`EgressIsolationUnavailable` | `HostUnavailable` |
//! | `Timeout` | `Timeout` |
//! | `ProtocolMismatch`・`ProtocolViolation` | `ProtocolMismatch` |
//! | `Host`（`not_loaded`） | `PageNotLoaded` |
//! | `Host`（`not_found`） | `ElementNotFound` |
//! | `Host`（`timeout`） | `Timeout` |
//! | `Host`（その他）・`InvalidRequest` | `Internal` |
//!
//! # egress 隔離（RENDER-3・SEC-2・XOS-1。#1436）
//!
//! 起動時に OS のサンドボックスで包み、ホストの通信を本体側の許可リスト付き CONNECT プロキシ
//! （[`crate::egress_proxy`]）に限る。実装と制約は [`isolation`] モジュールの doc を参照
//! （Linux: bwrap・macOS: sandbox-exec・Windows: 隔離なし）。設定 `[rendering] egress_isolation`
//! の値は [`HostProcessConfig::egress_isolation`] で受ける。`best-effort`（既定）は隔離できなければ
//! 警告（コールバック未設定なら stderr に 1 行）して隔離なしで起動し、`required` は [`HostProcessError::EgressIsolationUnavailable`] で
//! 起動しない。隔離中は WebSocket が使えない。cli の版の検査（`--version`）も同じ判定・同じ包み方で、
//! プロキシなし・送信先なしで実行する（[`run_host_version`]・#1553）。
//!
//! # 未実装・暫定
//!
//! - Windows の RSS 監視と Job Object は未実装（新規 `unsafe` と `windows-sys` が要るため承認事項。
//!   REPAIR-3）。プローブが無い OS では監視しない。
//! - [`DEFAULT_RSS_LIMIT_BYTES`]・[`BREAKER_COOLDOWN`]・[`HostTimeouts::load_grace`] は暫定値
//!   （Servo の実測は TASK-82）。

mod breaker;
mod env;
mod isolation;
mod resolve;
mod rss;
mod stderr_tail;
mod version_run;
mod workdir;

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use breaker::{BREAKER_COOLDOWN, BREAKER_THRESHOLD, BREAKER_WINDOW};
pub use env::{FORCED_ENV_NAMES, PASSTHROUGH_ENV_NAMES};
pub use isolation::{
    EgressIsolationStatus, EgressIsolationUnavailable, EgressIsolationWarning, IsolationBackend,
    IsolationProbe, RELAY_UDS_PATH_MAX_BYTES, TmpfsUnlimitedReason,
    egress_isolation_warning_message, isolation_warning_message, probe_system,
};
pub use resolve::{
    HOST_BINARY_ENV, HOST_BINARY_STEM, HostBinary, resolve_host_binary,
    resolve_host_binary_from_process,
};
pub use stderr_tail::STDERR_TAIL_BYTES;
pub use version_run::{
    DEFAULT_HOST_VERSION_TIMEOUT, HOST_VERSION_ARGS, HOST_VERSION_STDOUT_MAX, HostVersionOutput,
    HostVersionRunConfig, HostVersionRunError, run_host_version,
};

use crate::codec::{
    MAX_HELLO_FRAME_BYTES, MAX_REQUEST_FRAME_BYTES, decode_hello, decode_response, effective_limit,
    encode_request, read_frame, response_frame_limit, write_frame,
};
use crate::egress_proxy::{BoundEndpoint, EgressEndpoint, EgressProxy, EgressProxyConfig};
use crate::protocol::{
    Hello, HelloLimits, HostError, HostErrorKind, LoadParams, LoadResult, MAX_LOAD_TIMEOUT_MS,
    MAX_SELECTOR_BYTES, MAX_URL_BYTES, Method, MethodResult, PROTOCOL_VERSION, ProtocolError,
    Request, RequestId,
};
use breaker::CircuitBreaker;
use fandhe_browser_core::EgressIsolationMode;
use isolation::{Plan, Sandbox};
use rss::{PidSource, RssMonitor};
/// 診断・テスト用の再公開（`real_host` の `EGRESS-PROCTREE` 記録が使う。RENDER-3・#1460・#1466）。
///
/// 別 crate の結合テストからは `cfg(test)` の項目が見えず、feature の追加もしないため、`doc(hidden)`
/// の再公開が唯一の経路になる。公開するのは pid だけを受ける入口で、任意パスを読む関数は出さない。
#[doc(hidden)]
pub use rss::{ProcTreeError, WrappedProcTree, walk_wrapped_proc_tree_live};
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
    /// `hello` を返した後にホストが終了した・パイプが壊れた。
    Crashed,
    /// `hello` を受け取る前にホストの stdout が閉じた・切り詰められた・読めなかった
    /// （RENDER-3・TASK-38・#1539）。引数検査での即終了など、CLI とホストの版の不一致が
    /// 典型だが、版以外の原因（隔離の失敗等）でも同じ分類になる。終了コードは見ない。
    /// ブレーカーには `Crashed` と同じく数える。
    ExitedBeforeHello,
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
    /// `egress_isolation = required` だが隔離できないため起動しなかった（RENDER-3・#1436）。
    /// 理由は `Debug` で取れる。環境の問題でありホストの失敗ではないため、ブレーカーには数えない
    /// （数えると `CircuitOpen` に化けて理由が見えなくなる）。
    EgressIsolationUnavailable(EgressIsolationUnavailable),
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
            Self::ExitedBeforeHello => "render host exited before completing startup",
            Self::ResourceLimitExceeded => "render host exceeded its memory limit",
            Self::CircuitOpen => "render host restarts are suspended after repeated failures",
            Self::ProtocolMismatch => "render host protocol version is not supported",
            Self::ProtocolViolation(_) => "render host violated the protocol",
            Self::Host(_) => "render host reported an error",
            Self::InvalidRequest => "invalid render host request",
            Self::EgressIsolationUnavailable(_) => {
                "render host egress isolation is required but unavailable"
            }
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
            Self::ExitedBeforeHello => f.write_str("ExitedBeforeHello"),
            Self::ResourceLimitExceeded => f.write_str("ResourceLimitExceeded"),
            Self::CircuitOpen => f.write_str("CircuitOpen"),
            Self::ProtocolMismatch => f.write_str("ProtocolMismatch"),
            Self::InvalidRequest => f.write_str("InvalidRequest"),
            Self::EgressIsolationUnavailable(r) => f
                .debug_tuple("EgressIsolationUnavailable")
                .field(r)
                .finish(),
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
                | Self::ExitedBeforeHello
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
        self.handshake = d.min(MAX_STAGE_TIMEOUT);
        self
    }
    /// `load` の既定を変える。
    pub fn with_load_default(mut self, d: Duration) -> Self {
        self.load_default = d.min(MAX_STAGE_TIMEOUT);
        self
    }
    /// `load` の上限を変える（[`MAX_LOAD_TIMEOUT_MS`] を超える値は丸める）。
    pub fn with_load_max(mut self, d: Duration) -> Self {
        self.load_max = d.min(Duration::from_millis(u64::from(MAX_LOAD_TIMEOUT_MS)));
        self
    }
    /// `screenshot` を変える。
    pub fn with_screenshot(mut self, d: Duration) -> Self {
        self.screenshot = d.min(MAX_STAGE_TIMEOUT);
        self
    }
    /// 要素クエリを変える。
    pub fn with_element_query(mut self, d: Duration) -> Self {
        self.element_query = d.min(MAX_STAGE_TIMEOUT);
        self
    }
    /// `shutdown` の猶予を変える。
    pub fn with_shutdown_grace(mut self, d: Duration) -> Self {
        self.shutdown_grace = d.min(MAX_STAGE_TIMEOUT);
        self
    }
    /// `load` の上乗せを変える。
    pub fn with_load_grace(mut self, d: Duration) -> Self {
        self.load_grace = d.min(MAX_STAGE_TIMEOUT);
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
    /// egress 隔離の要求水準（既定 `best-effort`。`RENDER-3`・#1436）。
    pub egress_isolation: EgressIsolationMode,
    /// 内部アドレス宛てをプロキシが許すか（既定 false）。`HostRenderer` が自身の
    /// `allow_private_network_access` と同じ値を書き込み、`load` の URL 検査と食い違わないようにする。
    pub allow_private_network_access: bool,
    /// 隔離可否の判定元（既定 [`IsolationProbe::System`]。差し替えはテスト用）。
    pub isolation_probe: IsolationProbe,
    isolation_warning: IsolationWarning,
    /// テスト用: `isolation_probe` の代わりに使うサンドボックス（本番ビルドには現れない。#1498）。
    #[cfg(test)]
    injected_sandbox: Option<Sandbox>,
}

/// 隔離まわりの警告（隔離なしに落ちた・tmpfs に上限を付けられない）を通知する。コールバックがあれば
/// それだけを呼び、無ければ `out`（本番は stderr）に `warning: <文言>` を 1 行書く（RENDER-3・#1451・
/// #1502）。書き込みの失敗は無視する。render は `tracing` に依存しないため stderr を既定にしている。
fn report_isolation_warning(
    warning: &IsolationWarning,
    event: EgressIsolationWarning,
    out: &mut dyn std::io::Write,
) {
    match warning.0.as_ref() {
        Some(f) => f(event),
        None => {
            let _ = writeln!(out, "warning: {}", egress_isolation_warning_message(event));
        }
    }
}

/// 隔離まわりの警告の通知先。コールバックがあればそれだけを呼び、無ければ stderr に 1 行出す
/// （RENDER-3・#1451・#1502。黙って隔離なしや上限なしで動く方が危険なため）。抑止したい利用側は
/// 何もしないコールバックを渡す。cli は `eprintln!` するコールバックを渡す。
#[derive(Clone, Default)]
struct IsolationWarning(Option<Arc<dyn Fn(EgressIsolationWarning) + Send + Sync>>);

impl fmt::Debug for IsolationWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "IsolationWarning(set)"
        } else {
            "IsolationWarning(none)"
        })
    }
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
            egress_isolation: EgressIsolationMode::default(),
            allow_private_network_access: false,
            isolation_probe: IsolationProbe::default(),
            isolation_warning: IsolationWarning::default(),
            #[cfg(test)]
            injected_sandbox: None,
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
    /// egress 隔離の要求水準を差し替える。
    pub fn with_egress_isolation(mut self, mode: EgressIsolationMode) -> Self {
        self.egress_isolation = mode;
        self
    }
    /// プロキシが内部アドレス宛てを許すかを差し替える（意味は core の `FetchOptions` と同じ）。
    pub fn with_allow_private_network_access(mut self, allow: bool) -> Self {
        self.allow_private_network_access = allow;
        self
    }
    /// 隔離可否の判定元を差し替える（テスト用。本番は既定の `System`）。
    pub fn with_isolation_probe(mut self, probe: IsolationProbe) -> Self {
        self.isolation_probe = probe;
        self
    }
    /// 隔離まわりの警告（[`EgressIsolationWarning`]。`best-effort` で隔離なしに落ちた・tmpfs に上限を
    /// 付けられない）の通知先を設定する。種類ごとに 1 度ずつ呼ぶ（1 つのプロセスで両方が出ることが
    /// ある。[`egress_isolation_warning_message`] で文言を作れる）。未設定なら stderr に 1 行出す。
    pub fn with_isolation_warning(
        mut self,
        f: Arc<dyn Fn(EgressIsolationWarning) + Send + Sync>,
    ) -> Self {
        self.isolation_warning = IsolationWarning(Some(f));
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

/// 起動コマンドと付随する資源（`build_command` の結果）。
struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
    status: EgressIsolationStatus,
    proxy: Option<EgressProxy>,
    sandbox: Option<Sandbox>,
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
    /// 子の親スレッドを生かし続けるための停止用 Sender（`teardown` で drop）。
    parent_stop: Option<Sender<()>>,
    isolation: EgressIsolationStatus,
    /// egress プロキシ。`workdir` より先に落とす（ソケットファイルを消してから WorkDir を消す）。
    _proxy: Option<EgressProxy>,
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
    /// 隔離なしの警告を出したか（HostProcess ごとに 1 回）。
    isolation_warned: bool,
    /// tmpfs の容量上限が無いことの警告を出したか（HostProcess ごとに 1 回。#1498）。
    tmpfs_limit_warned: bool,
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

/// 段階タイムアウトの上限（1 時間）。設定値がこれを超えても丸める。
const MAX_STAGE_TIMEOUT: Duration = Duration::from_secs(3600);

/// 現在時刻に `d` を足した期限。`d` は [`MAX_STAGE_TIMEOUT`] に丸め、
/// 加算がオーバーフローしても panic せず現在時刻（即期限切れ）に倒す。
fn deadline_after(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d.min(MAX_STAGE_TIMEOUT)).unwrap_or(now)
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
            isolation_warned: false,
            tmpfs_limit_warned: false,
        }
    }

    /// 起動中のホストの egress 隔離の状態（診断・テスト用）。
    pub fn egress_isolation(&self) -> Option<EgressIsolationStatus> {
        self.running.as_ref().map(|r| r.isolation)
    }

    /// RSS 監視が解決できた pid（診断・テスト用）。bwrap で包んだ場合は bwrap の子（Servo ホスト）。
    pub fn monitored_pid(&self) -> Option<u32> {
        self.running
            .as_ref()
            .and_then(|r| r.monitor.as_ref())
            .and_then(RssMonitor::monitored_pid)
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
        self.call_on_page_checked(page, nav_generation, request, |_| Ok(()))
    }

    /// [`Self::call_on_page`] に、再ロードの結果を検査するフックを足したもの。
    ///
    /// 再ロードが起きたときだけ `on_reload` が呼ばれ、`Err` なら描画要求を送らずその
    /// エラーを返す（クラッシュ後の再ロードでリダイレクト先が変わっても、呼び出し側が
    /// `final_url` を再検査できるようにする。SSRF 緩和。#235）。
    pub fn call_on_page_checked(
        &mut self,
        page: &LoadParams,
        nav_generation: u64,
        request: Request,
        on_reload: impl FnOnce(&LoadResult) -> Result<(), HostProcessError>,
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
            let loaded = self.load_inner(page, nav_generation)?;
            if let Err(e) = on_reload(&loaded) {
                // 不許可のページがホストに載ったままにならないよう、記録を捨てて次回は再ロードさせる。
                self.loaded = None;
                return Err(e);
            }
        }
        let (wait, phase) = self.wait_for(&request);
        self.exchange(&request, wait, phase)
    }

    /// `shutdown` 要求 → 猶予 → kill → wait → 一時ディレクトリ削除。何度呼んでもよい。
    pub fn shutdown(&mut self) {
        if let Some(r) = self.running.as_ref() {
            let deadline = deadline_after(self.config.timeouts.shutdown_grace);
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
                Duration::from_millis(u64::from(t.load_timeout_ms(p.timeout_ms())))
                    .saturating_add(t.load_grace),
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
        let previous = self.loaded.take();
        let result = self.exchange(&request, wait, phase);
        // 送信前に拒否された要求（InvalidRequest）ではホストの状態が変わらないため、
        // ロード済みページの記録を復元してクライアントとホストの食い違いを防ぐ。
        if matches!(result, Err(HostProcessError::InvalidRequest)) {
            self.loaded = previous;
        }
        match result? {
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
            roundtrip(r, payload, limit, deadline_after(wait), phase)
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
                HostProcessError::Crashed
                    | HostProcessError::ExitedBeforeHello
                    | HostProcessError::Timeout(_)
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
        // bwrap は pid 名前空間を持つため、ラッパーを kill すれば ns 内のホストも落ちる
        // （`isolation` のモジュール doc。RENDER-3）。
        drop(r.monitor.take());
        if let Ok(mut c) = r.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // 子を wait した後で親スレッドを終わらせる（PDEATHSIG は親スレッドの終了で発火するため）。
        drop(r.parent_stop.take());
        // stderr は子の終了で EOF になる。診断に取り込むため少しだけ待つ（上限付き）。
        if let Some(h) = r.stderr_thread.take() {
            let deadline = deadline_after(Duration::from_millis(500));
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

    /// 隔離の方針を決める。Linux の bwrap では WorkDir 配下の UDS のパスも検査し、使えなければ
    /// モードに従って降格（`best-effort`）または拒否（`required`）する。戻り値の `PathBuf` は
    /// Linux の bwrap のときだけ `Some`（正規化済みのソケット用ディレクトリ）。
    fn plan_isolation(
        &self,
        workdir: &WorkDir,
    ) -> Result<(Plan, Option<isolation::SocketDir>), HostProcessError> {
        let mode = self.config.egress_isolation;
        let availability = match self.config.isolation_probe {
            IsolationProbe::System => isolation::probe_sandbox(),
            IsolationProbe::Unavailable(r) => Err(r),
        };
        #[cfg(test)]
        let availability = match &self.config.injected_sandbox {
            Some(s) => Ok(s.clone()),
            None => availability,
        };
        let plan = isolation::decide(mode, availability)?;
        if !matches!(plan, Plan::Isolated(Sandbox::Bwrap { .. })) {
            return Ok((plan, None));
        }
        // 親を隠せない（WorkDirParentNotHideable）などもここで `decide` を通り、`required` は拒否、
        // `best-effort` は警告つきで隔離なしになる（#1492）。
        let Plan::Isolated(Sandbox::Bwrap { hidden, .. }) = &plan else {
            return Ok((plan, None));
        };
        match isolation::socket_dir_for(workdir.path(), hidden) {
            Ok(dir) => Ok((plan, Some(dir))),
            Err(reason) => Ok((isolation::decide(mode, Err(reason))?, None)),
        }
    }

    /// プロキシを起動し、ホストを包むコマンドを組み立てる。`Unisolated` は従来どおりの直接起動。
    fn build_command(
        &mut self,
        plan: Plan,
        socket_dir: Option<isolation::SocketDir>,
    ) -> Result<Launch, HostProcessError> {
        let host = self.config.binary.path().to_path_buf();
        let sandbox = match plan {
            Plan::Unisolated(reason) => {
                if !self.isolation_warned {
                    self.isolation_warned = true;
                    report_isolation_warning(
                        &self.config.isolation_warning,
                        EgressIsolationWarning::Unisolated(reason),
                        &mut std::io::stderr().lock(),
                    );
                }
                return Ok(Launch {
                    program: host,
                    args: HOST_ARGS.iter().map(OsString::from).collect(),
                    status: EgressIsolationStatus::Unisolated(reason),
                    proxy: None,
                    sandbox: None,
                });
            }
            Plan::Isolated(s) => s,
        };
        if let Sandbox::Bwrap { hidden, .. } = &sandbox
            && !self.tmpfs_limit_warned
            && let Some(reason) = match hidden.tmpfs_limit() {
                isolation::TmpfsLimit::Capped(_) => None,
                isolation::TmpfsLimit::Unsupported => Some(TmpfsUnlimitedReason::SizeUnsupported),
                isolation::TmpfsLimit::Undetected(_) => Some(TmpfsUnlimitedReason::DetectionFailed),
            }
        {
            self.tmpfs_limit_warned = true;
            report_isolation_warning(
                &self.config.isolation_warning,
                EgressIsolationWarning::TmpfsUnlimited(reason),
                &mut std::io::stderr().lock(),
            );
        }
        let endpoint = match (&sandbox, &socket_dir) {
            (Sandbox::Bwrap { .. }, Some(sd)) => EgressEndpoint::UnixSocketIn(sd.dir.clone()),
            (Sandbox::SandboxExec, _) => EgressEndpoint::LoopbackTcp,
            // plan_isolation が Bwrap に必ず dir を付ける。到達しない防御。
            (Sandbox::Bwrap { .. }, None) => return Err(self.fail_spawn()),
        };
        let proxy_config = EgressProxyConfig::default()
            .with_allow_private_network_access(self.config.allow_private_network_access);
        // 隔離が使える環境でプロキシだけ起動できないときは、隔離なしへ落とさず失敗にする。
        let proxy = match EgressProxy::start(endpoint, proxy_config) {
            Ok(p) => p,
            Err(_) => return Err(self.fail_spawn()),
        };
        let (program, args) = match (&sandbox, proxy.endpoint(), &socket_dir) {
            (Sandbox::Bwrap { bwrap, hidden }, BoundEndpoint::Unix(path), Some(sd)) => {
                if isolation::relay_socket_path_ok(path).is_err() {
                    return Err(self.fail_spawn());
                }
                // WorkDir（rw）と、隠れる場所にあるホストの実行ファイル（ro）だけを tmpfs の後に戻す。
                // UDS が WorkDir の外なら隠れて届かないので起動失敗にする（#1485）。
                // 親の tmpfs（兄弟 WorkDir の UDS を隠す。#1492）も同じ引数列に含める。
                match isolation::linux_argv(
                    bwrap,
                    hidden,
                    &sd.dir,
                    sd.parent_to_hide.as_deref(),
                    &host,
                    path,
                ) {
                    Some(v) => v,
                    None => return Err(self.fail_spawn()),
                }
            }
            (Sandbox::SandboxExec, BoundEndpoint::Tcp(addr), _) => {
                let Some(port) = NonZeroU16::new(addr.port()) else {
                    return Err(self.fail_spawn());
                };
                isolation::macos_argv(&host, port)
            }
            _ => return Err(self.fail_spawn()),
        };
        Ok(Launch {
            program,
            args,
            status: EgressIsolationStatus::Isolated(sandbox.backend()),
            proxy: Some(proxy),
            sandbox: Some(sandbox),
        })
    }

    fn spawn(&mut self) -> Result<(), HostProcessError> {
        let workdir = WorkDir::create(&self.config.temp_root)?;
        let (plan, socket_dir) = self.plan_isolation(&workdir)?;
        // 隔離（bwrap）時は tmpfs で隠れた後に戻るのは正規化済みの WorkDir だけなので、cwd と
        // 子の環境変数（HOME・TMPDIR・XDG_*）もそのパスから作る。temp_root が symlink 経由でも
        // 子の指し先が消えない（#1485）。
        let (child_root, child_home, child_tmp) =
            child_paths(socket_dir.as_ref().map(|sd| sd.dir.as_path()), &workdir);
        let mut launch = self.build_command(plan, socket_dir)?;
        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args)
            .env_clear()
            .current_dir(&child_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        env::apply(&mut cmd, &child_root, &child_home, &child_tmp);
        // bwrap の `--die-with-parent` は PR_SET_PDEATHSIG を使い、親**スレッド**の終了で発火する。
        // 呼び出し元のスレッド（tokio の blocking スレッド等）が先に終わると、HostProcess が生きて
        // いるのにホストが殺されるため、専用の親スレッドで起動し、`teardown` まで生かし続ける。
        let (child_tx, child_rx) = mpsc::channel::<std::io::Result<Child>>();
        let (parent_stop, stop_rx) = mpsc::channel::<()>();
        let parent = std::thread::Builder::new()
            .name("render-host-parent".into())
            .spawn(move || {
                let spawned = cmd.spawn();
                let ok = spawned.is_ok();
                if let Err(unsent) = child_tx.send(spawned) {
                    if let Ok(mut c) = unsent.0 {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    return;
                }
                if ok {
                    let _ = stop_rx.recv();
                }
            });
        if parent.is_err() {
            return Err(self.fail_spawn());
        }
        let mut child = match child_rx.recv() {
            Ok(Ok(c)) => c,
            _ => return Err(self.fail_spawn()),
        };
        let pid = child.id();
        let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            reap_unregistered(&mut child);
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
                reap_unregistered(&mut child);
                return Err(self.fail_spawn());
            }
        };

        let child = Arc::new(Mutex::new(child));
        let exceeded = Arc::new(AtomicBool::new(false));
        // bwrap に包むと pid は bwrap になり、ホストはその子になる。
        let pid_source = match launch.sandbox {
            Some(Sandbox::Bwrap { .. }) => PidSource::WrappedChildOf(pid),
            _ => PidSource::Direct(pid),
        };
        let monitor = match RssMonitor::start(
            pid_source,
            self.config.rss_limit_bytes,
            self.config.rss_poll_interval,
            Arc::clone(&child),
            Arc::clone(&exceeded),
        ) {
            Ok(m) => m,
            Err(_) => {
                // 監視できないまま走らせない（RSS 上限が黙って無効になるのを防ぐ）。
                if let Ok(mut c) = child.lock() {
                    reap_unregistered(&mut c);
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
            parent_stop: Some(parent_stop),
            isolation: launch.status,
            _proxy: launch.proxy.take(),
            workdir,
        });
        self.handshake()
    }

    fn fail_spawn(&mut self) -> HostProcessError {
        self.breaker.record_failure(Instant::now());
        HostProcessError::SpawnFailed
    }

    fn handshake(&mut self) -> Result<(), HostProcessError> {
        let deadline = deadline_after(self.config.timeouts.handshake);
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
        // `hello` 前の終了は `hello` 後の `Crashed` と区別する。`classify_read` は `roundtrip` と
        // 共用のため、ここ（ハンドシェイク）だけで変換する（#1539）。
        let outcome = outcome.map_err(|e| match e {
            HostProcessError::Crashed => HostProcessError::ExitedBeforeHello,
            other => other,
        });
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

/// `running` へ未登録のまま起動が失敗したときの後始末（`teardown` と同じ手段）。bwrap 配下でも
/// pid 名前空間のため、ラッパーを kill・wait すれば ns 内のホストも落ちる（RENDER-3・#1436）。
fn reap_unregistered(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 子の cwd・`HOME`・`TMPDIR` の元になるパスを返す。`canonical_root`（隔離時の正規化済み
/// WorkDir）があればそれを、なければ WorkDir のパスをそのまま使う（RENDER-3・#1485）。
fn child_paths(canonical_root: Option<&Path>, workdir: &WorkDir) -> (PathBuf, PathBuf, PathBuf) {
    match canonical_root {
        Some(root) => (root.to_path_buf(), root.join("home"), root.join("tmp")),
        None => (workdir.path().to_path_buf(), workdir.home(), workdir.tmp()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::VisibilityParams;

    /// RENDER-3（#1485）: temp_root が symlink 経由でも、隔離時の子の cwd・HOME・TMPDIR は
    /// tmpfs の後に戻す正規化済み WorkDir の中を指す。
    #[cfg(unix)]
    #[test]
    fn render3_child_paths_use_canonical_workdir_via_symlinked_temp_root() {
        // macOS の temp_dir は /private/var/folders/... と長く UDS パス上限を超えるため、
        // macOS だけ短い /tmp 直下を使う（契約は弱めない）。
        #[cfg(target_os = "macos")]
        let base = std::path::PathBuf::from("/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();
        let real = WorkDir::create(&base).expect("real base");
        let link = real.path().join("link");
        std::os::unix::fs::symlink(&base, &link).expect("symlink");
        let workdir = WorkDir::create(&link).expect("workdir");
        let hidden = isolation::HiddenPaths::new(std::path::PathBuf::from("/home/u"));
        let canonical = isolation::socket_dir_for(workdir.path(), &hidden)
            .expect("socket dir")
            .dir;
        assert_ne!(canonical, workdir.path());
        let (cwd, home, tmp) = child_paths(Some(&canonical), &workdir);
        assert_eq!(cwd, canonical);
        assert_eq!(home, canonical.join("home"));
        assert_eq!(tmp, canonical.join("tmp"));
        assert!(home.is_dir() && tmp.is_dir());
        let (cwd2, _, _) = child_paths(None, &workdir);
        assert_eq!(cwd2, workdir.path());
    }

    /// RENDER-3（#1451）: コールバックが無いときは stderr に `warning: ` つきの 1 行を出す。
    #[test]
    fn render3_unisolated_warning_defaults_to_stderr_line() {
        let mut out: Vec<u8> = Vec::new();
        report_isolation_warning(
            &IsolationWarning::default(),
            EgressIsolationWarning::Unisolated(EgressIsolationUnavailable::SandboxToolNotFound),
            &mut out,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "warning: render host egress isolation is unavailable (sandbox tool (bwrap or sandbox-exec) was not found); starting render host without egress isolation\n"
        );
    }

    /// RENDER-3（#1451）: コールバックがあればそれだけを呼び、出力先には書かない（二重出力しない）。
    #[test]
    fn render3_unisolated_warning_uses_callback_only() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let warning = IsolationWarning(Some(Arc::new(move |r| sink.lock().unwrap().push(r))));
        let mut out: Vec<u8> = Vec::new();
        report_isolation_warning(
            &warning,
            EgressIsolationWarning::Unisolated(EgressIsolationUnavailable::SandboxToolNotFound),
            &mut out,
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![EgressIsolationWarning::Unisolated(
                EgressIsolationUnavailable::SandboxToolNotFound
            )]
        );
        assert!(out.is_empty());
    }

    /// RENDER-3・SEC-2（#1444・#1450・#1458）: `spawn` が使うのと**同じ部品**（`WorkDir::create` →
    /// `isolation::socket_dir_for` → `EgressProxy::start(UnixSocketIn)` → `isolation::linux_argv`）を
    /// 直接呼び、UDS ディレクトリが自プロセスが排他的に作った 0700・非 symlink・自 uid 所有で、
    /// ソケットは 0600、ホストへ渡す引数の末尾が `--egress-relay-uds <ソケット>` であることを固定する。
    /// `EgressEndpoint::UnixSocketIn` の呼び出し側の契約の固定が目的。
    ///
    /// 通していない区間: `spawn` 自体と、`plan_isolation`・`build_command` の組み合わせ
    /// （`Plan::Isolated(Bwrap)` のときに `socket_dir` を渡すディスパッチ）。実バイナリと
    /// 環境依存の bwrap プローブが要るため、その区間が変わっても本テストは検出しない。
    #[cfg(unix)]
    #[test]
    fn render_3_sec_2_uds_dir_is_created_by_host_process_0700_and_owned() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // macOS の temp_dir は canonicalize で /private/var/folders/... と長くなり、UDS パス上限
        // （RELAY_UDS_PATH_MAX_BYTES）を超えるため、macOS だけ短い /tmp 直下を使う（契約は弱めない）。
        #[cfg(target_os = "macos")]
        let base = std::path::PathBuf::from("/tmp");
        #[cfg(not(target_os = "macos"))]
        let base = std::env::temp_dir();
        let workdir = WorkDir::create(&base).expect("workdir");
        let hidden = isolation::HiddenPaths::new(std::path::PathBuf::from("/home/u"));
        let socket_dir = isolation::socket_dir_for(workdir.path(), &hidden).expect("socket dir");
        let dir = socket_dir.dir.clone();
        assert_eq!(
            dir,
            std::fs::canonicalize(workdir.path()).expect("canonicalize")
        );
        let dir_meta = std::fs::symlink_metadata(&dir).expect("dir meta");
        assert!(dir_meta.file_type().is_dir() && !dir_meta.file_type().is_symlink());
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);

        let proxy = EgressProxy::start(
            EgressEndpoint::UnixSocketIn(dir.clone()),
            EgressProxyConfig::default(),
        )
        .expect("proxy");
        let BoundEndpoint::Unix(sock) = proxy.endpoint().clone() else {
            panic!("expected a unix endpoint");
        };
        assert_eq!(sock, dir.join(isolation::EGRESS_SOCKET_NAME));
        let sock_meta = std::fs::symlink_metadata(&sock).expect("sock meta");
        assert_eq!(sock_meta.permissions().mode() & 0o777, 0o600);
        // std に geteuid が無いため、このプロセスが bind したソケットの uid を自 uid として使う。
        assert_eq!(dir_meta.uid(), sock_meta.uid());
        // ホストへ渡す引数の末尾も同じソケットに繋がる。
        let (_, args) = isolation::linux_argv(
            std::path::Path::new("/usr/bin/bwrap"),
            &hidden,
            &dir,
            socket_dir.parent_to_hide.as_deref(),
            std::path::Path::new("/host"),
            &sock,
        )
        .expect("argv");
        let tail: Vec<&std::ffi::OsStr> = args
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(|a| a.as_os_str())
            .collect();
        assert_eq!(
            tail,
            [std::ffi::OsStr::new("--egress-relay-uds"), sock.as_os_str()]
        );
        drop(proxy);
        assert!(!sock.exists());
        let root = workdir.path().to_path_buf();
        drop(workdir);
        assert!(!root.exists());
    }

    /// RENDER-3: 未登録の起動失敗でも、ラッパーを kill・wait で回収する。
    #[cfg(unix)]
    #[test]
    fn render3_reap_unregistered_kills_and_waits_wrapper() {
        let mut wrapper = Command::new("/bin/sh")
            .args(["-c", "sleep 60 & wait"])
            .spawn()
            .expect("spawn sh");
        reap_unregistered(&mut wrapper);
        assert!(matches!(wrapper.try_wait(), Ok(Some(_))));
    }

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
        let max = HostTimeouts::default()
            .with_handshake(Duration::MAX)
            .with_shutdown_grace(Duration::MAX)
            .with_load_grace(Duration::MAX);
        assert_eq!(max.handshake, MAX_STAGE_TIMEOUT);
        let _ = deadline_after(Duration::MAX);
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

    /// RENDER-3・#1498・#1502: コールバックが無いとき、tmpfs の上限なしの警告は理由ごとの固定文言を
    /// 1 行（`warning: ` つき）で書く。
    #[test]
    fn render3_tmpfs_unlimited_warning_is_one_fixed_line() {
        for (reason, text) in [
            (
                TmpfsUnlimitedReason::SizeUnsupported,
                "warning: render host sandbox (bwrap) does not support --size; tmpfs mounts are not size-limited\n",
            ),
            (
                TmpfsUnlimitedReason::DetectionFailed,
                "warning: render host sandbox (bwrap) --size support could not be detected; tmpfs mounts are not size-limited\n",
            ),
        ] {
            let mut out: Vec<u8> = Vec::new();
            report_isolation_warning(
                &IsolationWarning::default(),
                EgressIsolationWarning::TmpfsUnlimited(reason),
                &mut out,
            );
            assert_eq!(String::from_utf8(out).unwrap(), text);
        }
    }

    /// RENDER-3・#1502: 上限なしの警告もコールバックだけを通り、出力先には書かれない。
    #[test]
    fn render3_tmpfs_unlimited_warning_goes_through_callback() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let warning = IsolationWarning(Some(Arc::new(move |w| sink.lock().unwrap().push(w))));
        let mut out: Vec<u8> = Vec::new();
        report_isolation_warning(
            &warning,
            EgressIsolationWarning::TmpfsUnlimited(TmpfsUnlimitedReason::DetectionFailed),
            &mut out,
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![EgressIsolationWarning::TmpfsUnlimited(
                TmpfsUnlimitedReason::DetectionFailed
            )]
        );
        assert!(out.is_empty());
    }

    /// 偽 bwrap・偽ホストの置き場（`/tmp` 直下の短い専用ディレクトリ。UDS のパス上限に収める）。
    #[cfg(target_os = "linux")]
    fn fresh_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::fs::canonicalize("/tmp").unwrap().join(format!(
            "fbq-{tag}-{}-{}-{nanos}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "linux")]
    fn write_exec(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// 起動後に bwrap が失敗する構成で `load` を回し、降格しないことを固定する。
    ///
    /// 戻り値は (`load` の失敗の Debug 表記の列, 偽 bwrap の呼び出し回数, 偽ホストが直接実行されたか,
    /// 隔離なしの警告の件数, `egress_isolation()` が `Unisolated` を返したか, 偽 bwrap が親の tmpfs を
    /// 受け取らなかったか, tmpfs の上限なしの警告の件数)。
    #[cfg(target_os = "linux")]
    type RunGot = (Vec<String>, usize, bool, usize, bool, bool, usize);

    #[cfg(target_os = "linux")]
    fn run_failing_bwrap(
        mode: EgressIsolationMode,
        bwrap_body: Option<&str>,
        uncovered_parent: bool,
    ) -> RunGot {
        let dir = fresh_dir("bw");
        let temp_root = dir.join("root");
        std::fs::create_dir(&temp_root).unwrap();
        let count = dir.join("count");
        let direct = dir.join("direct");
        let noparent = dir.join("noparent");
        let host = dir.join("host");
        write_exec(
            &host,
            &format!("#!/bin/sh\ntouch {}\nexit 0\n", direct.display()),
        );
        let bwrap = dir.join("bwrap");
        if let Some(body) = bwrap_body {
            let body = body
                .replace("@COUNT@", &count.display().to_string())
                .replace("@NOPARENT@", &noparent.display().to_string())
                .replace("@PARENT@", &temp_root.display().to_string());
            write_exec(&bwrap, &body);
        }
        let hidden = if uncovered_parent {
            // `/tmp` を覆わない固定対象にして、WorkDir の親（temp_root）を覆う必要がある状況を作る。
            isolation::HiddenPaths::from_resolved(
                vec![PathBuf::from("/run")],
                PathBuf::from("/run/home"),
                isolation::TmpfsLimit::Unsupported,
            )
        } else {
            isolation::HiddenPaths::new(PathBuf::from("/home/u"))
        };
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&warnings);
        let mut config = HostProcessConfig::new(
            resolve::resolve_host_binary(Some(&host), None, None).expect("host binary"),
        )
        .with_temp_root(temp_root)
        .with_egress_isolation(mode)
        .with_isolation_warning(Arc::new(move |r| sink.lock().unwrap().push(r)));
        config.injected_sandbox = Some(Sandbox::Bwrap { bwrap, hidden });
        let mut h = HostProcess::new(config);
        let page = LoadParams::new("https://example.com/", None).expect("page");
        let mut errs = Vec::new();
        for _ in 0..=BREAKER_THRESHOLD {
            errs.push(format!("{:?}", h.load(&page, 1).expect_err("must fail")));
        }
        let unisolated = matches!(
            h.egress_isolation(),
            Some(EgressIsolationStatus::Unisolated(_))
        );
        let calls = std::fs::read_to_string(&count).map_or(0, |t| t.lines().count());
        let direct_ran = direct.exists();
        let (n_warn, n_tmpfs) = {
            let w = warnings.lock().unwrap();
            let unisolated_n = w
                .iter()
                .filter(|e| matches!(e, EgressIsolationWarning::Unisolated(_)))
                .count();
            (unisolated_n, w.len() - unisolated_n)
        };
        let no_parent = noparent.exists();
        drop(h);
        let _ = std::fs::remove_dir_all(&dir);
        (
            errs, calls, direct_ran, n_warn, unisolated, no_parent, n_tmpfs,
        )
    }

    /// 起動前の降格（`WorkDirHiddenBySandbox`・`SocketPathInvalid`・`WorkDirParentNotHideable` が
    /// `best-effort` で警告つきの隔離なしになること）は設計どおりで、ここの対象外。ここで固定するのは
    /// **起動後**の bwrap の失敗が `decide` を通らず、隔離なしの直接起動に落ちないこと。
    #[cfg(target_os = "linux")]
    fn assert_no_downgrade(got: RunGot, tmpfs_warnings: usize, first: &str, calls: usize) {
        let (errs, n_calls, direct, n_warn, unisolated, no_parent, n_tmpfs) = got;
        let mut want: Vec<String> = vec![first.to_string(); BREAKER_THRESHOLD];
        want.push("CircuitOpen".to_string());
        assert_eq!(errs, want);
        assert_eq!(n_calls, calls);
        assert!(!direct, "host must never be spawned directly");
        assert_eq!(n_warn, 0, "no unisolated warning");
        assert_eq!(n_tmpfs, tmpfs_warnings, "tmpfs-unlimited warnings");
        assert!(!unisolated);
        // 親の tmpfs を要する構成では、偽 bwrap が親の tmpfs を受け取っている（印が無い）。
        assert!(!no_parent, "fake bwrap must receive the parent tmpfs");
    }

    /// RENDER-3・SEC-2（#1498・P2-2）: bwrap が起動後に失敗（mount 失敗の想定）しても、`best-effort`
    /// でも `required` でも隔離なしの直接起動に降格せず、失敗として数えて遮断器が開く。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_bwrap_runtime_failure_never_downgrades_to_unisolated() {
        let body = "#!/bin/sh\necho x >> @COUNT@\necho \"bwrap: Can't mount tmpfs on /newroot/x\" >&2\nsleep 0.1\nexit 1\n";
        for mode in [
            EgressIsolationMode::BestEffort,
            EgressIsolationMode::Required,
        ] {
            let got = run_failing_bwrap(mode, Some(body), false);
            // 失敗の種類は具体値で固定する（ホストがハンドシェイク前に EOF）。
            assert_eq!(got.0.first().map(String::as_str), Some("ExitedBeforeHello"));
            assert_no_downgrade(got, 0, "ExitedBeforeHello", BREAKER_THRESHOLD);
        }
    }

    /// RENDER-3・SEC-2（#1498・P2-2）: bwrap のパスが無い（起動できない）場合は `SpawnFailed` で、
    /// 隔離なしには落ちない。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_bwrap_missing_at_spawn_never_downgrades_to_unisolated() {
        for mode in [
            EgressIsolationMode::BestEffort,
            EgressIsolationMode::Required,
        ] {
            let got = run_failing_bwrap(mode, None, false);
            assert_eq!(got.0.first().map(String::as_str), Some("SpawnFailed"));
            assert_no_downgrade(got, 0, "SpawnFailed", 0);
        }
    }

    /// RENDER-3・SEC-2（#1498・P2-2）: WorkDir の親の tmpfs（`--tmpfs <親>`）だけが実行時に失敗しても、
    /// 降格せず失敗として数える（親はプローブに含まれないため実起動で初めて失敗しうる）。偽 bwrap は
    /// 親の tmpfs が引数にあるときだけ失敗し、無ければ印（`noparent`）を残して区別する。
    #[cfg(target_os = "linux")]
    #[test]
    fn render3_parent_tmpfs_failure_never_downgrades_to_unisolated() {
        let body = "#!/bin/sh\necho x >> @COUNT@\nprev=\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--tmpfs\" ] && [ \"$a\" = \"@PARENT@\" ]; then\n    echo \"bwrap: Can't mount tmpfs on /newroot@PARENT@\" >&2\n    sleep 0.1\n    exit 1\n  fi\n  prev=\"$a\"\ndone\ntouch @NOPARENT@\nexit 0\n";
        for mode in [
            EgressIsolationMode::BestEffort,
            EgressIsolationMode::Required,
        ] {
            let got = run_failing_bwrap(mode, Some(body), true);
            assert_eq!(got.0.first().map(String::as_str), Some("ExitedBeforeHello"));
            assert_no_downgrade(got, 1, "ExitedBeforeHello", BREAKER_THRESHOLD);
        }
    }
}

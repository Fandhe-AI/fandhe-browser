//! egress プロキシの待ち受け・接続処理・トンネル転送（Unix 専用。RENDER-3・SEC-2・TASK-38・#1434）。
//!
//! [`super::EgressProxy::start`] から呼ばれる。accept スレッド 1 本が接続を受け、同時接続数の
//! 枠（RAII ガード）を取れた接続だけハンドラスレッドへ渡す。枠が無い接続はスレッドを作らず
//! accept スレッド上で 503 を返して閉じる（無制限にスレッドが増える経路を作らない）。
//! ハンドラは要求解析（[`super::request`]）→ 解決・全 IP 検査 → 検査済み `SocketAddr` への接続
//! → 200 応答 → 双方向転送の順に進む。
//!
//! 枠の考え方（#1444）: 同時接続数の枠はトンネル確立後も握り続け、アイドルは
//! [`super::IDLE_TIMEOUT`] で有界にする。ハンドシェイク中との別枠にはしない（servo-host の
//! 中継が両者を区別できず、上限を揃えられないため）。名前解決スレッドは別に
//! [`super::MAX_RESOLVER_THREADS`] で有界。

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::counters::{Counters, Event};
use super::request::{ConnectTarget, Host, Reject, parse_connect, read_request_head};
use super::{
    BoundEndpoint, CoreAddressPolicy, EgressAddressPolicy, EgressEndpoint, EgressProxyConfig,
    EgressProxyError, EgressProxyLimits, EgressProxyStats, EgressResolver, MAX_CONNECT_ATTEMPTS,
    MAX_RESOLVER_THREADS, SystemResolver,
};

/// UDS のソケットファイル名（固定。外部文字列をパスに入れない）。
const SOCKET_NAME: &str = "egress-proxy.sock";
/// accept ポーリング間隔（停止フラグを見る周期）。
const ACCEPT_POLL: Duration = Duration::from_millis(25);
/// 枠が無いときの 503 応答の書き込み timeout（accept スレッドを塞がない）。
const REJECT_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
/// エラー応答の書き込み timeout。
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// 転送バッファ長。
const PUMP_BUF: usize = 16 * 1024;

/// UDS と TCP を同じ型で扱うための接続。
enum Conn {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Conn {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Unix(s) => s.try_clone().map(Self::Unix),
            Self::Tcp(s) => s.try_clone().map(Self::Tcp),
        }
    }
    fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.set_read_timeout(d),
            Self::Tcp(s) => s.set_read_timeout(d),
        }
    }
    fn set_write_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.set_write_timeout(d),
            Self::Tcp(s) => s.set_write_timeout(d),
        }
    }
    fn set_nonblocking(&self, v: bool) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.set_nonblocking(v),
            Self::Tcp(s) => s.set_nonblocking(v),
        }
    }
    fn shutdown(&self, how: Shutdown) {
        // 既に閉じている相手への shutdown の失敗は無視してよい（終了処理のみ）。
        let _ = match self {
            Self::Unix(s) => s.shutdown(how),
            Self::Tcp(s) => s.shutdown(how),
        };
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.read(buf),
            Self::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    fn accept(&self) -> io::Result<Conn> {
        match self {
            Self::Unix(l) => l.accept().map(|(s, _)| Conn::Unix(s)),
            Self::Tcp(l) => l.accept().map(|(s, _)| Conn::Tcp(s)),
        }
    }
}

/// 全スレッドが共有する状態。
struct Shared {
    limits: EgressProxyLimits,
    policy: Arc<dyn EgressAddressPolicy>,
    resolver: Arc<dyn EgressResolver>,
    counters: Counters,
    stop: AtomicBool,
    active: AtomicUsize,
    /// 実行中（期限後に切り離されたものを含む）の名前解決スレッド数。
    resolving: AtomicUsize,
}

/// 名前解決スレッドの枠（スレッド終了時に返す）。
struct ResolverSlot(Arc<Shared>);

impl ResolverSlot {
    fn acquire(shared: &Arc<Shared>) -> Option<Self> {
        let prev = shared.resolving.fetch_add(1, Ordering::AcqRel);
        if prev >= MAX_RESOLVER_THREADS {
            shared.resolving.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(shared)))
    }
}

impl Drop for ResolverSlot {
    fn drop(&mut self) {
        self.0.resolving.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 期限つき名前解決の結果。
enum Resolved {
    Addrs(Vec<SocketAddr>),
    /// 解決失敗・空応答・期限超過・解決スレッドの枠不足（502）。
    Failed,
    /// プロキシ停止で中断（503）。
    Stopped,
}

/// 別スレッドで解決し、`connect_timeout` を期限として tick ごとに停止フラグを見て待つ
/// （getaddrinfo 自体は中断できないため、期限後はスレッドを切り離し、残存数を
/// [`MAX_RESOLVER_THREADS`] で有界にする。呼び出し側の接続枠は期限で解放される）。
fn resolve_with_deadline(name: &str, port: u16, shared: &Arc<Shared>) -> Resolved {
    let Some(slot) = ResolverSlot::acquire(shared) else {
        return Resolved::Failed;
    };
    let (tx, rx) = mpsc::channel();
    let resolver = Arc::clone(&shared.resolver);
    let host = name.to_owned();
    let spawned = thread::Builder::new()
        .name("egress-proxy-resolve".to_owned())
        .spawn(move || {
            let _slot = slot;
            // 受信側が期限で去っていると送信は失敗するが、結果は不要なので無視してよい。
            let _ = tx.send(resolver.resolve(&host, port));
        });
    if spawned.is_err() {
        shared.counters.bump(Event::SpawnFailed);
        return Resolved::Failed;
    }
    let started = Instant::now();
    let tick = shared.limits.tick();
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Resolved::Stopped;
        }
        let remaining = shared
            .limits
            .connect_timeout
            .saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Resolved::Failed;
        }
        match rx.recv_timeout(tick.min(remaining)) {
            Ok(Ok(v)) if !v.is_empty() => return Resolved::Addrs(v),
            Ok(_) | Err(RecvTimeoutError::Disconnected) => return Resolved::Failed,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// 同時接続数の枠（Drop で返す。トンネルが終わるまで保持する）。
struct Slot(Arc<Shared>);

impl Slot {
    fn acquire(shared: &Arc<Shared>) -> Option<Self> {
        let prev = shared.active.fetch_add(1, Ordering::AcqRel);
        if prev >= shared.limits.max_connections {
            shared.active.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(shared)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 起動中のプロキシの実体（accept スレッドとソケットファイル）。
pub(super) struct Runtime {
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
    socket_path: Option<PathBuf>,
}

impl Runtime {
    pub(super) fn stats(&self) -> EgressProxyStats {
        self.shared.counters.snapshot()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept.take() {
            // accept スレッドは ACCEPT_POLL ごとに停止フラグを見て抜け、listener を閉じる。
            let _ = handle.join();
        }
        if let Some(path) = self.socket_path.take() {
            remove_socket_file(&path);
        }
    }
}

/// 自分が作ったソケットファイルだけを消す（socket であることを確かめる。任意ファイルの
/// 削除経路を作らない）。
fn remove_socket_file(path: &Path) {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_socket()
    {
        let _ = std::fs::remove_file(path);
    }
}

/// `sun_path` の上限（NUL 込み）から余裕を見た最大長。
fn max_socket_path_len() -> usize {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        107
    } else {
        103
    }
}

/// ソケット用ディレクトリの検査対象の属性（bind 前後の照合用。純関数で検査するため値で持つ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirFacts {
    is_dir: bool,
    mode: u32,
    uid: u32,
    dev: u64,
    ino: u64,
}

impl DirFacts {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            is_dir: meta.file_type().is_dir(),
            mode: meta.mode(),
            uid: meta.uid(),
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

/// bind 前後のディレクトリ属性と、bind したソケットの所有者 uid を照合する（SEC-2・#1444）。
///
/// std に `geteuid` が無い（libc 等は追加しない）ため、bind 直後のソケットはこのプロセスが作った
/// ものという前提で、その uid をプロセスの uid の代わりに使う。ディレクトリの所有者がソケットの
/// uid と違う、bind の間に dev・ino・uid が変わった、ディレクトリでない、group/other に権限がある
/// のいずれかなら拒否する。
fn verify_socket_dir(
    before: DirFacts,
    after: DirFacts,
    socket_uid: u32,
) -> Result<(), EgressProxyError> {
    let ok = before.is_dir
        && after.is_dir
        && before.mode & 0o077 == 0
        && after.mode & 0o077 == 0
        && after.uid == socket_uid
        && (before.dev, before.ino, before.uid) == (after.dev, after.ino, after.uid);
    if ok {
        Ok(())
    } else {
        Err(EgressProxyError::InvalidSocketDir)
    }
}

/// `dir` 配下に UDS を bind する。`dir` は呼び出し側が自プロセスで作った 0700 のディレクトリ
/// （[`EgressEndpoint::UnixSocketIn`] の契約）。stat から bind までの競合は bind の前後の
/// 照合（[`verify_socket_dir`]）で縮めるが、`openat` 系なしでは閉じきれず契約を前提にする。
fn bind_unix(dir: &Path) -> Result<(Listener, BoundEndpoint, PathBuf), EgressProxyError> {
    if !dir.is_absolute() {
        return Err(EgressProxyError::InvalidSocketDir);
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|_| EgressProxyError::InvalidSocketDir)?;
    let before = DirFacts::of(&meta);
    if !before.is_dir || before.mode & 0o077 != 0 {
        return Err(EgressProxyError::InvalidSocketDir);
    }
    let path = dir.join(SOCKET_NAME);
    if std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).len() > max_socket_path_len() {
        return Err(EgressProxyError::SocketPathTooLong);
    }
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(EgressProxyError::AlreadyExists);
    }
    // bind 直後は umask 由来の権限だが、親ディレクトリが 0700 のためこの間に他ユーザーは
    // 到達できない。直後に 0600 へ絞る。
    let listener = UnixListener::bind(&path).map_err(EgressProxyError::Bind)?;
    let verified = (|| {
        let sock =
            std::fs::symlink_metadata(&path).map_err(|_| EgressProxyError::InvalidSocketDir)?;
        let after = std::fs::symlink_metadata(dir)
            .map_err(|_| EgressProxyError::InvalidSocketDir)
            .map(|m| DirFacts::of(&m))?;
        verify_socket_dir(before, after, sock.uid())
    })();
    if let Err(e) = verified {
        drop(listener);
        remove_socket_file(&path);
        return Err(e);
    }
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        remove_socket_file(&path);
        return Err(EgressProxyError::Bind(e));
    }
    Ok((
        Listener::Unix(listener),
        BoundEndpoint::Unix(path.clone()),
        path,
    ))
}

pub(super) fn start(
    endpoint: EgressEndpoint,
    config: EgressProxyConfig,
) -> Result<(BoundEndpoint, Runtime), EgressProxyError> {
    let (listener, bound, socket_path) = match endpoint {
        EgressEndpoint::UnixSocketIn(dir) => {
            let (l, b, p) = bind_unix(&dir)?;
            (l, b, Some(p))
        }
        EgressEndpoint::LoopbackTcp => {
            // 任意のアドレスは受け取らず 127.0.0.1 固定（外部インターフェースへ公開しない）。
            let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(EgressProxyError::Bind)?;
            let addr = l.local_addr().map_err(EgressProxyError::Bind)?;
            (Listener::Tcp(l), BoundEndpoint::Tcp(addr), None)
        }
    };
    let cleanup = |path: &Option<PathBuf>| {
        if let Some(p) = path {
            remove_socket_file(p);
        }
    };
    let nonblocking = match &listener {
        Listener::Unix(l) => l.set_nonblocking(true),
        Listener::Tcp(l) => l.set_nonblocking(true),
    };
    if let Err(e) = nonblocking {
        drop(listener);
        cleanup(&socket_path);
        return Err(EgressProxyError::Bind(e));
    }
    let policy: Arc<dyn EgressAddressPolicy> = config.policy.unwrap_or_else(|| {
        Arc::new(CoreAddressPolicy {
            allow_private_network_access: config.allow_private_network_access,
        })
    });
    let resolver: Arc<dyn EgressResolver> =
        config.resolver.unwrap_or_else(|| Arc::new(SystemResolver));
    let shared = Arc::new(Shared {
        limits: config.limits,
        policy,
        resolver,
        counters: Counters::default(),
        stop: AtomicBool::new(false),
        active: AtomicUsize::new(0),
        resolving: AtomicUsize::new(0),
    });
    let thread_shared = Arc::clone(&shared);
    let spawned = thread::Builder::new()
        .name("egress-proxy-accept".to_owned())
        .spawn(move || accept_loop(listener, thread_shared));
    match spawned {
        Ok(handle) => Ok((
            bound,
            Runtime {
                shared,
                accept: Some(handle),
                socket_path,
            },
        )),
        Err(e) => {
            // listener はクロージャごと破棄済み。
            cleanup(&socket_path);
            Err(EgressProxyError::Spawn(e))
        }
    }
}

fn accept_loop(listener: Listener, shared: Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok(conn) => dispatch(conn, &shared),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // WouldBlock のほか、fd 枯渇等の一時エラーでも busy loop しないよう待つ。
            Err(_) => thread::sleep(ACCEPT_POLL),
        }
    }
}

fn dispatch(mut conn: Conn, shared: &Arc<Shared>) {
    shared.counters.bump(Event::Accepted);
    // BSD 系では accept した socket が listener の O_NONBLOCK を継承するため必ず戻す。
    if conn.set_nonblocking(false).is_err() {
        return;
    }
    let Some(slot) = Slot::acquire(shared) else {
        shared.counters.bump(Event::OverCapacity);
        let _ = conn.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
        write_status(&mut conn, 503, "Service Unavailable");
        return;
    };
    let handler_shared = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("egress-proxy-conn".to_owned())
        .spawn(move || handle(conn, slot, &handler_shared));
    if spawned.is_err() {
        // 閉じたクロージャ内の conn・slot は破棄され、枠も返る。
        shared.counters.bump(Event::SpawnFailed);
    }
}

fn write_status(conn: &mut Conn, code: u16, reason: &str) {
    let msg = format!("HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    let _ = conn.write_all(msg.as_bytes());
    let _ = conn.flush();
    conn.shutdown(Shutdown::Both);
}

fn reject_response(conn: &mut Conn, reject: Reject, shared: &Shared) {
    let (code, reason, event) = match reject {
        Reject::BadRequest => (400, "Bad Request", Some(Event::BadRequest)),
        Reject::MethodNotAllowed => (405, "Method Not Allowed", Some(Event::MethodNotAllowed)),
        Reject::HeaderTooLarge => (
            431,
            "Request Header Fields Too Large",
            Some(Event::HeaderTooLarge),
        ),
        Reject::Deadline => (408, "Request Timeout", Some(Event::HeaderDeadline)),
        Reject::Stopped => (503, "Service Unavailable", None),
    };
    if let Some(event) = event {
        shared.counters.bump(event);
    }
    write_status(conn, code, reason);
}

/// 解決・全 IP 検査・接続。成功なら接続済みの上流を返す。失敗時は応答済みで `None`。
fn connect_upstream(
    target: &ConnectTarget,
    conn: &mut Conn,
    shared: &Arc<Shared>,
) -> Option<TcpStream> {
    let addrs: Vec<SocketAddr> = match &target.host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
        Host::Name(name) => match resolve_with_deadline(name, target.port, shared) {
            Resolved::Addrs(v) => v,
            Resolved::Failed => {
                shared.counters.bump(Event::ResolveFailed);
                write_status(conn, 502, "Bad Gateway");
                return None;
            }
            Resolved::Stopped => {
                write_status(conn, 503, "Service Unavailable");
                return None;
            }
        },
    };
    // 解決した全 IP を検査し、1 つでも拒否対象なら上流へ一切接続しない
    // （DNS リバインディング・混在応答対策。security.md「SSRF」）。
    if addrs.iter().any(|a| !shared.policy.is_allowed(a.ip())) {
        shared.counters.bump(Event::DeniedAddress);
        write_status(conn, 403, "Forbidden");
        return None;
    }
    // 接続は 1 本の専用スレッドが同一ソケットで待ち続け（tick ごとに作り直さない）、
    // 呼び出し側は tick ごとに停止フラグを見る（RENDER-3）。スレッドの寿命は
    // `connect_timeout` の総期限で有界（停止時は結果を捨てて切り離す）。
    let total = shared.limits.connect_timeout;
    let started = Instant::now();
    let tick = shared.limits.tick();
    let (tx, rx) = mpsc::channel();
    let targets: Vec<SocketAddr> = addrs.iter().take(MAX_CONNECT_ATTEMPTS).copied().collect();
    let spawned = thread::Builder::new()
        .name("egress-proxy-connect".to_owned())
        .spawn(move || {
            for addr in targets {
                let remaining = total.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    break;
                }
                // 検査済みの SocketAddr へそのまま接続する（名前を引き直さない）。
                if let Ok(s) = TcpStream::connect_timeout(&addr, remaining) {
                    // 受信側が去っていれば s は破棄される。
                    let _ = tx.send(s);
                    return;
                }
            }
        });
    if spawned.is_err() {
        shared.counters.bump(Event::SpawnFailed);
        write_status(conn, 502, "Bad Gateway");
        return None;
    }
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            write_status(conn, 503, "Service Unavailable");
            return None;
        }
        match rx.recv_timeout(tick) {
            Ok(s) => return Some(s),
            Err(RecvTimeoutError::Timeout) => {}
            // 全アドレス失敗または期限切れでスレッドが終了した。
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    shared.counters.bump(Event::UpstreamFailed);
    write_status(conn, 502, "Bad Gateway");
    None
}

fn handle(mut conn: Conn, slot: Slot, shared: &Arc<Shared>) {
    // slot はトンネルが終わるまで保持する。
    let _slot = slot;
    let limits = shared.limits;
    let deadline = Instant::now() + limits.header_deadline;
    let tick = limits.tick();
    let head = {
        let reader = match conn.try_clone() {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut reader = reader;
        let ctl = match conn.try_clone() {
            Ok(c) => c,
            Err(_) => return,
        };
        read_request_head(
            &mut reader,
            &mut |d| ctl.set_read_timeout(Some(d)),
            deadline,
            tick,
            limits.max_header_bytes,
            &shared.stop,
        )
    };
    let _ = conn.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT));
    let target = match head.and_then(|h| parse_connect(&h, limits.max_header_bytes)) {
        Ok(t) => t,
        Err(reject) => return reject_response(&mut conn, reject, shared),
    };
    let Some(upstream) = connect_upstream(&target, &mut conn, shared) else {
        return;
    };
    if conn
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .is_err()
    {
        return;
    }
    shared.counters.bump(Event::Allowed);
    tunnel(conn, Conn::Tcp(upstream), shared);
}

/// 双方向の転送が共有するアイドル判定の状態。
struct TunnelCtx {
    started: Instant,
    last_activity_ms: AtomicU64,
    idle_counted: AtomicBool,
    idle_limit: Duration,
}

impl TunnelCtx {
    fn touch(&self) {
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_activity_ms.store(ms, Ordering::Relaxed);
    }
    fn idle_exceeded(&self) -> bool {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        now.saturating_sub(last) >= u64::try_from(self.idle_limit.as_millis()).unwrap_or(u64::MAX)
    }
}

/// 確立後のバイト列を改変せず双方向に転送する（SEC-2。ヘッダの付加・書き換えをしない）。
fn tunnel(client: Conn, upstream: Conn, shared: &Arc<Shared>) {
    let tick = shared.limits.tick();
    let ctx = Arc::new(TunnelCtx {
        started: Instant::now(),
        last_activity_ms: AtomicU64::new(0),
        idle_counted: AtomicBool::new(false),
        idle_limit: shared.limits.idle_timeout,
    });
    // 書き込みも tick で打ち切り、`write_all_checked` が停止フラグとアイドルを見る。
    // （長い timeout だと、相手が読まない間は停止を確認できない。）
    let setup = |c: &Conn| {
        c.set_read_timeout(Some(tick))?;
        c.set_write_timeout(Some(tick))
    };
    if setup(&client).is_err() || setup(&upstream).is_err() {
        return;
    }
    let (Ok(client_w), Ok(upstream_w)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    // 上流→クライアントは追加スレッド 1 本、クライアント→上流はこのスレッドで転送する。
    let helper_shared = Arc::clone(shared);
    let helper_ctx = Arc::clone(&ctx);
    let helper = thread::Builder::new()
        .name("egress-proxy-pump".to_owned())
        .spawn(move || pump(upstream, client_w, &helper_ctx, &helper_shared));
    match helper {
        Ok(handle) => {
            pump(client, upstream_w, &ctx, shared);
            let _ = handle.join();
        }
        Err(_) => {
            shared.counters.bump(Event::SpawnFailed);
            client.shutdown(Shutdown::Both);
            upstream_w.shutdown(Shutdown::Both);
        }
    }
}

/// `reader` から読んで `writer` へ書く片方向の転送。
///
/// 読み取り timeout は tick 周期にし、timeout のたびに停止フラグと「双方向の無通信時間」を見る。
/// 片方向ごとの timeout をアイドル上限にすると、片方向だけが流れる長いダウンロードを切って
/// しまうため、最終活動時刻を両方向で共有する。
fn pump(mut reader: Conn, mut writer: Conn, ctx: &TunnelCtx, shared: &Shared) {
    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break abort(&reader, &writer);
        }
        match reader.read(&mut buf) {
            // EOF: 反対側へ書き込み側のハーフクローズを伝え、もう一方向の終了を待つ。
            Ok(0) => break writer.shutdown(Shutdown::Write),
            Ok(n) => {
                ctx.touch();
                let Some(chunk) = buf.get(..n) else {
                    break abort(&reader, &writer);
                };
                if write_all_checked(&mut writer, chunk, ctx, shared).is_err() {
                    break abort(&reader, &writer);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if ctx.idle_exceeded() {
                    if !ctx.idle_counted.swap(true, Ordering::Relaxed) {
                        shared.counters.bump(Event::IdleTimeout);
                    }
                    break abort(&reader, &writer);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break abort(&reader, &writer),
        }
    }
}

/// 書き込み timeout（tick）のたびに停止フラグと無通信時間を見ながら全量を書く。
/// 相手が読み取りを止めても、停止要求・アイドル上限で抜けられる。
fn write_all_checked(
    writer: &mut Conn,
    mut data: &[u8],
    ctx: &TunnelCtx,
    shared: &Shared,
) -> io::Result<()> {
    while !data.is_empty() {
        if shared.stop.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        match writer.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                ctx.touch();
                data = data.get(n..).unwrap_or(&[]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if ctx.idle_exceeded() {
                    if !ctx.idle_counted.swap(true, Ordering::Relaxed) {
                        shared.counters.bump(Event::IdleTimeout);
                    }
                    return Err(io::ErrorKind::TimedOut.into());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// 両ソケットを閉じ、反対方向のスレッドの read も起こす。
fn abort(a: &Conn, b: &Conn) {
    a.shutdown(Shutdown::Both);
    b.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> DirFacts {
        DirFacts {
            is_dir: true,
            mode: 0o040_700,
            uid: 1000,
            dev: 7,
            ino: 42,
        }
    }

    /// SEC-2（#1444）: すべて一致すれば通る。
    #[test]
    fn sec_2_socket_dir_matching_facts_is_accepted() {
        assert!(verify_socket_dir(facts(), facts(), 1000).is_ok());
    }

    /// SEC-2（#1444）: ディレクトリの所有者がソケットの uid（自プロセス）と違えば拒否する。
    #[test]
    fn sec_2_socket_dir_owner_mismatch_is_rejected() {
        let other = DirFacts { uid: 0, ..facts() };
        let r = verify_socket_dir(other, other, 1000);
        assert!(matches!(r, Err(EgressProxyError::InvalidSocketDir)));
    }

    /// SEC-2（#1444）: bind の間に dev・ino が変わった、または権限が緩んだら拒否する。
    #[test]
    fn sec_2_socket_dir_swapped_during_bind_is_rejected() {
        for after in [
            DirFacts { ino: 43, ..facts() },
            DirFacts { dev: 8, ..facts() },
            DirFacts {
                mode: 0o040_755,
                ..facts()
            },
            DirFacts {
                is_dir: false,
                ..facts()
            },
        ] {
            let r = verify_socket_dir(facts(), after, 1000);
            assert!(
                matches!(r, Err(EgressProxyError::InvalidSocketDir)),
                "{after:?}"
            );
        }
    }
}

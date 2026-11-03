//! netns 内の `127.0.0.1` から本体の CONNECT プロキシ（UDS）へバイト列を転送する中継（Linux 向け）。
//!
//! 役割: bwrap `--unshare-net` の netns 内では外へ出られないため、Servo の proxy pref
//! （`http://127.0.0.1:<中継のポート>`）の宛先としてここで待ち受け、各接続を `--egress-relay-uds`
//! で渡された UDS 1 本へ双方向に転送する（RENDER-3・SEC-2・TASK-38・#1435。設計
//! `servo-host-egress-isolation.md` §8.1。Servo なしでの成立は #1431 の実測）。
//! 呼び出し元は `main` の `--stdio`（`serve` の前に [`start`]）。
//!
//! 前提（#1444）: `--egress-relay-uds` は #1436 が `bwrap --unshare-net` で包むときだけ渡す。
//! 隔離なしで渡すとホストの `127.0.0.1` に中継が開き、同一ホストの他プロセスから本体の
//! プロキシへ届き得る。実行時の netns 判定は入れていない（入れる場合は別途検討）。
//!
//! 不変条件:
//! - 宛先は UDS 1 本固定で、CONNECT の内容は解釈・改変しない。宛先の許可判定は本体のプロキシ
//!   （#1434）の責務で、この中継は開放プロキシにならない。中継が自分で出すバイト列は、固定の 503 応答
//!   だけ（同時接続数の上限超過、および本体プロキシへの dial の失敗・タイムアウト。転送する内容への
//!   介入ではない。#1450）。
//! - `127.0.0.1` にだけ bind する。外部入力経路で `unwrap`/`expect`/添字を使わない。
//! - 同時接続数・無通信時間に上限を持つ。上限値は本体プロキシ（`fandhe_browser_render::egress_proxy`
//!   の `MAX_CONCURRENT_CONNECTIONS`・`IDLE_TIMEOUT`・`CONNECT_TIMEOUT`）と揃える
//!   （別 workspace のため import せず定数を複製。変えるときは両方を同時に変える）。

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::num::NonZeroU16;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 同時に転送する接続数の上限。本体の `egress_proxy::MAX_CONCURRENT_CONNECTIONS` の複製
/// （変えるときは両方を同時に変える。根拠は本体側の doc。一致は `scripts/check-egress-constants.sh` が検査）。
const MAX_CONNECTIONS: usize = 128;
/// 双方向とも無通信が続いたら切る時間。本体の `egress_proxy::IDLE_TIMEOUT` の複製
/// （変えるときは両方を同時に変える。根拠は本体側の doc）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// UDS への接続待ちの上限。本体の `egress_proxy::CONNECT_TIMEOUT` と同値の複製。本体プロキシが
/// accept を止めても枠を返却するための期限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// timeout 後も UDS 接続でブロックして残り得る dial スレッドの同時数の上限。接続枠（`active`）とは
/// 別で、本体の `egress_proxy::MAX_RESOLVER_THREADS` と同じ考え方（#1444）。
const MAX_DIAL_THREADS: usize = 32;
/// 503 を書く際の timeout（本体の `REJECT_WRITE_TIMEOUT` と同値の複製。値の一致は
/// `scripts/check-egress-constants.sh` が検査する。#1450）。
const REJECT_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
/// 上限超過、および本体プロキシへの dial の失敗・タイムアウトで返す固定の応答（RENDER-3・SEC-2・#1450）。
const UNAVAILABLE_RESPONSE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
/// 読み取りタイムアウト。アイドル判定の刻みとして使う。
const TICK: Duration = Duration::from_millis(250);
/// 転送バッファ（固定長。無制限確保をしない）。
const BUF_SIZE: usize = 16 * 1024;

/// 中継の上限。本番は既定値。テストだけが既定以下へ締められる（引数・環境変数からは変えない）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct RelayLimits {
    pub(crate) max_connections: usize,
    pub(crate) idle_timeout: Duration,
    pub(crate) tick: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_dial_threads: usize,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            idle_timeout: IDLE_TIMEOUT,
            tick: TICK,
            connect_timeout: CONNECT_TIMEOUT,
            max_dial_threads: MAX_DIAL_THREADS,
        }
    }
}

impl RelayLimits {
    /// 既定値を超えないように丸める。
    fn clamped(self) -> Self {
        Self {
            max_connections: self.max_connections.min(MAX_CONNECTIONS),
            idle_timeout: self.idle_timeout.min(IDLE_TIMEOUT),
            tick: self.tick.clamp(Duration::from_millis(1), TICK),
            connect_timeout: self.connect_timeout.min(CONNECT_TIMEOUT),
            max_dial_threads: self.max_dial_threads.min(MAX_DIAL_THREADS),
        }
    }
}

/// 起動済みの中継。プロセス終了まで保持する（accept スレッドは join しない）。
pub(crate) struct Relay {
    port: NonZeroU16,
    /// 転送中の接続数。テストだけが読む。
    #[cfg(test)]
    active: Arc<AtomicUsize>,
}

impl Relay {
    /// 中継が待ち受ける `127.0.0.1` のポート。
    pub(crate) fn port(&self) -> NonZeroU16 {
        self.port
    }

    /// 現在転送中の接続数（テスト用）。
    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

/// 既定の上限で中継を起動する。`uds` を渡す呼び出しは、`bwrap --unshare-net` 等の隔離で包む
/// ときに限る前提（モジュール doc 参照。#1444）。
pub(crate) fn start(uds: PathBuf) -> io::Result<Relay> {
    start_with(uds, RelayLimits::default())
}

/// 上限つきで中継を起動する。bind・スレッド生成の失敗は `Err`（呼び出し側が fail-closed で終了する）。
pub(crate) fn start_with(uds: PathBuf, limits: RelayLimits) -> io::Result<Relay> {
    let limits = limits.clamped();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener
        .local_addr()
        .ok()
        .and_then(|a| NonZeroU16::new(a.port()))
        .ok_or_else(|| io::Error::other("cannot determine relay port"))?;
    let active = Arc::new(AtomicUsize::new(0));
    let accept_active = Arc::clone(&active);
    let dialing = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("egress-relay-accept".to_string())
        .spawn(move || accept_loop(&listener, &uds, limits, &accept_active, &dialing))?;
    Ok(Relay {
        port,
        #[cfg(test)]
        active,
    })
}

fn accept_loop(
    listener: &TcpListener,
    uds: &Path,
    limits: RelayLimits,
    active: &Arc<AtomicUsize>,
    dialing: &Arc<AtomicUsize>,
) {
    for incoming in listener.incoming() {
        let Ok(mut tcp) = incoming else {
            // EMFILE 等でのビジーループを避ける。
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        // 上限超過は固定の 503 を短い timeout で書いて閉じる（accept スレッドを塞がない）。
        // accept スレッドだけが増やすため判定と加算は競合しない。
        if active.load(Ordering::SeqCst) >= limits.max_connections {
            reject_unavailable(&mut tcp);
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let guard = Guard(Arc::clone(active));
        let path = uds.to_path_buf();
        let dialing = Arc::clone(dialing);
        // spawn 失敗時はクロージャごと drop され、guard が減算し tcp が閉じる。
        let _ = std::thread::Builder::new()
            .name("egress-relay-conn".to_string())
            .spawn(move || handle(tcp, &path, limits, guard, &dialing, spawn_up_thread));
    }
}

/// 固定の 503 を短い timeout で書く（相手が読まなくても呼び出しスレッドを塞がない）。
/// 上限超過（accept スレッド）と dial の失敗・タイムアウト（接続スレッド）で共用する。
/// 受信内容は解釈しないため、未読データがあると close が RST になり得るが、それは許容する。
fn reject_unavailable(tcp: &mut TcpStream) {
    let _ = tcp.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
    let _ = tcp.write_all(UNAVAILABLE_RESPONSE);
}

/// 上りの転送スレッドの生成関数。本番は [`spawn_up_thread`]。生成失敗の経路をテストで通すため
/// `handle` の引数にしている（`cfg(test)` 分岐を作らず本番と同じ経路を通す）。
type SpawnUp = fn(Box<dyn FnOnce() + Send + 'static>) -> io::Result<std::thread::JoinHandle<()>>;

/// 本番の [`SpawnUp`]。名前つきスレッドを生成する。
fn spawn_up_thread(
    f: Box<dyn FnOnce() + Send + 'static>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("egress-relay-up".to_string())
        .spawn(f)
}

/// 接続の終了時に `active` を 1 回だけ減らす。
struct Guard(Arc<AtomicUsize>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 枠（dial スレッド数）を原子的に取得する。上限未満のときだけ加算し（判定と加算を `try_update`（旧 fetch_update）で
/// 不可分にして並行取得でも上限を超えない）、上限到達時は枠を取らず `None` を返す。
/// 枠の返却は戻り値 `Guard` の drop で全経路（成功・失敗・早期 return）一律に行う。
fn try_acquire(counter: &Arc<AtomicUsize>, max: usize) -> Option<Guard> {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < max).then_some(n + 1)
        })
        .ok()?;
    Some(Guard(Arc::clone(counter)))
}

/// UDS へ期限つきで接続する。std の `UnixStream::connect` はブロッキングで、相手の accept
/// キューが満杯だと戻らないため、接続は別スレッドで行い `connect_timeout` で待ちを切る。
///
/// 枠は 2 種類あり、互いに独立（#1444）:
/// - 接続枠（`active`）: timeout 時点で呼び出し元の `handle` が戻り、その `Guard` の drop で一度だけ
///   返る。dial スレッドが後で終わっても `active` には触れない（二重に返さない）。
/// - dial スレッド枠（`dialing`）: timeout 後もブロックして残り得る dial スレッドの数を
///   `max_dial_threads` までに抑える（無制限増殖の防止）。dial スレッドの終了時に返る。
///   枠が尽きていたらスレッドを作らず即 `None`（待たせない）で、呼び出し元が接続枠を返す。
fn connect_uds(uds: &Path, limits: RelayLimits, dialing: &Arc<AtomicUsize>) -> Option<UnixStream> {
    let guard = try_acquire(dialing, limits.max_dial_threads)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let path = uds.to_path_buf();
    std::thread::Builder::new()
        .name("egress-relay-dial".to_string())
        .spawn(move || {
            let _guard = guard;
            let _ = tx.send(UnixStream::connect(path));
        })
        .ok()?;
    rx.recv_timeout(limits.connect_timeout).ok()?.ok()
}

/// 1 接続の転送。`accept_loop` の接続スレッドから呼ばれる。dial の失敗・タイムアウトでは固定の
/// 503 を返して閉じる。上りスレッドの生成に失敗したときは、下りだけが滞留しないよう両端を
/// 即座に閉じて接続枠を返す（RENDER-3・SEC-2・#1450）。
fn handle(
    mut tcp: TcpStream,
    uds: &Path,
    limits: RelayLimits,
    _guard: Guard,
    dialing: &Arc<AtomicUsize>,
    spawn_up: SpawnUp,
) {
    let Some(unix) = connect_uds(uds, limits, dialing) else {
        reject_unavailable(&mut tcp);
        return;
    };
    let _ = tcp.set_nodelay(true);
    let last = Arc::new(Mutex::new(Instant::now()));
    let (Ok(tcp_r), Ok(unix_r)) = (tcp.try_clone(), unix.try_clone()) else {
        let _ = tcp.shutdown(Shutdown::Both);
        let _ = unix.shutdown(Shutdown::Both);
        return;
    };
    let up_last = Arc::clone(&last);
    let Ok(forward_up) = spawn_up(Box::new(move || {
        pump(PumpEnd::Tcp(tcp_r), PumpEnd::Unix(unix), &up_last, limits);
    })) else {
        // 失敗したクロージャの drop で閉じるのは clone 側の fd だけなので、元の fd を明示的に閉じる。
        let _ = tcp.shutdown(Shutdown::Both);
        let _ = unix_r.shutdown(Shutdown::Both);
        return;
    };
    pump(PumpEnd::Unix(unix_r), PumpEnd::Tcp(tcp), &last, limits);
    let _ = forward_up.join();
}

/// TCP・UDS のどちらでも同じ転送処理を使うための薄い列挙。
enum PumpEnd {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl PumpEnd {
    fn configure(&self, limits: RelayLimits) {
        match self {
            Self::Tcp(s) => {
                let _ = s.set_read_timeout(Some(limits.tick));
                let _ = s.set_write_timeout(Some(limits.idle_timeout));
            }
            Self::Unix(s) => {
                let _ = s.set_read_timeout(Some(limits.tick));
                let _ = s.set_write_timeout(Some(limits.idle_timeout));
            }
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Unix(s) => s.read(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.write_all(buf),
            Self::Unix(s) => s.write_all(buf),
        }
    }

    fn shutdown(&self, how: Shutdown) {
        let _ = match self {
            Self::Tcp(s) => s.shutdown(how),
            Self::Unix(s) => s.shutdown(how),
        };
    }
}

/// `from` を読み `to` へ書く（片方向）。EOF は `to` の書き込み側だけ閉じて半閉じを伝え、
/// エラー・アイドル超過は両端を閉じて逆方向のスレッドも終わらせる。
fn pump(mut from: PumpEnd, mut to: PumpEnd, last: &Mutex<Instant>, limits: RelayLimits) {
    from.configure(limits);
    to.configure(limits);
    let mut buf = [0u8; BUF_SIZE];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                to.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                let Some(chunk) = buf.get(..n) else { break };
                if to.write_all(chunk).is_err() {
                    break;
                }
                touch(last);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if idle_for(last) >= limits.idle_timeout {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    from.shutdown(Shutdown::Both);
    to.shutdown(Shutdown::Both);
}

fn touch(last: &Mutex<Instant>) {
    if let Ok(mut t) = last.lock() {
        *t = Instant::now();
    }
}

/// 最後の通信からの経過。ロックが壊れていたら打ち切り側（最大）に倒す。
fn idle_for(last: &Mutex<Instant>) -> Duration {
    last.lock().map_or(Duration::MAX, |t| t.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 接続待ち枠は上限ちょうど（127 -> 128）まで取れ、上限到達後は取れず、返却で再取得できる。
    #[test]
    fn try_acquire_stops_at_limit_and_releases_on_drop() {
        let c = Arc::new(AtomicUsize::new(127));
        let g = try_acquire(&c, 128).expect("127 -> 128 は取得できる");
        assert_eq!(c.load(Ordering::SeqCst), 128);
        assert!(try_acquire(&c, 128).is_none());
        assert_eq!(c.load(Ordering::SeqCst), 128);
        drop(g);
        assert_eq!(c.load(Ordering::SeqCst), 127);
        assert!(try_acquire(&c, 128).is_some());
    }

    /// 並行取得でも成功数が上限を超えない。
    #[test]
    fn try_acquire_concurrent_never_exceeds_limit() {
        let c = Arc::new(AtomicUsize::new(0));
        let guards: Vec<_> = (0..16)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || try_acquire(&c, 4))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(guards.len(), 4);
        assert_eq!(c.load(Ordering::SeqCst), 4);
    }

    /// 一意な 0700 ディレクトリ（Drop で削除）。UDS パス長が上限内であることを確かめる。
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            let n = SEQ.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("fbr{}-{n}", std::process::id()));
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            Self(dir)
        }
        fn sock(&self) -> PathBuf {
            let p = self.0.join("p.sock");
            assert!(p.as_os_str().len() <= crate::args::UDS_PATH_MAX_BYTES);
            p
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 受理数を数えるエコーサーバー。
    fn echo_server(path: &Path) -> Arc<AtomicUsize> {
        let listener = UnixListener::bind(path).unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                count.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut s = s;
                    let mut buf = [0u8; 1024];
                    loop {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => {
                                let _ = s.shutdown(Shutdown::Write);
                                return;
                            }
                            Ok(n) => {
                                if s.write_all(&buf[..n]).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        accepted
    }

    fn connect(relay: &Relay) -> TcpStream {
        let s = TcpStream::connect((Ipv4Addr::LOCALHOST, relay.port().get())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s
    }

    fn roundtrip(s: &mut TcpStream, msg: &[u8]) {
        s.write_all(msg).unwrap();
        let mut got = vec![0u8; msg.len()];
        s.read_exact(&mut got).unwrap();
        assert_eq!(got, msg);
    }

    /// EOF まで読み、固定の 503 とバイト単位で一致することを確かめる（クライアントは何も送らない）。
    fn assert_unavailable_then_eof(c: &mut TcpStream) {
        let mut got = Vec::new();
        let _ = c.read_to_end(&mut got);
        assert_eq!(got, UNAVAILABLE_RESPONSE);
    }

    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// RENDER-3・SEC-2（#1435）: TCP→UDS の往復と半閉じの伝搬。
    #[test]
    fn roundtrip_and_half_close() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        echo_server(&sock);
        let relay = start(sock).unwrap();
        let mut c = connect(&relay);
        roundtrip(&mut c, b"ping");
        c.shutdown(Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        assert_eq!(c.read_to_end(&mut rest).unwrap(), 0);
        assert!(wait_until(|| relay.active() == 0));
    }

    /// 同時接続数の上限: 超過分は閉じられ、UDS 側には届かない。解放後は再び受け付ける。
    #[test]
    fn connection_limit_is_enforced() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        let accepted = echo_server(&sock);
        let limits = RelayLimits {
            max_connections: 2,
            ..RelayLimits::default()
        };
        let relay = start_with(sock, limits).unwrap();
        let mut a = connect(&relay);
        let mut b = connect(&relay);
        roundtrip(&mut a, b"a");
        roundtrip(&mut b, b"b");
        assert_eq!(relay.active(), 2);
        let mut c = connect(&relay);
        let mut got = Vec::new();
        let _ = c.read_to_end(&mut got);
        assert_eq!(got, UNAVAILABLE_RESPONSE);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        drop(a);
        assert!(wait_until(|| relay.active() == 1));
        let mut d = connect(&relay);
        roundtrip(&mut d, b"d");
    }

    /// 無通信の接続はアイドル時間で切られ、`active` が 0 に戻る。
    #[test]
    fn idle_connection_is_closed() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        echo_server(&sock);
        let limits = RelayLimits {
            idle_timeout: Duration::from_millis(300),
            tick: Duration::from_millis(50),
            ..RelayLimits::default()
        };
        let relay = start_with(sock, limits).unwrap();
        let mut c = connect(&relay);
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
        assert!(wait_until(|| relay.active() == 0));
    }

    /// UDS 側から一方向に流れ続ける接続はアイドル時間を超えても切られない。
    #[test]
    fn one_way_traffic_is_not_cut() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                for _ in 0..12 {
                    if s.write_all(b"x").is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        let limits = RelayLimits {
            idle_timeout: Duration::from_millis(400),
            tick: Duration::from_millis(50),
            ..RelayLimits::default()
        };
        let relay = start_with(sock, limits).unwrap();
        let mut c = connect(&relay);
        let mut got = [0u8; 12];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"xxxxxxxxxxxx");
    }

    /// RENDER-3・SEC-2（#1450）: UDS が存在しなくても panic せず固定の 503 を返して閉じ、
    /// `active` が 0 に戻る。
    #[test]
    fn render_3_missing_uds_returns_503() {
        let dir = TmpDir::new();
        let relay = start(dir.sock()).unwrap();
        let mut c = connect(&relay);
        assert_unavailable_then_eof(&mut c);
        assert!(wait_until(|| relay.active() == 0));
    }

    /// RENDER-3・SEC-2（#1450）: UDS の accept キューが満杯でも接続待ちは `connect_timeout` で切れ、
    /// 固定の 503 を返して枠が返却される。
    #[test]
    fn render_3_stuck_uds_connect_times_out_with_503_and_releases_slot() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        // accept しない listener。accept キューが埋まって connect が戻らなくなるまで、
        // フィラースレッドが順に接続して保持する（戻らなくなった時点で進捗が止まる）。
        let _listener = UnixListener::bind(&sock).unwrap();
        let held: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
        let filler_held = Arc::clone(&held);
        let filler_path = sock.clone();
        // 停止フラグと上限で filler の暴走（CPU・fd の占有）を防ぐ。
        let stop = Arc::new(AtomicBool::new(false));
        let filler_stop = Arc::clone(&stop);
        let _filler = std::thread::spawn(move || {
            while !filler_stop.load(Ordering::Relaxed) {
                match UnixStream::connect(&filler_path) {
                    Ok(s) => {
                        let mut h = filler_held.lock().unwrap();
                        if h.len() >= 4096 {
                            break;
                        }
                        h.push(s);
                    }
                    Err(_) => break,
                }
            }
        });
        let mut last = 0;
        let mut stable = 0;
        while stable < 6 {
            std::thread::sleep(Duration::from_millis(100));
            let n = held.lock().unwrap().len();
            stable = if n == last && n > 0 { stable + 1 } else { 0 };
            last = n;
        }
        let limits = RelayLimits {
            connect_timeout: Duration::from_millis(300),
            ..RelayLimits::default()
        };
        let relay = start_with(sock, limits).unwrap();
        let mut c = connect(&relay);
        assert_unavailable_then_eof(&mut c);
        assert!(wait_until(|| relay.active() == 0));
    }

    /// RENDER-3・SEC-2（#1450）: 上りスレッドの生成に失敗したら、アイドル上限（300 秒）を待たず
    /// 即座に両端を閉じ、接続枠を返す。
    #[test]
    fn render_3_spawn_up_failure_closes_both_ends_and_releases_slot() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        let listener = UnixListener::bind(&sock).unwrap();
        let (peer_tx, peer_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                let _ = peer_tx.send(s);
            }
        });
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = tcp_listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (server_side, _) = tcp_listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let active = Arc::new(AtomicUsize::new(1));
        let guard = Guard(Arc::clone(&active));
        let dialing = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_dialing = Arc::clone(&dialing);
        std::thread::spawn(move || {
            handle(
                server_side,
                &sock,
                RelayLimits::default(),
                guard,
                &worker_dialing,
                |_| Err(io::Error::other("injected spawn failure")),
            );
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "handle must return promptly when the up thread cannot be spawned"
        );
        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).unwrap(), 0);
        let mut peer = peer_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // macOS は相手（handle 側）が既に閉じたソケットへの setsockopt が EINVAL
        // （InvalidInput）を返す。ここへ来た時点で相手側は閉じ済みなので、この種別だけ
        // 許容し、それ以外のエラーは従来どおり panic させる。以降の read が EOF（0）を
        // 返すことの検証は維持する。
        if let Err(e) = peer.set_read_timeout(Some(Duration::from_secs(5))) {
            assert_eq!(
                e.kind(),
                io::ErrorKind::InvalidInput,
                "unexpected error: {e}"
            );
        }
        assert_eq!(peer.read(&mut buf).unwrap(), 0);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    /// 上限は既定以下にだけ締められる（dial スレッド枠を含む）。
    #[test]
    fn limits_are_clamped_to_defaults() {
        let l = RelayLimits {
            max_connections: usize::MAX,
            max_dial_threads: usize::MAX,
            ..RelayLimits::default()
        }
        .clamped();
        assert_eq!(l.max_connections, MAX_CONNECTIONS);
        assert_eq!(l.max_dial_threads, MAX_DIAL_THREADS);
    }

    /// RENDER-3・SEC-2（#1444・#1450）: dial スレッド枠が尽きたら待たずに 503 で失敗して接続枠を返し、
    /// dial スレッドが後で終わっても接続枠を二重に返さない（`active` が巻き戻らない）。
    #[test]
    fn dial_thread_cap_fails_fast_with_503_and_active_slot_is_released_once() {
        let dir = TmpDir::new();
        let sock = dir.sock();
        let listener = UnixListener::bind(&sock).unwrap();
        let held: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
        let filler_held = Arc::clone(&held);
        let filler_path = sock.clone();
        // 停止フラグと上限で filler の暴走（CPU・fd の占有）を防ぐ。
        let stop = Arc::new(AtomicBool::new(false));
        let filler_stop = Arc::clone(&stop);
        let filler = std::thread::spawn(move || {
            while !filler_stop.load(Ordering::Relaxed) {
                match UnixStream::connect(&filler_path) {
                    Ok(s) => {
                        let mut h = filler_held.lock().unwrap();
                        if h.len() >= 4096 {
                            break;
                        }
                        h.push(s);
                    }
                    Err(_) => break,
                }
            }
        });
        let mut last = 0;
        let mut stable = 0;
        while stable < 6 {
            std::thread::sleep(Duration::from_millis(100));
            let n = held.lock().unwrap().len();
            stable = if n == last && n > 0 { stable + 1 } else { 0 };
            last = n;
        }
        let limits = RelayLimits {
            connect_timeout: Duration::from_secs(2),
            max_dial_threads: 2,
            ..RelayLimits::default()
        };
        let relay = start_with(sock, limits).unwrap();
        // 2 本は dial スレッドを使い、timeout で閉じられて接続枠が返る。
        for _ in 0..2 {
            let mut c = connect(&relay);
            assert_unavailable_then_eof(&mut c);
        }
        assert!(wait_until(|| relay.active() == 0));
        // 3 本目: dial スレッドは 2 本ともブロックしたままで枠が無く、timeout を待たず閉じられる。
        let started = Instant::now();
        let mut c = connect(&relay);
        assert_unavailable_then_eof(&mut c);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(wait_until(|| relay.active() == 0));
        // 詰まりを解く: 溜めた接続を捨て、listener が accept を始める。dial スレッドが戻っても
        // `active` は 0 のまま（二重返却なら usize が巻き戻る）。
        // filler は止めてから接続を捨て、accept 側も停止フラグで終わらせる。
        // accept を始めてから filler を止める（connect で塞がった filler を解放するため）。
        stop.store(true, Ordering::Relaxed);
        held.lock().unwrap().clear();
        listener.set_nonblocking(true).unwrap();
        let accept_stop = Arc::new(AtomicBool::new(false));
        let acceptor_flag = Arc::clone(&accept_stop);
        let acceptor = std::thread::spawn(move || {
            while !acceptor_flag.load(Ordering::Relaxed) {
                if listener.accept().is_err() {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(relay.active(), 0);
        filler.join().unwrap();
        accept_stop.store(true, Ordering::Relaxed);
        acceptor.join().unwrap();
    }
}

//! netns 内の `127.0.0.1` から本体の CONNECT プロキシ（UDS）へバイト列を転送する中継（Linux 向け）。
//!
//! 役割: bwrap `--unshare-net` の netns 内では外へ出られないため、Servo の proxy pref
//! （`http://127.0.0.1:<中継のポート>`）の宛先としてここで待ち受け、各接続を `--egress-relay-uds`
//! で渡された UDS 1 本へ双方向に転送する（RENDER-3・SEC-2・TASK-38・#1435。設計
//! `servo-host-egress-isolation.md` §8.1。Servo なしでの成立は #1431 の実測）。
//! 呼び出し元は `main` の `--stdio`（`serve` の前に [`start`]）。
//!
//! 不変条件:
//! - 宛先は UDS 1 本固定で、CONNECT の内容は解釈・改変しない。宛先の許可判定は本体のプロキシ
//!   （#1434）の責務で、この中継は開放プロキシにならない。
//! - `127.0.0.1` にだけ bind する。外部入力経路で `unwrap`/`expect`/添字を使わない。
//! - 同時接続数・無通信時間に上限を持つ。上限値は本体プロキシ（#1440）の暫定値に揃える
//!   （別 workspace のため import せず定数を複製。実測後の確定は #1440 と同時に扱う）。

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::num::NonZeroU16;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 同時に転送する接続数の上限（暫定。#1440 の本体プロキシと同値）。
const MAX_CONNECTIONS: usize = 128;
/// 双方向とも無通信が続いたら切る時間（暫定。#1440 と同値）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
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
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_connections: MAX_CONNECTIONS,
            idle_timeout: IDLE_TIMEOUT,
            tick: TICK,
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

/// 既定の上限で中継を起動する。
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
    std::thread::Builder::new()
        .name("egress-relay-accept".to_string())
        .spawn(move || accept_loop(&listener, &uds, limits, &accept_active))?;
    Ok(Relay {
        port,
        #[cfg(test)]
        active,
    })
}

fn accept_loop(listener: &TcpListener, uds: &Path, limits: RelayLimits, active: &Arc<AtomicUsize>) {
    for incoming in listener.incoming() {
        let Ok(tcp) = incoming else {
            // EMFILE 等でのビジーループを避ける。
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        // 上限超過は即クローズ（drop）。accept スレッドだけが増やすため判定と加算は競合しない。
        if active.load(Ordering::SeqCst) >= limits.max_connections {
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let guard = Guard(Arc::clone(active));
        let path = uds.to_path_buf();
        // spawn 失敗時はクロージャごと drop され、guard が減算し tcp が閉じる。
        let _ = std::thread::Builder::new()
            .name("egress-relay-conn".to_string())
            .spawn(move || handle(tcp, &path, limits, guard));
    }
}

/// 接続の終了時に `active` を 1 回だけ減らす。
struct Guard(Arc<AtomicUsize>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle(tcp: TcpStream, uds: &Path, limits: RelayLimits, _guard: Guard) {
    let Ok(unix) = UnixStream::connect(uds) else {
        return;
    };
    let _ = tcp.set_nodelay(true);
    let last = Arc::new(Mutex::new(Instant::now()));
    let (Ok(tcp_r), Ok(unix_r)) = (tcp.try_clone(), unix.try_clone()) else {
        return;
    };
    let forward_up = {
        let last = Arc::clone(&last);
        std::thread::Builder::new()
            .name("egress-relay-up".to_string())
            .spawn(move || pump(PumpEnd::Tcp(tcp_r), PumpEnd::Unix(unix), &last, limits))
    };
    pump(PumpEnd::Unix(unix_r), PumpEnd::Tcp(tcp), &last, limits);
    if let Ok(h) = forward_up {
        let _ = h.join();
    }
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
    use std::sync::atomic::AtomicU32;

    static SEQ: AtomicU32 = AtomicU32::new(0);

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
        let mut buf = [0u8; 1];
        match c.read(&mut buf) {
            Ok(0) | Err(_) => {}
            Ok(n) => panic!("unexpected {n} bytes on over-limit connection"),
        }
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

    /// UDS が存在しなくても panic せず接続を閉じ、`active` が 0 に戻る。
    #[test]
    fn missing_uds_closes_connection() {
        let dir = TmpDir::new();
        let relay = start(dir.sock()).unwrap();
        let mut c = connect(&relay);
        let mut buf = [0u8; 1];
        assert!(matches!(c.read(&mut buf), Ok(0) | Err(_)));
        assert!(wait_until(|| relay.active() == 0));
    }
}

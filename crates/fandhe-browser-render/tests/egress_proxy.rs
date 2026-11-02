//! egress 用 CONNECT プロキシの結合テスト（RENDER-3・SEC-2・CORE-1・TASK-38・#1434）。
//!
//! 相手は `127.0.0.1:0` の echo サーバーで、外部ネットワークへは出ない。上限値は短縮して
//! 実秒（10 s・300 s）を待たない。テスト内の `unwrap` は外部入力の経路ではないため許す。

#[cfg(windows)]
#[test]
fn render_3_windows_is_unsupported() {
    use fandhe_browser_render::egress_proxy::{
        EgressEndpoint, EgressProxy, EgressProxyConfig, EgressProxyError,
    };
    let r = EgressProxy::start(EgressEndpoint::LoopbackTcp, EgressProxyConfig::default());
    assert!(matches!(r, Err(EgressProxyError::UnsupportedPlatform)));
}

#[cfg(unix)]
mod unix_tests {
    use std::io::{Read, Write};
    use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use fandhe_browser_render::egress_proxy::{
        BoundEndpoint, EgressAddressPolicy, EgressEndpoint, EgressProxy, EgressProxyConfig,
        EgressProxyError, EgressProxyLimits, EgressResolver,
    };

    static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// 0700 の短い名前の一時ディレクトリ（macOS の `sun_path` 上限対策）。
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let n = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("efp{}_{n}", std::process::id()));
            std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 受信した接続数を数える echo サーバー。
    struct Echo {
        addr: SocketAddr,
        accepted: Arc<AtomicUsize>,
    }
    fn echo() -> Echo {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        thread::spawn(move || {
            for s in l.incoming().flatten() {
                count.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let mut s = s;
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 || s.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Echo { addr, accepted }
    }

    trait Rw: Read + Write {}
    impl<T: Read + Write> Rw for T {}

    fn open(ep: &BoundEndpoint) -> Box<dyn Rw> {
        let t = Some(Duration::from_secs(5));
        match ep {
            BoundEndpoint::Unix(p) => {
                let s = UnixStream::connect(p).unwrap();
                s.set_read_timeout(t).unwrap();
                Box::new(s)
            }
            BoundEndpoint::Tcp(a) => {
                let s = TcpStream::connect(a).unwrap();
                s.set_read_timeout(t).unwrap();
                Box::new(s)
            }
        }
    }

    /// 応答ヘッダ（`\r\n\r\n` まで）を読む。EOF・エラーならそこまで。
    fn read_head(c: &mut dyn Rw) -> String {
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        while !out.ends_with(b"\r\n\r\n") {
            match c.read(&mut b) {
                Ok(1) => out.push(b[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn connect_req(host: &str, port: u16) -> String {
        format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n")
    }

    fn allow_private() -> EgressProxyConfig {
        EgressProxyConfig::default().with_allow_private_network_access(true)
    }

    fn roundtrip(c: &mut dyn Rw) {
        c.write_all(b"nonce-1234").unwrap();
        let mut got = [0u8; 10];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"nonce-1234");
    }

    fn assert_eof(c: &mut dyn Rw) {
        let mut b = [0u8; 1];
        assert!(matches!(c.read(&mut b), Ok(0) | Err(_)));
    }

    /// RENDER-3: UDS で CONNECT → 200 → 往復できる。
    #[test]
    fn render_3_uds_roundtrip() {
        let dir = TempDir::new();
        let e = echo();
        let p = EgressProxy::start(EgressEndpoint::UnixSocketIn(dir.0.clone()), allow_private())
            .unwrap();
        let mut c = open(p.endpoint());
        c.write_all(connect_req("127.0.0.1", e.addr.port()).as_bytes())
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 200 "));
        roundtrip(&mut *c);
        assert_eq!(p.stats().allowed, 1);
    }

    /// RENDER-3: loopback TCP でも往復でき、待ち受けは 127.0.0.1。
    #[test]
    fn render_3_tcp_roundtrip() {
        let e = echo();
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, allow_private()).unwrap();
        match p.endpoint() {
            BoundEndpoint::Tcp(a) => assert_eq!(a.ip(), IpAddr::from([127, 0, 0, 1])),
            other => panic!("unexpected endpoint {other:?}"),
        }
        let mut c = open(p.endpoint());
        c.write_all(connect_req("127.0.0.1", e.addr.port()).as_bytes())
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 200 "));
        roundtrip(&mut *c);
    }

    /// CORE-1・RENDER-3: 既定では内部アドレス宛て（IP リテラル・名前）が 403 で、上流へ届かない。
    #[test]
    fn core_1_default_denies_loopback_literal_and_name() {
        let e = echo();
        let p =
            EgressProxy::start(EgressEndpoint::LoopbackTcp, EgressProxyConfig::default()).unwrap();
        for host in ["127.0.0.1", "localhost"] {
            let mut c = open(p.endpoint());
            c.write_all(connect_req(host, e.addr.port()).as_bytes())
                .unwrap();
            assert!(read_head(&mut *c).starts_with("HTTP/1.1 403 "), "{host}");
        }
        assert_eq!(e.accepted.load(Ordering::SeqCst), 0);
        assert_eq!(p.stats().denied_address, 2);
        assert_eq!(p.stats().allowed, 0);
    }

    struct FnPolicy(fn(IpAddr) -> bool);
    impl EgressAddressPolicy for FnPolicy {
        fn is_allowed(&self, ip: IpAddr) -> bool {
            (self.0)(ip)
        }
    }

    struct FixedResolver {
        addrs: Vec<SocketAddr>,
        calls: Arc<AtomicUsize>,
    }
    impl EgressResolver for FixedResolver {
        fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<SocketAddr>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.addrs.clone())
        }
    }

    /// CORE-1・SEC-2: 解決結果に許可と拒否が混在すれば 403（全 IP を検査）。解決は 1 回だけ。
    #[test]
    fn core_1_mixed_resolution_is_denied_and_resolved_once() {
        let e = echo();
        let calls = Arc::new(AtomicUsize::new(0));
        let cfg = EgressProxyConfig::default()
            .with_address_policy(Arc::new(FnPolicy(|ip| ip.is_loopback())))
            .with_resolver(Arc::new(FixedResolver {
                addrs: vec![
                    e.addr,
                    SocketAddr::new("192.0.2.1".parse().unwrap(), e.addr.port()),
                ],
                calls: Arc::clone(&calls),
            }));
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, cfg).unwrap();
        let mut c = open(p.endpoint());
        c.write_all(connect_req("mixed.test", e.addr.port()).as_bytes())
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 403 "));
        assert_eq!(e.accepted.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// RENDER-3: 注入ポリシーが許可すれば名前経路でも 200 で往復でき、名前は引き直さない。
    #[test]
    fn render_3_resolved_address_is_used_without_reresolving() {
        let e = echo();
        let calls = Arc::new(AtomicUsize::new(0));
        let cfg = EgressProxyConfig::default()
            .with_address_policy(Arc::new(FnPolicy(|ip| ip.is_loopback())))
            .with_resolver(Arc::new(FixedResolver {
                addrs: vec![e.addr],
                calls: Arc::clone(&calls),
            }));
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, cfg).unwrap();
        let mut c = open(p.endpoint());
        c.write_all(connect_req("named.test", e.addr.port()).as_bytes())
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 200 "));
        roundtrip(&mut *c);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// RENDER-3: GET は 405。
    #[test]
    fn render_3_non_connect_is_405() {
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, allow_private()).unwrap();
        let mut c = open(p.endpoint());
        c.write_all(b"GET http://127.0.0.1/ HTTP/1.1\r\n\r\n")
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 405 "));
        assert_eq!(p.stats().method_not_allowed, 1);
    }

    /// SEC-2: 要求行への LF 混入は 400 で閉じる。
    #[test]
    fn sec_2_bare_lf_is_400() {
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, allow_private()).unwrap();
        let mut c = open(p.endpoint());
        c.write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\nHost: x\r\n\r\n")
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 400 "));
        assert_eq!(p.stats().bad_request, 1);
    }

    /// RENDER-3: ヘッダ 8 KiB 超は閉じる。
    #[test]
    fn render_3_oversized_header_is_closed() {
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, allow_private()).unwrap();
        let mut c = open(p.endpoint());
        let mut req = b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nX: ".to_vec();
        req.extend(std::iter::repeat_n(b'a', 9000));
        // 閉じられた後の書き込み失敗は想定内。
        let _ = c.write_all(&req);
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 431 "));
        assert_eq!(p.stats().header_too_large, 1);
    }

    fn short_limits() -> EgressProxyLimits {
        EgressProxyLimits {
            header_deadline: Duration::from_millis(200),
            idle_timeout: Duration::from_millis(300),
            ..Default::default()
        }
    }

    /// RENDER-3: ヘッダ受信の期限超過で閉じる。
    #[test]
    fn render_3_header_deadline_closes() {
        let cfg = EgressProxyConfig::default()
            .with_allow_private_network_access(true)
            .with_limits(short_limits());
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, cfg).unwrap();
        let mut c = open(p.endpoint());
        c.write_all(b"CONNECT 127.0.0.1:1 HTT").unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 408 "));
        assert_eq!(p.stats().header_deadline, 1);
    }

    /// RENDER-3: 同時接続数の上限を超えた接続は 503 で拒否され、空けば再び受け付ける。
    #[test]
    fn render_3_connection_cap_rejects_then_recovers() {
        let e = echo();
        let cfg = EgressProxyConfig::default()
            .with_allow_private_network_access(true)
            .with_limits(EgressProxyLimits {
                max_connections: 2,
                ..Default::default()
            });
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, cfg).unwrap();
        let mut a = open(p.endpoint());
        let b = open(p.endpoint());
        a.write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n").unwrap();
        // 2 本が枠を取るまで待つ（accept は順序どおりに処理される）。
        let mut b = b;
        b.write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n").unwrap();
        thread::sleep(Duration::from_millis(200));
        let mut third = open(p.endpoint());
        assert!(read_head(&mut *third).starts_with("HTTP/1.1 503 "));
        assert_eq!(p.stats().over_capacity, 1);
        drop(a);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut c = open(p.endpoint());
            c.write_all(connect_req("127.0.0.1", e.addr.port()).as_bytes())
                .unwrap();
            if read_head(&mut *c).starts_with("HTTP/1.1 200 ") {
                break;
            }
            assert!(Instant::now() < deadline, "slot was not released");
            thread::sleep(Duration::from_millis(50));
        }
        drop(b);
    }

    /// RENDER-3: 無通信のトンネルはアイドル timeout で閉じ、通信中は閉じない。
    #[test]
    fn render_3_idle_tunnel_closes_but_active_one_stays() {
        let e = echo();
        let cfg = EgressProxyConfig::default()
            .with_allow_private_network_access(true)
            .with_limits(short_limits());
        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, cfg).unwrap();
        let mut c = open(p.endpoint());
        c.write_all(connect_req("127.0.0.1", e.addr.port()).as_bytes())
            .unwrap();
        assert!(read_head(&mut *c).starts_with("HTTP/1.1 200 "));
        // アイドル上限（300 ms）より短い間隔で通信し続ける間は閉じない。
        for _ in 0..8 {
            thread::sleep(Duration::from_millis(100));
            roundtrip(&mut *c);
        }
        assert_eq!(p.stats().idle_timeout, 0);
        assert_eof(&mut *c);
        assert_eq!(p.stats().idle_timeout, 1);
    }

    /// RENDER-3: Drop でソケットファイルが消え、待ち受けが閉じる。ソケットは 0600。
    #[test]
    fn render_3_drop_removes_socket_and_closes_listener() {
        let dir = TempDir::new();
        let p = EgressProxy::start(EgressEndpoint::UnixSocketIn(dir.0.clone()), allow_private())
            .unwrap();
        let BoundEndpoint::Unix(path) = p.endpoint().clone() else {
            panic!("expected unix endpoint");
        };
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        drop(p);
        assert!(std::fs::symlink_metadata(&path).is_err());

        let p = EgressProxy::start(EgressEndpoint::LoopbackTcp, allow_private()).unwrap();
        let BoundEndpoint::Tcp(addr) = p.endpoint().clone() else {
            panic!("expected tcp endpoint");
        };
        p.shutdown();
        assert!(TcpStream::connect(addr).is_err());
    }

    /// SEC-2: ソケット用ディレクトリの検証（0755・既存ファイル・長すぎるパス・相対パス）。
    #[test]
    fn sec_2_socket_dir_is_validated() {
        let dir = TempDir::new();
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = EgressProxy::start(EgressEndpoint::UnixSocketIn(dir.0.clone()), allow_private());
        assert!(matches!(r, Err(EgressProxyError::InvalidSocketDir)));

        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.0.join("egress-proxy.sock"), b"x").unwrap();
        let r = EgressProxy::start(EgressEndpoint::UnixSocketIn(dir.0.clone()), allow_private());
        assert!(matches!(r, Err(EgressProxyError::AlreadyExists)));
        // 既存ファイルは消されない。
        assert!(dir.0.join("egress-proxy.sock").exists());

        let long = dir.0.join("d".repeat(150));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&long)
            .unwrap();
        let r = EgressProxy::start(EgressEndpoint::UnixSocketIn(long), allow_private());
        assert!(matches!(r, Err(EgressProxyError::SocketPathTooLong)));

        let r = EgressProxy::start(
            EgressEndpoint::UnixSocketIn(PathBuf::from("relative")),
            allow_private(),
        );
        assert!(matches!(r, Err(EgressProxyError::InvalidSocketDir)));
    }

    /// RENDER-3: 0 の上限は `InvalidConfig`。
    #[test]
    fn render_3_zero_limits_are_invalid() {
        let cfg = EgressProxyConfig::default().with_limits(EgressProxyLimits {
            max_connections: 0,
            ..Default::default()
        });
        let r = EgressProxy::start(EgressEndpoint::LoopbackTcp, cfg);
        assert!(matches!(r, Err(EgressProxyError::InvalidConfig)));
    }
}

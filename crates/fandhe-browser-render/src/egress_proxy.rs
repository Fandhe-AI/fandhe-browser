//! Servo ホストの egress 用 CONNECT プロキシ（許可リスト付き。RENDER-3・SEC-2・TASK-38・#1434）。
//!
//! 設計 `docs/design/servo-host-egress-isolation.md` の方式 B では、OS 隔離が「プロキシ以外へ
//! 出られない」ことを担い、宛先 IP の判定（SSRF・DNS リバインディング対策。security.md
//! 「SSRF」）は本プロキシが接続時に担う。Servo 0.7.0 は proxy pref を設定すると http・https
//! とも CONNECT でプロキシへ接続するため、扱うのは `CONNECT host:port HTTP/1.1` だけ。
//!
//! - 宛先の名前解決はプロキシ自身が行い、解決した**全** IP を core の
//!   [`fandhe_browser_core::is_disallowed_address`]（CORE-1）で検査する。1 つでも拒否対象なら
//!   403 で閉じ、検査済みの `SocketAddr` にそのまま接続する（名前を引き直さない）
//! - `allow_private_network_access`（意味は core の `FetchOptions` と同じ。既定 false）が true の
//!   ときだけ内部アドレスを許す。ポートは制限しない
//! - 確立後のバイト列は改変せず双方向に転送する。ヘッダの付加・書き換えはしない（SEC-2）
//! - 待ち受けは Linux では UDS、macOS では `127.0.0.1:0` の TCP を呼び出し側が選ぶ
//!   （[`EgressEndpoint::platform_default`]）。Windows は未対応（[`EgressProxyError::UnsupportedPlatform`]）
//! - 依存追加・`unsafe` なし。std のスレッドと同期 IO で実装し、スレッド数は同時接続数の枠で有界
//!
//! # 範囲外（スタブではなく別 issue。RENDER-3・TASK-38）
//!
//! `HostProcess` への組み込み（WorkDir 配下へのソケット配置・netns 内の中継・servo-host の proxy
//! pref 注入）、bwrap / Seatbelt、Windows の隔離は本モジュールの範囲外。macOS の TCP 待ち受けは
//! 同一ユーザーの他プロセスからも届くため、Seatbelt 側で絞る前提。認証は置かない（loopback / UDS
//! のみ。設計 §2）。

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
mod counters;
#[cfg(unix)]
mod request;
#[cfg(unix)]
mod server;

/// ヘッダ全体の上限。一般的な HTTP サーバーのヘッダ上限と同じ 8 KiB。Servo（hyper_util の
/// `Tunnel`）が送るのは要求行と Host（と任意の Proxy-Authorization）だけで十分収まる。
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// accept からヘッダ終端までの総時間（slowloris 対策）。loopback / UDS の相手なので通常は
/// ミリ秒で届く。
pub const HEADER_DEADLINE: Duration = Duration::from_secs(10);
/// 同時接続数（トンネル確立後も含む）。ブラウザの同時ソケット数の目安から決めた。1 接続あたり
/// 最大 2 スレッドなので、スレッド数は高々この 2 倍に有界。
pub const MAX_CONCURRENT_CONNECTIONS: usize = 128;
/// 上流への TCP 接続全体の時間（複数アドレスを試す場合も合計でこの値まで）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// トンネル確立後の無通信（双方向とも）の上限。片方向だけ流れ続ける長いダウンロードは切らない。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// 停止フラグとアイドル判定を見る周期の上限。
pub const TUNNEL_TICK: Duration = Duration::from_millis(250);
/// 実際に接続を試す解決済みアドレスの件数の上限（検査は全件行う）。
pub const MAX_CONNECT_ATTEMPTS: usize = 8;

/// プロキシ内部の上限値。定数以下にだけ締められる（安全側にしか動かせない）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EgressProxyLimits {
    /// ヘッダ全体の上限バイト数（[`MAX_HEADER_BYTES`] 以下）。
    pub max_header_bytes: usize,
    /// ヘッダ受信の総時間（[`HEADER_DEADLINE`] 以下）。
    pub header_deadline: Duration,
    /// 同時接続数（[`MAX_CONCURRENT_CONNECTIONS`] 以下）。
    pub max_connections: usize,
    /// 上流への接続時間（[`CONNECT_TIMEOUT`] 以下）。
    pub connect_timeout: Duration,
    /// トンネルのアイドル上限（[`IDLE_TIMEOUT`] 以下）。
    pub idle_timeout: Duration,
}

impl Default for EgressProxyLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: MAX_HEADER_BYTES,
            header_deadline: HEADER_DEADLINE,
            max_connections: MAX_CONCURRENT_CONNECTIONS,
            connect_timeout: CONNECT_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
        }
    }
}

impl EgressProxyLimits {
    /// 0、または定数を上回る値を `InvalidConfig` にする。
    #[cfg_attr(not(unix), allow(dead_code))]
    fn validate(&self) -> Result<(), EgressProxyError> {
        let ok = (1..=MAX_HEADER_BYTES).contains(&self.max_header_bytes)
            && !self.header_deadline.is_zero()
            && self.header_deadline <= HEADER_DEADLINE
            && (1..=MAX_CONCURRENT_CONNECTIONS).contains(&self.max_connections)
            && !self.connect_timeout.is_zero()
            && self.connect_timeout <= CONNECT_TIMEOUT
            && !self.idle_timeout.is_zero()
            && self.idle_timeout <= IDLE_TIMEOUT;
        if ok {
            Ok(())
        } else {
            Err(EgressProxyError::InvalidConfig)
        }
    }

    /// 停止フラグ・アイドル判定の周期。短縮した上限でもアイドル判定が粗くならないようにする。
    #[cfg(unix)]
    fn tick(&self) -> Duration {
        TUNNEL_TICK
            .min(self.idle_timeout / 2)
            .max(Duration::from_millis(5))
    }
}

/// 宛先 IP を許可するかの判定。
///
/// 既定は core の判定（[`CoreAddressPolicy`]）。差し替えは**テスト用**で、本番
/// （HostProcess・cli）は既定を使う。
pub trait EgressAddressPolicy: Send + Sync {
    /// 接続してよい IP なら `true`。
    fn is_allowed(&self, ip: IpAddr) -> bool;
}

/// core の [`fandhe_browser_core::is_disallowed_address`] に従う既定の判定（CORE-1）。
#[derive(Clone, Copy, Debug, Default)]
pub struct CoreAddressPolicy {
    /// `FetchOptions::allow_private_network_access` と同じ意味。true のとき全 IP を許す。
    pub allow_private_network_access: bool,
}

impl EgressAddressPolicy for CoreAddressPolicy {
    fn is_allowed(&self, ip: IpAddr) -> bool {
        self.allow_private_network_access || !fandhe_browser_core::is_disallowed_address(ip)
    }
}

/// ホスト名の解決。既定は OS のリゾルバ（[`SystemResolver`]）。差し替えは**テスト用**。
pub trait EgressResolver: Send + Sync {
    /// `host` を解決する。IP リテラルの宛先はここを通らない。
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// `ToSocketAddrs`（getaddrinfo）による既定のリゾルバ。
///
/// 注意: getaddrinfo は同期でブロックし、解決そのものには timeout がない
/// （`connect_timeout` は TCP 接続にしか効かない）。応答しない DNS 宛先を多数同時に
/// 指定されると、その分だけ同時接続の枠とスレッドが長時間塞がる。枠の上限で有界であり、
/// 入力元が信頼できる Servo ホストである前提のため既知の制約として受容する。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl EgressResolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok((host, port).to_socket_addrs()?.collect())
    }
}

/// プロキシの設定。
#[derive(Clone, Default)]
pub struct EgressProxyConfig {
    /// 内部アドレス宛てを許すか（既定 false。core の `FetchOptions` と同じ意味）。
    pub allow_private_network_access: bool,
    /// 上限値。
    pub limits: EgressProxyLimits,
    policy: Option<Arc<dyn EgressAddressPolicy>>,
    resolver: Option<Arc<dyn EgressResolver>>,
}

impl EgressProxyConfig {
    /// 内部アドレス宛てを許すかを設定する。
    #[must_use]
    pub fn with_allow_private_network_access(mut self, allow: bool) -> Self {
        self.allow_private_network_access = allow;
        self
    }

    /// 上限値を設定する（定数以下にだけ締められる。起動時に検証）。
    #[must_use]
    pub fn with_limits(mut self, limits: EgressProxyLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 宛先判定を差し替える（テスト用。本番は既定を使う）。
    #[must_use]
    pub fn with_address_policy(mut self, policy: Arc<dyn EgressAddressPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// リゾルバを差し替える（テスト用。本番は既定を使う）。
    #[must_use]
    pub fn with_resolver(mut self, resolver: Arc<dyn EgressResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }
}

/// 待ち受けの種別。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EgressEndpoint {
    /// 指定ディレクトリ配下の UDS（Linux 向け）。ディレクトリは絶対パス・非 symlink・0700 で、
    /// ソケット名は固定の `egress-proxy.sock`（外部文字列をパスに入れない）。ソケットは 0600。
    UnixSocketIn(PathBuf),
    /// `127.0.0.1:0` の TCP（macOS 向け。loopback 以外へは bind しない）。
    LoopbackTcp,
}

impl EgressEndpoint {
    /// OS ごとの既定（Linux は UDS、それ以外の Unix は loopback TCP）。選択は呼び出し側
    /// （後続の HostProcess 組み込み）の責務。
    pub fn platform_default(dir: PathBuf) -> Self {
        if cfg!(target_os = "linux") {
            Self::UnixSocketIn(dir)
        } else {
            Self::LoopbackTcp
        }
    }
}

/// bind 済みの待ち受け先（servo-host の proxy pref へ渡す値の元）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundEndpoint {
    /// UDS のパス。
    Unix(PathBuf),
    /// loopback TCP のアドレス。
    Tcp(SocketAddr),
}

/// 件数のスナップショット（可観測性。REPAIR-4）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EgressProxyStats {
    /// accept した接続数。
    pub accepted: u64,
    /// トンネルを確立した数。
    pub allowed: u64,
    /// 宛先 IP の拒否（403）。
    pub denied_address: u64,
    /// CONNECT 以外のメソッド（405）。
    pub method_not_allowed: u64,
    /// 形式不正（400）。
    pub bad_request: u64,
    /// ヘッダ上限超過（431）。
    pub header_too_large: u64,
    /// ヘッダ受信の期限超過（408）。
    pub header_deadline: u64,
    /// 同時接続数の上限超過（503）。
    pub over_capacity: u64,
    /// 名前解決の失敗（502）。
    pub resolve_failed: u64,
    /// 上流への接続失敗（502）。
    pub upstream_failed: u64,
    /// アイドル timeout で閉じたトンネル数。
    pub idle_timeout: u64,
    /// スレッド生成の失敗。
    pub spawn_failed: u64,
}

/// 起動時のエラー。
#[derive(Debug)]
pub enum EgressProxyError {
    /// この OS では未対応（Windows）。
    UnsupportedPlatform,
    /// ソケット用ディレクトリが絶対パス・非 symlink・0700 を満たさない。
    InvalidSocketDir,
    /// ソケットパスが `sun_path` の上限を超える。
    SocketPathTooLong,
    /// ソケットパスに既に何かがある（消さない）。
    AlreadyExists,
    /// bind・権限設定の失敗。
    Bind(io::Error),
    /// 上限値が 0 または定数超過。
    InvalidConfig,
    /// accept スレッドを生成できない。
    Spawn(io::Error),
}

impl fmt::Display for EgressProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => write!(f, "egress proxy is not supported on this OS"),
            Self::InvalidSocketDir => write!(
                f,
                "socket directory must be an absolute, non-symlink directory with mode 0700"
            ),
            Self::SocketPathTooLong => write!(f, "socket path is too long for sun_path"),
            Self::AlreadyExists => write!(f, "socket path already exists"),
            Self::Bind(e) => write!(f, "failed to bind egress proxy listener: {e}"),
            Self::InvalidConfig => {
                write!(f, "egress proxy limits must be non-zero and within caps")
            }
            Self::Spawn(e) => write!(f, "failed to spawn egress proxy thread: {e}"),
        }
    }
}

impl std::error::Error for EgressProxyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(e) | Self::Spawn(e) => Some(e),
            _ => None,
        }
    }
}

/// 起動中の egress プロキシ。Drop で待ち受けを閉じ、UDS ならソケットファイルを消す。
/// 確立済みトンネルは停止フラグを tick ごとに見て閉じる。
pub struct EgressProxy {
    endpoint: BoundEndpoint,
    #[cfg(unix)]
    runtime: server::Runtime,
}

impl EgressProxy {
    /// 待ち受けを開始する。Windows では常に `UnsupportedPlatform`。
    #[cfg(unix)]
    pub fn start(
        endpoint: EgressEndpoint,
        config: EgressProxyConfig,
    ) -> Result<Self, EgressProxyError> {
        config.limits.validate()?;
        let (endpoint, runtime) = server::start(endpoint, config)?;
        Ok(Self { endpoint, runtime })
    }

    /// 待ち受けを開始する。Windows では常に `UnsupportedPlatform`。
    #[cfg(not(unix))]
    pub fn start(
        endpoint: EgressEndpoint,
        config: EgressProxyConfig,
    ) -> Result<Self, EgressProxyError> {
        let _ = (endpoint, config);
        Err(EgressProxyError::UnsupportedPlatform)
    }

    /// bind 済みの待ち受け先。
    pub fn endpoint(&self) -> &BoundEndpoint {
        &self.endpoint
    }

    /// 件数のスナップショット。
    pub fn stats(&self) -> EgressProxyStats {
        #[cfg(unix)]
        {
            self.runtime.stats()
        }
        #[cfg(not(unix))]
        {
            EgressProxyStats::default()
        }
    }

    /// 明示的に停止する（Drop と同じ）。
    pub fn shutdown(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RENDER-3: 既定の上限は定数と一致し、検証を通る。
    #[test]
    fn render_3_default_limits_match_constants() {
        let l = EgressProxyLimits::default();
        assert_eq!(l.max_header_bytes, 8192);
        assert_eq!(l.header_deadline, Duration::from_secs(10));
        assert_eq!(l.max_connections, 128);
        assert_eq!(l.connect_timeout, Duration::from_secs(10));
        assert_eq!(l.idle_timeout, Duration::from_secs(300));
        assert!(l.validate().is_ok());
    }

    /// RENDER-3: 上限は締める方向だけを許し、0 と超過は `InvalidConfig`。
    #[test]
    fn render_3_limits_only_tighten() {
        let tight = EgressProxyLimits {
            max_connections: 2,
            idle_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        assert!(tight.validate().is_ok());
        for bad in [
            EgressProxyLimits {
                max_header_bytes: 0,
                ..Default::default()
            },
            EgressProxyLimits {
                max_header_bytes: MAX_HEADER_BYTES + 1,
                ..Default::default()
            },
            EgressProxyLimits {
                max_connections: 0,
                ..Default::default()
            },
            EgressProxyLimits {
                max_connections: 129,
                ..Default::default()
            },
            EgressProxyLimits {
                header_deadline: Duration::ZERO,
                ..Default::default()
            },
            EgressProxyLimits {
                idle_timeout: Duration::from_secs(301),
                ..Default::default()
            },
            EgressProxyLimits {
                connect_timeout: Duration::from_secs(11),
                ..Default::default()
            },
        ] {
            assert!(
                matches!(bad.validate(), Err(EgressProxyError::InvalidConfig)),
                "{bad:?}"
            );
        }
    }

    /// CORE-1・RENDER-3: 既定は内部アドレスを拒否し、許可設定で通す。
    #[test]
    fn core_1_default_policy_denies_internal_addresses() {
        let deny = CoreAddressPolicy::default();
        for ip in [
            "127.0.0.1",
            "::ffff:10.0.0.1",
            "192.0.2.1",
            "::1",
            "169.254.1.1",
        ] {
            assert!(!deny.is_allowed(ip.parse().unwrap()), "{ip}");
        }
        assert!(deny.is_allowed("93.184.216.35".parse().unwrap()));
        let allow = CoreAddressPolicy {
            allow_private_network_access: true,
        };
        assert!(allow.is_allowed("127.0.0.1".parse().unwrap()));
    }
}

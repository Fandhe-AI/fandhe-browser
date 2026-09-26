//! fetch: URL からの HTTP/HTTPS 取得を担うモジュール。
//!
//! `cdp`（`Page.navigate` 等）や将来の CLI から呼ばれ、取得した生のバイト列を
//! `parse` モジュール（TASK-24.4・#38）へ渡す下位レイヤー（TASK-24.2・
//! ビヘイビア `CORE-1`）。ネットワーク応答は信頼できない外部入力として扱い、
//! 次の方針を徹底する（coding-rust.md「エラーハンドリング」・security.md
//! 「不安全な設計」「SSRF」）:
//!
//! - 全体タイムアウト・接続タイムアウト・リダイレクト回数・レスポンス本文の
//!   各上限を [`FetchOptions`] で設定し、超過時は `panic` せず [`crate::Error`]
//!   を返す
//! - `http`/`https` 以外の scheme（`file:` 等）は要求前・リダイレクト先の
//!   両方で拒否する
//! - 取得先の接続先アドレス（初回リクエスト・各リダイレクト先の双方、かつ
//!   DNS 解決結果・IP リテラル host の双方）がループバック・プライベート
//!   アドレス等の内部アドレスである場合、
//!   [`FetchOptions::allow_private_network_access`] が `false`（既定）なら
//!   拒否する（DNS 名は [`SafeResolver`]、IP リテラル host は
//!   [`reject_disallowed_address`]。IP リテラルは hyper-util が DNS 解決を
//!   経由せず直接使うため `SafeResolver` だけでは検出できない。
//!   security.md「SSRF」）。ブラウザでは `localhost` への到達も正当な用途が
//!   あるため、呼び出し側が明示的に opt-in できるようにする
//! - 外部入力の経路（レスポンスヘッダ・本文）で `unwrap`/`expect`/添字アクセス
//!   を使わない
//! - DNS 解決（[`SafeResolver`]）は名前解決のたびに専用の OS スレッドを
//!   生成するため、同時実行数に [`MAX_CONCURRENT_DNS_RESOLUTIONS`] の上限を
//!   設け、上限到達時は新規スレッドを生成せず
//!   [`crate::Error::TooManyConcurrentDnsResolutions`] を返す（PR #430
//!   コードレビュー指摘 P0: 無制限なスレッド生成による DoS を防ぐ）
//!
//! 本実装は async API（[`Fetcher::get`]）のみを提供し、`reqwest::blocking` は
//! 使わない（tokio ランタイム内から呼ぶと panic するため。CDP サーバー
//! （TASK-41・#171）は tokio 前提の想定で、この制約と衝突する）。
//!
//! ## 本 Issue（#36）の範囲外（将来仕様。REPAIR-3: 実装済みを装わない）
//!
//! - ローカルファイル（`file:`）の取得: 受け入れ基準外のため対応しない
//! - DNS リバインディング対策の強化（TOCTOU）: [`SafeResolver`] は解決の
//!   都度アドレスを検証するため多くのケースは防げるが、解決結果と実際の
//!   TCP 接続確立の間に別 IP へ切り替わる極端な race までは保証しない
//! - Cookie セッション維持（ビヘイビア CORE-5 (8)）
//! - 文字コード判定（ビヘイビア CORE-5 (7)）: 本文は常にバイト列で返す
//! - gzip 等の展開・HTTP/2・プロキシ設定: 挙動の決定性を優先し、`no_proxy()`
//!   で環境変数のプロキシ設定を無視する

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder, Url};

use crate::error::{Error, Result};

/// デフォルトの全体タイムアウト（30 秒）。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// デフォルトの接続タイムアウト（10 秒）。
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// デフォルトの最大リダイレクト回数。
const DEFAULT_MAX_REDIRECTS: usize = 10;
/// デフォルトの最大レスポンス本文サイズ（16 MiB）。
const DEFAULT_MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// 本文読み込み用バッファの初期確保サイズの上限（64 KiB）。
///
/// `Content-Length` ヘッダは外部入力であり、事前検査（[`Fetcher::get`]）を
/// 通過した後でも `max_body_bytes` に近い巨大な値を偽って宣言し得る
/// （ヘッダ自体は上限以下でも、単に大きい確保を要求してくる場合）。
/// 初期確保をこの固定上限で頭打ちにし、実際の受信量に応じて `extend_from_slice`
/// が必要な分だけ再確保するようにすることで、宣言だけによるアロケーション
/// の増幅（DoS。security.md「不安全な設計」）を避ける。
const INITIAL_BODY_CAPACITY_LIMIT: u64 = 64 * 1024;

/// `Fetcher::new` の呼び出し元が指定する取得条件。
///
/// `#[non_exhaustive]` により、後続タスクでのフィールド追加（gzip 展開の
/// 有効化等）が呼び出し側の構造体リテラル初期化を破壊的変更にしない
/// （REPAIR-4: 戻り値・入力は将来拡張できる構造にする）。`Default` と
/// `with_*` メソッドで組み立てる。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchOptions {
    /// リクエスト全体のタイムアウト（接続確立からレスポンス完了まで）。
    pub timeout: Duration,
    /// TCP/TLS 接続確立のタイムアウト。
    pub connect_timeout: Duration,
    /// 許可する最大リダイレクト回数。リダイレクト先への遷移がこの回数を
    /// 超えた時点（`attempt.previous().len() > max_redirects` となった
    /// 時点）で [`Error::TooManyRedirects`] を返す。
    pub max_redirects: usize,
    /// 許可する最大レスポンス本文サイズ（バイト）。`Content-Length` による
    /// 事前検査と、ストリーミング読み込み中の逐次検査の両方で使う。
    pub max_body_bytes: u64,
    /// ループバック・プライベートアドレス等の内部アドレスへの接続を許可
    /// するか（既定 `false` = 拒否）。
    ///
    /// `false` の場合、初回リクエスト・各リダイレクト先の双方について、
    /// DNS 名は解決結果を（[`SafeResolver`]）、IP リテラル host はそのまま
    /// （[`reject_disallowed_address`]）検査し、内部アドレスであれば接続前に
    /// [`Error::DisallowedAddress`] を返す（security.md「SSRF」）。ブラウザ的
    /// な用途では `localhost`（開発サーバー等）への到達が正当な場合がある
    /// ため、呼び出し側が明示的に `true` を設定すれば許可できる。既定は
    /// 安全側（拒否）に倒す。
    pub allow_private_network_access: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            allow_private_network_access: false,
        }
    }
}

impl FetchOptions {
    /// 既定値で [`FetchOptions`] を作る（[`Default`] と同義）。
    pub fn new() -> Self {
        Self::default()
    }

    /// リクエスト全体のタイムアウトを設定する。
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 接続確立のタイムアウトを設定する。
    #[must_use]
    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// 許可する最大リダイレクト回数を設定する。
    #[must_use]
    pub fn with_max_redirects(mut self, max_redirects: usize) -> Self {
        self.max_redirects = max_redirects;
        self
    }

    /// 許可する最大レスポンス本文サイズ（バイト）を設定する。
    #[must_use]
    pub fn with_max_body_bytes(mut self, max_body_bytes: u64) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// ループバック・プライベートアドレス等の内部アドレスへの接続許可を
    /// 設定する（既定 `false`。`true` にすると `localhost` 等の開発用途を
    /// 許可する。security.md「SSRF」・[`FetchOptions::allow_private_network_access`]）。
    #[must_use]
    pub fn with_allow_private_network_access(mut self, allow_private_network_access: bool) -> Self {
        self.allow_private_network_access = allow_private_network_access;
        self
    }

    /// 各フィールドが `Fetcher::new` で受理できる値かを検証する
    /// （ゼロ値は「即座に失敗する設定」であり呼び出し側の誤りである
    /// 可能性が高いため、`InvalidInput` として早期に拒否する）。
    fn validate(&self) -> Result<()> {
        if self.timeout.is_zero() {
            return Err(Error::InvalidInput {
                message: "FetchOptions::timeout must not be zero".to_string(),
            });
        }
        if self.connect_timeout.is_zero() {
            return Err(Error::InvalidInput {
                message: "FetchOptions::connect_timeout must not be zero".to_string(),
            });
        }
        if self.max_body_bytes == 0 {
            return Err(Error::InvalidInput {
                message: "FetchOptions::max_body_bytes must not be zero".to_string(),
            });
        }
        Ok(())
    }
}

/// HTTP/HTTPS でページを取得するクライアント。内部に `reqwest::Client` を
/// 保持し、接続プールを再利用する（呼び出しのたびに構築しない）。
///
/// `cdp` の `Page.navigate` ハンドラ・将来の CLI から生成・保持され、
/// [`Fetcher::get`] を介して呼び出される想定（TASK-24.2・CORE-1）。
pub struct Fetcher {
    client: Client,
    options: FetchOptions,
}

/// rustls の crypto provider を 1 回だけ install するためのガード。
///
/// `Fetcher::new` は複数回呼ばれ得るが、`rustls::crypto::CryptoProvider` の
/// install はプロセス全体で 1 回のみ許可される（2 回目以降は `Err` を返す）。
/// 既に別の provider（例: 呼び出し元アプリケーションが独自に install した
/// もの）が入っている場合は、そのエラーを無視して既存の provider を使う
/// （`unwrap` しない。coding-rust.md「エラーハンドリング」）。
static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();

fn ensure_crypto_provider_installed() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        // 戻り値の `Result<(), Arc<CryptoProvider>>` は「既に別の provider が
        // install 済み」を示すだけで、この crate にとってはどちらの provider
        // が実際に使われるかに関わらず後続の TLS 接続は成立する。
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// リダイレクトポリシーが打ち切りを検出したときに埋め込む非公開マーカー。
/// `reqwest::Error::source()` を辿って `downcast_ref` で判定するためだけに
/// 存在し、Display 文字列を外部へ公開しない（送信エラーの写像内で判定した
/// 後、`Error::TooManyRedirects`/`Error::DisallowedScheme` へ書き換える）。
#[derive(Debug)]
enum RedirectMarker {
    TooManyRedirects {
        limit: usize,
    },
    DisallowedScheme {
        scheme: String,
    },
    /// リダイレクト先の host が IP リテラルで、かつループバック・
    /// プライベートアドレス等の内部アドレスである場合（security.md
    /// 「SSRF」）。[`SafeResolver`] は IP リテラル host に対しては呼ばれない
    /// （hyper-util の `HttpConnector` が IP リテラルを DNS 解決なしで直接
    /// 使うため。`try_parse` 経路）ため、`Policy::custom` 側でも
    /// [`reject_disallowed_address`] を呼んで同じ検証をする。
    DisallowedAddress {
        address: String,
    },
}

impl std::fmt::Display for RedirectMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedirectMarker::TooManyRedirects { limit } => {
                write!(f, "too many redirects (limit: {limit})")
            }
            RedirectMarker::DisallowedScheme { scheme } => {
                write!(f, "disallowed redirect scheme: {scheme}")
            }
            RedirectMarker::DisallowedAddress { address } => {
                write!(f, "disallowed redirect target address: {address}")
            }
        }
    }
}

impl std::error::Error for RedirectMarker {}

/// [`SafeResolver`] が内部アドレスを検出したときに埋め込む非公開マーカー。
/// `RedirectMarker` と同じ downcast の仕組み（[`map_reqwest_error`]）で
/// [`Error::DisallowedAddress`] へ書き換える。
#[derive(Debug)]
struct DisallowedAddressMarker {
    address: IpAddr,
}

/// 同時に起動できる DNS 解決専用スレッド（[`resolve_blocking`]）の上限。
///
/// `SafeResolver::resolve` は名前解決のたびに `resolve_blocking` を呼び、
/// 都度専用の OS スレッドを新規生成する。外部から多数の異なる URL を
/// 取得させられる経路（`cdp` の `Page.navigate` 等）では、DNS 応答が
/// 遅延・停止してもこの同期呼び出しは中断されないため、上限を設けないと
/// スレッド・メモリが無制限に増える（security.md「不安全な設計」。
/// PR #430 コードレビュー指摘 P0）。値はプロセス全体で共有するリソース
/// 上限の経験則的な固定値であり、`FetchOptions` のフィールドにはしない
/// （`Fetcher` インスタンスごとに変えられると、複数の `Fetcher` を生成
/// された場合に上限の意味が失われるため）。
const MAX_CONCURRENT_DNS_RESOLUTIONS: usize = 64;

/// 現在実行中の DNS 解決スレッド数。プロセス全体で共有し、
/// [`resolve_blocking`] がスレッド生成前に
/// [`try_acquire_dns_resolution_slot`] で確保、スレッド完了時に
/// [`release_dns_resolution_slot`] で解放する。
static ACTIVE_DNS_RESOLUTIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 実行中の DNS 解決スレッド数が [`MAX_CONCURRENT_DNS_RESOLUTIONS`] 未満で
/// あれば 1 枠確保する（CAS ループ）。確保できた場合のみ `true` を返す。
/// 呼び出し元（[`resolve_blocking`]）は、スレッド生成に成功した場合・
/// 失敗した場合のいずれの経路でも必ず [`release_dns_resolution_slot`] を
/// 呼んで解放する契約を守る。
fn try_acquire_dns_resolution_slot() -> bool {
    use std::sync::atomic::Ordering;
    let mut current = ACTIVE_DNS_RESOLUTIONS.load(Ordering::Relaxed);
    loop {
        if current >= MAX_CONCURRENT_DNS_RESOLUTIONS {
            return false;
        }
        match ACTIVE_DNS_RESOLUTIONS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// [`try_acquire_dns_resolution_slot`] で確保した枠を解放する。
fn release_dns_resolution_slot() {
    ACTIVE_DNS_RESOLUTIONS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
}

/// [`resolve_blocking`] が同時実行の DNS 解決スレッド上限
/// （[`MAX_CONCURRENT_DNS_RESOLUTIONS`]）に達したときに埋め込む非公開
/// マーカー。`DisallowedAddressMarker` と同じ downcast の仕組み
/// （[`map_reqwest_error`]）で [`Error::TooManyConcurrentDnsResolutions`]
/// へ書き換える（PR #430 コードレビュー指摘 P0）。
#[derive(Debug)]
struct TooManyDnsResolutionsMarker;

impl std::fmt::Display for TooManyDnsResolutionsMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many concurrent DNS resolutions (limit: {MAX_CONCURRENT_DNS_RESOLUTIONS})"
        )
    }
}

impl std::error::Error for TooManyDnsResolutionsMarker {}

impl std::fmt::Display for DisallowedAddressMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "disallowed target address: {}", self.address)
    }
}

impl std::error::Error for DisallowedAddressMarker {}

/// [`reqwest::dns::Resolve`] へ渡す `BoxError`（`reqwest` 内部の型 alias は
/// `pub(crate)` のため名指しできず、同じ具象型を直接綴る）。
type DnsBoxError = Box<dyn std::error::Error + Send + Sync>;

/// IPv4 アドレスが一般公開向けの到達性を持たない特殊用途アドレスかを
/// 判定する（security.md「SSRF」）。
///
/// `std::net::Ipv4Addr` の安定 API（`is_loopback`・`is_private` 等）は
/// IANA の "IANA IPv4 Special-Purpose Address Registry" の一部しか
/// カバーしない（共有アドレス空間 `100.64.0.0/10`・ベンチマーク用
/// `198.18.0.0/15` 等が抜け落ちる）ため、それらだけでは内部・非公開
/// アドレスの一部を許可してしまう。ここでは「一般公開アドレスとして
/// 到達を許可してよい」を明確に定義し、レジストリに列挙された特殊用途
/// ブロックを漏れなく列挙することでその補集合（= 拒否対象）を判定する
/// （個別分類の enum ではなく既知ブロックの網羅的な denylist で
/// allowlist と同義の判定にする）。
fn is_disallowed_ipv4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback() // 127.0.0.0/8
        || v4.is_private() // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
        || v4.is_link_local() // 169.254.0.0/16
        || v4.is_unspecified() // 0.0.0.0
        || v4.is_broadcast() // 255.255.255.255
        || v4.is_documentation() // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
        || octets[0] == 0 // 0.0.0.0/8 ("this network")
        || (octets[0] == 100 && (64..=127).contains(&octets[1])) // 100.64.0.0/10 共有アドレス空間（CGN）
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0) // 192.0.0.0/24 IETF Protocol Assignments
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99) // 192.88.99.0/24 6to4 Relay Anycast
        || (octets[0] == 198 && (18..=19).contains(&octets[1])) // 198.18.0.0/15 ベンチマーク用
        || octets[0] >= 224 // 224.0.0.0/4 マルチキャスト + 240.0.0.0/4 予約済み
}

/// IPv6 アドレスが非推奨のサイトローカルブロック `fec0::/10`
/// （RFC 3879 で非推奨だが到達可能な環境が残る内部アドレス）に属するかを
/// 判定する。`std::net::Ipv6Addr` にはこの判定の安定 API が無い
/// （`is_unique_local` は現行の `fc00::/7` のみを対象とし `fec0::/10` を
/// カバーしない）ため、先頭 10 ビット（`segments()[0] & 0xffc0 == 0xfec0`）
/// を直接検査する（CORE-1。#36 PR #430 コードレビュー指摘）。
fn is_ipv6_site_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfec0
}

/// [`IpAddr`] が内部アドレス（ループバック・プライベートアドレス等）か
/// どうかを判定する（[`SafeResolver`] から使う）。IPv6 の v4-mapped
/// アドレス（`::ffff:a.b.c.d`）は埋め込まれた IPv4 アドレスとして判定し、
/// v4 側の分類をすり抜けられないようにする。IP リテラル指定経路
/// （[`url_ip_literal`]）・DNS 解決経路（[`SafeResolver::resolve`]）の
/// 双方がこの関数を経由するため、ここで拒否対象を追加すれば両経路に
/// 反映される。
fn is_disallowed_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_disallowed_ipv4(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || is_ipv6_site_local(v6)
                || v6.to_ipv4_mapped().is_some_and(is_disallowed_ipv4)
        }
    }
}

/// [`Fetcher::new`] が `ClientBuilder::dns_resolver` へ渡すカスタムリゾルバ。
///
/// `http`/`https` の全接続（初回リクエスト・リダイレクト先の双方）は必ず
/// このリゾルバの [`Resolve::resolve`] を経由するため、`Policy::custom`
/// （scheme 検査）と異なりリダイレクト先ごとの個別配線を要さず、ここ 1 箇所
/// で「取得先とリダイレクト先の内部アドレスを検証する」（security.md
/// 「SSRF」）を満たす。host が IP リテラルの場合は DNS を介さずそのまま
/// 検証される（`std::net::ToSocketAddrs` が IP リテラルをそのまま返すため）。
///
/// DNS 解決は `std::net::ToSocketAddrs`（システムの getaddrinfo）を使うが、
/// これは同期 API であるため、そのまま非同期ブロック内で直接呼び出すと
/// DNS が遅延・停止した際に呼び出し元の tokio ワーカースレッドを占有し、
/// 他タスクの実行や `FetchOptions::connect_timeout`/`timeout` の計時までも
/// 止めてしまう（security.md「不安全な設計」）。本 crate は tokio を直接の
/// 依存に持たない契約（`fandhe-browser-core/Cargo.toml`: tokio は結合テスト用の
/// dev-dependency のみ。dependency-policy.md によりユーザー承認なしに
/// ランタイム依存へ昇格できない）ため、`tokio::task::spawn_blocking` は
/// 使えない。代わりに [`resolve_blocking`] が `std::thread::spawn` で
/// 解決専用の OS スレッドへオフロードし、[`BlockingDnsFuture`]（`std` のみで
/// 組んだ最小限の oneshot future）で非同期に結果を受け取ることで、
/// 呼び出し元の非同期ランタイムのスレッドをブロックしない。
struct SafeResolver {
    allow_private_network_access: bool,
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private_network_access = self.allow_private_network_access;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_blocking(host).await?;
            if !allow_private_network_access
                && let Some(addr) = addrs.iter().find(|addr| is_disallowed_address(addr.ip()))
            {
                return Err(Box::new(DisallowedAddressMarker { address: addr.ip() }) as DnsBoxError);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// [`resolve_blocking`] の完了通知を保持する内部状態。
///
/// `result` と `waker` を同一の `Mutex` の下に置くことで、送信側
/// （解決スレッド）と受信側（[`BlockingDnsFuture::poll`]）のどちらが先に
/// 実行されても「結果を書き込んだのに waker が未登録で wake が失われる」
/// レースを起こさない（両者とも同じロックを取ってから読み書きするため）。
struct BlockingDnsInner {
    result: Option<std::result::Result<Vec<SocketAddr>, DnsBoxError>>,
    waker: Option<std::task::Waker>,
}

/// `host` を専用スレッドで解決した結果を運ぶ、`std` のみで組んだ最小限の
/// oneshot future（[`SafeResolver::resolve`] 用。tokio 非依存の理由は
/// [`SafeResolver`] のドキュメント参照）。
struct BlockingDnsFuture(Arc<std::sync::Mutex<BlockingDnsInner>>);

impl std::future::Future for BlockingDnsFuture {
    type Output = std::result::Result<Vec<SocketAddr>, DnsBoxError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut inner = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = inner.result.take() {
            return std::task::Poll::Ready(result);
        }
        inner.waker = Some(cx.waker().clone());
        std::task::Poll::Pending
    }
}

/// `host` の名前解決（`std::net::ToSocketAddrs`）を専用の OS スレッドで
/// 実行し、[`BlockingDnsFuture`] で非同期に結果を返す。ポートは `reqwest`
/// が URL 由来の値で上書きする契約（`Resolve` のドキュメント）のため、
/// ここでは 0 を渡す。スレッド生成自体が失敗した場合（OS リソース枯渇等）
/// もエラーとして返し、`panic`/`unwrap` はしない。
///
/// - 空文字列 host は明示的に拒否する。Unix/macOS の `getaddrinfo` は
///   空文字列に対して `Err` を返すが、Windows は同じ空文字列に対して
///   ローカルインターフェースのアドレス群を返す（`Err` にならない）ため、
///   OS の解決結果に処理を委ねると 3 OS で挙動が食い違う（CI 実測。
///   PR #430・ci.md「3 OS 一級対応」）。OS の意味論差を吸収し、3 OS で
///   同一の `Err` 挙動にするため、スレッド生成前にここで判定する
/// - 同時実行の DNS 解決スレッド数が [`MAX_CONCURRENT_DNS_RESOLUTIONS`]
///   に達している場合、新規スレッドを生成せず [`TooManyDnsResolutionsMarker`]
///   を伴う `Err` を返す（PR #430 コードレビュー指摘 P0。security.md
///   「不安全な設計」）
fn resolve_blocking(host: String) -> BlockingDnsFuture {
    let state = Arc::new(std::sync::Mutex::new(BlockingDnsInner {
        result: None,
        waker: None,
    }));

    if host.is_empty() {
        {
            let mut inner = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.result = Some(Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty DNS host",
            )) as DnsBoxError));
        }
        return BlockingDnsFuture(state);
    }

    if !try_acquire_dns_resolution_slot() {
        {
            let mut inner = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.result = Some(Err(Box::new(TooManyDnsResolutionsMarker) as DnsBoxError));
        }
        return BlockingDnsFuture(state);
    }

    let spawn_result = std::thread::Builder::new()
        .name("fandhe-browser-dns-resolve".to_string())
        .spawn({
            let state = Arc::clone(&state);
            move || {
                let outcome = (host.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(|addrs| addrs.collect::<Vec<SocketAddr>>())
                    .map_err(|source| Box::new(source) as DnsBoxError);
                release_dns_resolution_slot();
                let mut inner = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.result = Some(outcome);
                if let Some(waker) = inner.waker.take() {
                    waker.wake();
                }
            }
        });
    if let Err(source) = spawn_result {
        release_dns_resolution_slot();
        let mut inner = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.result = Some(Err(Box::new(source) as DnsBoxError));
    }
    BlockingDnsFuture(state)
}

impl Fetcher {
    /// `options` を検証したうえで `Fetcher` を構築する。
    ///
    /// - crypto provider（ring）をプロセス内で 1 回だけ install する
    /// - `options` の不正な値（ゼロのタイムアウト・ゼロの本文上限）を
    ///   [`Error::InvalidInput`] として拒否する
    /// - タイムアウト・接続タイムアウト・リダイレクトポリシー・User-Agent を
    ///   設定し、環境変数のプロキシ設定は `no_proxy()` で無視する（挙動の
    ///   決定性のため。プロキシ対応自体は将来仕様）
    pub fn new(options: FetchOptions) -> Result<Self> {
        ensure_crypto_provider_installed();
        options.validate()?;

        let max_redirects = options.max_redirects;
        let allow_private_network_access = options.allow_private_network_access;
        let redirect_policy = Policy::custom(move |attempt| {
            if attempt.previous().len() > max_redirects {
                return attempt.error(RedirectMarker::TooManyRedirects {
                    limit: max_redirects,
                });
            }
            let scheme = attempt.url().scheme().to_string();
            match scheme.as_str() {
                "http" | "https" => {}
                other => {
                    return attempt.error(RedirectMarker::DisallowedScheme {
                        scheme: other.to_string(),
                    });
                }
            }
            // `SafeResolver` は DNS 名の解決結果しか検証しない（IP リテラル
            // host は hyper-util が DNS 解決を経由せず直接使うため）。
            // リダイレクト先の host が IP リテラルの場合はここで直接検証する
            // （`reject_disallowed_address` のドキュメント参照）。
            if let Some(ip) = url_ip_literal(attempt.url())
                && !allow_private_network_access
                && is_disallowed_address(ip)
            {
                return attempt.error(RedirectMarker::DisallowedAddress {
                    address: ip.to_string(),
                });
            }
            attempt.follow()
        });

        let resolver = Arc::new(SafeResolver {
            allow_private_network_access: options.allow_private_network_access,
        });
        let client = ClientBuilder::new()
            .timeout(options.timeout)
            .connect_timeout(options.connect_timeout)
            .redirect(redirect_policy)
            .no_proxy()
            .dns_resolver(resolver)
            .user_agent(concat!("fandhe-browser/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|source| map_reqwest_error(source, &options))?;

        Ok(Self { client, options })
    }

    /// `url` を GET で取得する。
    ///
    /// 呼び出し元との契約: 本メソッドはランタイムを内蔵しない async fn だが、
    /// 内部の `reqwest::Client`（hyper 経由）は tokio 前提であり、呼び出し元
    /// （`cdp` の `Page.navigate` ハンドラ・将来の CLI 等）が tokio ランタイム
    /// 上で await する必要がある（tokio ランタイムの外で呼ぶと失敗する）。
    ///
    /// - `url` を [`reqwest::Url`] として解析し、`http`/`https` 以外の
    ///   scheme（`file:`・`data:` 等）は送信前に [`Error::DisallowedScheme`]
    ///   として拒否する（解析失敗は [`Error::InvalidInput`]）
    /// - `url` の host が IP リテラルで、かつ
    ///   `FetchOptions::allow_private_network_access` が `false`（既定）の
    ///   ときループバック・プライベートアドレス等であれば、送信前に
    ///   [`Error::DisallowedAddress`] として拒否する
    ///   （[`reject_disallowed_address`]。security.md「SSRF」）
    /// - `Content-Length` ヘッダが `max_body_bytes` を超える場合は本文を
    ///   読まずに [`Error::ResponseTooLarge`] を返す（(a) 事前検査）
    /// - ヘッダが無い・嘘の値でも、`chunk()` による逐次読み込み中に蓄積
    ///   サイズを `checked_add` で検査し、上限超過時点で同じエラーを返す
    ///   （(b) ストリーミング検査）
    /// - 4xx/5xx は `Err` にせず `Ok` で返す（404 ページ等もパース対象に
    ///   なり得るため、判断は呼び出し側に委ねる）
    pub async fn get(&self, url: &str) -> Result<FetchResponse> {
        let parsed = Url::parse(url).map_err(|source| Error::InvalidInput {
            message: format!("invalid URL: {source}"),
        })?;
        reject_disallowed_scheme(&parsed)?;
        reject_disallowed_address(&parsed, self.options.allow_private_network_access)?;

        let response = self
            .client
            .get(parsed)
            .send()
            .await
            .map_err(|source| map_reqwest_error(source, &self.options))?;

        let status = response.status().as_u16();
        reject_unresolved_disallowed_redirect(&response)?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if let Some(content_length) = response.content_length()
            && content_length > self.options.max_body_bytes
        {
            return Err(Error::ResponseTooLarge {
                limit: self.options.max_body_bytes,
            });
        }

        let mut final_url = response.url().clone();
        // userinfo（`user:pass@host`）はリダイレクト後の URL にも残り得る
        // ため、公開する `final_url` からは常に取り除く（security.md
        // 秘密情報混入防止）。`set_username`/`set_password` は既に空の
        // 場合も含め常に `Ok` を返す入力（空文字列・`None`）のみを渡す。
        let _ = final_url.set_username("");
        let _ = final_url.set_password(None);

        // 初期確保は宣言された `Content-Length` をそのまま信用せず、固定の
        // 小さい上限（`INITIAL_BODY_CAPACITY_LIMIT`）で頭打ちにする。事前検査
        // （上の `content_length` チェック）は「宣言値が上限を超えていないか」
        // しか見ないため、上限ぎりぎりの巨大な値を宣言されると、そのまま
        // `with_capacity` に渡すと確保だけで数十 MiB を要求されかねない
        // （security.md「不安全な設計」: 宣言だけによるアロケーション増幅）。
        let capacity = response
            .content_length()
            .unwrap_or(0)
            .min(self.options.max_body_bytes)
            .min(INITIAL_BODY_CAPACITY_LIMIT);
        let mut body = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));

        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| map_reqwest_error(source, &self.options))?
        {
            let new_len = body
                .len()
                .checked_add(chunk.len())
                .ok_or(Error::ResponseTooLarge {
                    limit: self.options.max_body_bytes,
                })?;
            // `new_len`（`usize`）と `max_body_bytes`（`u64`）の比較は、
            // 32bit ターゲットで `usize` が `u64` より小さい可能性があるため
            // `as u64` によるキャストではなく `u64::try_from` を使う
            // （変換失敗時もアロケーション増幅を避けるため安全側の `Err` に
            // 倒す。coding-rust.md「エラーハンドリング」: checked 演算）。
            let new_len_u64 = u64::try_from(new_len).map_err(|_| Error::ResponseTooLarge {
                limit: self.options.max_body_bytes,
            })?;
            if new_len_u64 > self.options.max_body_bytes {
                return Err(Error::ResponseTooLarge {
                    limit: self.options.max_body_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        Ok(FetchResponse {
            status,
            final_url: final_url.to_string(),
            content_type,
            body,
        })
    }
}

/// URL の scheme を検証し、`http`/`https` 以外を拒否する
/// （security.md「SSRF」: `file:`・`data:` 等の無検証アクセスを避ける）。
fn reject_disallowed_scheme(url: &Url) -> Result<()> {
    match url.scheme() {
        "http" | "https" => Ok(()),
        other => Err(Error::DisallowedScheme {
            scheme: other.to_string(),
        }),
    }
}

/// `url` の host が IP リテラルの場合に取り出す。ホスト名（DNS 名）の場合は
/// `None`（[`SafeResolver`] が DNS 解決時に検証するため、ここでは扱わない）。
///
/// `url` クレート（`reqwest::Url` の実体）は `pub(crate)`／`reqwest` が
/// 型を再公開していない `url::Host` を返す `Url::host()` の代わりに、
/// `Url::host_str()`（IPv6 は `[` `]` 付き。`Ipv6Addr` は `[`/`]` を含む
/// 文字列を `FromStr` で受理しないため `trim_matches` で除去してから解析
/// する）を経由して `IpAddr::parse` する。ホスト名（DNS 名）は
/// `IpAddr::parse` が `Err` になるため自然に `None` へ落ちる。
fn url_ip_literal(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed.parse().ok()
}

/// `url` の host が IP リテラルで、かつループバック・プライベートアドレス
/// 等の内部アドレスである場合に拒否する（security.md「SSRF」）。
///
/// [`SafeResolver`] は DNS 名の解決結果を検証するが、hyper-util の
/// `HttpConnector` は host が既に IP リテラルであれば DNS 解決を経由せず
/// 直接そのアドレスへ接続する（`dns::SocketAddrs::try_parse` が成功する
/// 経路）ため、[`Resolve::resolve`] は一切呼ばれない。そのため IP リテラル
/// host は本関数で個別に検証する必要がある（`Fetcher::get` の初回リクエスト
/// と `Policy::custom` のリダイレクト先の両方から呼ぶ）。
fn reject_disallowed_address(url: &Url, allow_private_network_access: bool) -> Result<()> {
    if allow_private_network_access {
        return Ok(());
    }
    if let Some(ip) = url_ip_literal(url)
        && is_disallowed_address(ip)
    {
        return Err(Error::DisallowedAddress {
            address: ip.to_string(),
        });
    }
    Ok(())
}

/// `Fetcher::new` のリダイレクトポリシー（`Policy::custom`）が発火しない
/// リダイレクトを検出し、scheme を検証する。
///
/// 内部で使う `reqwest`（`tower-http` の `follow_redirect` ミドルウェア）は、
/// `Location` ヘッダをリクエスト URL に対して解決した結果が
/// `http::Uri`（authority 必須）としてパースできない場合（例:
/// `file:///etc/passwd` のように authority を持たない URL）、リダイレクト
/// ポリシーを一切呼び出さずに、その 3xx 応答をそのまま最終応答として返す。
/// このため `Policy::custom` 内の scheme チェックだけでは、この種の
/// リダイレクト先を拒否できない（未解決のまま `Ok` になってしまう）。
///
/// ここでは、応答が 3xx かつ `Location` ヘッダを持つ場合に限り、リクエスト
/// URL（`response.url()`）を基準に `Location` を手動で解決し、scheme を
/// 検証する。`http`/`https` の場合は `Policy::custom` 側で正常に追跡される
/// はずなので、ここに到達すること自体が想定外だが、念のため許可する
/// （二重チェックにしかならず、実害はない）。
fn reject_unresolved_disallowed_redirect(response: &reqwest::Response) -> Result<()> {
    if !(300..400).contains(&response.status().as_u16()) {
        return Ok(());
    }
    let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
        return Ok(());
    };
    let Ok(location_str) = location.to_str() else {
        return Ok(());
    };
    let Ok(location_url) = response.url().join(location_str) else {
        return Ok(());
    };
    reject_disallowed_scheme(&location_url)
}

/// `reqwest::Error` を [`Error`] へ写像する。
///
/// - タイムアウト（`is_timeout()`）は [`Error::Timeout`] にする。全体
///   タイムアウトと接続タイムアウトを reqwest 側は区別しないため、
///   `options.timeout` を代表値として使う
/// - リダイレクト起因のエラーは、`source()` を辿って [`RedirectMarker`] へ
///   `downcast_ref` できればそれを対応する `Error` に写像する。マーカーが
///   見つからない場合（`RedirectMarker` を判別できないリダイレクトエラー）は
///   `TooManyRedirects` へ憶測で丸めず、以下の `Network` 経路へ流す
///   （`TooManyRedirects`/`DisallowedScheme` は `Policy::custom`
///   （[`RedirectMarker`] downcast 経路）からしか出さないことで、レビューで
///   両者を区別できるようにする）
/// - [`SafeResolver`] が内部アドレスを検出したエラーは、同じ `source()` 走査
///   で [`DisallowedAddressMarker`] へ `downcast_ref` できれば
///   [`Error::DisallowedAddress`] に写像する（`reqwest` は DNS リゾルバの
///   エラーを `DnsError`（`source()` で内側のエラーへ連鎖する）でラップして
///   返すため、`RedirectMarker` と同じ走査ループで検出できる）
/// - [`resolve_blocking`] が同時実行の DNS 解決スレッド上限に達したエラーは、
///   同じ走査で [`TooManyDnsResolutionsMarker`] へ `downcast_ref` できれば
///   [`Error::TooManyConcurrentDnsResolutions`] に写像する（PR #430
///   コードレビュー指摘 P0）
/// - それ以外は `without_url()` で URL（userinfo・クエリを含み得る）を
///   取り除いたメッセージを [`Error::Network`] に詰める（外部型
///   `reqwest::Error` 自体は保持しない）
fn map_reqwest_error(source: reqwest::Error, options: &FetchOptions) -> Error {
    if source.is_timeout() {
        return Error::Timeout {
            limit: options.timeout,
        };
    }

    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&source);
    while let Some(err) = cause {
        if let Some(marker) = err.downcast_ref::<RedirectMarker>() {
            return match marker {
                RedirectMarker::TooManyRedirects { limit } => {
                    Error::TooManyRedirects { limit: *limit }
                }
                RedirectMarker::DisallowedScheme { scheme } => Error::DisallowedScheme {
                    scheme: scheme.clone(),
                },
                RedirectMarker::DisallowedAddress { address } => Error::DisallowedAddress {
                    address: address.clone(),
                },
            };
        }
        if let Some(marker) = err.downcast_ref::<DisallowedAddressMarker>() {
            return Error::DisallowedAddress {
                address: marker.address.to_string(),
            };
        }
        if err.downcast_ref::<TooManyDnsResolutionsMarker>().is_some() {
            return Error::TooManyConcurrentDnsResolutions {
                limit: MAX_CONCURRENT_DNS_RESOLUTIONS,
            };
        }
        cause = err.source();
    }

    Error::Network {
        message: source.without_url().to_string(),
    }
}

/// [`Fetcher::get`] の結果。
///
/// `#[non_exhaustive]` により、後続タスクでのフィールド追加（レスポンス
/// ヘッダ全体の保持等）が呼び出し側の構造体リテラル・非網羅 `match` を
/// 破壊的変更にしない（REPAIR-4）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchResponse {
    status: u16,
    final_url: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl FetchResponse {
    /// HTTP ステータスコード。4xx/5xx でも `Err` にはならず、ここに格納
    /// される（呼び出し側がハンドリングを判断する）。
    pub fn status(&self) -> u16 {
        self.status
    }

    /// リダイレクト後の最終 URL（userinfo は除去済み）。
    pub fn final_url(&self) -> &str {
        &self.final_url
    }

    /// `Content-Type` レスポンスヘッダの値（存在する場合）。
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// レスポンス本文の生バイト列。文字コード判定（ビヘイビア CORE-5 (7)）
    /// は本 crate の範囲外のため、呼び出し側または `parse` モジュール
    /// （TASK-24.4・#38）が行う想定。
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// レスポンス本文を UTF-8 として非可逆変換した文字列（不正なバイト列は
    /// 置換文字に置き換える）。厳密な文字コード判定（`charset` ヘッダ・
    /// meta タグ由来）は範囲外の将来仕様であり、これは参考用の簡易変換に
    /// 留まる。
    pub fn body_text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    /// [`ACTIVE_DNS_RESOLUTIONS`]（プロセス全体で共有するグローバル状態）に
    /// 依存するテスト同士を直列化するためのテスト専用ロック。`cargo test`
    /// は同一バイナリ内のテストを既定で並列実行するため、このロックなしで
    /// 同時実行数の上限到達を検証すると、他の DNS 解決テストが偶発的に
    /// 上限超過と誤判定されてフレーキーになる（PR #430 コードレビュー
    /// 指摘 P0 の回帰テストを安定させるため）。
    static DNS_CONCURRENCY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `url_ip_literal` が IPv4
    /// リテラル host を `IpAddr` として取り出す。
    #[test]
    fn core_1_url_ip_literal_extracts_ipv4() {
        let url = Url::parse("http://127.0.0.1:8080/").expect("valid URL");
        assert_eq!(
            url_ip_literal(&url),
            Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
        );
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `url_ip_literal` が IPv6
    /// リテラル host（`[` `]` 付き）を `IpAddr` として取り出す。
    #[test]
    fn core_1_url_ip_literal_extracts_ipv6() {
        let url = Url::parse("http://[::1]:8080/").expect("valid URL");
        assert_eq!(url_ip_literal(&url), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `url_ip_literal` は
    /// ホスト名（DNS 名）に対しては `None` を返す（[`SafeResolver`] が別途
    /// 検証するため、ここでは扱わない）。
    #[test]
    fn core_1_url_ip_literal_is_none_for_domain_name() {
        let url = Url::parse("http://example.com/").expect("valid URL");
        assert_eq!(url_ip_literal(&url), None);
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `is_disallowed_address` が
    /// ループバック・プライベート・リンクローカル・未指定・ブロードキャスト
    /// ・ドキュメント用の各 IPv4 アドレスを内部アドレスとして判定する
    /// （security.md「SSRF」）。
    #[test]
    fn core_1_is_disallowed_address_detects_ipv4_categories() {
        let disallowed = [
            "127.0.0.1",       // loopback
            "10.0.0.1",        // private (10/8)
            "172.16.0.1",      // private (172.16/12)
            "192.168.1.1",     // private (192.168/16)
            "169.254.1.1",     // link-local
            "0.0.0.0",         // unspecified
            "255.255.255.255", // broadcast
            "192.0.2.1",       // documentation (TEST-NET-1)
        ];
        for addr in disallowed {
            let ip: IpAddr = addr.parse().expect("valid IPv4 literal");
            assert!(is_disallowed_address(ip), "{addr} should be disallowed");
        }
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘。SSRF P0 再指摘）:
    /// `is_disallowed_address` は `std::net::Ipv4Addr` の安定 API が
    /// カバーしない特殊用途ブロック（共有アドレス空間・ベンチマーク用等）
    /// も内部アドレスとして拒否する（`is_disallowed_ipv4` のドキュメント
    /// 参照）。
    #[test]
    fn core_1_is_disallowed_address_detects_ipv4_special_purpose_blocks() {
        let disallowed = [
            "100.64.0.1",      // 100.64.0.0/10 共有アドレス空間（CGN）
            "100.127.255.254", // 100.64.0.0/10 の範囲末尾
            "198.18.0.1",      // 198.18.0.0/15 ベンチマーク用
            "198.19.255.254",  // 198.18.0.0/15 の範囲末尾
            "192.0.0.1",       // 192.0.0.0/24 IETF Protocol Assignments
            "192.88.99.1",     // 192.88.99.0/24 6to4 Relay Anycast
            "224.0.0.1",       // マルチキャスト
            "240.0.0.1",       // 予約済み
            "0.0.0.1",         // 0.0.0.0/8 ("this network")
        ];
        for addr in disallowed {
            let ip: IpAddr = addr.parse().expect("valid IPv4 literal");
            assert!(is_disallowed_address(ip), "{addr} should be disallowed");
        }
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `is_disallowed_address` は
    /// グローバルに到達可能な IPv4 アドレスを内部アドレスとして誤判定しない。
    #[test]
    fn core_1_is_disallowed_address_allows_global_ipv4() {
        let allowed = ["8.8.8.8", "1.1.1.1", "93.184.216.34"];
        for addr in allowed {
            let ip: IpAddr = addr.parse().expect("valid IPv4 literal");
            assert!(!is_disallowed_address(ip), "{addr} should be allowed");
        }
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `is_disallowed_address` は
    /// IPv6 のループバック・ユニークローカル・リンクローカル・非推奨の
    /// サイトローカル（`fec0::/10`。PR #430 コードレビュー指摘。RFC 3879 で
    /// 非推奨だが到達可能な環境が残るため引き続き拒否する）、および
    /// v4-mapped アドレス（`::ffff:127.0.0.1`）に埋め込まれた IPv4 側の
    /// 分類のいずれもすり抜けない。
    #[test]
    fn core_1_is_disallowed_address_detects_ipv6_categories() {
        let disallowed = [
            "::1",              // loopback
            "::",               // unspecified
            "fc00::1",          // unique local
            "fe80::1",          // unicast link-local
            "fec0::1",          // 非推奨サイトローカル（fec0::/10 の先頭）
            "feff:ffff::1",     // 非推奨サイトローカル（fec0::/10 の末尾）
            "::ffff:127.0.0.1", // v4-mapped loopback
            "::ffff:10.0.0.1",  // v4-mapped private
        ];
        for addr in disallowed {
            let ip: IpAddr = addr.parse().expect("valid IPv6 literal");
            assert!(is_disallowed_address(ip), "{addr} should be disallowed");
        }
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `is_disallowed_address` は
    /// グローバルに到達可能な IPv6 アドレスを内部アドレスとして誤判定しない。
    #[test]
    fn core_1_is_disallowed_address_allows_global_ipv6() {
        let ip: IpAddr = "2606:4700:4700::1111".parse().expect("valid IPv6 literal");
        assert!(!is_disallowed_address(ip), "global IPv6 should be allowed");
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `SafeResolver::resolve` は
    /// `allow_private_network_access == false`（既定）のとき、host が
    /// ループバックの IP リテラルであれば
    /// `DisallowedAddressMarker`（`downcast_ref` で判別できるマーカー）を
    /// 返す。`Fetcher::get` はこのリゾルバを初回リクエスト・リダイレクト先の
    /// 両方の接続で使う（[`Fetcher::new`] の `ClientBuilder::dns_resolver`）
    /// ため、この単体テストは「接続の種類（初回かリダイレクト先か）に関係
    /// なく同一の判定になる」ことをリゾルバ単体で保証する。
    #[tokio::test]
    // DNS_CONCURRENCY_TEST_LOCK は `#[tokio::test]` の既定（単一スレッド
    // ランタイム）で、同一テスト内に他タスクが存在しないため await を挟んでも
    // デッドロックしない（テスト間の直列化のみが目的）。
    #[allow(clippy::await_holding_lock)]
    async fn core_1_safe_resolver_rejects_loopback_by_default() {
        let _guard = DNS_CONCURRENCY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resolver = SafeResolver {
            allow_private_network_access: false,
        };
        let name: Name = "127.0.0.1".parse().expect("valid resolver name");
        // `Addrs`（`Ok` 側の型）は `Iterator` トレイトオブジェクトで `Debug`
        // を実装しないため、`expect_err` は使えず `match` で判定する。
        match resolver.resolve(name).await {
            Ok(_) => panic!("既定ではループバックの解決結果は拒否されるはず"),
            Err(err) => assert!(
                err.downcast_ref::<DisallowedAddressMarker>().is_some(),
                "unexpected error: {err}"
            ),
        }
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘）: `SafeResolver::resolve` は
    /// `allow_private_network_access == true` のとき、同じループバックの
    /// host を許可する（opt-in の単体確認）。
    #[tokio::test]
    // DNS_CONCURRENCY_TEST_LOCK は `#[tokio::test]` の既定（単一スレッド
    // ランタイム）で、同一テスト内に他タスクが存在しないため await を挟んでも
    // デッドロックしない（テスト間の直列化のみが目的）。
    #[allow(clippy::await_holding_lock)]
    async fn core_1_safe_resolver_allows_loopback_when_opted_in() {
        let _guard = DNS_CONCURRENCY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resolver = SafeResolver {
            allow_private_network_access: true,
        };
        let name: Name = "127.0.0.1".parse().expect("valid resolver name");
        let mut addrs = resolver
            .resolve(name)
            .await
            .expect("opt-in 時はループバックの解決結果が許可されるはず");
        assert_eq!(
            addrs.next().map(|addr| addr.ip()),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘 P1）: `resolve_blocking` は
    /// `std::net::ToSocketAddrs` による同期 DNS 解決を非同期に完了させ、
    /// 成功時は解決結果（`SocketAddr` の一覧）を返す（[`SafeResolver`] の
    /// ドキュメント「解決専用の OS スレッドへオフロードする」の機能確認）。
    #[tokio::test]
    // DNS_CONCURRENCY_TEST_LOCK は `#[tokio::test]` の既定（単一スレッド
    // ランタイム）で、同一テスト内に他タスクが存在しないため await を挟んでも
    // デッドロックしない（テスト間の直列化のみが目的）。
    #[allow(clippy::await_holding_lock)]
    async fn core_1_resolve_blocking_resolves_ip_literal_host() {
        let _guard = DNS_CONCURRENCY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let addrs = resolve_blocking("127.0.0.1".to_string())
            .await
            .expect("127.0.0.1 の解決は成功するはず");
        assert!(
            addrs
                .iter()
                .any(|addr| addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "127.0.0.1 の解決結果に自身のアドレスが含まれるはず: {addrs:?}"
        );
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘 P1・Windows CI 失敗の修正）:
    /// `resolve_blocking` は空文字列 host を明示的に拒否し `Err` を返す
    /// （`panic` しない。[`SafeResolver::resolve`] が `?` でそのまま伝播する
    /// 経路の確認）。Unix/macOS の `getaddrinfo` は空文字列 host に対して
    /// `Err` を返すが、Windows は同じ入力に対してローカルインターフェース
    /// のアドレス群を返すため、OS の解決結果に委ねると 3 OS で挙動が
    /// 食い違う（`resolve_blocking` のドキュメント参照）。この明示チェックに
    /// より 3 OS で同一の `Err` になることをここで確認する。
    #[tokio::test]
    // DNS_CONCURRENCY_TEST_LOCK は `#[tokio::test]` の既定（単一スレッド
    // ランタイム）で、同一テスト内に他タスクが存在しないため await を挟んでも
    // デッドロックしない（テスト間の直列化のみが目的）。
    #[allow(clippy::await_holding_lock)]
    async fn core_1_resolve_blocking_returns_err_on_resolution_failure() {
        let _guard = DNS_CONCURRENCY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = resolve_blocking(String::new()).await;
        assert!(
            result.is_err(),
            "空文字列 host の解決は Err になるはず: {result:?}"
        );
    }

    /// CORE-1（#36。PR #430 コードレビュー指摘 P0）: 同時実行の DNS 解決数が
    /// 上限（[`MAX_CONCURRENT_DNS_RESOLUTIONS`]）に達している間、
    /// `resolve_blocking` は新規スレッドを生成せず、
    /// `TooManyDnsResolutionsMarker`（`downcast_ref` で判別できるマーカー）
    /// を伴う `Err` を即座に返す（無制限なスレッド生成による DoS を防ぐ。
    /// security.md「不安全な設計」）。
    #[tokio::test]
    // DNS_CONCURRENCY_TEST_LOCK は `#[tokio::test]` の既定（単一スレッド
    // ランタイム）で、同一テスト内に他タスクが存在しないため await を挟んでも
    // デッドロックしない（テスト間の直列化のみが目的）。
    #[allow(clippy::await_holding_lock)]
    async fn core_1_resolve_blocking_rejects_when_concurrency_limit_reached() {
        let _guard = DNS_CONCURRENCY_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut acquired = 0usize;
        while try_acquire_dns_resolution_slot() {
            acquired += 1;
        }
        assert_eq!(
            acquired, MAX_CONCURRENT_DNS_RESOLUTIONS,
            "テスト開始時点で他の枠が確保されていないはず"
        );

        let result = resolve_blocking("127.0.0.1".to_string()).await;
        match result {
            Ok(addrs) => panic!("上限到達時は解決を試みないはず: {addrs:?}"),
            Err(err) => assert!(
                err.downcast_ref::<TooManyDnsResolutionsMarker>().is_some(),
                "unexpected error: {err}"
            ),
        }

        for _ in 0..acquired {
            release_dns_resolution_slot();
        }
    }

    /// CORE-1（#37。TASK-24.3・MS-1）: `FetchOptions::default()`（`FetchOptions::new()`
    /// と同義）が具体値の既定値を返す（REPAIR-4「戻り値は具体値で書く」の
    /// 単体確認。既存の結合テストはこれらの既定値を前提にするだけで、値
    /// そのものは検証していなかった）。
    #[test]
    fn core_1_fetch_options_default_values() {
        let options = FetchOptions::default();
        assert_eq!(options.timeout, Duration::from_secs(30));
        assert_eq!(options.connect_timeout, Duration::from_secs(10));
        assert_eq!(options.max_redirects, 10);
        assert_eq!(options.max_body_bytes, 16 * 1024 * 1024);
        assert!(!options.allow_private_network_access);

        let via_new = FetchOptions::new();
        assert_eq!(via_new.timeout, options.timeout);
        assert_eq!(via_new.connect_timeout, options.connect_timeout);
        assert_eq!(via_new.max_redirects, options.max_redirects);
        assert_eq!(via_new.max_body_bytes, options.max_body_bytes);
        assert_eq!(
            via_new.allow_private_network_access,
            options.allow_private_network_access
        );
    }

    /// CORE-1（#37。TASK-24.3・MS-1）: 各 `with_*` ビルダーが対応フィールドを
    /// 指定した具体値に設定する（他のフィールドは既定値のまま変わらない）。
    #[test]
    fn core_1_fetch_options_builders_set_fields() {
        let options = FetchOptions::new()
            .with_timeout(Duration::from_millis(1234))
            .with_connect_timeout(Duration::from_millis(567))
            .with_max_redirects(3)
            .with_max_body_bytes(2048)
            .with_allow_private_network_access(true);

        assert_eq!(options.timeout, Duration::from_millis(1234));
        assert_eq!(options.connect_timeout, Duration::from_millis(567));
        assert_eq!(options.max_redirects, 3);
        assert_eq!(options.max_body_bytes, 2048);
        assert!(options.allow_private_network_access);
    }

    /// CORE-1（#37。TASK-24.3・MS-1）: `FetchOptions::validate` はゼロ値の
    /// `timeout`・`connect_timeout`・`max_body_bytes` それぞれについて
    /// `Error::InvalidInput` を返し、`message` にフィールド名を含む
    /// （既存の結合テスト `core_1_fetcher_new_rejects_zero_*` はバリアントの
    /// 種類のみ確認していたため、メッセージの具体値でここを補強する）。
    #[test]
    fn core_1_validate_rejects_zero_values_with_message() {
        let cases: [(FetchOptions, &str); 3] = [
            (FetchOptions::new().with_timeout(Duration::ZERO), "timeout"),
            (
                FetchOptions::new().with_connect_timeout(Duration::ZERO),
                "connect_timeout",
            ),
            (FetchOptions::new().with_max_body_bytes(0), "max_body_bytes"),
        ];
        for (options, expected_field) in cases {
            match options.validate() {
                Ok(()) => panic!("{expected_field} = 0 は InvalidInput になるはず"),
                Err(Error::InvalidInput { message }) => {
                    assert!(
                        message.contains(expected_field),
                        "message は {expected_field:?} を含むはず: {message}"
                    );
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
    }

    /// CORE-1（#37。TASK-24.3・MS-1）: `reject_disallowed_scheme` は `http`/`https`
    /// を許可し、それ以外（`file`・`data`・`javascript`・`ftp`・`ws`・`about`）
    /// を `Error::DisallowedScheme { scheme }` として拒否する
    /// （テーブル駆動。security.md「SSRF」）。
    #[test]
    fn core_1_reject_disallowed_scheme_table() {
        let allowed = ["http://example.com/", "https://example.com/"];
        for url in allowed {
            let parsed = Url::parse(url).expect("valid URL");
            assert!(
                reject_disallowed_scheme(&parsed).is_ok(),
                "{url} は許可されるはず"
            );
        }

        let disallowed = [
            ("file:///etc/passwd", "file"),
            ("data:text/html,x", "data"),
            ("javascript:alert(1)", "javascript"),
            ("ftp://example.com/", "ftp"),
            ("ws://example.com/", "ws"),
            ("about:blank", "about"),
        ];
        for (url, expected_scheme) in disallowed {
            let parsed = Url::parse(url).expect("valid URL");
            match reject_disallowed_scheme(&parsed) {
                Ok(()) => panic!("{url} は拒否されるはず"),
                Err(Error::DisallowedScheme { scheme }) => {
                    assert_eq!(scheme, expected_scheme, "unexpected scheme for {url}");
                }
                Err(other) => panic!("unexpected error for {url}: {other:?}"),
            }
        }
    }

    /// CORE-1（#37。TASK-24.3・MS-1）: `reject_disallowed_address` は
    /// `allow_private_network_access` の値で挙動を切り替える。
    /// - `false`（既定）: ループバック IP リテラルを `DisallowedAddress` で拒否
    /// - `true`（opt-in）: 同じループバック IP リテラルを許可
    /// - host が DNS 名（IP リテラルでない）の場合は `allow` の値に関わらず
    ///   `Ok`（[`SafeResolver`] が解決結果側で別途検証するため、この関数の
    ///   責務範囲外であることの確認）
    #[test]
    fn core_1_reject_disallowed_address_bypassed_when_opted_in() {
        let loopback = Url::parse("http://127.0.0.1:8080/").expect("valid URL");
        match reject_disallowed_address(&loopback, false) {
            Ok(()) => panic!("既定ではループバック IP リテラルは拒否されるはず"),
            Err(Error::DisallowedAddress { address }) => {
                assert_eq!(address, "127.0.0.1");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        assert!(
            reject_disallowed_address(&loopback, true).is_ok(),
            "opt-in 時はループバック IP リテラルが許可されるはず"
        );

        let domain = Url::parse("http://example.com/").expect("valid URL");
        assert!(
            reject_disallowed_address(&domain, false).is_ok(),
            "DNS 名の host は allow=false でも Ok（SafeResolver が別途検証する）はず"
        );
        assert!(
            reject_disallowed_address(&domain, true).is_ok(),
            "DNS 名の host は allow=true でも Ok のはず"
        );
    }
}

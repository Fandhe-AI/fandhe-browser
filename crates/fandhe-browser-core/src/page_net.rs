//! ページ内 `fetch` / `XMLHttpRequest` の要求キューと上限（TASK-111・MS-6・Issue #783・
//! ビヘイビア `JS-8`・`JS-6`）。
//!
//! ページ JS の GET 取得を、JS エンジンの外（ネットワーク）へ直接出さず、「要求を受理して
//! キューへ積む → ランナーが既存の [`crate::fetch::Fetcher`] だけで取得 → 結果を JS へ配送」
//! という経路に限定するための状態と検証を持つ。HTTP クライアントは持たない（SSRF 対策の
//! scheme・内部アドレス・リダイレクト検査は Fetcher に一元化。`SEC-2`）。
//!
//! 呼び出し元は次の 2 か所。
//!
//! - [`crate::dom_bridge::DomBridge`] の `netRequest` 等の op（shim `net.js` から呼ばれる。
//!   URL・メソッド・本文有無はページ由来の untrusted 入力としてここで検証する）
//! - [`crate::page_runner`] のイベントループ段（保留中の要求を 1 件ずつ取り出して
//!   Fetcher で取得し、`__fandheNetDeliver(id)` で配送する）
//!
//! # 上限（`JS-6`。超過は reject と診断で表す。ページは打ち切らない）
//!
//! | 上限 | 既定 | 境界 |
//! | ---- | ---- | ---- |
//! | 要求件数 | 50 | 50 件目まで受理し、51 件目以降を reject |
//! | 累計レスポンス本文 | 4 MiB | ちょうど上限までは受理し、超える応答・以降の要求を reject |
//! | 1 件の本文 | Fetcher の `max_body_bytes` | Fetcher が受信中に強制する |
//!
//! # スタブ・簡易実装の残り（REPAIR-3）
//!
//! - GET 以外のメソッド・要求ボディ・同期 XHR は reject する（`UnsupportedMethod` 等）。
//! - 別オリジンの要求・リダイレクト先は reject する（`CrossOrigin`。CORS 実装までの暫定。`SEC-2`）。
//! - ヘッダ・Cookie・認証情報は送らない（Fetcher の既定のみ）。`fetch` の `init` の
//!   `headers` / `credentials` 等は無視し、件数だけ診断に残す。
//! - CORS 判定・Content-Type と charset の判定（`CORE-5` (7)）・ネットワーク待ちの独立した
//!   時間予算は未実装。応答は仮想時間 0 の遅延で、同時刻のタイマーより先に配送する近似。

use std::collections::VecDeque;

use reqwest::Url;

use crate::error::Error;

/// 要求件数の既定上限（`JS-6`）。
pub const DEFAULT_MAX_PAGE_NET_REQUESTS: usize = 50;
/// 累計レスポンス本文バイト数の既定上限（4 MiB。`JS-6`）。
pub const DEFAULT_MAX_PAGE_NET_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// 配送時に JS へ渡す本文チャンクの最大バイト数（子プロセス経路のフレーム上限より十分小さい）。
pub const NET_BODY_CHUNK_BYTES: usize = 64 * 1024;

/// ページ内ネットワーク要求の上限設定（`JS-6`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageNetLimits {
    max_requests: usize,
    max_response_bytes: u64,
}

impl Default for PageNetLimits {
    fn default() -> Self {
        Self {
            max_requests: DEFAULT_MAX_PAGE_NET_REQUESTS,
            max_response_bytes: DEFAULT_MAX_PAGE_NET_RESPONSE_BYTES,
        }
    }
}

impl PageNetLimits {
    /// 要求件数の上限を設定する。
    #[must_use]
    pub fn with_max_requests(mut self, limit: usize) -> Self {
        self.max_requests = limit;
        self
    }

    /// 累計レスポンス本文バイト数の上限を設定する。
    #[must_use]
    pub fn with_max_response_bytes(mut self, limit: u64) -> Self {
        self.max_response_bytes = limit;
        self
    }

    /// 要求件数の上限。
    pub fn max_requests(&self) -> usize {
        self.max_requests
    }

    /// 累計レスポンス本文バイト数の上限。
    pub fn max_response_bytes(&self) -> u64 {
        self.max_response_bytes
    }
}

/// 要求の発行元 API。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetRequestKind {
    /// `fetch()`。
    Fetch,
    /// `XMLHttpRequest`。
    Xhr,
}

impl NetRequestKind {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Xhr => "xhr",
        }
    }
}

/// reject の理由（URL・本文は持たない。クエリにトークンを含み得るため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetRejectKind {
    /// GET 以外のメソッド。
    UnsupportedMethod,
    /// 要求ボディ付き。
    RequestBody,
    /// 同期 XHR。
    SyncXhr,
    /// 相対 URL を解決できない・URL が不正・http / https 以外。
    InvalidUrl,
    /// URL に userinfo（`user:pass@`）を含む。
    Credentials,
    /// ページと別オリジン（要求先またはリダイレクト先）。CORS 未実装の間は応答を公開しない。
    CrossOrigin,
    /// 要求件数の上限超過。
    RequestLimit,
    /// 累計レスポンス本文バイト数の上限超過。
    ResponseBytesLimit,
    /// 1 件の本文が Fetcher の上限を超えた。
    ResponseTooLarge,
    /// ページの実時間上限に達した。
    WallTime,
    /// Fetcher が scheme を拒否した（`file:` 等・リダイレクト先を含む）。
    DisallowedScheme,
    /// Fetcher が内部アドレスを拒否した（ループバック・プライベート等・リダイレクト先を含む）。
    DisallowedAddress,
    /// リダイレクト回数の上限超過。
    TooManyRedirects,
    /// Fetcher のタイムアウト。
    Timeout,
    /// ネットワークエラー。
    Network,
    /// 上記以外の取得失敗（fail-closed）。
    Other,
}

impl NetRejectKind {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnsupportedMethod => "unsupported_method",
            Self::RequestBody => "request_body",
            Self::SyncXhr => "sync_xhr",
            Self::InvalidUrl => "invalid_url",
            Self::Credentials => "credentials",
            Self::CrossOrigin => "cross_origin",
            Self::RequestLimit => "request_limit",
            Self::ResponseBytesLimit => "response_bytes_limit",
            Self::ResponseTooLarge => "response_too_large",
            Self::WallTime => "wall_time",
            Self::DisallowedScheme => "disallowed_scheme",
            Self::DisallowedAddress => "disallowed_address",
            Self::TooManyRedirects => "too_many_redirects",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::Other => "other",
        }
    }

    /// Fetcher のエラーを分類する（URL・メッセージは写さない）。
    ///
    /// `ResponseTooLarge` は呼び出し側が累計上限か 1 件の上限かを判断するため、ここでは
    /// 1 件の上限として写す。
    pub(crate) fn from_fetch_error(e: &Error) -> Self {
        match e {
            Error::ResponseTooLarge { .. } => Self::ResponseTooLarge,
            Error::InvalidInput { .. } => Self::InvalidUrl,
            Error::DisallowedScheme { .. } => Self::DisallowedScheme,
            Error::DisallowedAddress { .. } => Self::DisallowedAddress,
            Error::TooManyRedirects { .. } => Self::TooManyRedirects,
            Error::Timeout { .. } => Self::Timeout,
            Error::Network { .. } | Error::TooManyConcurrentDnsResolutions { .. } => Self::Network,
            _ => Self::Other,
        }
    }
}

/// reject された要求 1 件（診断用。URL は持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetRejection {
    /// 発行元 API。
    pub request: NetRequestKind,
    /// reject の理由。
    pub kind: NetRejectKind,
}

/// ページ内ネットワーク要求の実行結果（`JS-8`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct NetReport {
    pub(crate) started: usize,
    pub(crate) completed: usize,
    pub(crate) response_bytes: u64,
    pub(crate) pending_at_end: usize,
}

impl NetReport {
    /// 受理した要求の件数。
    pub fn started(&self) -> usize {
        self.started
    }

    /// JS へ配送した件数（取得に失敗して reject を配送したものを含む）。
    pub fn completed(&self) -> usize {
        self.completed
    }

    /// 累計で保持したレスポンス本文のバイト数（非可逆変換後）。
    pub fn response_bytes(&self) -> u64 {
        self.response_bytes
    }

    /// 終了時点で配送されなかった保留中の要求数（打ち切り時に残る）。
    pub fn pending_at_end(&self) -> usize {
        self.pending_at_end
    }
}

/// 受理済みで未取得の要求（ランナーが取得する）。
#[derive(Debug, Clone)]
pub(crate) struct PendingRequest {
    pub(crate) id: u32,
    pub(crate) kind: NetRequestKind,
    pub(crate) url: Url,
}

/// 配送する結果（ランナーが [`NetQueue::begin_delivery`] へ渡す）。
#[derive(Debug)]
pub(crate) enum NetOutcome {
    /// 取得できた応答。
    Response {
        status: u16,
        body: String,
        final_url: String,
    },
    /// 取得せず・取得に失敗して reject する。
    Rejected(NetRejectKind),
}

#[derive(Debug)]
struct Delivery {
    id: u32,
    consumed: bool,
    status: Option<u16>,
    body: String,
    offset: usize,
    final_url: String,
}

/// ページ単位の要求キュー（`DomBridge` の `PageState` が保持する）。
#[derive(Debug)]
pub(crate) struct NetQueue {
    limits: PageNetLimits,
    accepted: usize,
    next_id: u32,
    pending: VecDeque<PendingRequest>,
    used_bytes: u64,
    limit_hit: bool,
    delivering: Option<Delivery>,
    completed: usize,
}

/// 相対 URL を `base`（ページの URL）基準で解決し、http / https かつ同一オリジンかつ userinfo 無しだけを通す。
///
/// scheme の早期拒否であり、最終判定（内部アドレス・リダイレクト先）は Fetcher が行う。
/// 解決できない・`about:blank`（`base` が `None`）は [`NetRejectKind::InvalidUrl`]。
pub(crate) fn resolve_request_url(base: Option<&Url>, raw: &str) -> Result<Url, NetRejectKind> {
    let base = base.ok_or(NetRejectKind::InvalidUrl)?;
    let url = base.join(raw).map_err(|_| NetRejectKind::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(NetRejectKind::InvalidUrl);
    }
    // CORS は未実装のため、同一オリジンの要求だけを通す（別オリジンの応答本文を公開しない）。
    if url.origin() != base.origin() {
        return Err(NetRejectKind::CrossOrigin);
    }
    // reqwest は userinfo から Basic 認証を自動付与するため、認証情報を載せた URL は取得しない。
    if !url.username().is_empty() || url.password().is_some() {
        return Err(NetRejectKind::Credentials);
    }
    Ok(url)
}

impl NetQueue {
    pub(crate) fn new(limits: PageNetLimits) -> Self {
        Self {
            limits,
            accepted: 0,
            next_id: 1,
            pending: VecDeque::new(),
            used_bytes: 0,
            limit_hit: false,
            delivering: None,
            completed: 0,
        }
    }

    pub(crate) fn set_limits(&mut self, limits: PageNetLimits) {
        self.limits = limits;
    }

    /// 残りの累計バイト数。
    pub(crate) fn remaining_bytes(&self) -> u64 {
        if self.limit_hit {
            return 0;
        }
        self.limits
            .max_response_bytes
            .saturating_sub(self.used_bytes)
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn pop_pending(&mut self) -> Option<PendingRequest> {
        self.pending.pop_front()
    }

    /// op の時点の検証と受理。受理したら要求 ID、reject なら理由を返す。
    ///
    /// 種別は `fetch` / `xhr` / `xhr_sync`（同期 XHR は常に reject）。未知の種別は `None`。
    pub(crate) fn request(
        &mut self,
        base: Option<&Url>,
        kind: &str,
        method: &str,
        has_body: bool,
        raw_url: &str,
    ) -> Option<(NetRequestKind, Result<u32, NetRejectKind>)> {
        let (request, sync) = match kind {
            "fetch" => (NetRequestKind::Fetch, false),
            "xhr" => (NetRequestKind::Xhr, false),
            "xhr_sync" => (NetRequestKind::Xhr, true),
            _ => return None,
        };
        Some((
            request,
            self.admit(base, request, sync, method, has_body, raw_url),
        ))
    }

    fn admit(
        &mut self,
        base: Option<&Url>,
        request: NetRequestKind,
        sync: bool,
        method: &str,
        has_body: bool,
        raw_url: &str,
    ) -> Result<u32, NetRejectKind> {
        if sync {
            return Err(NetRejectKind::SyncXhr);
        }
        if !method.eq_ignore_ascii_case("GET") {
            return Err(NetRejectKind::UnsupportedMethod);
        }
        if has_body {
            return Err(NetRejectKind::RequestBody);
        }
        let url = resolve_request_url(base, raw_url)?;
        if self.limit_hit || self.used_bytes >= self.limits.max_response_bytes {
            self.limit_hit = true;
            return Err(NetRejectKind::ResponseBytesLimit);
        }
        if self.accepted >= self.limits.max_requests {
            return Err(NetRejectKind::RequestLimit);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(NetRejectKind::RequestLimit)?;
        self.accepted += 1;
        self.pending.push_back(PendingRequest {
            id,
            kind: request,
            url,
        });
        Ok(id)
    }

    /// 配送を開始する。累計バイトを数え、上限に達した reject は以降の要求を止める。
    pub(crate) fn begin_delivery(&mut self, id: u32, outcome: NetOutcome) {
        self.completed += 1;
        let delivery = match outcome {
            NetOutcome::Response {
                status,
                body,
                final_url,
            } => {
                let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
                self.used_bytes = self.used_bytes.saturating_add(len);
                Delivery {
                    id,
                    consumed: false,
                    status: Some(status),
                    body,
                    offset: 0,
                    final_url,
                }
            }
            NetOutcome::Rejected(kind) => {
                if kind == NetRejectKind::ResponseBytesLimit {
                    self.limit_hit = true;
                }
                Delivery {
                    id,
                    consumed: false,
                    status: None,
                    body: String::new(),
                    offset: 0,
                    final_url: String::new(),
                }
            }
        };
        self.delivering = Some(delivery);
    }

    /// 配送中の ID に対する一回限りの許可。許可できたら status（失敗は 0）、できなければ -1。
    pub(crate) fn consume(&mut self, id: u32) -> i32 {
        match self.delivering.as_mut() {
            Some(d) if d.id == id && !d.consumed => {
                d.consumed = true;
                d.status.map_or(0, i32::from)
            }
            _ => -1,
        }
    }

    /// 配送中の ID の本文を `max` バイト以下の文字境界で切って返す。終端・不許可は空文字。
    pub(crate) fn read_chunk(&mut self, id: u32, max: usize) -> String {
        let Some(d) = self
            .delivering
            .as_mut()
            .filter(|d| d.id == id && d.consumed)
        else {
            return String::new();
        };
        let rest = d.body.get(d.offset..).unwrap_or("");
        let mut end = rest.len().min(max);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = rest.get(..end).unwrap_or("").to_owned();
        d.offset += chunk.len();
        chunk
    }

    /// 配送中の ID の最終 URL。不許可は空文字。
    pub(crate) fn final_url(&self, id: u32) -> String {
        self.delivering
            .as_ref()
            .filter(|d| d.id == id && d.consumed)
            .map(|d| d.final_url.clone())
            .unwrap_or_default()
    }

    /// 配送を終え、保持していた本文を解放する（JS の振る舞いに依存しない）。
    pub(crate) fn finish_delivery(&mut self) {
        self.delivering = None;
    }

    pub(crate) fn report(&self) -> NetReport {
        NetReport {
            started: self.accepted,
            completed: self.completed,
            response_bytes: self.used_bytes,
            // 取得中に打ち切った要求も含めるため、受理数と配送数から算出する。
            pending_at_end: self.accepted.saturating_sub(self.completed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.test/app/index.html").expect("url")
    }

    fn q(limits: PageNetLimits) -> NetQueue {
        NetQueue::new(limits)
    }

    fn req(q: &mut NetQueue, url: &str) -> Result<u32, NetRejectKind> {
        let b = base();
        q.request(Some(&b), "fetch", "GET", false, url)
            .expect("known kind")
            .1
    }

    /// JS-8: 相対 URL は navigate 先 URL を基準に解決する。
    #[test]
    fn js_8_resolve_relative_against_location() {
        let b = base();
        let cases = [
            ("a.json", "https://example.test/app/a.json"),
            ("../x", "https://example.test/x"),
            ("/p?z=1", "https://example.test/p?z=1"),
            ("https://example.test/abs", "https://example.test/abs"),
        ];
        for (raw, want) in cases {
            assert_eq!(
                resolve_request_url(Some(&b), raw)
                    .expect("resolve")
                    .as_str(),
                want
            );
        }
    }

    /// JS-8: userinfo・非 http(s)・about:blank 基準は拒否する。
    #[test]
    fn js_8_reject_userinfo_and_non_http_schemes() {
        let b = base();
        assert_eq!(
            resolve_request_url(Some(&b), "https://u:p@example.test/"),
            Err(NetRejectKind::Credentials)
        );
        assert_eq!(
            resolve_request_url(Some(&b), "//u@example.test/"),
            Err(NetRejectKind::Credentials)
        );
        for raw in [
            "//other.test/p",
            "http://example.test/p",
            "https://example.test:8443/p",
        ] {
            assert_eq!(
                resolve_request_url(Some(&b), raw),
                Err(NetRejectKind::CrossOrigin),
                "{raw}"
            );
        }
        for raw in [
            "file:///etc/passwd",
            "data:text/plain,x",
            "ftp://h.test/",
            "javascript:1",
        ] {
            assert_eq!(
                resolve_request_url(Some(&b), raw),
                Err(NetRejectKind::InvalidUrl),
                "{raw}"
            );
        }
        assert_eq!(
            resolve_request_url(None, "a.json"),
            Err(NetRejectKind::InvalidUrl)
        );
    }

    /// JS-8: GET 以外・ボディ付き・同期 XHR・未知の種別。
    #[test]
    fn js_8_method_body_and_sync_are_rejected() {
        let b = base();
        let mut queue = q(PageNetLimits::default());
        let mut call = |kind, method, body| queue.request(Some(&b), kind, method, body, "a");
        assert_eq!(
            call("fetch", "POST", false),
            Some((NetRequestKind::Fetch, Err(NetRejectKind::UnsupportedMethod)))
        );
        assert_eq!(
            call("xhr", "get", true),
            Some((NetRequestKind::Xhr, Err(NetRejectKind::RequestBody)))
        );
        assert_eq!(
            call("xhr_sync", "GET", false),
            Some((NetRequestKind::Xhr, Err(NetRejectKind::SyncXhr)))
        );
        assert_eq!(call("beacon", "GET", false), None);
        assert!(matches!(call("xhr", "get", false), Some((_, Ok(1)))));
    }

    /// JS-6: 既定値と、要求件数の境界（50 件目は受理、51 件目は reject）。
    #[test]
    fn js_6_net_request_count_boundary_50_ok_51_rejected() {
        assert_eq!(DEFAULT_MAX_PAGE_NET_REQUESTS, 50);
        assert_eq!(DEFAULT_MAX_PAGE_NET_RESPONSE_BYTES, 4 * 1024 * 1024);
        let mut queue = q(PageNetLimits::default());
        for n in 1..=50u32 {
            assert_eq!(req(&mut queue, "a"), Ok(n));
        }
        assert_eq!(req(&mut queue, "a"), Err(NetRejectKind::RequestLimit));
        assert_eq!(queue.report().started(), 50);
        assert_eq!(queue.pending_len(), 50);
    }

    /// JS-8: 取得中に打ち切った（キューから外したが未配送の）要求も未配送件数に含める。
    #[test]
    fn js_8_pending_at_end_counts_requests_taken_but_undelivered() {
        let mut queue = q(PageNetLimits::default());
        req(&mut queue, "a").expect("accepted");
        assert!(queue.pop_pending().is_some());
        let report = queue.report();
        assert_eq!(report.started(), 1);
        assert_eq!(report.completed(), 0);
        assert_eq!(report.pending_at_end(), 1);
    }

    fn ok_body(len: usize) -> NetOutcome {
        NetOutcome::Response {
            status: 200,
            body: "a".repeat(len),
            final_url: "https://example.test/".to_string(),
        }
    }

    /// JS-6: 累計バイトは上限ちょうどまで受理し、到達後の要求は reject する。
    #[test]
    fn js_6_net_total_bytes_boundary() {
        let mut queue = q(PageNetLimits::default().with_max_response_bytes(10));
        assert_eq!(queue.remaining_bytes(), 10);
        let id = req(&mut queue, "a").expect("accepted");
        let p = queue.pop_pending().expect("pending");
        assert_eq!(p.id, id);
        queue.begin_delivery(id, ok_body(10));
        assert_eq!(queue.report().response_bytes(), 10);
        assert_eq!(queue.remaining_bytes(), 0);
        queue.finish_delivery();
        assert_eq!(req(&mut queue, "a"), Err(NetRejectKind::ResponseBytesLimit));
    }

    /// JS-6: 上限に達した reject を配送すると、その後の要求も reject になる。
    #[test]
    fn js_6_net_limit_rejection_stops_later_requests() {
        let mut queue = q(PageNetLimits::default().with_max_response_bytes(10));
        let id = req(&mut queue, "a").expect("accepted");
        queue.pop_pending();
        queue.begin_delivery(id, NetOutcome::Rejected(NetRejectKind::ResponseBytesLimit));
        assert_eq!(queue.remaining_bytes(), 0);
        assert_eq!(req(&mut queue, "a"), Err(NetRejectKind::ResponseBytesLimit));
    }

    /// JS-6: 配送は ID 一致・一回限りで、本文は文字境界のチャンクで読み、終了後は解放される。
    #[test]
    fn js_6_delivery_is_single_use_and_chunked_on_char_boundaries() {
        let mut queue = q(PageNetLimits::default());
        let id = req(&mut queue, "a").expect("accepted");
        queue.pop_pending();
        queue.begin_delivery(
            id,
            NetOutcome::Response {
                status: 404,
                body: "あいう".to_string(),
                final_url: "https://example.test/z".to_string(),
            },
        );
        // 消費前・他 ID は読めない。
        assert_eq!(queue.read_chunk(id, 100), "");
        assert_eq!(queue.consume(id + 1), -1);
        assert_eq!(queue.consume(id), 404);
        assert_eq!(queue.consume(id), -1);
        assert_eq!(queue.final_url(id), "https://example.test/z");
        // 3 バイト文字を 4 バイト上限で切る: 1 文字ずつ。
        assert_eq!(queue.read_chunk(id, 4), "あ");
        assert_eq!(queue.read_chunk(id, 100), "いう");
        assert_eq!(queue.read_chunk(id, 100), "");
        queue.finish_delivery();
        assert_eq!(queue.consume(id), -1);
        assert_eq!(queue.final_url(id), "");
    }

    /// JS-8: 失敗の配送は status 0 を返す。
    #[test]
    fn js_8_rejected_delivery_consumes_as_zero() {
        let mut queue = q(PageNetLimits::default());
        let id = req(&mut queue, "a").expect("accepted");
        queue.pop_pending();
        queue.begin_delivery(id, NetOutcome::Rejected(NetRejectKind::Network));
        assert_eq!(queue.consume(id), 0);
        assert_eq!(queue.report().completed(), 1);
    }

    /// JS-8: Fetcher のエラー分類（URL・メッセージを写さない）。
    #[test]
    fn js_8_from_fetch_error_table() {
        let e = Error::DisallowedAddress {
            address: "127.0.0.1".to_string(),
        };
        assert_eq!(
            NetRejectKind::from_fetch_error(&e),
            NetRejectKind::DisallowedAddress
        );
        let e = Error::ResponseTooLarge { limit: 1 };
        assert_eq!(
            NetRejectKind::from_fetch_error(&e),
            NetRejectKind::ResponseTooLarge
        );
    }
}

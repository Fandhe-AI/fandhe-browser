//! core の [`Renderer`] を Servo ホスト（別プロセス）越しに実装する `HostRenderer`
//! （RENDER-1・RENDER-3・PLUG-1・SEC-2。TASK-38（38.6）・#235・設計
//! `docs/design/servo-host-design.md` §2.2・§2.4・§2.5・§2.7・§3.1・§3.3）。
//!
//! cdp・ai は `AppState` 経由の `dyn Renderer` でしか描画に触れない。本型は [`HostProcess`]
//! （子プロセス管理）を包み、ホストのメソッドを次のように対応させる。cli への注入は #236。
//!
//! | [`Renderer`] | ホストのメソッド |
//! | ------------ | ---------------- |
//! | `load` | `load` |
//! | `capture_screenshot` | `screenshot` |
//! | `element_visibility` | `visibility` |
//! | `bounding_box` | `bounding_box` |
//!
//! # 方針
//!
//! - ホストの応答は untrusted。フレーム長・画像寸法は codec・protocol が確保前に検証済みで、
//!   本モジュールは base64 の出力サイズ（確保前）と PNG シグネチャを追加で検査する。期待と違う
//!   結果 variant・不正な画像は [`RenderError::ProtocolMismatch`] にし、成功を装わない。
//! - `load` の送信前に URL の到達先を core の [`check_network_target_url`] で検査する
//!   （scheme・IP リテラル・`localhost`）。ホストが返した `final_url` も同じ検査を通す。
//!   これは Servo 内部のリダイレクトやサブリソース取得の部分的な緩和にすぎず、DNS 名が
//!   プライベート IP に解決される場合・DNS リバインディングは防げない（設計 §2.5・§13 の
//!   オーナー判断待ち）。
//! - 失敗した `load` のあとは「現在のページ」を捨てる。以降の描画系は
//!   [`RenderError::PageNotLoaded`] になり、前ページの描画を返さない（fail-closed）。
//! - ホストの起動世代の管理とクラッシュ後の再ロードは [`HostProcess::call_on_page`] が担う。
//!   本型は追跡せず、ナビゲーション世代だけを持つ。
//! - UA・ヘッダ・ビューポート等の識別面を変える設定は持たない（SEC-2）。
//!
//! # 未実装・暫定
//!
//! - URL 拒否専用の `RenderError` variant は無い（core の公開 API 変更になるため）。拒否は
//!   [`RenderError::Internal`] の固定文言で表す（URL・アドレスは含めない）。
//! - 描画系の同時 in-flight は 1（`Mutex` を呼び出しの間だけ持つ。設計 §2.4）。

mod base64;

use std::fmt;
use std::sync::Mutex;

use fandhe_browser_core::check_network_target_url;
use fandhe_browser_core::render::{
    BoundingBox, ElementRef, ElementVisibility, ImageFormat, PageLoad, PageRef, RenderError,
    Renderer, Screenshot, ScreenshotOptions,
};

use crate::host_process::{HostProcess, HostProcessConfig, HostProcessError};
use crate::protocol::{
    BoundingBoxParams, HostErrorKind, LoadParams, MethodResult, Request, ScreenshotParams,
    VisibilityParams,
};

/// PNG ファイルの先頭 8 バイト。
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// [`HostRenderer`] の設定。フィールドは非公開（REPAIR-4）。
///
/// UA 等の識別面を変えるパラメータは意図的に持たない（SEC-2）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostRendererConfig {
    process: HostProcessConfig,
    allow_private_network_access: bool,
    load_timeout_ms: Option<u32>,
}

impl HostRendererConfig {
    /// 子プロセス設定から作る。プライベートネットワークは既定で拒否（core の fetch と同じ）。
    pub fn new(process: HostProcessConfig) -> Self {
        Self {
            process,
            allow_private_network_access: false,
            load_timeout_ms: None,
        }
    }

    /// loopback・プライベート IP・`localhost` への `load` を許可するか（既定 false）。
    pub fn with_allow_private_network_access(mut self, allow: bool) -> Self {
        self.allow_private_network_access = allow;
        self
    }

    /// `load` のタイムアウト（ms）。`None` はホスト側の既定。範囲は `load` 時に検証する。
    pub fn with_load_timeout_ms(mut self, timeout_ms: Option<u32>) -> Self {
        self.load_timeout_ms = timeout_ms;
        self
    }
}

struct Inner {
    process: HostProcess,
    /// ナビゲーション世代。成功・失敗を問わず `load` のたびに増やす。
    nav_generation: u64,
    /// 直近に成功した `load` の URL。`None` なら描画系は `PageNotLoaded`。
    current: Option<LoadParams>,
}

/// core の [`Renderer`] を Servo ホスト越しに実装する（プロファイルごとに 1 つ作る）。
///
/// 遅延起動で、`new` ではホストを起動しない。cli（#236）が `AppState` へ注入する。
pub struct HostRenderer {
    inner: Mutex<Inner>,
    allow_private_network_access: bool,
    load_timeout_ms: Option<u32>,
}

impl HostRenderer {
    /// 設定から作る。ホストは最初の `load` で起動する。
    pub fn new(config: HostRendererConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                process: HostProcess::new(config.process),
                nav_generation: 0,
                current: None,
            }),
            allow_private_network_access: config.allow_private_network_access,
            load_timeout_ms: config.load_timeout_ms,
        }
    }

    /// 状態のロックを取る。poison 時は複数段階の状態（子プロセス・世代）が不整合でありうる
    /// ため回復せず、fail-closed でエラーにする。
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, RenderError> {
        self.inner
            .lock()
            .map_err(|_| RenderError::Internal("renderer state poisoned".to_owned()))
    }

    /// 現在のページに対する描画系要求を送る共通経路。
    fn call(&self, request: Request) -> Result<MethodResult, RenderError> {
        let mut inner = self.lock()?;
        let Inner {
            process,
            nav_generation,
            current,
        } = &mut *inner;
        let page = current.as_ref().ok_or(RenderError::PageNotLoaded)?;
        process
            .call_on_page(page, *nav_generation, request)
            .map_err(map_host_error)
    }
}

impl fmt::Debug for HostRenderer {
    /// URL にトークンが入りうるため、長さだけを出す（OWASP A02）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("HostRenderer");
        match self.inner.lock() {
            Ok(inner) => s.field("nav_generation", &inner.nav_generation).field(
                "current_url_len",
                &inner.current.as_ref().map(|p| p.url().len()),
            ),
            Err(_) => s.field("state", &"poisoned"),
        };
        s.finish_non_exhaustive()
    }
}

fn internal(reason: &'static str) -> RenderError {
    RenderError::Internal(reason.to_owned())
}

/// [`HostProcessError`] を [`RenderError`] へ写像する（`host_process` のモジュール表の実装）。
///
/// ホスト由来の message・URL は `Internal` に詰めない（外部へ漏れる経路を作らない）。
fn map_host_error(err: HostProcessError) -> RenderError {
    match err {
        HostProcessError::Disabled | HostProcessError::InvalidHostBinary(_) => {
            RenderError::RenderingDisabled
        }
        HostProcessError::SpawnFailed
        | HostProcessError::WorkDirFailed
        | HostProcessError::Crashed
        | HostProcessError::ResourceLimitExceeded
        | HostProcessError::CircuitOpen => RenderError::HostUnavailable,
        HostProcessError::Timeout(_) => RenderError::Timeout,
        HostProcessError::ProtocolMismatch | HostProcessError::ProtocolViolation(_) => {
            RenderError::ProtocolMismatch
        }
        HostProcessError::Host(e) => match e.kind() {
            HostErrorKind::NotLoaded => RenderError::PageNotLoaded,
            HostErrorKind::NotFound => RenderError::ElementNotFound,
            HostErrorKind::Timeout => RenderError::Timeout,
            _ => internal("render host reported an error"),
        },
        HostProcessError::InvalidRequest => internal("invalid render host request"),
    }
}

/// 要素参照をセレクタ文字列にする。未知の参照種別は成功にせずエラーにする。
fn selector_of(element: &ElementRef) -> Result<&str, RenderError> {
    match element {
        ElementRef::Selector(s) => Ok(s.as_str()),
        _ => Err(internal("unsupported element reference")),
    }
}

impl Renderer for HostRenderer {
    fn load(&self, page: &PageRef) -> Result<PageLoad, RenderError> {
        if page.url() == "about:blank" {
            return Err(RenderError::PageNotLoaded);
        }
        let url = check_network_target_url(page.url(), self.allow_private_network_access)
            .map_err(|_| internal("url rejected"))?;
        let params = LoadParams::new(&url, self.load_timeout_ms)
            .map_err(|_| internal("invalid load parameters"))?;

        let mut inner = self.lock()?;
        // 失敗した load のあとの描画が前ページを返さないよう、先に捨てる（RenderGate と同じ fail-closed）。
        inner.current = None;
        inner.nav_generation = inner
            .nav_generation
            .checked_add(1)
            .ok_or_else(|| internal("navigation generation overflow"))?;
        let generation = inner.nav_generation;
        let result = inner
            .process
            .load(&params, generation)
            .map_err(map_host_error)?;
        // ホストが返した最終 URL も同じポリシーで再検査する（Servo 内部リダイレクトの部分的な緩和）。
        let final_url =
            check_network_target_url(result.final_url(), self.allow_private_network_access)
                .map_err(|_| internal("final url rejected"))?;
        inner.current = Some(params);
        Ok(PageLoad::new(final_url))
    }

    fn capture_screenshot(&self, options: &ScreenshotOptions) -> Result<Screenshot, RenderError> {
        let result = self.call(Request::Screenshot(ScreenshotParams::new(
            options.full_page,
        )))?;
        let MethodResult::Screenshot(shot) = result else {
            return Err(RenderError::ProtocolMismatch);
        };
        let data = base64::decode(shot.data_base64(), base64::MAX_DECODED_BYTES)
            .map_err(|_| RenderError::ProtocolMismatch)?;
        if !data.starts_with(&PNG_SIGNATURE) {
            return Err(RenderError::ProtocolMismatch);
        }
        Ok(Screenshot::new(
            ImageFormat::Png,
            shot.width(),
            shot.height(),
            data,
        ))
    }

    fn element_visibility(&self, element: &ElementRef) -> Result<ElementVisibility, RenderError> {
        let params = VisibilityParams::new(selector_of(element)?)
            .map_err(|_| internal("invalid selector"))?;
        match self.call(Request::Visibility(params))? {
            MethodResult::Visibility(v) => Ok(ElementVisibility::new(v.visible())),
            _ => Err(RenderError::ProtocolMismatch),
        }
    }

    fn bounding_box(&self, element: &ElementRef) -> Result<BoundingBox, RenderError> {
        let params = BoundingBoxParams::new(selector_of(element)?)
            .map_err(|_| internal("invalid selector"))?;
        match self.call(Request::BoundingBox(params))? {
            MethodResult::BoundingBox(b) => {
                Ok(BoundingBox::new(b.x(), b.y(), b.width(), b.height()))
            }
            _ => Err(RenderError::ProtocolMismatch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_process::{InvalidBinaryReason, TimeoutPhase};
    use crate::protocol::{Method, Response};
    use serde_json::json;

    fn host_error(kind: &str) -> HostProcessError {
        let v = json!({"v": 1, "id": 1, "error": {"kind": kind, "message": "https://secret.example/?t=1"}});
        let r = Response::from_json(&v, Method::Screenshot).expect("response");
        HostProcessError::Host(r.outcome.expect_err("error response"))
    }

    /// RENDER-3: `HostProcessError` の写像表（host_process のモジュールコメント）を全 arm 検証する。
    #[test]
    fn render_3_error_mapping_table() {
        use RenderError as R;
        let cases: Vec<(HostProcessError, R)> = vec![
            (HostProcessError::Disabled, R::RenderingDisabled),
            (
                HostProcessError::InvalidHostBinary(InvalidBinaryReason::NotFound),
                R::RenderingDisabled,
            ),
            (HostProcessError::SpawnFailed, R::HostUnavailable),
            (HostProcessError::WorkDirFailed, R::HostUnavailable),
            (HostProcessError::Crashed, R::HostUnavailable),
            (HostProcessError::ResourceLimitExceeded, R::HostUnavailable),
            (HostProcessError::CircuitOpen, R::HostUnavailable),
            (HostProcessError::Timeout(TimeoutPhase::Load), R::Timeout),
            (HostProcessError::ProtocolMismatch, R::ProtocolMismatch),
            (
                HostProcessError::ProtocolViolation(crate::protocol::ProtocolError::InvalidParams(
                    "x",
                )),
                R::ProtocolMismatch,
            ),
            (host_error("not_loaded"), R::PageNotLoaded),
            (host_error("not_found"), R::ElementNotFound),
            (host_error("timeout"), R::Timeout),
        ];
        for (input, expected) in cases {
            let label = format!("{input:?}");
            let got = map_host_error(input);
            assert_eq!(format!("{got:?}"), format!("{expected:?}"), "{label}");
        }
        for e in [
            host_error("internal"),
            host_error("load_failed"),
            HostProcessError::InvalidRequest,
        ] {
            let got = map_host_error(e);
            let detail = got.internal_detail().expect("Internal");
            assert!(!detail.contains("secret.example"), "{detail}");
        }
    }

    /// REPAIR-4: `dyn Renderer` として使え、`Send + Sync` である。
    #[test]
    fn render_3_host_renderer_is_object_safe_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HostRenderer>();
        fn _object(_: &dyn Renderer) {}
    }

    /// OWASP A02: `Debug` に URL を出さない。
    #[test]
    fn render_3_debug_does_not_leak_url() {
        let bin = crate::host_process::resolve_host_binary(
            std::env::current_exe().ok().as_deref(),
            None,
            None,
        )
        .expect("binary");
        let r = HostRenderer::new(HostRendererConfig::new(HostProcessConfig::new(bin)));
        let s = format!("{r:?}");
        assert!(s.contains("nav_generation"), "{s}");
        assert!(!s.contains("http"), "{s}");
    }
}

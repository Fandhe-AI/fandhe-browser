//! Servo ホストの WebView 生成・ナビゲーション・post-load フレーム待機と、`load` ハンドラ。
//!
//! 役割: `engine` が作った Servo 上で WebView を 1 枚生成し、読み込み完了と最初の描画フレームを
//! 期限付きで待つ層（`RENDER-3`・`RENDER-4`・TASK-38.2・#231。設計 §2.2・§2.4・§2.5）。
//! 呼び出し元は `server` のディスパッチ（[`ServoHandler`] 経由）と `main` の `--selftest`
//! （低レベルの [`navigate`]）。スクリーンショット（TASK-38.3・#232）はこの層が保持する現在ページの
//! 上で `WebView::take_screenshot` を期限付きで待ち、PNG 化は `screenshot` モジュールに任せる。
//! 可視性・境界ボックスは #233・#234 が同じ上に載る。
//!
//! 待機の方針（PoC-13 の失敗の再発防止）: 条件は「`LoadStatus::Complete` かつ受信フレーム数が 1 以上」。
//! Complete より前に届いたフレームも数え、「Complete より後の新しいフレーム」は要求しない
//! （静的ページでは来ないことがある）。`notify_new_frame_ready` では必ず `WebView::paint()` を呼ぶ
//! （呼ばないとフレームが進まない。TASK-36）。JS の評価でフレームを起こす手法は使わない
//! （ページ状態を書き換えるため）。`SoftwareRenderingContext` では `show()` を呼ばずに
//! フレームが届く（Servo 0.6.0 の tests/common と同じ手順）。
//!
//! 制約: HTTP ステータスは Servo 0.6.0 の埋め込み API から取得できないため `None` を返す
//! （固定値で成功を装わない。`REPAIR-3`・`REPAIR-4`）。標準出力へは書かず、ログに URL・タイトル・
//! クラッシュ理由を出さない（機密を含みうる。設計 §2.1）。

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use servo::{
    LoadStatus, RgbaImage, ScreenshotCaptureError, ServoUrl, WebView, WebViewBuilder,
    WebViewDelegate,
};

use crate::engine::Engine;
use crate::screenshot::{self, SCREENSHOT_TIMEOUT};
use crate::server::{
    BoundingBoxResult, ErrorKind, Handler, HandlerError, LoadParams, LoadResult, Method,
    ScreenshotParams, ScreenshotResult, SelectorParams, VisibilityResult, is_valid_url,
};

/// イベントループのポーリング間隔（ビジーループにしない）。
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Complete 到達後に最初のフレームを待つ上限（全体の期限と小さい方を採る。`RENDER-4`）。
const POST_LOAD_FRAME_WAIT_MAX: Duration = Duration::from_secs(10);

/// Servo が組み込みのエラーページに付けるタイトル（ネットワークエラー・証明書エラー）。
const ERROR_PAGE_TITLES: [&str; 2] = ["Error loading page", "Certificate error"];

/// ページ層の失敗。`server::ErrorKind` への対応は [`PageError::kind`] に置く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum PageError {
    /// URL が許可規則（http / https のみ・userinfo なし）に合わない。
    InvalidUrl,
    /// 期限内に読み込みまたは最初のフレームが完了しなかった。
    Timeout,
    /// ナビゲーションが失敗した（エラーページ・不正な最終 URL）。
    LoadFailed,
    /// ページのプロセスがクラッシュした。
    Crashed,
    /// Servo の生成に失敗した（再生成は不可。設計 §2.7）。
    Engine,
    /// 描画結果の取得に失敗した（コンテキストからの読み出し失敗）。
    Capture,
}

impl PageError {
    fn kind(self) -> ErrorKind {
        match self {
            Self::InvalidUrl => ErrorKind::InvalidParams,
            Self::Timeout => ErrorKind::Timeout,
            Self::LoadFailed | Self::Crashed => ErrorKind::LoadFailed,
            Self::Engine | Self::Capture => ErrorKind::Internal,
        }
    }
}

/// `poll` が値を返すまでイベントループを回して待つ。`deadline` 超過は `Timeout`、
/// `crashed` が真なら `Crashed`（フレーム・コールバックが来ない場合も待ち続けない。`RENDER-4`）。
///
/// `navigate` と `CurrentPage::capture` が共有する。Servo に依存しない形にして単体テストできる。
fn wait_until<T>(
    deadline: Instant,
    mut poll: impl FnMut() -> Option<T>,
    crashed: impl Fn() -> bool,
    mut spin: impl FnMut(),
) -> Result<T, PageError> {
    loop {
        if let Some(v) = poll() {
            return Ok(v);
        }
        if crashed() {
            return Err(PageError::Crashed);
        }
        if Instant::now() >= deadline {
            return Err(PageError::Timeout);
        }
        spin();
    }
}

/// ページ層の読み込み結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) struct PageLoad {
    pub final_url: String,
    /// HTTP ステータス。Servo 0.6.0 では取得できないため常に `None`（`REPAIR-3`・`REPAIR-4`）。
    pub status: Option<u16>,
}

/// フレーム受信とクラッシュを数える delegate。WebView と共有する（`Rc`）。
#[derive(Default)]
struct PageDelegate {
    frames: Cell<u64>,
    crashed: Cell<bool>,
}

impl WebViewDelegate for PageDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        self.frames.set(self.frames.get().saturating_add(1));
        // paint() を呼ばないとフレームが進まない（TASK-36）。
        webview.paint();
    }

    fn notify_crashed(&self, _webview: WebView, _reason: String, _backtrace: Option<String>) {
        // 理由と backtrace は URL 等の機密を含みうるため記録しない。
        self.crashed.set(true);
    }
}

/// 現在ページ。WebView は Engine より先に drop する必要がある（所有側のフィールド順で担保）。
pub(crate) struct CurrentPage {
    webview: WebView,
    delegate: Rc<PageDelegate>,
}

impl CurrentPage {
    /// 現在のビューポート全体を `deadline` まで待って取得する（`take_screenshot`。TASK-38.3）。
    ///
    /// コールバックが来ない場合も `deadline` で `Timeout` を返す。
    fn capture(&self, engine: &Engine, deadline: Instant) -> Result<RgbaImage, PageError> {
        let slot: Rc<Cell<Option<Result<RgbaImage, ScreenshotCaptureError>>>> =
            Rc::new(Cell::new(None));
        let writer = slot.clone();
        self.webview
            .take_screenshot(None, move |r| writer.set(Some(r)));
        let result = wait_until(
            deadline,
            || slot.take(),
            || self.delegate.crashed.get(),
            || {
                engine.servo().spin_event_loop();
                std::thread::sleep(POLL_INTERVAL);
            },
        )?;
        result.map_err(|e| match e {
            ScreenshotCaptureError::WebViewDoesNotExist => PageError::Crashed,
            _ => PageError::Capture,
        })
    }
}

/// WebView を生成し、`Complete` と最初のフレームを `deadline` まで待つ。
///
/// URL の scheme 検証はしない（`--selftest` の `data:` を通すため）。外部入力の経路
/// （[`ServoHandler`]）は呼び出し前に必ず検証すること。
pub(crate) fn navigate(
    engine: &Engine,
    url: ServoUrl,
    deadline: Instant,
) -> Result<CurrentPage, PageError> {
    let delegate = Rc::new(PageDelegate::default());
    let webview = WebViewBuilder::new(engine.servo(), engine.rendering_context())
        .delegate(delegate.clone())
        .url(url.into_url())
        .build();

    let spin = || {
        engine.servo().spin_event_loop();
        std::thread::sleep(POLL_INTERVAL);
    };
    let crashed = || delegate.crashed.get();
    wait_until(
        deadline,
        || (webview.load_status() == LoadStatus::Complete).then_some(()),
        crashed,
        spin,
    )?;
    // Complete より前に届いたフレームも数える。上限は全体の期限と専用上限の小さい方。
    let frame_deadline = deadline.min(Instant::now() + POST_LOAD_FRAME_WAIT_MAX);
    wait_until(
        frame_deadline,
        || (delegate.frames.get() > 0).then_some(()),
        crashed,
        spin,
    )?;
    // コンテキストに最新フレームが描かれた状態を保証する。
    webview.paint();
    Ok(CurrentPage { webview, delegate })
}

/// Servo 組み込みのエラーページのタイトルか。
///
/// ヒューリスティックで、ページ側が同じタイトルを付けると `load_failed` に倒れる（fail-closed）。
/// 将来仕様: Servo がナビゲーション失敗を通知する API を出したらそれへ置き換える（`REPAIR-3`）。
fn is_error_page_title(title: Option<&str>) -> bool {
    title.is_some_and(|t| ERROR_PAGE_TITLES.contains(&t))
}

/// `server::Handler` の実装。Engine は最初の `load` で遅延生成する。
///
/// 呼び出し元は `main` の `--stdio`。hello 送信後に Servo を起動するため、ハンドシェイクの
/// 10 秒制約に影響しない（`new` は何も起動しない）。
pub(crate) struct ServoHandler {
    // 宣言順が drop 順: WebView を Engine（Servo）より先に破棄する。入れ替えないこと。
    page: Option<CurrentPage>,
    engine: Option<Engine>,
    /// 生成に失敗済み。Servo は 1 プロセス 1 回のため再試行しない（親がプロセスを作り直す）。
    engine_failed: bool,
}

impl ServoHandler {
    pub(crate) fn new() -> Self {
        Self {
            page: None,
            engine: None,
            engine_failed: false,
        }
    }

    fn ensure_engine(&mut self) -> Result<(), PageError> {
        if self.engine.is_some() {
            return Ok(());
        }
        if self.engine_failed {
            return Err(PageError::Engine);
        }
        match Engine::new() {
            Ok(e) => {
                self.engine = Some(e);
                Ok(())
            }
            Err(_) => {
                self.engine_failed = true;
                Err(PageError::Engine)
            }
        }
    }

    fn load_page(&mut self, p: &LoadParams) -> Result<PageLoad, PageError> {
        // 失敗しても現在のページは捨てる（クライアントと同じ意味論）。
        self.page = None;
        // 入口（request.rs）に加えてホスト側でも再検証する（二重化。SSRF）。
        if !is_valid_url(&p.url) {
            return Err(PageError::InvalidUrl);
        }
        let url = ServoUrl::parse(&p.url).map_err(|_| PageError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(PageError::InvalidUrl);
        }
        let deadline = Instant::now() + p.timeout;
        self.ensure_engine()?;
        let engine = self.engine.as_ref().ok_or(PageError::Engine)?;
        let page = navigate(engine, url, deadline)?;
        if page.delegate.crashed.get() {
            return Err(PageError::Crashed);
        }
        if is_error_page_title(page.webview.page_title().as_deref()) {
            return Err(PageError::LoadFailed);
        }
        let final_url = page
            .webview
            .url()
            .map(|u| u.to_string())
            .ok_or(PageError::LoadFailed)?;
        // リダイレクト後に許可外の URL へ移った場合も切り詰めず拒否する。
        if !is_valid_url(&final_url) {
            return Err(PageError::LoadFailed);
        }
        self.page = Some(page);
        Ok(PageLoad {
            final_url,
            status: None,
        })
    }
}

impl Handler for ServoHandler {
    fn capabilities(&self) -> &'static [Method] {
        &[Method::Load, Method::Screenshot]
    }

    fn load(&mut self, p: &LoadParams) -> Result<LoadResult, HandlerError> {
        let page = self.load_page(p).map_err(|e| HandlerError(e.kind()))?;
        Ok(LoadResult {
            final_url: page.final_url,
            status: page.status,
        })
    }

    /// 現在ページのビューポート（固定 1280x800）を PNG の base64 で返す（`RENDER-3`・TASK-38.3）。
    ///
    /// 将来仕様: `full_page=true` は文書の高さに合わせた撮影が要り、固定ビューポート（設計 §2.3・
    /// `SEC-2`）と衝突するため v1 では `unsupported` を返す（成功を装わない。`REPAIR-3`）。
    fn screenshot(&mut self, p: &ScreenshotParams) -> Result<ScreenshotResult, HandlerError> {
        if p.full_page {
            return Err(HandlerError(ErrorKind::Unsupported));
        }
        let (Some(page), Some(engine)) = (self.page.as_ref(), self.engine.as_ref()) else {
            return Err(HandlerError(ErrorKind::NotLoaded));
        };
        let deadline = Instant::now() + SCREENSHOT_TIMEOUT;
        let image = match page.capture(engine, deadline) {
            Ok(img) => img,
            Err(e) => {
                // クラッシュしたページは保持しない（以後は not_loaded になる）。
                if e == PageError::Crashed {
                    self.page = None;
                }
                return Err(HandlerError(e.kind()));
            }
        };
        screenshot::build_result(image.width(), image.height(), image.as_raw())
            .map_err(|e| HandlerError(e.kind()))
    }
    // 以下は #233・#234 で置き換える。成功を装わない（REPAIR-3）。
    fn visibility(&mut self, _p: &SelectorParams) -> Result<VisibilityResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
    fn bounding_box(&mut self, _p: &SelectorParams) -> Result<BoundingBoxResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RENDER-4: ページ層のエラーはプロトコルの kind へ固定で対応する。
    #[test]
    fn page_error_maps_to_error_kind() {
        assert_eq!(PageError::InvalidUrl.kind(), ErrorKind::InvalidParams);
        assert_eq!(PageError::Timeout.kind(), ErrorKind::Timeout);
        assert_eq!(PageError::LoadFailed.kind(), ErrorKind::LoadFailed);
        assert_eq!(PageError::Crashed.kind(), ErrorKind::LoadFailed);
        assert_eq!(PageError::Engine.kind(), ErrorKind::Internal);
        assert_eq!(PageError::Capture.kind(), ErrorKind::Internal);
    }

    /// RENDER-4: 来ないフレーム・コールバックを待ち続けず、期限で Timeout を返す。
    #[test]
    fn wait_until_times_out_when_nothing_arrives() {
        let started = Instant::now();
        let r: Result<(), PageError> = wait_until(
            started + Duration::from_millis(50),
            || None,
            || false,
            || std::thread::sleep(Duration::from_millis(1)),
        );
        assert_eq!(r, Err(PageError::Timeout));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn wait_until_reports_crash_and_value() {
        let far = Instant::now() + Duration::from_secs(60);
        let crashed: Result<(), PageError> = wait_until(far, || None, || true, || {});
        assert_eq!(crashed, Err(PageError::Crashed));
        let mut n = 0;
        let value = wait_until(
            far,
            || {
                n += 1;
                (n == 3).then_some(n)
            },
            || false,
            || {},
        );
        assert_eq!(value, Ok(3));
    }

    /// RENDER-3: 未ロードは not_loaded、full_page は unsupported（Engine を作らない）。
    #[test]
    fn screenshot_without_page_is_rejected() {
        let mut h = ServoHandler::new();
        let err = h
            .screenshot(&ScreenshotParams { full_page: false })
            .err()
            .expect("must fail");
        assert_eq!(err.0, ErrorKind::NotLoaded);
        let err = h
            .screenshot(&ScreenshotParams { full_page: true })
            .err()
            .expect("must fail");
        assert_eq!(err.0, ErrorKind::Unsupported);
    }

    #[test]
    fn error_page_titles_are_detected() {
        assert!(is_error_page_title(Some("Error loading page")));
        assert!(is_error_page_title(Some("Certificate error")));
        assert!(!is_error_page_title(Some("Example Domain")));
        assert!(!is_error_page_title(None));
    }

    #[test]
    fn capabilities_list_load_and_screenshot() {
        assert_eq!(
            ServoHandler::new().capabilities(),
            &[Method::Load, Method::Screenshot]
        );
    }
}

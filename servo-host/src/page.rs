//! Servo ホストの WebView 生成・ナビゲーション・post-load フレーム待機と、`load` ハンドラ。
//!
//! 役割: `engine` が作った Servo 上で WebView を 1 枚生成し、読み込み完了と最初の描画フレームを
//! 期限付きで待つ層（`RENDER-3`・`RENDER-4`・TASK-38.2・#231。設計 §2.2・§2.4・§2.5）。
//! 呼び出し元は `server` のディスパッチ（[`ServoHandler`] 経由）と `main` の `--selftest`
//! （低レベルの [`navigate`]）。スクリーンショット・可視性・境界ボックスは #232〜#234 が
//! この層が保持する現在ページの上に載る。
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

use servo::{LoadStatus, ServoUrl, WebView, WebViewBuilder, WebViewDelegate};

use crate::engine::Engine;
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
}

impl PageError {
    fn kind(self) -> ErrorKind {
        match self {
            Self::InvalidUrl => ErrorKind::InvalidParams,
            Self::Timeout => ErrorKind::Timeout,
            Self::LoadFailed | Self::Crashed => ErrorKind::LoadFailed,
            Self::Engine => ErrorKind::Internal,
        }
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
    while webview.load_status() != LoadStatus::Complete {
        if delegate.crashed.get() {
            return Err(PageError::Crashed);
        }
        if Instant::now() >= deadline {
            return Err(PageError::Timeout);
        }
        spin();
    }
    // Complete より前に届いたフレームも数える。上限は全体の期限と専用上限の小さい方。
    let frame_deadline = deadline.min(Instant::now() + POST_LOAD_FRAME_WAIT_MAX);
    while delegate.frames.get() == 0 {
        if delegate.crashed.get() {
            return Err(PageError::Crashed);
        }
        if Instant::now() >= frame_deadline {
            return Err(PageError::Timeout);
        }
        spin();
    }
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
        &[Method::Load]
    }

    fn load(&mut self, p: &LoadParams) -> Result<LoadResult, HandlerError> {
        let page = self.load_page(p).map_err(|e| HandlerError(e.kind()))?;
        Ok(LoadResult {
            final_url: page.final_url,
            status: page.status,
        })
    }

    // 以下は #232〜#234 で置き換える。成功を装わない（REPAIR-3）。
    fn screenshot(&mut self, _p: &ScreenshotParams) -> Result<ScreenshotResult, HandlerError> {
        Err(HandlerError(ErrorKind::Unsupported))
    }
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
    }

    #[test]
    fn error_page_titles_are_detected() {
        assert!(is_error_page_title(Some("Error loading page")));
        assert!(is_error_page_title(Some("Certificate error")));
        assert!(!is_error_page_title(Some("Example Domain")));
        assert!(!is_error_page_title(None));
    }

    #[test]
    fn capabilities_list_only_load() {
        assert_eq!(ServoHandler::new().capabilities(), &[Method::Load]);
    }
}

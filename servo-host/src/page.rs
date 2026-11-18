//! Servo ホストの WebView 生成・ナビゲーション・post-load フレーム待機と、`load` ハンドラ。
//!
//! 役割: `engine` が作った Servo 上で WebView を 1 枚生成し、読み込み完了と最初の描画フレームを
//! 期限付きで待つ層（`RENDER-3`・`RENDER-4`・TASK-38.2・#231。設計 §2.2・§2.4・§2.5）。
//! 呼び出し元は `server` のディスパッチ（[`ServoHandler`] 経由）と `main` の `--selftest`
//! （低レベルの [`navigate`]）。スクリーンショット（TASK-38.3・#232）はこの層が保持する現在ページの
//! 上で `WebView::take_screenshot` を期限付きで待ち、PNG 化は `screenshot` モジュールに任せる。
//! 可視性（TASK-38.4・#233）は同じ上で `WebView::evaluate_javascript` を期限付きで待ち、
//! スクリプト生成と結果解釈は `layout` に任せる。境界ボックスも同じ経路で取得する（#234）。
//! 操作モード（`--interactive`。TASK-122・設計 §14）では、DOM の取得（`dom_snapshot`）も同じ経路で行い
//! （直列化のスクリプトと結果の解釈は `dom`）、ページ識別子（起動ごとの `session` と文書番号 `doc_seq`）を持つ。
//! `doc_seq` は `load`・URL 変更の通知・新しい文書の読み込みの開始で上げる（#1582 の判断 A）。
//!
//! 待機の方針（PoC-13 の失敗の再発防止）: 条件は「`LoadStatus::Complete` かつ受信フレーム数が 1 以上」。
//! Complete より前に届いたフレームも数え、「Complete より後の新しいフレーム」は要求しない
//! （静的ページでは来ないことがある）。`notify_new_frame_ready` では必ず `WebView::paint()` を呼ぶ
//! （呼ばないとフレームが進まない。TASK-36）。JS の評価でフレームを起こす手法は使わない
//! （ページ状態を書き換えるため）。`SoftwareRenderingContext` では `show()` を呼ばずに
//! フレームが届く（Servo 0.7.0 の tests/common と同じ手順）。
//!
//! 制約: HTTP ステータスは Servo 0.7.0 の埋め込み API から取得できないため `None` を返す
//! （固定値で成功を装わない。`REPAIR-3`・`REPAIR-4`）。標準出力へは書かず、ログに URL・タイトル・
//! クラッシュ理由を出さない（機密を含みうる。設計 §2.1）。

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use servo::{
    JSValue, JavaScriptEvaluationError, LoadStatus, RgbaImage, ScreenshotCaptureError, ServoUrl,
    WebView, WebViewBuilder, WebViewDelegate,
};

use crate::dom;
use crate::engine::{Engine, LoopbackProxy};
use crate::layout;
use crate::screenshot::{self, SCREENSHOT_TIMEOUT};
use crate::server::{
    BoundingBoxResult, DOM_SNAPSHOT_TIMEOUT_MS, DomSnapshotResult, ErrorKind, Handler,
    HandlerError, LoadParams, LoadResult, MAX_URL_BYTES, Method, PageInfo, ScreenshotParams,
    ScreenshotResult, SelectorParams, VisibilityResult, is_valid_url,
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
    /// スクリプト評価が失敗した（コンパイル・実行・シリアライズ・WebView 未準備）。
    Script,
    /// 評価対象の文書が既に無い（ページを破棄する）。
    DocumentGone,
}

impl PageError {
    fn kind(self) -> ErrorKind {
        match self {
            Self::InvalidUrl => ErrorKind::InvalidParams,
            Self::Timeout => ErrorKind::Timeout,
            Self::LoadFailed | Self::Crashed => ErrorKind::LoadFailed,
            Self::Engine | Self::Capture | Self::Script => ErrorKind::Internal,
            Self::DocumentGone => ErrorKind::NotLoaded,
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
    /// HTTP ステータス。Servo 0.7.0 では取得できないため常に `None`（`REPAIR-3`・`REPAIR-4`）。
    pub status: Option<u16>,
}

/// フレーム受信とクラッシュを数える delegate。WebView と共有する（`Rc`）。
#[derive(Default)]
struct PageDelegate {
    frames: Cell<u64>,
    crashed: Cell<bool>,
    /// 文書番号（`doc_seq`）。[`ServoHandler`] と共有し、WebView を作り直しても続けて数える（設計 §14.2）。
    doc_seq: Rc<Cell<u64>>,
}

impl PageDelegate {
    fn bump_doc_seq(&self) {
        self.doc_seq.set(self.doc_seq.get().saturating_add(1));
    }
}

impl WebViewDelegate for PageDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        self.frames.set(self.frames.get().saturating_add(1));
        // paint() を呼ばないとフレームが進まない（TASK-36）。
        webview.paint();
    }

    fn load_web_resource(&self, _webview: WebView, load: servo::WebResourceLoad) {
        // 読み込み先を許可リストで制限する（RENDER-3・SEC-2）。
        crate::guard::check(load);
    }

    fn request_navigation(&self, _webview: WebView, request: servo::NavigationRequest) {
        // ページから始まる遷移の scheme を絞る（RENDER-10・TASK-120）。既定（drop）は許可になるため、
        // 許可・拒否を必ず明示する（fail-closed）。
        if crate::guard::navigation_allowed(request.url.as_str()) {
            request.allow();
        } else {
            request.deny();
        }
    }

    fn notify_url_changed(&self, _webview: WebView, _url: url::Url) {
        // 同じ文書の中の変更（`pushState`・`replaceState`・hash）でも上げる（2026-11-18 の判断 A。#1582）。
        self.bump_doc_seq();
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        // 新しい文書の読み込みの開始（スクリプト起点の遷移・戻る・進む）。初回の読み込みは `Started` が
        // 来ないため、`ServoHandler::load_page` が別に上げる。
        if status == LoadStatus::Started {
            self.bump_doc_seq();
        }
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

    /// 読み取り専用スクリプトを `deadline` まで待って評価する（`evaluate_javascript`。TASK-38.4）。
    ///
    /// エラー詳細（メッセージ・スタック）はセレクタ等を含みうるため捨てる。呼び出しごとに新しい
    /// スロットを使い、期限後に届いたコールバックは捨てられたスロットへ書くだけで実害はない。
    fn evaluate(
        &self,
        engine: &Engine,
        script: String,
        deadline: Instant,
    ) -> Result<JSValue, PageError> {
        let slot: Rc<Cell<Option<Result<JSValue, JavaScriptEvaluationError>>>> =
            Rc::new(Cell::new(None));
        let writer = slot.clone();
        self.webview
            .evaluate_javascript(script, move |r| writer.set(Some(r)));
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
            JavaScriptEvaluationError::DocumentNotFound => PageError::DocumentGone,
            _ => PageError::Script,
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
    doc_seq: Rc<Cell<u64>>,
) -> Result<CurrentPage, PageError> {
    let delegate = Rc::new(PageDelegate {
        doc_seq,
        ..PageDelegate::default()
    });
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

/// ホストの起動ごとに変わる値（32 桁の 16 進）。`RandomState` の鍵は OS の乱数から作られるため、
/// 起動ごとに変わる（予測できないことは求めない。設計 §14.2）。依存を足さずに作る。
fn new_session() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(0);
        h.finish()
    };
    format!("{:016x}{:016x}", part(), part())
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
    /// egress プロキシの場所。`Engine` 生成時に Servo の proxy pref へ渡す（#1435）。
    proxy: Option<LoopbackProxy>,
    /// 操作モード（`--interactive`）。真のときだけ `dom_snapshot` を載せ、`load` に `page` を返す（設計 §14.1）。
    interactive: bool,
    /// ホストの起動ごとに変わる値（設計 §14.2）。
    session: String,
    /// 文書番号。delegate と共有する。
    doc_seq: Rc<Cell<u64>>,
    /// 最新の `dom_snapshot` の識別子の通し番号。
    snapshot_seq: u64,
    /// 最新の `dom_snapshot` の対応（番号 → 要素 ID）。文書が変わる `load` で捨てる。
    snapshot: Option<Snapshot>,
}

/// `dom_snapshot` 1 回分の対応。ホストは最新の 1 件だけを持つ（設計 §14.4）。
struct Snapshot {
    // ponytail: act（TASK-126）が `id` と照合し、`element_ids` を `window.webdriverElement` で引き直す。
    // それまでは保持だけで読まない。
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    element_ids: Vec<String>,
}

impl ServoHandler {
    pub(crate) fn new(proxy: Option<LoopbackProxy>, interactive: bool) -> Self {
        Self {
            page: None,
            engine: None,
            engine_failed: false,
            proxy,
            interactive,
            session: new_session(),
            doc_seq: Rc::default(),
            snapshot_seq: 0,
            snapshot: None,
        }
    }

    fn page_info(&self) -> PageInfo {
        PageInfo {
            session: self.session.clone(),
            doc_seq: self.doc_seq.get(),
        }
    }

    fn ensure_engine(&mut self) -> Result<(), PageError> {
        if self.engine.is_some() {
            return Ok(());
        }
        if self.engine_failed {
            return Err(PageError::Engine);
        }
        match Engine::new(self.proxy) {
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
        // 失敗しても現在のページは捨てる（クライアントと同じ意味論）。`load` は常に新しい文書なので
        // 文書番号を上げ、前の `dom_snapshot` の対応も捨てる（設計 §14.2）。
        self.page = None;
        self.snapshot = None;
        self.doc_seq.set(self.doc_seq.get().saturating_add(1));
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
        let page = navigate(engine, url, deadline, self.doc_seq.clone())?;
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
        if self.interactive {
            &[
                Method::Load,
                Method::Screenshot,
                Method::Visibility,
                Method::BoundingBox,
                Method::DomSnapshot,
            ]
        } else {
            &[
                Method::Load,
                Method::Screenshot,
                Method::Visibility,
                Method::BoundingBox,
            ]
        }
    }

    fn load(&mut self, p: &LoadParams) -> Result<LoadResult, HandlerError> {
        let page = self.load_page(p).map_err(|e| HandlerError(e.kind()))?;
        Ok(LoadResult {
            final_url: page.final_url,
            status: page.status,
            page: self.interactive.then(|| self.page_info()),
        })
    }

    /// 主フレームの DOM を番号つきの HTML で返す（`RENDER-11`・TASK-122。設計 §14.3）。
    ///
    /// `page` は照合しない（いまの文書を返す。§14.2）。現在の URL が http / https でなければ
    /// `invalid_state`（`about:blank` へ移った後など。#1581 の引き継ぎ）。
    fn dom_snapshot(&mut self) -> Result<DomSnapshotResult, HandlerError> {
        let (Some(page), Some(engine)) = (self.page.as_ref(), self.engine.as_ref()) else {
            return Err(HandlerError(ErrorKind::NotLoaded));
        };
        let url = page
            .webview
            .url()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or(HandlerError(ErrorKind::InvalidState))?
            .to_string();
        if url.len() > MAX_URL_BYTES {
            return Err(HandlerError(ErrorKind::TooLarge));
        }
        let deadline = Instant::now() + Duration::from_millis(DOM_SNAPSHOT_TIMEOUT_MS);
        let value = match page.evaluate(engine, dom::serialize_script(), deadline) {
            Ok(v) => v,
            Err(e) => {
                if matches!(e, PageError::Crashed | PageError::DocumentGone) {
                    self.page = None;
                }
                return Err(HandlerError(e.kind()));
            }
        };
        let s = dom::parse_serialized(value).map_err(|e| HandlerError(e.kind()))?;
        self.snapshot_seq = self.snapshot_seq.saturating_add(1);
        let id = format!("s{}", self.snapshot_seq);
        let element_count = s.element_ids.len() as u64;
        self.snapshot = Some(Snapshot {
            id: id.clone(),
            element_ids: s.element_ids,
        });
        Ok(DomSnapshotResult {
            page: self.page_info(),
            url,
            html: s.html,
            element_count,
            snapshot: id,
            iframes: s.iframes,
            shadow_roots: s.shadow_roots,
            redacted: s.redacted,
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

    /// 現在ページで `selector` の最初の一致要素が描画されるかを返す（`RENDER-3`・TASK-38.4）。
    ///
    /// セレクタの検証を最初に行うため、未ロードでも不正なセレクタは `invalid_params`。
    fn visibility(&mut self, p: &SelectorParams) -> Result<VisibilityResult, HandlerError> {
        let script = layout::visibility_script(&p.selector).map_err(|e| HandlerError(e.kind()))?;
        let value = self.run_query(script)?;
        layout::parse_visibility(value).map_err(|e| HandlerError(e.kind()))
    }

    /// 現在ページで `selector` の最初の一致要素の境界ボックスを返す（`RENDER-3`・TASK-38.5・#234）。
    ///
    /// 座標はビューポート相対の CSS px。描画されない要素は (0,0,0,0)（`layout` の `//!` 参照）。
    /// セレクタの検証を最初に行うため、未ロードでも不正なセレクタは `invalid_params`。
    fn bounding_box(&mut self, p: &SelectorParams) -> Result<BoundingBoxResult, HandlerError> {
        let script =
            layout::bounding_box_script(&p.selector).map_err(|e| HandlerError(e.kind()))?;
        let value = self.run_query(script)?;
        layout::parse_bounding_box(value).map_err(|e| HandlerError(e.kind()))
    }
}

impl ServoHandler {
    /// 要素クエリ用スクリプトを現在ページで 5 秒の期限付きで評価する（visibility・bounding_box 共通）。
    fn run_query(&mut self, script: String) -> Result<servo::JSValue, HandlerError> {
        let (Some(page), Some(engine)) = (self.page.as_ref(), self.engine.as_ref()) else {
            return Err(HandlerError(ErrorKind::NotLoaded));
        };
        let deadline = Instant::now() + layout::QUERY_TIMEOUT;
        match page.evaluate(engine, script, deadline) {
            Ok(v) => Ok(v),
            Err(e) => {
                // クラッシュ・文書消失したページは保持しない（以後は not_loaded になる）。
                if matches!(e, PageError::Crashed | PageError::DocumentGone) {
                    self.page = None;
                }
                Err(HandlerError(e.kind()))
            }
        }
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
        assert_eq!(PageError::Script.kind(), ErrorKind::Internal);
        assert_eq!(PageError::DocumentGone.kind(), ErrorKind::NotLoaded);
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
        let mut h = ServoHandler::new(None, false);
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
    fn capabilities_list_all_implemented_methods() {
        assert_eq!(
            ServoHandler::new(None, false).capabilities(),
            &[
                Method::Load,
                Method::Screenshot,
                Method::Visibility,
                Method::BoundingBox,
            ]
        );
    }

    /// RENDER-11・TASK-122: 操作モードのハンドラだけが dom_snapshot を載せる。未ロードは not_loaded。
    #[test]
    fn interactive_handler_advertises_dom_snapshot() {
        let mut h = ServoHandler::new(None, true);
        assert!(h.capabilities().contains(&Method::DomSnapshot));
        assert!(
            !ServoHandler::new(None, false)
                .capabilities()
                .contains(&Method::DomSnapshot)
        );
        assert_eq!(
            h.dom_snapshot().err(),
            Some(HandlerError(ErrorKind::NotLoaded))
        );
    }

    /// 設計 §14.2: session は 32 桁の 16 進で、起動（ハンドラ）ごとに変わる。
    #[test]
    fn session_is_hex_and_changes() {
        let a = new_session();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, new_session());
    }

    /// RENDER-3: 未ロードは not_loaded、不正なセレクタは未ロードでも invalid_params。
    #[test]
    fn visibility_without_page_is_rejected() {
        let mut h = ServoHandler::new(None, false);
        let params = |s: &str| SelectorParams {
            selector: s.to_string(),
        };
        let err = h.visibility(&params("#a")).err().expect("must fail");
        assert_eq!(err.0, ErrorKind::NotLoaded);
        let err = h.visibility(&params("a\u{1}")).err().expect("must fail");
        assert_eq!(err.0, ErrorKind::InvalidParams);
        assert!(h.engine.is_none());
    }

    /// RENDER-3（TASK-38.5・#234）: 未ロードは not_loaded、不正なセレクタは未ロードでも invalid_params。
    #[test]
    fn bounding_box_without_page_is_rejected() {
        let mut h = ServoHandler::new(None, false);
        let params = |s: &str| SelectorParams {
            selector: s.to_string(),
        };
        let err = h.bounding_box(&params("#a")).err().expect("must fail");
        assert_eq!(err.0, ErrorKind::NotLoaded);
        let err = h.bounding_box(&params("a\u{1}")).err().expect("must fail");
        assert_eq!(err.0, ErrorKind::InvalidParams);
        assert!(h.engine.is_none());
    }
}

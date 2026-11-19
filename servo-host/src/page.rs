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
//! 操作（`act`）と待機（`wait`）も同じ現在ページの上で行う（TASK-126。スクリプトは `act`、クリックの判定は
//! WebDriver の命令〔`servo-base` の受け口〕。設計 §14.4・§14.5）。
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

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use servo::{
    InputEvent, InputEventId, InputEventResult, JSValue, JavaScriptEvaluationError, Key, KeyState,
    KeyboardEvent, LoadStatus, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    RgbaImage, ScreenshotCaptureError, ServoUrl, WebDriverCommandMsg, WebDriverScriptCommand,
    WebView, WebViewBuilder, WebViewDelegate, WebViewPoint,
};
use servo_base::generic_channel::{TryReceiveError, channel, oneshot};
use servo_base::id::BrowsingContextId;

use crate::act::{self, ConditionOutcome, ExpectOutcome};
use crate::engine::VIEWPORT;

use crate::dom;
use crate::engine::{Engine, LoopbackProxy};
use crate::layout;
use crate::screenshot::{self, SCREENSHOT_TIMEOUT};
use crate::server::{
    ACT_TIMEOUT_MS, ActParams, ActResult, Action, BoundingBoxResult, DOM_SNAPSHOT_TIMEOUT_MS,
    DomSnapshotResult, ErrorKind, Handler, HandlerError, LoadParams, LoadResult, MAX_URL_BYTES,
    Method, PageInfo, ScreenshotParams, ScreenshotResult, SelectorParams, VisibilityResult,
    WaitCondition, WaitParams, WaitResult, is_valid_url,
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
    /// 送ったがまだ処理の済んでいない入力イベント（`act` が処理の完了を待つ。TASK-126）。
    pending_inputs: RefCell<HashSet<InputEventId>>,
    /// `act` の間に、ページから始まる遷移を許可したか（`navigated` の判定で読み込みの開始まで待つ。TASK-126）。
    navigation_requested: Cell<bool>,
    /// `act` の間に、扱わない要求（新しいウィンドウ・ファイル選択）が来たか（`unsupported` で返す。設計 §14.4）。
    unsupported_requested: Cell<bool>,
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
            self.navigation_requested.set(true);
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

    fn request_create_new(&self, _parent: WebView, _request: servo::CreateNewWebViewRequest) {
        // 新しいウィンドウは作らない（要求は捨てると拒否になる。ページから見ると `window.open` が `null`）。
        self.unsupported_requested.set(true);
    }

    fn show_embedder_control(&self, _webview: WebView, control: servo::EmbedderControl) {
        // ファイル選択は扱わない（捨てると「選ばなかった」になる）。`select` の選択肢・IME・ダイアログは既定の
        // まま捨てる（ダイアログは 2026-11-19 の判断。#1592）。
        if matches!(control, servo::EmbedderControl::FilePicker(_)) {
            self.unsupported_requested.set(true);
        }
    }

    fn notify_input_event_handled(
        &self,
        _webview: WebView,
        event_id: InputEventId,
        _result: InputEventResult,
    ) {
        self.pending_inputs.borrow_mut().remove(&event_id);
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
        .user_content_manager(engine.user_content_manager())
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
    /// `act` の `snapshot` と照合する識別子。
    id: String,
    /// 番号の順の要素 ID（`JSValue::Element`）。`act` が照合と WebDriver の命令に使う。
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
                Method::Act,
                Method::Wait,
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
        let current_url = || {
            page.webview
                .url()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .map(|u| u.to_string())
        };
        // 評価の待ちの間もイベントループが回り、遷移や `pushState` の通知が処理されうる。評価の前後で
        // 文書番号が変わったら、古い文書の HTML・要素 ID に新しい URL・番号を付けないよう結果を捨て、
        // 期限内で取り直す（設計 §14.2・§14.3）。期限を過ぎたら timeout。
        let deadline = Instant::now() + Duration::from_millis(DOM_SNAPSHOT_TIMEOUT_MS);
        let (s, url, doc_seq) = loop {
            let before = self.doc_seq.get();
            current_url().ok_or(HandlerError(ErrorKind::InvalidState))?;
            let value = match page.evaluate(engine, dom::serialize_script(), deadline) {
                Ok(v) => v,
                Err(e) => {
                    if matches!(e, PageError::Crashed | PageError::DocumentGone) {
                        self.page = None;
                    }
                    return Err(HandlerError(e.kind()));
                }
            };
            let url = current_url().ok_or(HandlerError(ErrorKind::InvalidState))?;
            if self.doc_seq.get() == before {
                break (dom::parse_serialized(value), url, before);
            }
            if Instant::now() >= deadline {
                return Err(HandlerError(ErrorKind::Timeout));
            }
        };
        let s = s.map_err(|e| HandlerError(e.kind()))?;
        if url.len() > MAX_URL_BYTES {
            return Err(HandlerError(ErrorKind::TooLarge));
        }
        self.snapshot_seq = self.snapshot_seq.saturating_add(1);
        let id = format!("s{}", self.snapshot_seq);
        let element_count = s.element_ids.len() as u64;
        self.snapshot = Some(Snapshot {
            id: id.clone(),
            element_ids: s.element_ids,
        });
        Ok(DomSnapshotResult {
            page: PageInfo {
                session: self.session.clone(),
                doc_seq,
            },
            url,
            html: s.html,
            element_count,
            snapshot: id,
            iframes: s.iframes,
            shadow_roots: s.shadow_roots,
            redacted: s.redacted,
        })
    }

    /// 要素を操作する（`RENDER-10`・TASK-126。設計 §14.4）。
    ///
    /// 検査の順: キーの解釈（`invalid_params`）→ 現在の URL（`invalid_state`）→ `page`・`snapshot`・番号（`stale`）
    /// → 照合値（`stale`）・`disabled`（`not_actionable`）→ 操作 → 入力の処理の完了。入力文字・キーはログ・エラー・
    /// 応答に出さない（応答は `page` と `navigated` だけ）。人の操作に見せる遅延・軌跡・乱数は入れない（`SEC-2`）。
    /// 期限（[`ACT_TIMEOUT_MS`]）を過ぎたら `timeout`（本体はホストを破棄して `session_lost` にする。§14.6）。
    fn act(&mut self, p: &ActParams) -> Result<ActResult, HandlerError> {
        let key = match &p.action {
            Action::Press { key } => {
                Some(act::parse_key(key).ok_or(HandlerError(ErrorKind::InvalidParams))?)
            }
            _ => None,
        };
        let (Some(page), Some(engine)) = (self.page.as_ref(), self.engine.as_ref()) else {
            return Err(HandlerError(ErrorKind::NotLoaded));
        };
        if page
            .webview
            .url()
            .is_none_or(|u| !matches!(u.scheme(), "http" | "https"))
        {
            return Err(HandlerError(ErrorKind::InvalidState));
        }
        let start = self.doc_seq.get();
        if p.page.session != self.session || p.page.doc_seq != start {
            return Err(HandlerError(ErrorKind::Stale));
        }
        let element = self
            .snapshot
            .as_ref()
            .filter(|s| s.id == p.snapshot)
            .and_then(|s| s.element_ids.get(p.node))
            .ok_or(HandlerError(ErrorKind::Stale))?
            .clone();
        let deadline = Instant::now() + Duration::from_millis(ACT_TIMEOUT_MS);
        let result = page.perform(engine, &element, p, key.as_ref(), deadline, start);
        if page.delegate.crashed.get() {
            self.page = None;
            return Err(HandlerError(ErrorKind::LoadFailed));
        }
        match result {
            Ok(()) => {}
            // 評価の途中で文書が消えた。遷移と重なったなら stale、そうでなければページを捨てて not_loaded
            // （`run_query` と同じ扱い）。
            Err(ErrorKind::NotLoaded) if self.doc_seq.get() != start => {
                return Err(HandlerError(ErrorKind::Stale));
            }
            Err(ErrorKind::NotLoaded) => {
                self.page = None;
                return Err(HandlerError(ErrorKind::NotLoaded));
            }
            Err(e) => return Err(HandlerError(e)),
        }
        Ok(ActResult {
            page: self.page_info(),
            navigated: self.doc_seq.get() != start,
        })
    }

    /// 条件の成立を待つ（`AISNAP-18`・TASK-126。設計 §14.5）。
    ///
    /// 成立すれば `page`、`timeout_ms` までに成立しなければ `timeout`（ホストは破棄しない）。待機中に文書が
    /// 変わったら `stale`（`load_complete` を除く）。セレクタの構文が不正なら `invalid_params`。
    fn wait(&mut self, p: &WaitParams) -> Result<WaitResult, HandlerError> {
        let (Some(page), Some(engine)) = (self.page.as_ref(), self.engine.as_ref()) else {
            return Err(HandlerError(ErrorKind::NotLoaded));
        };
        let start = self.doc_seq.get();
        if p.page.session != self.session || p.page.doc_seq != start {
            return Err(HandlerError(ErrorKind::Stale));
        }
        let deadline = Instant::now() + p.timeout;
        let script = act::condition_script(&p.condition);
        loop {
            if page.delegate.crashed.get() {
                self.page = None;
                return Err(HandlerError(ErrorKind::LoadFailed));
            }
            let met = match &script {
                None => page.webview.load_status() == LoadStatus::Complete,
                Some(s) => {
                    let until = deadline.min(Instant::now() + layout::QUERY_TIMEOUT);
                    match page.evaluate(engine, s.clone(), until) {
                        Ok(v) => match act::parse_condition(&v) {
                            ConditionOutcome::Met => true,
                            ConditionOutcome::NotYet => false,
                            ConditionOutcome::Invalid => {
                                return Err(HandlerError(ErrorKind::InvalidParams));
                            }
                        },
                        // 文書の入れ替えの途中は評価できないことがある。文書番号の検査に任せる。
                        Err(PageError::DocumentGone | PageError::Script) => false,
                        Err(PageError::Timeout) => false,
                        Err(e) => {
                            if e == PageError::Crashed {
                                self.page = None;
                            }
                            return Err(HandlerError(e.kind()));
                        }
                    }
                }
            };
            if !matches!(p.condition, WaitCondition::LoadComplete) && self.doc_seq.get() != start {
                return Err(HandlerError(ErrorKind::Stale));
            }
            if met {
                return Ok(WaitResult {
                    page: self.page_info(),
                });
            }
            if Instant::now() >= deadline {
                return Err(HandlerError(ErrorKind::Timeout));
            }
            let until = (Instant::now() + WAIT_POLL).min(deadline);
            while Instant::now() < until {
                engine.servo().spin_event_loop();
                std::thread::sleep(POLL_INTERVAL);
            }
        }
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

/// 待機の間にイベントループを回す間隔（`wait` の条件を調べ直す間隔でもある）。
const WAIT_POLL: Duration = Duration::from_millis(50);
/// 入力イベントの処理が済んでから、遷移の要求の通知を受け取るために回す時間。
const ACT_SETTLE: Duration = Duration::from_millis(100);
/// 遷移の要求を許可した後、新しい文書の読み込みの開始を待つ上限。`act` は読み込みを待たない（設計 §14.6）ため
/// 短く切り、過ぎたらその時点の文書番号で `navigated` を返す（最善努力。応答の遅い遷移・iframe の遷移・
/// 文書のできない応答では偽になりうる）。
const ACT_NAVIGATION_WAIT: Duration = Duration::from_secs(2);

/// WebDriver の命令の答えを 1 回受け取る。まだ来ていなければ `None`、受け口が切れていれば `stale`
/// （閲覧文脈が入れ替わって命令が捨てられた。遷移と重なった場合）。
fn reply<T>(r: Result<T, TryReceiveError>) -> Option<Result<T, ErrorKind>> {
    match r {
        Ok(v) => Some(Ok(v)),
        Err(TryReceiveError::Empty) => None,
        Err(_) => Some(Err(ErrorKind::Stale)),
    }
}

impl CurrentPage {
    /// WebDriver の命令（ページの JS を通らない Rust 側の判定）を送り、`deadline` まで答えを待つ。受け口は
    /// 呼び出し側が `servo-base` の `channel()` で作る（答えの型を命令から推論させ、`ErrorStatus` を名指ししない
    /// ため `webdriver` crate の依存が要らない）。
    fn webdriver<R>(
        &self,
        engine: &Engine,
        deadline: Instant,
        command: WebDriverScriptCommand,
        recv: impl FnMut() -> Option<Result<R, ErrorKind>>,
    ) -> Result<R, ErrorKind> {
        engine
            .servo()
            .execute_webdriver_command(WebDriverCommandMsg::ScriptCommand(
                BrowsingContextId::from(self.webview.id()),
                command,
            ));
        wait_until(
            deadline,
            recv,
            || self.delegate.crashed.get(),
            || {
                engine.servo().spin_event_loop();
                std::thread::sleep(POLL_INTERVAL);
            },
        )
        .map_err(PageError::kind)?
    }

    /// 入力イベントを送り、処理が済んだかを [`PageDelegate::pending_inputs`] で追えるように記録する。
    fn send_input(&self, event: InputEvent) {
        let id = self.webview.notify_input_event(event);
        self.delegate.pending_inputs.borrow_mut().insert(id);
    }

    /// 送った入力イベントの処理がすべて済むまで待つ。その後、遷移の要求が来ていれば新しい文書の読み込みの
    /// 開始（文書番号の更新）まで待ち、来ていなければ要求の通知を受け取る分だけ回す（`navigated` の判定）。
    fn settle(&self, engine: &Engine, deadline: Instant, start: u64) -> Result<(), ErrorKind> {
        let spin = || {
            engine.servo().spin_event_loop();
            std::thread::sleep(POLL_INTERVAL);
        };
        wait_until(
            deadline,
            || {
                self.delegate
                    .pending_inputs
                    .borrow()
                    .is_empty()
                    .then_some(())
            },
            || self.delegate.crashed.get(),
            spin,
        )
        .map_err(PageError::kind)?;
        let until = (Instant::now() + ACT_SETTLE).min(deadline);
        while Instant::now() < until && !self.delegate.navigation_requested.get() {
            spin();
        }
        if self.delegate.navigation_requested.get() {
            let until = (Instant::now() + ACT_NAVIGATION_WAIT).min(deadline);
            while Instant::now() < until
                && self.delegate.doc_seq.get() == start
                && !self.delegate.crashed.get()
            {
                spin();
            }
        }
        Ok(())
    }

    /// クリックしてよいかを WebDriver の `ElementClick` で確かめる（要素を画面に入れ、他の要素〔iframe を含む〕に
    /// 隠れていないことをページが偽れない形で調べる。`option` 以外では副作用はスクロールだけ）。`option` は Servo が
    /// 選択とイベントまで済ませてしまうため `not_actionable`（`select` を使う）。
    fn check_clickable(
        &self,
        engine: &Engine,
        element: &str,
        deadline: Instant,
    ) -> Result<(), ErrorKind> {
        let (tx, rx) = channel().ok_or(ErrorKind::Internal)?;
        let r = self.webdriver(
            engine,
            deadline,
            WebDriverScriptCommand::ElementClick(element.to_owned(), tx),
            || reply(rx.try_recv()),
        )?;
        match r {
            Ok(Some(_)) => Ok(()),
            _ => Err(ErrorKind::NotActionable),
        }
    }

    /// 要素をクリックする（設計 §14.4）。[`Self::check_clickable`] と同じ点（WebDriver の
    /// `GetElementInViewCenterPoint`。最初の矩形を画面で切り取った中心）へ、マウスの入力イベントを送る。
    fn click(&self, engine: &Engine, element: &str, deadline: Instant) -> Result<(), ErrorKind> {
        self.check_clickable(engine, element, deadline)?;
        // 一回限りの受け口は待ち時間つきで受け取れないため、別のスレッドで受けて渡す。
        let (tx, rx) = oneshot().ok_or(ErrorKind::Internal)?;
        let (done, got) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .spawn(move || {
                let _ = done.send(rx.recv().ok());
            })
            .map_err(|_| ErrorKind::Internal)?;
        let point = self.webdriver(
            engine,
            deadline,
            WebDriverScriptCommand::GetElementInViewCenterPoint(element.to_owned(), tx),
            || match got.try_recv() {
                Ok(Some(v)) => Some(Ok(v)),
                Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some(Err(ErrorKind::Stale))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            },
        )?;
        let Ok(Some((x, y))) = point else {
            return Err(ErrorKind::NotActionable);
        };
        if !(0..i64::from(VIEWPORT.0)).contains(&x) || !(0..i64::from(VIEWPORT.1)).contains(&y) {
            return Err(ErrorKind::NotActionable);
        }
        // CSS px の点（`Point2D<f32, CSSPixel>`。euclid は再輸出されないため、再輸出された型から単位を付け替える）。
        let point = WebViewPoint::Page(servo::DevicePoint::new(x as f32, y as f32).cast_unit());
        self.send_input(InputEvent::MouseMove(MouseMoveEvent::new(point)));
        for action in [MouseButtonAction::Down, MouseButtonAction::Up] {
            self.send_input(InputEvent::MouseButton(MouseButtonEvent::new(
                action,
                MouseButton::Primary,
                point,
            )));
        }
        Ok(())
    }

    /// 要素にフォーカスがあるか（WebDriver の `GetActiveElement`。ページの JS を通らない）。
    fn focused(
        &self,
        engine: &Engine,
        element: &str,
        deadline: Instant,
    ) -> Result<bool, ErrorKind> {
        let (tx, rx) = channel().ok_or(ErrorKind::Internal)?;
        let active = self.webdriver(
            engine,
            deadline,
            WebDriverScriptCommand::GetActiveElement(tx),
            || reply(rx.try_recv()),
        )?;
        Ok(active.as_deref() == Some(element))
    }

    /// キーを 1 回押して離す（入力イベント。JS へ連結しない）。
    fn press(&self, key: &Key) {
        for state in [KeyState::Down, KeyState::Up] {
            self.send_input(InputEvent::Keyboard(KeyboardEvent::from_state_and_key(
                state,
                key.clone(),
            )));
        }
    }

    /// `act` の操作の本体。照合 → 操作してよいかの検査 → 操作 → 入力の処理が済むまで待つ（設計 §14.4 の検査の順）。
    /// 副作用（値の消去・選択）は、操作してよいかを確かめてから行う。
    fn perform(
        &self,
        engine: &Engine,
        element: &str,
        p: &ActParams,
        key: Option<&Key>,
        deadline: Instant,
        start: u64,
    ) -> Result<(), ErrorKind> {
        let v = self
            .evaluate(engine, act::expect_script(element, &p.expect), deadline)
            .map_err(PageError::kind)?;
        match act::parse_expect(&v) {
            ExpectOutcome::Ok => {}
            ExpectOutcome::Stale => return Err(ErrorKind::Stale),
            ExpectOutcome::Disabled => return Err(ErrorKind::NotActionable),
        }
        self.delegate.navigation_requested.set(false);
        self.delegate.unsupported_requested.set(false);
        // `option` は WebDriver の `ElementClick` が選択とイベントまで行うため、命令を送る前に断る（`select` を使う）。
        if p.expect.tag == "option" && !matches!(p.action, Action::Select { .. }) {
            return Err(ErrorKind::NotActionable);
        }
        match &p.action {
            Action::Click => self.click(engine, element, deadline)?,
            Action::Fill { text } => {
                // 操作してよいかを確かめてから既存の値を消し（WebDriver の `ElementClear`。フォーカスが外れる）、
                // クリックでフォーカスを移して 1 文字ずつキーの入力イベントで渡す（JS へ連結しない）。
                self.check_clickable(engine, element, deadline)?;
                let (tx, rx) = channel().ok_or(ErrorKind::Internal)?;
                self.webdriver(
                    engine,
                    deadline,
                    WebDriverScriptCommand::ElementClear(element.to_owned(), tx),
                    || reply(rx.try_recv()),
                )?
                .map_err(|_| ErrorKind::NotActionable)?;
                self.click(engine, element, deadline)?;
                self.settle(engine, deadline, start)?;
                if !self.focused(engine, element, deadline)? {
                    return Err(ErrorKind::NotActionable);
                }
                for ch in text.chars() {
                    self.press(&Key::Character(ch.to_string()));
                }
                self.settle(engine, deadline, start)?;
                if !self.focused(engine, element, deadline)? {
                    return Err(ErrorKind::NotActionable);
                }
            }
            Action::Select { values } => {
                // 操作してよいかを確かめてから、スクリプトで選択肢を設定する（単一選択の `select` は WebDriver の
                // 命令では選べない。`act::select_script`）。
                self.check_clickable(engine, element, deadline)?;
                let v = self
                    .evaluate(engine, act::select_script(element, values), deadline)
                    .map_err(PageError::kind)?;
                match v {
                    JSValue::String(s) if s == "ok" => {}
                    JSValue::String(s) if s == "stale" => return Err(ErrorKind::Stale),
                    JSValue::String(s) if s == "multi" => return Err(ErrorKind::InvalidParams),
                    _ => return Err(ErrorKind::NotActionable),
                }
            }
            Action::Press { .. } => {
                // フォーカスが無ければクリックで移す（リンクの遷移・チェックボックスの反転などクリックの副作用も起きる）。
                let key = key.ok_or(ErrorKind::InvalidParams)?;
                if !self.focused(engine, element, deadline)? {
                    self.click(engine, element, deadline)?;
                    self.settle(engine, deadline, start)?;
                    if !self.focused(engine, element, deadline)? {
                        return Err(ErrorKind::NotActionable);
                    }
                }
                self.press(key);
            }
        }
        self.settle(engine, deadline, start)?;
        if self.delegate.unsupported_requested.get() {
            return Err(ErrorKind::Unsupported);
        }
        Ok(())
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

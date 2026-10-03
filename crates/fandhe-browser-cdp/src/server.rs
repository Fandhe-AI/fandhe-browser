//! cdp crate 固有の状態型 `CdpState`（TASK-41（41.2）・#170、ビヘイビア `CDP-1`・`AISNAP-6`・MS-3）。
//!
//! core の共通 `AppState`（TASK-41.1）を変更せず `Arc` で内包し、その上に CDP 固有の
//! ターゲット表・セッション表（[`crate::target`]）とブラウザ ID を載せる。
//! `AppState` は cli（TASK-41.5）が生成して ai と同一インスタンスを共有する
//! （`AISNAP-6`）。本型は HTTP・WebSocket・非同期ランタイムの型を含まない。
//!
//! # スタブについて
//!
//! `/json/*` ルータ（[`router`]。TASK-41.3）と `/devtools/browser/{id}` の WS 受け口設定
//! （[`browser_websocket_config`]。TASK-41.4。公開入口は [`endpoints`]）は実装済み。WS のメッセージハンドラは
//! [`crate::protocol`] のディスパッチャへ委譲する（TASK-42.1）。組込みメソッドは `Page.navigate`（TASK-42.2）のみで、残りの個別メソッドは
//! TASK-42.2 以降（REPAIR-3）。
//! 初期ターゲット（`about:blank`）の自動作成は行わず、後続タスクの判断に委ねる。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fandhe_backend_core::plugin_websocket::pattern::PathPatternError;
use fandhe_backend_core::plugin_websocket::{
    PingIntervalError, WebSocketConfig, WsHandshakeContext,
};
use fandhe_backend_http::request::RequestHead;
use fandhe_backend_http::response::Response;
use fandhe_backend_routes::Router;
use fandhe_browser_core::AppState;

use crate::discovery::{self, Authority, BROWSER_WS_PATH_PATTERN, HostError};
use crate::dom::IssuedDocuments;
use crate::method_log::{ReceivedMethodLog, ReceivedMethodsSnapshot};
use crate::navigation::TargetNavigations;
use crate::protocol::Dispatcher;
use crate::target::{BrowserId, TargetRegistry};
use crate::ws;

/// `CdpState::new` が払い出すブラウザ ID 用のプロセス内連番。
static BROWSER_SEQ: AtomicU64 = AtomicU64::new(1);

/// CDP サーバーのルータ・ハンドラが共有する状態。共有は `Arc<CdpState>` で行う（`Clone` なし）。
///
/// ブラウザ ID は識別子であって認証情報ではない。アクセス制御は `SEC-4`
/// （ローカルホスト限定バインド等）と TASK-41.4・41.5 の責務。
pub struct CdpState {
    app: Arc<AppState>,
    browser_id: BrowserId,
    registry: TargetRegistry,
    /// ターゲット別の遷移状態（`Page.navigate` が書き、`DOM.getDocument` が読む。`CDP-1`）。
    navigations: TargetNavigations,
    /// `DOM.getDocument` が nodeId を払い出した文書の記録（`DOM.querySelector` が世代検証に使う。`CDP-1`）。
    dom_issued: IssuedDocuments,
    /// 受信メソッド名の集計（`Dispatcher::dispatch_on` が書く。`CDP-6`）。
    method_log: ReceivedMethodLog,
}

impl CdpState {
    /// 決定的（プロセス内連番）なブラウザ ID で構築する。
    pub fn new(app: Arc<AppState>) -> Self {
        let n = BROWSER_SEQ.fetch_add(1, Ordering::Relaxed);
        Self::with_browser_id(app, BrowserId::from_counter(n))
    }

    /// ブラウザ ID を指定して構築する（テスト・cli からの注入用）。
    pub fn with_browser_id(app: Arc<AppState>, browser_id: BrowserId) -> Self {
        Self {
            app,
            browser_id,
            registry: TargetRegistry::new(),
            navigations: TargetNavigations::new(),
            dom_issued: IssuedDocuments::new(),
            method_log: ReceivedMethodLog::new(),
        }
    }

    /// 受信メソッド集計（`Dispatcher::dispatch_on` が記録する）。
    pub(crate) fn method_log(&self) -> &ReceivedMethodLog {
        &self.method_log
    }

    /// 受信したメソッド名の件数スナップショット（`CDP-6` の受信ログ。読み取り専用）。
    ///
    /// 本番の出力先（ファイル・構造化ログ）は #221（TASK-10.3）で決める。未実装メソッドは
    /// `unimplemented` に数えられ、必須メソッドの洗い出しに使える。
    pub fn received_methods(&self) -> ReceivedMethodsSnapshot {
        self.method_log.snapshot()
    }

    /// 内包する core の共通状態。
    pub fn app_state(&self) -> &Arc<AppState> {
        &self.app
    }

    /// `/devtools/browser/{id}` の ID。
    pub fn browser_id(&self) -> &BrowserId {
        &self.browser_id
    }

    /// ターゲット・セッション表。
    pub fn registry(&self) -> &TargetRegistry {
        &self.registry
    }

    /// nodeId 払い出し済み文書の記録（crate 内部専用。`CDP-1`）。
    pub(crate) fn dom_issued(&self) -> &IssuedDocuments {
        &self.dom_issued
    }

    /// ターゲット別の遷移状態表（crate 内部専用。`CDP-1`）。
    pub(crate) fn navigations(&self) -> &TargetNavigations {
        &self.navigations
    }
}

impl fmt::Debug for CdpState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CdpState")
            .field("browser_id", &self.browser_id)
            .field("targets", &self.registry.target_count())
            .field("sessions", &self.registry.session_count())
            .finish()
    }
}

/// CDP の HTTP ルータと `/devtools/browser/{id}` の WebSocket 設定を一体で保持する組（`CDP-1`・
/// TASK-41.4・MS-3。REPAIR-3・公開 API の契約）。
///
/// `/json/version` は `webSocketDebuggerUrl` を返すため、ルータだけを使うと WebSocket が
/// 未配線のまま案内 URL へ接続失敗する。両者を別々に公開せず [`endpoints`] で同時に構築させ、
/// [`CdpEndpoints::into_parts`] で取り出して cli（TASK-41.5）が
/// `Server::new().handler(router).websocket(config)` へ必ず対で渡す形にする。
pub struct CdpEndpoints {
    router: Router,
    websocket: WebSocketConfig,
}

impl CdpEndpoints {
    /// `(ルータ, WebSocket 設定)` に分解する。`Server::handler` と `Server::websocket` へ
    /// 両方を渡すこと（片方だけだと `/json/version` の URL が接続不能になる）。
    pub fn into_parts(self) -> (Router, WebSocketConfig) {
        (self.router, self.websocket)
    }
}

/// ルータと WebSocket 設定を一体で構築する（`CDP-1`・TASK-41.3・41.4）。
///
/// 個別構築の [`router`]・[`browser_websocket_config`] は非公開で、`webSocketDebuggerUrl` を
/// 返しながら WebSocket が未配線という状態を公開 API から作れないようにする。
pub fn endpoints(state: &Arc<CdpState>) -> Result<CdpEndpoints, WsConfigError> {
    let websocket = browser_websocket_config(state)?;
    Ok(CdpEndpoints {
        router: router(Arc::clone(state)),
        websocket,
    })
}

/// JSON 応答の `Content-Type`（Chromium 互換）。
const JSON_CONTENT_TYPE: &str = "application/json; charset=UTF-8";

/// CDP ディスカバリ用の HTTP ルータ（`GET /json/version`・`GET /json/list`・`GET /json`。
/// `CDP-1`・TASK-41.3・MS-3）。
///
/// Playwright の `connectOverCDP`・Puppeteer が接続前に叩くエンドポイントを提供する。
/// `/json` は `/json/list` の互換エイリアス。Host ヘッダは localhost / IP リテラルのみ許可し
/// （DNS rebinding 対策。[`crate::discovery`]）、違反は 400 / 403 を返す。
///
/// `Router` は WebSocket を運べないため、`/devtools/browser/{id}` は
/// [`browser_websocket_config`] を `Server::websocket` へ渡して受ける。両者は公開 API の
/// [`endpoints`] が一体で構築する（本関数単体は非公開）。
/// cli は `Router::merge` で AI API のルータと合成する。本関数は bind・アクセス制御
/// （loopback 限定。`SEC-4`）を行わない。`/json/version` の `webSocketDebuggerUrl` は
/// 検証済み Host と [`CdpState::browser_id`] から組み立てる。
pub(crate) fn router(state: Arc<CdpState>) -> Router {
    let version_state = Arc::clone(&state);
    let list_state = Arc::clone(&state);
    let json_state = state;
    Router::new()
        .route("GET", "/json/version", move |head, _| {
            version_response(&version_state, head)
        })
        .route("GET", "/json/list", move |head, _| {
            list_response(&list_state, head)
        })
        .route("GET", "/json", move |head, _| {
            list_response(&json_state, head)
        })
}

/// `/json/version` の応答を作る。
fn version_response(state: &CdpState, head: &RequestHead) -> Response {
    match Authority::from_host_header(head.header("host")) {
        Ok(authority) => json_response(discovery::version_body(&authority, state.browser_id())),
        Err(e) => host_error_response(e),
    }
}

/// サーバー起点 Ping の間隔と Pong 待ち。`interval + pong_timeout` を core の
/// 既定 idle timeout（60 秒）未満にし、生存クライアントは Pong で idle が延長され続け、
/// 無応答の対向は 40 秒以内に切断される。
const WS_PING_INTERVAL: Duration = Duration::from_secs(20);
const WS_PONG_TIMEOUT: Duration = Duration::from_secs(20);

/// [`browser_websocket_config`] の構築エラー。
#[derive(Debug)]
#[non_exhaustive]
pub enum WsConfigError {
    /// パスパターンが不正。
    PathPattern(PathPatternError),
    /// Ping 間隔の設定が不正。
    PingInterval(PingIntervalError),
    /// コマンドディスパッチャの組込み表が不正（登録名の不正・重複）。
    Dispatcher,
}

impl fmt::Display for WsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathPattern(_) => f.write_str("invalid websocket path pattern"),
            Self::PingInterval(_) => f.write_str("invalid websocket ping interval"),
            Self::Dispatcher => f.write_str("invalid command dispatcher table"),
        }
    }
}

impl std::error::Error for WsConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PathPattern(e) => Some(e),
            Self::PingInterval(e) => Some(e),
            Self::Dispatcher => None,
        }
    }
}

/// `/devtools/browser/{id}` の WebSocket 受け口設定（`CDP-1`・TASK-41.4・MS-3）。
///
/// cli（TASK-41.5）が [`endpoints`] の結果を `Server::new().handler(router)
/// .websocket(config)` の形で組み立てる。受理判定
/// （Host・Origin・ブラウザ ID。[`crate::ws`]）は本設定のハンドシェイク検査で行う。
/// メッセージハンドラは [`crate::protocol`] のディスパッチャへ委譲する（TASK-42.1）。
/// bind・loopback 限定（`SEC-4`）・同時接続数上限は cli / core 側の責務。
/// メッセージ・フレームサイズ上限は core の既定値（DoS 安全側）のまま使う。
pub(crate) fn browser_websocket_config(
    state: &Arc<CdpState>,
) -> Result<WebSocketConfig, WsConfigError> {
    let browser_id = state.browser_id().clone();
    let dispatcher = Dispatcher::builtin().map_err(|_| WsConfigError::Dispatcher)?;
    let config = WebSocketConfig::default()
        .with_path_pattern(BROWSER_WS_PATH_PATTERN)
        .map_err(WsConfigError::PathPattern)?
        .with_ping_interval(WS_PING_INTERVAL, WS_PONG_TIMEOUT)
        .map_err(WsConfigError::PingInterval)?
        .with_handshake_check(move |ctx: &WsHandshakeContext<'_>| {
            ws::check_handshake(
                ctx.header("host"),
                ctx.header("origin"),
                ctx.param("id"),
                &browser_id,
            )
        })
        .with_handler(ws::BrowserSessionHandler::new(
            Arc::clone(state),
            dispatcher,
        ));
    Ok(config)
}

/// `/json/list`・`/json` の応答を作る。authority は使わないが、version と同じく
/// 不正な Host を持つ（DNS rebinding 経由の）リクエストにはターゲット一覧も渡さない。
fn list_response(state: &CdpState, head: &RequestHead) -> Response {
    if let Err(e) = Authority::from_host_header(head.header("host")) {
        return host_error_response(e);
    }
    json_response(discovery::list_body(&state.registry().targets()))
}

/// Host 検証エラーを 400 / 403 へ写像する。本体は入力値を含まない固定文言。
pub(crate) fn host_error_response(e: HostError) -> Response {
    let status = match e {
        HostError::NotAllowed => 403,
        _ => 400,
    };
    error_response(status, &e.to_string())
}

/// `{"error":"<固定文言>"}` の JSON エラー応答。`msg` は呼び出し側の固定文言に限る
/// （ヘッダ値などの入力を渡さない）。
pub(crate) fn error_response(status: u16, msg: &str) -> Response {
    let body = format!("{{\"error\":\"{msg}\"}}").into_bytes();
    Response::new(status, body).with_content_type(JSON_CONTENT_TYPE)
}

/// シリアライズ結果を 200 の JSON 応答にする。失敗時は panic せず 500 を返す。
fn json_response(body: Result<Vec<u8>, serde_json::Error>) -> Response {
    match body {
        Ok(b) => Response::new(200, b).with_content_type(JSON_CONTENT_TYPE),
        Err(_) => Response::new(500, br#"{"error":"internal error"}"#.to_vec())
            .with_content_type(JSON_CONTENT_TYPE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdp1_cdp_state_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CdpState>();
    }
}

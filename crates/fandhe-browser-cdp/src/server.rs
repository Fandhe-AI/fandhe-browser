//! cdp crate 固有の状態型 `CdpState`（TASK-41（41.2）・#170、ビヘイビア `CDP-1`・`AISNAP-6`・MS-3）。
//!
//! core の共通 `AppState`（TASK-41.1）を変更せず `Arc` で内包し、その上に CDP 固有の
//! ターゲット表・セッション表（[`crate::target`]）とブラウザ ID を載せる。
//! `AppState` は cli（TASK-41.5）が生成して ai と同一インスタンスを共有する
//! （`AISNAP-6`）。本型は HTTP・WebSocket・非同期ランタイムの型を含まない。
//!
//! # スタブについて
//!
//! `/json/*` ルータ（[`router`]。TASK-41.3）は実装済み。`/devtools/browser/{id}` 受け口
//! （TASK-41.4）は後続タスクで [`router`] へ追加する（REPAIR-3）。初期ターゲット
//! （`about:blank`）の自動作成は行わず、後続タスクの判断に委ねる。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fandhe_backend_http::request::RequestHead;
use fandhe_backend_http::response::Response;
use fandhe_backend_routes::Router;
use fandhe_browser_core::AppState;

use crate::discovery::{self, Authority, HostError};
use crate::target::{BrowserId, TargetRegistry};

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
        }
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

/// JSON 応答の `Content-Type`（Chromium 互換）。
const JSON_CONTENT_TYPE: &str = "application/json; charset=UTF-8";

/// CDP ディスカバリ用の HTTP ルータ（`GET /json/version`・`GET /json/list`・`GET /json`。
/// `CDP-1`・TASK-41.3・MS-3）。
///
/// Playwright の `connectOverCDP`・Puppeteer が接続前に叩くエンドポイントを提供する。
/// `/json` は `/json/list` の互換エイリアス。Host ヘッダは localhost / IP リテラルのみ許可し
/// （DNS rebinding 対策。[`crate::discovery`]）、違反は 400 / 403 を返す。
///
/// TASK-41.4（`/devtools/browser/{id}` の WS 受け口）が同じ関数へルートを追加し、cli
/// （TASK-41.5）が `Router::merge` で AI API のルータと合成する。本関数は bind・アクセス
/// 制御（loopback 限定。`SEC-4`）を行わない。`webSocketDebuggerUrl` が指す WS 受け口は
/// TASK-41.4 まで存在しない（REPAIR-3）。
pub fn router(state: Arc<CdpState>) -> Router {
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
    let authority = match Authority::from_host_header(head.header("host")) {
        Ok(a) => a,
        Err(e) => return host_error_response(e),
    };
    json_response(discovery::version_body(state.browser_id(), &authority))
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
fn host_error_response(e: HostError) -> Response {
    let status = match e {
        HostError::NotAllowed => 403,
        _ => 400,
    };
    let body = format!("{{\"error\":\"{e}\"}}").into_bytes();
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

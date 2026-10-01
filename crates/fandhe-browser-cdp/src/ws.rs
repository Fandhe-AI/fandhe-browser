//! `/devtools/browser/{id}` WebSocket 受け口の受理判定とメッセージハンドラ
//! （TASK-41（41.4）・#173、`CDP-1`・`SEC-2`・MS-3）。
//!
//! `crate::server::browser_websocket_config` が `WebSocketConfig` へ組み込む部品で、
//! ハンドシェイク判定は純関数として切り出し、3 OS で単体テストできるようにしている。
//! 実際のハンドシェイク（RFC 6455）・フレーミングは core の `fandhe-backend-core`
//! （websocket feature）が担い、cli（TASK-41.5）が `Server::websocket` で配線する。
//!
//! # スタブについて
//!
//! [`BrowserSessionHandler`] は `crate::protocol::Dispatcher` へ委譲するだけで、組込み
//! メソッド表は現状空（個別メソッドは TASK-42.2〜42.5、未実装メソッドの応答方針は
//! 42.6・`CDP-6`）。未実装メソッドへ成功を返さない（`SEC-2`）。

use std::sync::Arc;

use fandhe_backend_core::plugin_websocket::BoxFuture;
use fandhe_backend_core::plugin_websocket::handler::{
    WsHandlerError, WsMessage, WsMessageHandler, WsOutcome,
};
use fandhe_backend_http::response::Response;

use crate::discovery::Authority;
use crate::protocol::Dispatcher;
use crate::server::{CdpState, error_response, host_error_response};
use crate::target::BrowserId;

/// ハンドシェイクの受理判定。拒否時はそのまま返せる HTTP 応答を `Err` で返す。
///
/// 1. Host を localhost / IP リテラルに限定（DNS rebinding 対策。`/json/*` と同じ検証）
/// 2. `Origin` ヘッダ付きは拒否（ブラウザ上のページからの cross-site WebSocket
///    hijacking 対策。Playwright / Puppeteer 等の Node クライアントは Origin を送らない。
///    許可リスト機構は将来の設定項目）
/// 3. パスの `{id}` が自身の [`BrowserId`] と一致すること（未知の ID は 404）
///
/// 応答本体は固定文言のみで、ヘッダ値・ID を含めない。
pub(crate) fn check_handshake(
    host: Option<&str>,
    origin: Option<&str>,
    id: Option<&str>,
    expected: &BrowserId,
) -> Result<(), Response> {
    if let Err(e) = Authority::from_host_header(host) {
        return Err(host_error_response(e));
    }
    if origin.is_some() {
        return Err(error_response(403, "origin not allowed"));
    }
    match id.map(BrowserId::parse) {
        Some(Ok(got)) if got == *expected => Ok(()),
        _ => Err(error_response(404, "unknown browser id")),
    }
}

/// `/devtools/browser/{id}` セッションのメッセージハンドラ。
///
/// 受信テキストを [`Dispatcher`]（`crate::protocol`）へ渡し、レスポンスとイベントを
/// 順序どおり WebSocket テキストフレームで返す。`TASK-42`（42.1）・`CDP-1`。
pub(crate) struct BrowserSessionHandler {
    state: Arc<CdpState>,
    dispatcher: Dispatcher,
}

impl BrowserSessionHandler {
    /// `crate::server::browser_websocket_config` から接続設定の構築時に 1 度だけ作る。
    pub(crate) fn new(state: Arc<CdpState>, dispatcher: Dispatcher) -> Self {
        Self { state, dispatcher }
    }
}

impl WsMessageHandler for BrowserSessionHandler {
    fn name(&self) -> &'static str {
        "cdp-browser"
    }

    fn on_message(&self, msg: WsMessage) -> BoxFuture<'_, Result<WsOutcome, WsHandlerError>> {
        Box::pin(async move {
            Ok(match msg {
                WsMessage::Text(t) => {
                    let frames = self
                        .dispatcher
                        .dispatch(&self.state, &t)
                        .await
                        .into_frames();
                    WsOutcome::Reply(frames.into_iter().map(WsMessage::Text).collect())
                }
                // CDP はテキストフレームのみ。バイナリは受け付けず切断する。
                WsMessage::Binary(_) => WsOutcome::Close,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bid() -> BrowserId {
        BrowserId::parse("fixed-1").unwrap()
    }

    fn rejected(host: Option<&str>, origin: Option<&str>, id: Option<&str>) -> (u16, String) {
        let r = check_handshake(host, origin, id, &bid()).unwrap_err();
        (r.status, String::from_utf8(r.body).unwrap())
    }

    #[test]
    fn cdp1_ws_handshake_accepts_matching_id() {
        assert!(check_handshake(Some("127.0.0.1:9222"), None, Some("fixed-1"), &bid()).is_ok());
        assert!(check_handshake(Some("[::1]:9222"), None, Some("fixed-1"), &bid()).is_ok());
    }

    #[test]
    fn cdp1_ws_handshake_rejects_host_origin_and_id() {
        let h = Some("127.0.0.1:9222");
        let i = Some("fixed-1");
        assert_eq!(
            rejected(None, None, i),
            (400, r#"{"error":"invalid host header"}"#.to_owned())
        );
        assert_eq!(
            rejected(Some("evil.example:9222"), None, i),
            (403, r#"{"error":"host not allowed"}"#.to_owned())
        );
        assert_eq!(
            rejected(h, Some("https://evil.example"), i),
            (403, r#"{"error":"origin not allowed"}"#.to_owned())
        );
        let unknown = (404, r#"{"error":"unknown browser id"}"#.to_owned());
        assert_eq!(rejected(h, None, Some("other")), unknown);
        assert_eq!(rejected(h, None, Some("bad_id!")), unknown);
        assert_eq!(rejected(h, None, None), unknown);
    }
}

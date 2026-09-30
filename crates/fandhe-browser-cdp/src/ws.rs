//! `/devtools/browser/{id}` WebSocket 受け口の受理判定と暫定メッセージハンドラ
//! （TASK-41（41.4）・#173、`CDP-1`・`SEC-2`・MS-3）。
//!
//! [`crate::server::browser_websocket_config`] が `WebSocketConfig` へ組み込む部品で、
//! 判定・応答生成は純関数として切り出し、3 OS で単体テストできるようにしている。
//! 実際のハンドシェイク（RFC 6455）・フレーミングは core の `fandhe-backend-core`
//! （websocket feature）が担い、cli（TASK-41.5）が `Server::websocket` で配線する。
//!
//! # スタブについて
//!
//! [`BrowserSessionHandler`] は暫定実装で、CDP メソッドを一切処理しない。全リクエストへ
//! JSON-RPC エラーを返し、成功（`result`）は返さない（`SEC-2`：未実装メソッドへ
//! 一律成功を返す検出回避的な挙動の禁止）。TASK-42（`protocol.rs`・`CDP-5`/`CDP-6`）で
//! メソッドディスパッチ・`CdpState` 参照を持つハンドラへ置き換える。

use fandhe_backend_core::plugin_websocket::BoxFuture;
use fandhe_backend_core::plugin_websocket::handler::{
    WsHandlerError, WsMessage, WsMessageHandler, WsOutcome,
};
use fandhe_backend_http::response::Response;
use serde_json::{Value, json};

use crate::discovery::Authority;
use crate::server::{error_response, host_error_response};
use crate::target::BrowserId;

/// JSON-RPC: Method not found。
const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: Invalid Request。
const CODE_INVALID_REQUEST: i64 = -32600;

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

/// 受信テキストに対する暫定応答（JSON 文字列）を作る。
///
/// トップレベルがオブジェクトで `id` が整数なら「method not implemented」を、それ以外は
/// 「invalid request」を返す。method 名などの入力値は応答へエコーしない。
pub(crate) fn placeholder_reply(text: &str) -> String {
    let id = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("id").filter(|i| i.is_i64() || i.is_u64()).cloned());
    let reply = match id {
        Some(id) => json!({
            "id": id,
            "error": {"code": CODE_METHOD_NOT_FOUND, "message": "method not implemented"},
        }),
        None => json!({
            "error": {"code": CODE_INVALID_REQUEST, "message": "invalid request"},
        }),
    };
    reply.to_string()
}

/// `/devtools/browser/{id}` セッションの暫定メッセージハンドラ（TASK-42 で置換）。
pub(crate) struct BrowserSessionHandler;

impl WsMessageHandler for BrowserSessionHandler {
    fn name(&self) -> &'static str {
        "cdp-browser"
    }

    fn on_message(&self, msg: WsMessage) -> BoxFuture<'_, Result<WsOutcome, WsHandlerError>> {
        Box::pin(async move {
            Ok(match msg {
                WsMessage::Text(t) => {
                    WsOutcome::Reply(vec![WsMessage::Text(placeholder_reply(&t))])
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

    #[test]
    fn cdp1_ws_placeholder_reply_is_always_an_error() {
        let v: Value = serde_json::from_str(&placeholder_reply(
            r#"{"id":1,"method":"Browser.getVersion"}"#,
        ))
        .unwrap();
        assert_eq!(
            v,
            json!({"id": 1, "error": {"code": -32601, "message": "method not implemented"}})
        );
        for bad in [
            "not json",
            r#"{"method":"Browser.getVersion"}"#,
            r#"{"id":"1","method":"x"}"#,
            r#"[1,2]"#,
        ] {
            let v: Value = serde_json::from_str(&placeholder_reply(bad)).unwrap();
            assert_eq!(v["error"]["code"], -32600, "input: {bad}");
            assert!(v.get("result").is_none());
        }
    }

    #[test]
    fn cdp1_ws_placeholder_reply_does_not_echo_method() {
        let r = placeholder_reply(r#"{"id":7,"method":"Evil.secretMethod"}"#);
        assert!(!r.contains("Evil"));
        assert!(!r.contains("result"));
    }
}

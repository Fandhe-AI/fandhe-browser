//! Playwright / Puppeteer 接続互換のための最小ハンドラ群（TASK-43.3・`CDP-2`。段階 1 は 43.3a、段階 2a は 43.3b、段階 2c は 43.3c）。
//!
//! `protocol::builtin_handlers` から登録される。`Browser.getVersion`（43.3a）・
//! `Target.getTargetInfo`（43.3b）・`Target.setAutoAttach`・`Browser.setDownloadBehavior`（43.3c）を
//! 実装する。値は fandhe-browser 自身の実値に限り、Chrome / Chromium を装わない（`SEC-2`）。
//! `Target.setAutoAttach` は Playwright（`CRBrowser.connect`）だけでなく Puppeteer
//! （`TargetManager.initialize`。TASK-45）も使う共用メソッド。
//!
//! 43.3b では実体未接続のため登録を取り下げた 2 メソッドを、43.3c で実体に接続して登録した。
//! `Target.setAutoAttach` は `TargetRegistry::attach` と `Target.attachedToTarget` の送出に繋ぐ。
//! `Browser.setDownloadBehavior` は設定の検証と接続スコープでの保持までで、ダウンロード実行系
//! （保存・`Browser.downloadWillBegin`・`downloadProgress`）は未実装（`CDP-6`・`REPAIR-3`）。
//! 接続スコープの状態と寿命は [`crate::conn_state`]。方針は `docs/design/playwright-compat.md`。

use serde_json::{Value, json};

use crate::conn_state::{
    ConnStateError, DownloadBehavior, DownloadSettings, validate_download_path,
};
use crate::discovery::{PRODUCT, PROTOCOL_VERSION};
use crate::protocol::{
    BoxFuture, CdpError, CdpEvent, CommandContext, CommandHandler, HandlerOutput,
};
use crate::target::{SessionId, TargetId, TargetInfo};

/// `Browser.getVersion`（`CDP-2`・TASK-43.3a）。
///
/// `protocolVersion`・`product`・`userAgent` のみ返す。`userAgent` は `/json/version` の
/// `User-Agent` と同じ `fandhe-browser/<版>`。`revision`・`jsVersion` は cdp crate から実値を
/// 参照できず、`/json/version` が `V8-Version` を出さない方針と同じく偽値を出さないため省く
/// （`SEC-2`・`REPAIR-3`）。`params` と `sessionId` は読まず、入力値を応答へ反映しない。
pub(crate) struct BrowserGetVersion;

impl CommandHandler for BrowserGetVersion {
    fn handle<'a>(
        &'a self,
        _ctx: CommandContext<'a>,
        _params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move { Ok(HandlerOutput::result(version_result())) })
    }
}

/// `Target.getTargetInfo`（`CDP-2`・TASK-43.3b）。
///
/// `targetId` 省略はブラウザ自身（`type:"browser"`・`attached:true`）。指定時は
/// [`crate::target::TargetRegistry`] の実値から組み立てる。`browserContextId`・`openerId` は
/// 実体がないため出さない（2b）。未知・不正な `targetId` は `-32602`。
pub(crate) struct TargetGetTargetInfo;

impl CommandHandler for TargetGetTargetInfo {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let info = match params.get("targetId") {
                None => browser_target_info(ctx.state.browser_id().as_str()),
                Some(v) => {
                    let s = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    let id = TargetId::parse(s).map_err(|_| CdpError::INVALID_PARAMS)?;
                    let (t, attached) = ctx
                        .state
                        .registry()
                        .target_with_attached(&id)
                        .ok_or(CdpError::INVALID_PARAMS)?;
                    page_target_info(&t, attached)
                }
            };
            Ok(HandlerOutput::result(json!({ "targetInfo": info })))
        })
    }
}

/// `Target.setAutoAttach`（`CDP-2`・`CDP-7`・TASK-43.3c）。
///
/// `autoAttach:true` で、この接続がまだ attach していない既存ターゲットすべてへ
/// [`crate::target::TargetRegistry::attach`] し、ターゲットごとに `Target.attachedToTarget`
/// （flatten）を応答の後に送出する。2 回目以降は重複 attach しない。途中で失敗したら
/// この呼び出しの attach を巻き戻す。`autoAttach:false` はこの接続が attach したセッションを
/// 外して `Target.detachedFromTarget` を送出する。セッションの所有は接続スコープで、切断で解放する。
///
/// スコープ外の入力は成功を装わず `-32602` で拒否する: `sessionId` 付き（子ターゲットの自動 attach は
/// 子ターゲットのモデルが無く未実装）、`autoAttach:true` で `flatten` が `true` 以外
/// （`sendMessageToTarget` 方式は未実装）、`filter`（Puppeteer の除外フィルタを黙って無視すると
/// 除外すべきターゲットまで attach するため）、`autoAttach:true` で `waitForDebuggerOnStart:true`
/// （ターゲットの一時停止が未実装。イベントの `waitingForDebugger` は常に `false`）。
/// 接続なしの直接 dispatch は `NO_CONNECTION`。
/// イベントの `targetInfo` に `browserContextId` は載らない（段階 2b で追加）。
pub(crate) struct TargetSetAutoAttach;

impl CommandHandler for TargetSetAutoAttach {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let auto_attach = params
                .get("autoAttach")
                .and_then(Value::as_bool)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if params.get("filter").is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let wait_for_debugger = match params.get("waitForDebuggerOnStart") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(CdpError::INVALID_PARAMS),
            };
            let flatten = match params.get("flatten") {
                None | Some(Value::Bool(false)) => false,
                Some(Value::Bool(true)) => true,
                Some(_) => return Err(CdpError::INVALID_PARAMS),
            };
            let conn = ctx.conn.ok_or(CdpError::NO_CONNECTION)?;
            let state = ctx.state;
            if !auto_attach {
                let detached = state
                    .conn_scoped()
                    .disable_auto_attach(conn, state.registry())
                    .map_err(conn_error)?;
                let mut out = HandlerOutput::result(json!({}));
                for (target_id, session_id) in &detached {
                    out = out.with_event(detached_event(target_id, session_id));
                }
                return Ok(out);
            }
            // ターゲットの一時停止（`Runtime.runIfWaitingForDebugger` での再開）は未実装で、
            // 成功を装うと待機前提のクライアントが先に進めない（`REPAIR-3`・`CDP-6`）。
            if !flatten || wait_for_debugger {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let attached = state
                .conn_scoped()
                .enable_auto_attach(conn, state.registry())
                .map_err(conn_error)?;
            let mut out = HandlerOutput::result(json!({}));
            for (info, session_id) in &attached {
                out = out.with_event(attached_event(info, session_id));
            }
            Ok(out)
        })
    }
}

/// `Target.attachedToTarget`（flatten・ブラウザレベル）イベントを組み立てる。
pub(crate) fn attached_event(info: &TargetInfo, session_id: &SessionId) -> CdpEvent {
    CdpEvent {
        method: "Target.attachedToTarget",
        params: json!({
            "sessionId": session_id.as_str(),
            "targetInfo": page_target_info(info, true),
            "waitingForDebugger": false,
        }),
        session_id: None,
    }
}

/// `Target.detachedFromTarget`（flatten・ブラウザレベル）イベントを組み立てる。
pub(crate) fn detached_event(target_id: &TargetId, session_id: &SessionId) -> CdpEvent {
    CdpEvent {
        method: "Target.detachedFromTarget",
        params: json!({
            "sessionId": session_id.as_str(),
            "targetId": target_id.as_str(),
        }),
        session_id: None,
    }
}

/// 接続状態の失敗を CDP エラーへ写す（入力値を含めない固定文言）。
fn conn_error(e: ConnStateError) -> CdpError {
    match e {
        ConnStateError::NotLive => CdpError::NO_CONNECTION,
        ConnStateError::Registry(_) => CdpError::SERVER_ERROR,
    }
}

/// `Browser.setDownloadBehavior`（`CDP-2`・TASK-43.3c）。
///
/// ダウンロード実行系（ファイル保存・`Browser.downloadWillBegin`・`Browser.downloadProgress` の送出）は
/// 未実装（`CDP-6`・`REPAIR-3`）。fandhe-browser は現状ダウンロードを行わないため、実際に保証できる
/// `deny`・`default`（保存しない・イベントなし）だけを受理して保持する。`allow`・`allowAndName` と
/// `eventsEnabled:true` は成功を装わず `-32602`（`UNSUPPORTED_PARAMS`）で拒否する。
/// 設定は接続スコープで、切断で破棄する（プロファイルへ永続化しない）。
///
/// 検証: `behavior` は `deny`・`allow`・`allowAndName`・`default` のみ（`allow*` は上記のとおり拒否）。
/// `downloadPath` を与える場合の検証は`downloadPath` は絶対パス・`..` なし・NUL なし・長さ上限内で、サーバーは
/// そのパスに対して一切のファイル操作をしない（プロファイル境界・パストラバーサルを持ち込まない）。
/// `browserContextId` は browser context が未導入（段階 2b）のため指定を `-32602` で拒否する
/// （省略はブラウザ全体）。`eventsEnabled` は型検証して保持するがイベントは送出しない。
/// `sessionId` 付き（ブラウザドメインのコマンド）は拒否。接続なしは `NO_CONNECTION`。
pub(crate) struct BrowserSetDownloadBehavior;

impl CommandHandler for BrowserSetDownloadBehavior {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() || params.get("browserContextId").is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let behavior = params
                .get("behavior")
                .and_then(Value::as_str)
                .and_then(DownloadBehavior::parse)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let events_enabled = match params.get("eventsEnabled") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(CdpError::INVALID_PARAMS),
            };
            // 実行系が無く実際には保存もイベント送出もできないため、保証できない許可・イベント要求は
            // 成功を捏造せず拒否する（`CDP-6`・`REPAIR-3`）。受理するのは `deny`・`default` のみ。
            if behavior.needs_path() || events_enabled {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let download_path = match params.get("downloadPath") {
                None if behavior.needs_path() => return Err(CdpError::INVALID_PARAMS),
                None => None,
                Some(v) => {
                    let s = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    Some(validate_download_path(s).ok_or(CdpError::INVALID_PARAMS)?)
                }
            };
            let conn = ctx.conn.ok_or(CdpError::NO_CONNECTION)?;
            ctx.state
                .conn_scoped()
                .set_download(
                    conn,
                    DownloadSettings {
                        behavior,
                        download_path,
                        events_enabled,
                    },
                )
                .map_err(conn_error)?;
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// registry 上のターゲットの `targetInfo`。`browserContextId`・`openerId` は実体がないため出さない
/// （browser context は段階 2b の `Target.createBrowserContext` で導入する。`CDP-2`）。
fn page_target_info(t: &TargetInfo, attached: bool) -> Value {
    json!({
        "targetId": t.target_id().as_str(),
        "type": t.kind().as_str(),
        "title": t.url(),
        "url": t.url(),
        "attached": attached,
        "canAccessOpener": false,
    })
}

/// ブラウザ自身の `targetInfo`。
fn browser_target_info(browser_id: &str) -> Value {
    json!({
        "targetId": browser_id,
        "type": "browser",
        "title": "",
        "url": "",
        "attached": true,
        "canAccessOpener": false,
    })
}

/// `Browser.getVersion` の結果本体。定数のみから組み立てる。
fn version_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "product": PRODUCT,
        "userAgent": PRODUCT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{Authority, version_body};
    use crate::target::BrowserId;

    #[test]
    fn sec2_browser_get_version_product_derives_from_core_user_agent() {
        let v = version_result();
        assert_eq!(v["product"], fandhe_browser_core::USER_AGENT);
        assert_eq!(v["userAgent"], fandhe_browser_core::USER_AGENT);
    }

    #[test]
    fn cdp2_browser_get_version_returns_real_values_only() {
        let v = version_result();
        let product = format!("fandhe-browser/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            v,
            json!({"protocolVersion": "1.3", "product": product, "userAgent": product})
        );
        assert!(v.get("revision").is_none() && v.get("jsVersion").is_none());
        let s = v.to_string();
        for bad in [
            "Chrome",
            "Chromium",
            "HeadlessChrome",
            "Mozilla",
            "AppleWebKit",
        ] {
            assert!(!s.contains(bad), "{bad} in {s}");
        }
    }

    #[test]
    fn cdp2_browser_get_version_matches_json_version() {
        let a = Authority::from_host_header(Some("127.0.0.1:9222")).unwrap();
        let b = BrowserId::parse("fixed-1").unwrap();
        let j: Value = serde_json::from_slice(&version_body(&a, &b).unwrap()).unwrap();
        let v = version_result();
        assert_eq!(v["product"], j["Browser"]);
        assert_eq!(v["userAgent"], j["User-Agent"]);
        assert_eq!(v["protocolVersion"], j["Protocol-Version"]);
    }

    #[test]
    fn cdp2_browser_target_info_has_real_values_only() {
        let v = json!({"targetInfo": browser_target_info("fixed-1")});
        assert_eq!(
            v,
            json!({"targetInfo": {"targetId": "fixed-1", "type": "browser", "title": "", "url": "", "attached": true, "canAccessOpener": false}})
        );
        let s = v.to_string();
        for bad in ["Chrome", "Chromium", "HeadlessChrome"] {
            assert!(!s.contains(bad), "{bad} in {s}");
        }
    }

    mod handlers {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use fandhe_browser_core::AppState;
        use fandhe_browser_profile::Profile;

        use super::*;
        use crate::conn_state::DownloadBehavior;
        use crate::protocol::{ConnId, Dispatcher};
        use crate::server::CdpState;
        use crate::target::{MAX_SESSIONS, SessionId, TargetKind};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                Self(base.join(format!("fandhe-cdp-compat-test-{}-{n}", std::process::id())))
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn state(dir: &TempDir) -> Arc<CdpState> {
            let profile = Arc::new(Profile::open(&dir.0).expect("profile open"));
            Arc::new(CdpState::new(Arc::new(AppState::with_disabled_renderer(
                profile,
            ))))
        }

        /// 接続 `conn`（None なら接続なし）で 1 リクエストを処理し、フレームを JSON で返す。
        async fn call(st: &Arc<CdpState>, conn: Option<ConnId>, text: &str) -> Vec<Value> {
            Dispatcher::builtin()
                .unwrap()
                .dispatch_on(st, conn, text)
                .await
                .into_frames()
                .iter()
                .map(|f| serde_json::from_str(f).unwrap())
                .collect()
        }

        fn err(code: i64, msg: &str, id: i64) -> Value {
            json!({"id": id, "error": {"code": code, "message": msg}})
        }

        fn auto_attach(id: i64, extra: &str) -> String {
            format!(r#"{{"id":{id},"method":"Target.setAutoAttach","params":{{{extra}}}}}"#)
        }

        fn dl(id: i64, extra: &str) -> String {
            format!(r#"{{"id":{id},"method":"Browser.setDownloadBehavior","params":{{{extra}}}}}"#)
        }

        fn open_conn(st: &Arc<CdpState>, n: u64) -> ConnId {
            let c = ConnId::new(n);
            st.open_connection(c);
            c
        }

        fn new_page(st: &Arc<CdpState>, url: &str) -> TargetId {
            st.registry().create_target(TargetKind::Page, url).unwrap()
        }

        const ON: &str = r#""autoAttach":true,"flatten":true"#;

        #[tokio::test]
        async fn cdp2_set_auto_attach_attaches_existing_targets_and_emits_events() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t1 = new_page(&st, "about:blank");
            let t2 = new_page(&st, "https://example.test/");
            let req = auto_attach(
                7,
                r#""autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true"#,
            );
            let f = call(&st, Some(c), &req).await;
            assert_eq!(f.len(), 3);
            assert_eq!(f[0], json!({"id": 7, "result": {}}));
            let expected = [(&t1, "about:blank"), (&t2, "https://example.test/")];
            for (ev, (t, url)) in f[1..].iter().zip(expected) {
                assert_eq!(ev["method"], "Target.attachedToTarget");
                assert!(ev.get("sessionId").is_none());
                let p = &ev["params"];
                assert_eq!(p["waitingForDebugger"], false);
                let sid = SessionId::parse(p["sessionId"].as_str().unwrap()).unwrap();
                assert_eq!(st.registry().session_target(&sid), Some(t.clone()));
                assert_eq!(
                    p["targetInfo"],
                    json!({
                        "targetId": t.as_str(), "type": "page", "title": url, "url": url,
                        "attached": true, "canAccessOpener": false
                    })
                );
            }
            assert_eq!(st.registry().session_count(), 2);
        }

        #[tokio::test]
        async fn cdp2_set_auto_attach_is_idempotent() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let req = auto_attach(1, ON);
            assert_eq!(call(&st, Some(c), &req).await.len(), 1);
            new_page(&st, "about:blank");
            assert_eq!(call(&st, Some(c), &req).await.len(), 2);
            assert_eq!(call(&st, Some(c), &req).await.len(), 1);
            assert_eq!(st.registry().session_count(), 1);
        }

        #[tokio::test]
        async fn cdp2_set_auto_attach_rejects_bad_params_without_touching_registry() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            new_page(&st, "about:blank");
            let unsupported = err(-32602, "unsupported params", 1);
            let invalid = err(-32602, "invalid params", 1);
            let cases = [
                (
                    r#""autoAttach":true,"flatten":true,"filter":[{"type":"page","exclude":true}]"#,
                    &unsupported,
                ),
                (r#""autoAttach":true"#, &unsupported),
                (
                    r#""autoAttach":true,"flatten":true,"waitForDebuggerOnStart":true"#,
                    &unsupported,
                ),
                (r#""autoAttach":true,"flatten":false"#, &unsupported),
                (r#""autoAttach":true,"flatten":"yes""#, &invalid),
                (r#""flatten":true"#, &invalid),
                (r#""autoAttach":"true","flatten":true"#, &invalid),
                (
                    r#""autoAttach":true,"flatten":true,"waitForDebuggerOnStart":1"#,
                    &invalid,
                ),
            ];
            for (extra, want) in cases {
                let f = call(&st, Some(c), &auto_attach(1, extra)).await;
                assert_eq!(f, vec![want.clone()], "{extra}");
            }
            let with_session = r#"{"id":1,"method":"Target.setAutoAttach","sessionId":"S1","params":{"autoAttach":true,"flatten":true}}"#;
            assert_eq!(
                call(&st, Some(c), with_session).await,
                vec![
                    json!({"id":1,"sessionId":"S1","error":{"code":-32602,"message":"unsupported params"}})
                ]
            );
            assert_eq!(
                call(&st, None, &auto_attach(1, ON)).await,
                vec![err(-32000, "no connection", 1)]
            );
            assert_eq!(st.registry().session_count(), 0);
        }

        #[tokio::test]
        async fn cdp2_set_auto_attach_false_detaches_and_emits_detached() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            let foreign = st.registry().attach(&t).unwrap();
            let on = call(&st, Some(c), &auto_attach(1, ON)).await;
            let sid = on[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            let off = call(&st, Some(c), &auto_attach(2, r#""autoAttach":false"#)).await;
            assert_eq!(off.len(), 2);
            assert_eq!(off[0], json!({"id": 2, "result": {}}));
            assert_eq!(
                off[1],
                json!({"method": "Target.detachedFromTarget", "params": {"sessionId": sid, "targetId": t.as_str()}})
            );
            assert_eq!(st.registry().session_count(), 1);
            assert_eq!(st.registry().session_target(&foreign), Some(t));
            let again = call(&st, Some(c), &auto_attach(3, r#""autoAttach":false"#)).await;
            assert_eq!(again.len(), 1);
        }

        #[tokio::test]
        async fn cdp7_set_auto_attach_session_limit_rolls_back_and_errors() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t1 = new_page(&st, "about:blank");
            new_page(&st, "about:blank");
            for _ in 0..(MAX_SESSIONS - 1) {
                st.registry().attach(&t1).unwrap();
            }
            let before = st.registry().session_count();
            let f = call(&st, Some(c), &auto_attach(1, ON)).await;
            assert_eq!(f, vec![err(-32000, "server error", 1)]);
            assert_eq!(st.registry().session_count(), before);
        }

        #[tokio::test]
        async fn cdp7_reconcile_runs_on_error_paths_and_reports_vanished_sessions() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t1 = new_page(&st, "about:blank");
            let on = call(&st, Some(c), &auto_attach(1, ON)).await;
            let sid = on[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            // 未実装メソッドの応答（エラー）にも新ターゲットの attach が同伴する。
            let t2 = new_page(&st, "https://example.test/");
            let f = call(
                &st,
                Some(c),
                r#"{"id":2,"method":"Nope.nothing","params":{}}"#,
            )
            .await;
            assert_eq!(f.len(), 2);
            assert_eq!(f[0]["id"], 2);
            assert_eq!(f[1]["method"], "Target.attachedToTarget");
            assert_eq!(f[1]["params"]["targetInfo"]["targetId"], t2.as_str());
            // ターゲットが閉じられたら detachedFromTarget を送ってから所有記録を落とす。
            st.registry().close_target(&t1).unwrap();
            let f = call(
                &st,
                Some(c),
                r#"{"id":3,"method":"Nope.nothing","params":{}}"#,
            )
            .await;
            assert_eq!(f.len(), 2);
            assert_eq!(
                f[1],
                json!({"method": "Target.detachedFromTarget", "params": {"sessionId": sid, "targetId": t1.as_str()}})
            );
            assert_eq!(st.registry().session_count(), 1);
        }

        #[tokio::test]
        async fn cdp7_release_connection_frees_auto_attached_sessions_only() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (c1, c2) = (open_conn(&st, 1), open_conn(&st, 2));
            new_page(&st, "about:blank");
            let req = auto_attach(1, ON);
            call(&st, Some(c1), &req).await;
            call(&st, Some(c2), &req).await;
            assert_eq!(st.registry().session_count(), 2);
            st.release_connection(c1);
            assert_eq!(st.registry().session_count(), 1);
            // 解放後の接続は状態を持てない。
            assert_eq!(
                call(&st, Some(c1), &req).await,
                vec![err(-32000, "no connection", 1)]
            );
            st.release_connection(c2);
            assert_eq!(st.registry().session_count(), 0);
        }

        #[tokio::test]
        async fn cdp2_set_download_behavior_stores_validated_settings_per_connection() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            assert_eq!(
                call(&st, Some(c), &dl(4, r#""behavior":"default""#)).await,
                vec![json!({"id":4,"result":{}})]
            );
            let s = st.conn_scoped().download(c).unwrap();
            assert_eq!(
                (s.behavior, s.download_path, s.events_enabled),
                (DownloadBehavior::Default, None, false)
            );
            assert_eq!(
                call(&st, Some(c), &dl(5, r#""behavior":"deny""#)).await,
                vec![json!({"id":5,"result":{}})]
            );
            let s = st.conn_scoped().download(c).unwrap();
            assert_eq!(
                (s.behavior, s.download_path, s.events_enabled),
                (DownloadBehavior::Deny, None, false)
            );
            st.release_connection(c);
            assert!(st.conn_scoped().download(c).is_none());
        }

        #[tokio::test]
        async fn cdp2_set_download_behavior_rejects_bad_params_and_keeps_previous() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(&st, Some(c), &dl(1, r#""behavior":"deny""#)).await;
            let tmp = std::env::temp_dir();
            let abs = serde_json::to_string(tmp.to_str().unwrap()).unwrap();
            let dotdot = serde_json::to_string(tmp.join("..").join("x").to_str().unwrap()).unwrap();
            let long = format!("\"{}\"", "a".repeat(5000));
            let invalid = err(-32602, "invalid params", 2);
            let unsupported = err(-32602, "unsupported params", 2);
            let cases: Vec<(String, Value)> = vec![
                (r#""behavior":"nope""#.into(), invalid.clone()),
                (r#""behavior":5"#.into(), invalid.clone()),
                (String::new(), invalid.clone()),
                // 実行系が無いため許可系・イベント有効は成功を装わず拒否する（CDP-6・REPAIR-3）。
                (r#""behavior":"allow""#.into(), unsupported.clone()),
                (
                    format!(
                        r#""behavior":"allowAndName","downloadPath":{abs},"eventsEnabled":true"#
                    ),
                    unsupported.clone(),
                ),
                (
                    r#""behavior":"deny","eventsEnabled":true"#.into(),
                    unsupported.clone(),
                ),
                // パス検証（deny でも downloadPath を与えた場合は検証する）。
                (
                    r#""behavior":"deny","downloadPath":"relative/dir""#.into(),
                    invalid.clone(),
                ),
                (
                    format!(r#""behavior":"deny","downloadPath":{dotdot}"#),
                    invalid.clone(),
                ),
                (
                    r#""behavior":"deny","downloadPath":"/tmp/a\u0000b""#.into(),
                    invalid.clone(),
                ),
                (
                    format!(r#""behavior":"deny","downloadPath":{long}"#),
                    invalid.clone(),
                ),
                (
                    format!(r#""behavior":"deny","downloadPath":{abs},"eventsEnabled":"x""#),
                    invalid.clone(),
                ),
                (
                    r#""behavior":"deny","browserContextId":"X""#.into(),
                    err(-32602, "unsupported params", 2),
                ),
            ];
            for (extra, want) in cases {
                assert_eq!(
                    call(&st, Some(c), &dl(2, &extra)).await,
                    vec![want],
                    "{extra}"
                );
            }
            assert_eq!(
                st.conn_scoped().download(c).unwrap().behavior,
                DownloadBehavior::Deny
            );
            assert_eq!(
                call(&st, None, &dl(3, r#""behavior":"deny""#)).await,
                vec![err(-32000, "no connection", 3)]
            );
        }

        #[tokio::test]
        async fn cdp2_set_download_behavior_is_isolated_between_connections() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (c1, c2) = (open_conn(&st, 1), open_conn(&st, 2));
            call(&st, Some(c1), &dl(1, r#""behavior":"deny""#)).await;
            assert!(st.conn_scoped().download(c1).is_some());
            assert!(st.conn_scoped().download(c2).is_none());
        }
    }
}

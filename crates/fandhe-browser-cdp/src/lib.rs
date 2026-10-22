//! CDP（Chrome DevTools Protocol）互換サーバー crate。
//!
//! `fandhe-browser-cli` から起動され、Playwright / Puppeteer 等の CDP クライアントに
//! 対して DOM 操作・ページ制御 API を公開することを目指す（README「実装方針（要点）」の
//! CDP 互換方針を参照）。`fandhe-browser-core` の `AppState` に依存し、cdp 固有の
//! 状態（ターゲット表・セッション表）は [`server::CdpState`] が内包する。
//!
//! # スタブについて
//!
//! 状態型（CDP セッション・ターゲット表。`CDP-1`、TASK-41.2）と `/json/*`
//! ルータ（`server::router`。`CDP-1`、TASK-41.3）と `/devtools/browser/{id}` の WS 受け口
//! （`server::browser_websocket_config`。`CDP-1`、TASK-41.4。ハンドラは TASK-42.1 でディスパッチャへ委譲）は
//! [`server::endpoints`] が一体で公開する。さらに cli での組み立て・起動
//! （`fandhe-browser-cli` の `server` モジュール。TASK-41.5、`MS-3`）も実装済み。
//! 以下は未実装で、実装済みを装う公開 API・ダミー実装は置かない
//! （`code-comment-style.md`・REPAIR-3）。
//!
//! - JSON-RPC ディスパッチャ（`protocol.rs`。TASK-42.1）は実装済みで、組込みメソッドは `Page.navigate`（TASK-42.2）・`DOM.getDocument`（TASK-42.4。`sessionId` 付きはターゲット別の確定文書を返す。#658）・`DOM.querySelector`（TASK-42.5。#243）・`DOM.requestChildNodes`（#657）・`Browser.getVersion`（TASK-43.3a。`CDP-2`）・`Target.getTargetInfo`（TASK-43.3b。`CDP-2`）・`Target.setAutoAttach`・`Browser.setDownloadBehavior`（TASK-43.3c。`CDP-2`。前者は既存ターゲットへの自動 attach と `Target.attachedToTarget` 送出まで、後者は設定の検証と保持のみでダウンロード実行系は未実装。`browserContextId` 付きは context 単位で保持）・`Target.createBrowserContext`（TASK-43.3d・#909。`disposeOnDetach:true` のみ受理し接続切断で破棄。`Target.disposeBrowserContext` は未実装）・`Target.closeTarget`（TASK-43.3h・#1028。既定 context か接続所有 context のページを閉じ、自接続の `Target.detachedFromTarget` を送出。`Target.targetDestroyed` は `Target.setDiscoverTargets` 未実装のため未送出）・`Page.createIsolatedWorld` と `Runtime.executionContextCreated`（TASK-44.6・#1217。評価器が有効なときだけ。`runtime.rs`）・`Runtime.evaluate`・`Runtime.releaseObject`（TASK-44.7・#1218。isolated world のみ。`runtime/evaluate.rs`）・`Runtime.callFunctionOn` と `awaitPromise`（TASK-44.8・#1219。`runtime/evaluate/call_function_on.rs`）
//! - 個別 CDP メソッドのハンドラ（`CDP-1`/`CDP-5`/`CDP-7`、`TASK-42.2`〜`42.5`、`MS-4`）
//!
//! 未実装メソッドの応答方針（`-32601` を返し成功を捏造しない）と受信ログは TASK-42.6（`CDP-6`・`SEC-2`）で
//! 確定済み（`protocol.rs` のモジュールドキュメント・[`CdpState::received_methods`]）。

mod conn_state;
mod discovery;
mod dom;
mod method_log;
mod navigation;
mod page;
mod playwright_compat;
mod protocol;
mod runtime;
pub mod server;
pub mod target;
mod ws;

/// 結合テスト専用の薄い入口（feature `test-support`・`#[doc(hidden)]`）。ループバック取得を許可した
/// `Page.navigate`・`DOM.getDocument`・`DOM.querySelector`・`DOM.requestChildNodes` だけのディスパッチャを公開 API 越しに駆動する。
/// 本番の組込み表（`Dispatcher::builtin`）は内部アドレスを拒否するため、実ページ JS 実行の
/// 成功経路（`PageScriptOutcome::Executed`）を CDP の入口から検証する唯一の手段として置く
/// （`JS-4`・`CDP-1`）。既定ビルドには含まれない。
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    use std::sync::Arc;

    use fandhe_browser_core::{Error, FetchOptions};

    use crate::CdpState;
    use crate::dom::{DomGetDocument, DomQuerySelector, DomRequestChildNodes};
    use crate::page::PageNavigate;
    use crate::protocol::Dispatcher;

    /// ループバック取得を許可した組込み表の [`CdpEndpoints`] を返す（`CDP-2`・TASK-44.3）。
    /// 実 Playwright が `127.0.0.1` の fixture へ `goto` する結合テスト専用で、本番の
    /// [`endpoints`](crate::endpoints) は内部アドレスを拒否したまま（SSRF 防御）。
    pub fn endpoints_allowing_loopback(
        state: &Arc<CdpState>,
    ) -> Result<crate::CdpEndpoints, crate::WsConfigError> {
        crate::server::endpoints_allowing_loopback(state)
    }

    /// ループバック許可の `Page.navigate` と `DOM.getDocument`・`DOM.querySelector`・
    /// `DOM.requestChildNodes` を持つディスパッチャ（TASK-113・#786 の結合テスト用）。
    pub struct LoopbackDispatcher(Dispatcher);

    impl LoopbackDispatcher {
        /// 構築する。`Fetcher` 構築失敗のみ `Err`。
        pub fn new() -> Result<Self, Error> {
            let nav =
                PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))?;
            let d = Dispatcher::from_handlers(vec![
                ("Page.navigate", Box::new(nav)),
                ("DOM.getDocument", Box::new(DomGetDocument)),
                ("DOM.querySelector", Box::new(DomQuerySelector)),
                ("DOM.requestChildNodes", Box::new(DomRequestChildNodes)),
            ])
            .map_err(|_| Error::InvalidInput {
                message: "dispatcher".to_owned(),
            })?;
            Ok(Self(d))
        }

        /// 1 リクエストを処理し、送出順（応答 → イベント列）の JSON フレームを返す。
        pub async fn dispatch(&self, state: &Arc<CdpState>, text: &str) -> Vec<String> {
            self.0.dispatch_on(state, None, text).await.into_frames()
        }
    }

    /// 評価系（TASK-44.7・#1218）の結合テスト用ディスパッチャ。ループバック許可の `Page.navigate`・
    /// `Page.createIsolatedWorld`・`Runtime.enable`・`Runtime.evaluate`・`Runtime.releaseObject` を持つ。
    /// `sessionId` 付きのリクエスト（`text` の JSON に含める）をそのまま処理する。
    pub struct RuntimeDispatcher(Dispatcher);

    impl RuntimeDispatcher {
        /// 既定の上限（タイムアウト 5 秒等）で構築する。`Fetcher` 構築失敗のみ `Err`。
        pub fn new() -> Result<Self, Error> {
            Self::build(crate::runtime::EvalLimits::default())
        }

        /// `Runtime.evaluate` のタイムアウトを短くして構築する（時間のかかる上限テスト用）。
        pub fn with_eval_timeout(timeout: std::time::Duration) -> Result<Self, Error> {
            Self::build(crate::runtime::EvalLimits::default().with_timeout(timeout))
        }

        fn build(limits: crate::runtime::EvalLimits) -> Result<Self, Error> {
            let nav =
                PageNavigate::new(FetchOptions::new().with_allow_private_network_access(true))?;
            let d = Dispatcher::from_handlers(vec![
                ("Page.navigate", Box::new(nav)),
                (
                    "Page.createIsolatedWorld",
                    Box::new(crate::runtime::PageCreateIsolatedWorld),
                ),
                (
                    "Runtime.enable",
                    Box::new(crate::playwright_compat::RuntimeEnable),
                ),
                (
                    "Runtime.evaluate",
                    Box::new(crate::runtime::RuntimeEvaluate::new(limits)),
                ),
                (
                    "Runtime.releaseObject",
                    Box::new(crate::runtime::RuntimeReleaseObject),
                ),
                (
                    "Runtime.callFunctionOn",
                    Box::new(crate::runtime::RuntimeCallFunctionOn::new(limits)),
                ),
            ])
            .map_err(|_| Error::InvalidInput {
                message: "dispatcher".to_owned(),
            })?;
            Ok(Self(d))
        }

        /// 1 リクエストを処理し、送出順（応答前イベント → 応答 → イベント列）の JSON フレームを返す。
        pub async fn dispatch(&self, state: &Arc<CdpState>, text: &str) -> Vec<String> {
            self.0.dispatch_on(state, None, text).await.into_frames()
        }
    }
}

pub use method_log::{MethodCounts, ReceivedMethodsSnapshot};
pub use server::{CdpEndpoints, CdpState, WsConfigError, endpoints};
pub use target::{
    BrowserContextId, BrowserId, CdpStateError, MAX_BROWSER_CONTEXTS, MAX_SESSIONS, MAX_TARGETS,
    SessionId, TargetId, TargetInfo, TargetKind, TargetRegistry,
};

//! Playwright / Puppeteer 接続互換のための最小ハンドラ群（TASK-43.3・`CDP-2`。段階 1 は 43.3a、段階 2a は 43.3b、段階 2c は 43.3c、段階 2b は 43.3d、段階 2g は 43.3g、段階 2h は 43.3h、段階 3a は 43.3i、段階 3b は 43.3j、段階 3c は 43.3k、段階 3e は 43.3m）。
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
//! 43.3g（#942）で `Target.setAutoAttach` の `waitForDebuggerOnStart:true`（保持。新ターゲットの
//! 停止は 43.3e〔#910〕で実装）と `Browser.setDownloadBehavior` の `deny` + `eventsEnabled:true` を受理する。
//! 43.3d（#909）で `Target.createBrowserContext`（`disposeOnDetach:true` のみ。接続切断で破棄）と
//! `browserContextId` 付き `Browser.setDownloadBehavior`（context 単位で保持）を足し、全ページターゲットの
//! `targetInfo` に `browserContextId` を載せる。`Target.disposeBrowserContext` は未実装（`REPAIR-3`）。
//! 43.3e（#910）で `Target.createTarget`（応答の前に `Target.attachedToTarget` を送出）と
//! `Runtime.runIfWaitingForDebugger` を足す。
//! 43.3i（#1037）で `Page.enable`（セッション単位の有効フラグ。実効果は `Page.lifecycleEvent` 送出ゲートの片方のみ）と
//! `Page.getFrameTree`（メインフレームのみ）を足す。
//! 43.3j（#1038）で `Log.enable`・`Runtime.enable`・`Network.enable`（セッション単位の有効フラグのみ。
//! イベントは送らない。`REPAIR-3`）を足す。
//! 43.3k（#1039）で `Page.setLifecycleEventsEnabled`（セッション単位の有効フラグ。`Page.enable` 済みの
//! セッションの `Page.navigate` が `Page.lifecycleEvent` の `init`・`load` を送る）を足す。
//! 43.3l（#1040）で `Page.addScriptToEvaluateOnNewDocument`（空 `source` のみ受理・非空は拒否）と
//! セッション付き `Target.setAutoAttach`（状態の保持のみ・子 attach は 0 件）を足す。接続スコープの状態と寿命は [`crate::conn_state`]。方針は `docs/design/playwright-compat.md`。
//! 43.3m（#1041）で `Emulation.setFocusEmulationEnabled`・`Emulation.setEmulatedMedia` を足す。Playwright 既定値（`enabled:true`、
//! `media:""` と既定 4 feature）だけを受理して検証済みの値をセッション状態に保持し、どの API からも読まない（観測できる
//! 効果は無い。`REPAIR-3`）。`setDeviceMetricsOverride`・`setUserAgentOverride`・`setLocaleOverride`・`setTimezoneOverride` は
//! `-32601` のまま（`SEC-2`・`CDP-6`）。接続スコープの状態と寿命は [`crate::conn_state`]。方針は `docs/design/playwright-compat.md`。

use serde_json::{Value, json};

use crate::conn_state::{
    ConnStateError, DownloadBehavior, DownloadSettings, validate_download_path,
};
use crate::discovery::{PRODUCT, PROTOCOL_VERSION};
use crate::page::MAIN_FRAME_MIME_TYPE;
use crate::protocol::{
    BoxFuture, CdpError, CdpEvent, CommandContext, CommandHandler, HandlerOutput,
};
use crate::target::{
    BrowserContextId, CdpStateError, SessionDomain, SessionId, TargetId, TargetInfo,
};

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
/// [`crate::target::TargetRegistry`] の実値から組み立てる。`browserContextId` は所属 context の実値
/// （既存ページは既定 context。`CDP-2`）、`openerId` は実体がないため出さない。未知・不正な `targetId` は `-32602`。
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
/// `sessionId` 付き（子ターゲットの自動 attach。TASK-43.3l・#1040）は、`autoAttach` と
/// `waitForDebuggerOnStart` をセッションの状態として保持して `{}` を返す。イベントは 0 件で、
/// `conn_scoped` には触れないため接続なしの直接 dispatch でも成功する。差異（`REPAIR-3`）: 子ターゲット
/// （iframe・worker・OOPIF）のモデルが無く attach 対象は常に 0 件で、これは事実と一致する応答であり
/// 一律の成功ではない（`CDP-6`）。`waitForDebuggerOnStart` は保持するだけ。将来、子ターゲットのモデルを
/// 作ったらこの状態を読んで attach する（TASK-43・`CDP-2`）。未知・閉じたセッションは `INVALID_PARAMS`。
/// 状態はセッションの切断・ターゲットの close・context の破棄で消える。
///
/// スコープ外の入力は成功を装わず `-32602` で拒否する: `autoAttach:true` で `flatten` が `true` 以外
/// （`sendMessageToTarget` 方式は未実装）、`filter`（Puppeteer の除外フィルタを黙って無視すると
/// 除外すべきターゲットまで attach するため）。
///
/// `waitForDebuggerOnStart:true` は受理して接続状態に保持する（TASK-43.3g・#942）。CDP の定義
/// （Target.pdl）で止める対象は「以後に attach される新しいターゲット」で、Chromium も既存ターゲットを
/// `waiting_for_debugger=false` で attach する（`target_handler.cc` の `AutoAttach(..., false)`）。
/// 既存ターゲットの `Target.attachedToTarget` は `waitingForDebugger:false`。新しいターゲットの
/// 一時停止は `Target.createTarget`（[`TargetCreateTarget`]。#910）が行い、
/// `Runtime.runIfWaitingForDebugger` で再開する。
/// 接続なしの直接 dispatch は `NO_CONNECTION`。
/// イベントの `targetInfo` には所属 context の `browserContextId` が載る（TASK-43.3d・#909）。
pub(crate) struct TargetSetAutoAttach;

impl CommandHandler for TargetSetAutoAttach {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
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
            if let Some(sid) = ctx.session_id {
                // セッション付き（子ターゲットの自動 attach）: 状態を保持するだけで子は作らない。
                if auto_attach && !flatten {
                    return Err(CdpError::UNSUPPORTED_PARAMS);
                }
                let state = auto_attach.then_some(wait_for_debugger);
                ctx.state
                    .registry()
                    .set_child_auto_attach(sid, state)
                    .map_err(|_| CdpError::INVALID_PARAMS)?;
                return Ok(HandlerOutput::result(json!({})));
            }
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
            if !flatten {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            // `waitForDebuggerOnStart` は以後に作られる新しいターゲットだけを止める指示で、
            // 既存ターゲットは止めない（このため下の既存ターゲット通知は `waitingForDebugger:false`）。
            // 新しいターゲットの停止は `Target.createTarget` が接続状態の値を参照して行う（#910）。
            let attached = state
                .conn_scoped()
                .enable_auto_attach(conn, state.registry(), wait_for_debugger)
                .map_err(conn_error)?;
            let mut out = HandlerOutput::result(json!({}));
            for (info, session_id) in &attached {
                out = out.with_event(attached_event(info, session_id, false));
            }
            Ok(out)
        })
    }
}

/// `Target.attachedToTarget`（flatten・ブラウザレベル）イベントを組み立てる。
///
/// `waiting_for_debugger` は `waitForDebuggerOnStart` で止めたターゲットを作成した接続にだけ `true`。
/// 後から自動 attach で追従する他接続には `false`（その接続のためには待たない。クライアントごとの
/// 待ちは持たない簡略化で、Chromium との差異。`REPAIR-3`）。
pub(crate) fn attached_event(
    info: &TargetInfo,
    session_id: &SessionId,
    waiting_for_debugger: bool,
) -> CdpEvent {
    CdpEvent {
        method: "Target.attachedToTarget",
        params: json!({
            "sessionId": session_id.as_str(),
            "targetInfo": page_target_info(info, true),
            "waitingForDebugger": waiting_for_debugger,
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

/// `Target.createBrowserContext`（`CDP-2`・`CDP-7`・TASK-43.3d・#909）。
///
/// 接続が所有する browser context を作り `{"browserContextId"}` を返す。所有する context は接続の
/// 切断で所属ターゲットごと破棄される（[`crate::conn_state`]。`CdpState::release_connection`）。
/// Playwright の `newContext()` が `{disposeOnDetach:true}` で送る。
///
/// 受理するのは `disposeOnDetach:true` のみ。省略・`false` は、解放手段の `Target.disposeBrowserContext` が
/// 未実装で永続 context を作ると枠が枯渇する（`CDP-7`）ため `UNSUPPORTED_PARAMS`。`proxyServer`・
/// `proxyBypassList`・`originsWithUniversalNetworkAccess` は未実装で、黙って無視するとクライアントが
/// proxy 経由と誤認する（取得経路・識別面に関わる。`CDP-6`・`SEC-2`・`REPAIR-3`）ため `UNSUPPORTED_PARAMS`。
/// `disposeOnDetach` が bool でなければ `INVALID_PARAMS`。`sessionId` 付きは `UNSUPPORTED_PARAMS`、
/// 接続なしは `NO_CONNECTION`。上限（[`crate::target::MAX_BROWSER_CONTEXTS`]）超過は `SERVER_ERROR`。
pub(crate) struct TargetCreateBrowserContext;

impl CommandHandler for TargetCreateBrowserContext {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            for unsupported in [
                "proxyServer",
                "proxyBypassList",
                "originsWithUniversalNetworkAccess",
            ] {
                if params.get(unsupported).is_some() {
                    return Err(CdpError::UNSUPPORTED_PARAMS);
                }
            }
            match params.get("disposeOnDetach") {
                Some(Value::Bool(true)) => {}
                None | Some(Value::Bool(false)) => return Err(CdpError::UNSUPPORTED_PARAMS),
                Some(_) => return Err(CdpError::INVALID_PARAMS),
            }
            let conn = ctx.conn.ok_or(CdpError::NO_CONNECTION)?;
            let state = ctx.state;
            let id = state
                .conn_scoped()
                .create_owned_browser_context(conn, state.registry())
                .map_err(conn_error)?;
            Ok(HandlerOutput::result(
                json!({ "browserContextId": id.as_str() }),
            ))
        })
    }
}

/// `Target.closeTarget`（`CDP-2`・`CDP-7`・`SEC-2`・`REPAIR-3`・TASK-43.3h・#1028）。
///
/// Playwright は `newPage()` の初期化失敗時に `page._close()` でこれを送り、応答の成否は見ずに
/// `Target.detachedFromTarget` 等の close イベントを待つ。成功時は `{"success":true}` を返し、
/// 自接続が attach 済みのセッションごとに `Target.detachedFromTarget` を応答の後に送出する
/// （`DispatchOutcome` の規約どおり応答 → イベント）。他接続のセッションの通知は、その接続の
/// 次のコマンドに同伴する（push 経路が無いため。`ConnectionScoped::reconcile_auto_attach`）。
///
/// 閉じてよいのは既定 context か自接続が所有する context のターゲットだけ（判定は
/// `TargetRegistry::close_target_in_contexts`。`Target.createTarget` は作成先をこの 2 種に限るため
/// ターゲット単位の所有は持たない）。
/// 未知・他接続の context・ブラウザ自身の ID・不正な ID はすべて同じ固定文言の `INVALID_PARAMS`
/// にして存在を問い合わせる手段にしない。`sessionId` 付きは `UNSUPPORTED_PARAMS`、接続なしは
/// `NO_CONNECTION`。ブラウザ全体は終了しない。
///
/// 未実装（`REPAIR-3`）: `Target.targetDestroyed` は `Target.setDiscoverTargets` が未実装で
/// discover 有効な接続が存在しないため送らない。実装時は下の `detachedFromTarget` の後ろへ足す。
pub(crate) struct TargetCloseTarget;

impl CommandHandler for TargetCloseTarget {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let s = params
                .get("targetId")
                .and_then(Value::as_str)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let id = TargetId::parse(s).map_err(|_| CdpError::INVALID_PARAMS)?;
            let state = ctx.state;
            // ブラウザ ID は registry を引く前に拒否する（TargetId と書式が同じで衝突し得る）。
            if id.as_str() == state.browser_id().as_str() {
                return Err(CdpError::INVALID_PARAMS);
            }
            let conn = ctx.conn.ok_or(CdpError::NO_CONNECTION)?;
            let sessions = state
                .conn_scoped()
                .close_target(conn, state.registry(), &id)
                .map_err(|e| match e {
                    ConnStateError::NotLive => CdpError::NO_CONNECTION,
                    ConnStateError::Registry(_) => CdpError::INVALID_PARAMS,
                })?;
            state.navigations().forget(&id);
            let mut out = HandlerOutput::result(json!({ "success": true }));
            for sid in &sessions {
                out = out.with_event(detached_event(&id, sid));
            }
            // TODO(REPAIR-3): `Target.setDiscoverTargets` 実装時にここへ `Target.targetDestroyed` を足す。
            Ok(out)
        })
    }
}

/// `Target.createTarget`（`CDP-2`・`CDP-5`・`CDP-6`・`CDP-7`・`SEC-2`・`REPAIR-3`・TASK-43.3e・#910）。
///
/// Playwright の `newPage()`（`url:"about:blank"`・`browserContextId`）が送る。ページターゲットを作り
/// `{"targetId"}` を返す。呼び出し接続の自動 attach が有効なら同じロックで attach し、
/// `Target.attachedToTarget` を**応答より前に**送出する（Playwright は応答時点でその targetId の
/// ページがある前提で動くため。[`HandlerOutput::with_pre_event`]）。`waitForDebuggerOnStart` が有効な
/// 接続ではターゲットを止めて `waitingForDebugger:true` で通知し、そのセッションの
/// `Runtime.runIfWaitingForDebugger` まで初期 `about:blank` 文書を確定しない
/// （[`RuntimeRunIfWaitingForDebugger`]）。止めないターゲットは作成時に確定する。
///
/// 受理する `params` は `url`（`"about:blank"` のみ。取得を伴わず SSRF の面を作らない）と
/// `browserContextId`（省略時は既定 context。指定できるのは既定 context か自接続の context だけ）。
/// `url` が他の文字列・他のキー（`newWindow`・`background`・`width` 等）は黙って無視せず
/// `UNSUPPORTED_PARAMS`、`url` の欠落・型不正と `browserContextId` の型・書式不正・未知・他接続の
/// context は `INVALID_PARAMS`（未知と権限外は同じ固定文言で存在を漏らさない）。`sessionId` 付きは
/// `UNSUPPORTED_PARAMS`、接続なしは `NO_CONNECTION`、上限（ターゲット・セッション）超過は
/// `SERVER_ERROR`（巻き戻し済みで何も作らない）。
///
/// 未実装（`REPAIR-3`）: 任意 URL の読み込み、`Target.targetCreated`（`Target.setDiscoverTargets` が
/// 未実装）、初期 `about:blank` 読み込み時の `Page.frameNavigated`（`Page.enable` は 43.3i で実装済みだが未送出。段階 4 で要否を実測で判断）、クライアントごとの待ち。
/// 既定 context に作ったターゲットは作成接続の切断では閉じない（Chromium のタブと同じ。
/// `Target.closeTarget` で閉じる。上限 `MAX_TARGETS` が歯止め）。
pub(crate) struct TargetCreateTarget;

impl CommandHandler for TargetCreateTarget {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let Some(obj) = params.as_object() else {
                return Err(CdpError::INVALID_PARAMS);
            };
            if obj.keys().any(|k| k != "url" && k != "browserContextId") {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let url = obj
                .get("url")
                .and_then(Value::as_str)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if url != "about:blank" {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let context = match obj.get("browserContextId") {
                None => None,
                Some(v) => {
                    let s = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    Some(BrowserContextId::parse(s).map_err(|_| CdpError::INVALID_PARAMS)?)
                }
            };
            let conn = ctx.conn.ok_or(CdpError::NO_CONNECTION)?;
            let state = ctx.state;
            let created = state
                .conn_scoped()
                .create_target(conn, state.registry(), context.as_ref(), url)
                .map_err(|e| match e {
                    ConnStateError::NotLive => CdpError::NO_CONNECTION,
                    ConnStateError::Registry(CdpStateError::UnknownBrowserContext) => {
                        CdpError::INVALID_PARAMS
                    }
                    ConnStateError::Registry(_) => CdpError::SERVER_ERROR,
                })?;
            // 止めたターゲットは `Runtime.runIfWaitingForDebugger` まで初期文書を確定しない。
            if !created.waiting_for_debugger {
                state
                    .navigations()
                    .commit_initial_blank(state.registry(), created.info.target_id());
            }
            let mut out =
                HandlerOutput::result(json!({ "targetId": created.info.target_id().as_str() }));
            if let Some(sid) = &created.session {
                out = out.with_pre_event(attached_event(
                    &created.info,
                    sid,
                    created.waiting_for_debugger,
                ));
            }
            Ok(out)
        })
    }
}

/// `Runtime.runIfWaitingForDebugger`（`CDP-2`・`SEC-2`・`REPAIR-3`・TASK-43.3e・#910）。
///
/// `waitForDebuggerOnStart` で止めたターゲットを、止めたセッション自身が再開する。再開で初期
/// `about:blank` 文書を確定し `{}` を返す。止まっていないターゲットのセッションへは、Chromium と同じく
/// 何もせず `{}`（実際の no-op であり、ここでの成功は検出回避等の一律成功ではない）。`sessionId` 必須
/// （無ければ `UNSUPPORTED_PARAMS`）、未知・閉じたセッションは `INVALID_PARAMS`。
/// 止めたセッションが外れた場合の再開は `CdpState::resume_orphaned_paused`。
pub(crate) struct RuntimeRunIfWaitingForDebugger;

impl CommandHandler for RuntimeRunIfWaitingForDebugger {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        _params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let state = ctx.state;
            let registry = state.registry();
            if registry.session_target(sid).is_none() {
                return Err(CdpError::INVALID_PARAMS);
            }
            if let Some(tid) = registry.resume_by_session(sid) {
                state.navigations().commit_initial_blank(registry, &tid);
            }
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// `Page.enable`（`CDP-2`・TASK-43.3i・#1037）。
///
/// `sessionId` 付きで、そのセッションに有効フラグを立てて `{}` を返す（冪等）。フラグは registry の
/// セッション表と同じロック・寿命で持つ（`conn_state` でなく `target.rs` に置く理由: 自動 attach 由来でない
/// セッションも扱え、セッション削除と同時に消えて TOCTOU が起きないため）。実効果は
/// `Page.lifecycleEvent` 送出ゲートの片方（もう片方は [`PageSetLifecycleEventsEnabled`]。43.3k・#1039）だけで、
/// `Page.frameNavigated` 等の既存イベントの送出は変えない。
///
/// 差異（`REPAIR-3`）: 既存の `Page.frameNavigated`・`Page.loadEventFired` は `Page.enable` なしでも従来どおり
/// 送出する（Chromium は有効化後のみ。`Page.enable` を送らない既存の利用と Puppeteer 系の回帰を避けるため）。
/// この成功は実際の状態変更であり、検出回避の一律成功ではない（`SEC-2`）。
/// `sessionId` なしは `UNSUPPORTED_PARAMS`、未知・閉じたセッションは `INVALID_PARAMS`。
/// `params` にキーがあれば（`enableFileChooserOpenedEvent` を含む）黙って無視せず `UNSUPPORTED_PARAMS`
/// （フラグは立てない）。
pub(crate) struct PageEnable;

impl CommandHandler for PageEnable {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if !obj.is_empty() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            ctx.state
                .registry()
                .enable_page(sid)
                .map_err(|_| CdpError::INVALID_PARAMS)?;
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// `Page.setLifecycleEventsEnabled`（`CDP-2`・TASK-43.3k・#1039）。
///
/// `sessionId` 付きで `{"enabled": bool}` を受け、セッションの lifecycle 有効フラグを設定して `{}` を返す
/// （true・false とも冪等）。実効果は `Page.navigate`（`page.rs`）の送出ゲート: `Page.enable` 済みかつ本フラグが
/// 有効なセッションにだけ、確定した遷移で `Page.lifecycleEvent`（`init`・`load`）を送る。
/// 検証順: `sessionId` なしは `UNSUPPORTED_PARAMS`、`params` が非オブジェクトは `INVALID_PARAMS`、
/// `enabled` 以外のキーは `UNSUPPORTED_PARAMS`（黙って無視しない）、`enabled` が無い・bool でないは
/// `INVALID_PARAMS`、未知・閉じたセッションは `INVALID_PARAMS`。いずれも状態を変えない。
/// 止めたターゲット（`Runtime.runIfWaitingForDebugger` より前）でも受理する（Playwright は再開前に送る）。
///
/// 差異（`REPAIR-3`）: 送る名前は `init`・`load` のみ（他は `page::lifecycle_event` の doc 参照）。
/// 有効化時に現在の文書の lifecycle を再送しない（Chromium は再送する）。送り先は `Page.navigate` を送った
/// セッションだけで、同じターゲットの他セッションへは送らない（セッション単位の配送は TASK-43・`CDP-2`）。
/// フラグは fetch・ヘッダ・UA・`navigator` に影響しない（`SEC-2`）。実際の状態変更であり一律成功ではない（`CDP-6`）。
pub(crate) struct PageSetLifecycleEventsEnabled;

impl CommandHandler for PageSetLifecycleEventsEnabled {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if obj.keys().any(|k| k != "enabled") {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let enabled = obj
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or(CdpError::INVALID_PARAMS)?;
            ctx.state
                .registry()
                .set_lifecycle_events(sid, enabled)
                .map_err(|_| CdpError::INVALID_PARAMS)?;
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// `Page.addScriptToEvaluateOnNewDocument`（空 `source` のみ。`CDP-2`・TASK-43.3l・#1040）。
///
/// Playwright の `newPage()` が `source:""` で送る。実行すべきスクリプトが無いので no-op が正しい応答になり、
/// 受理するのは空の `source` だけ。非空の `source` は実行経路（`JS-4`・TASK-112）が無く、効いていないのに
/// 成功を返すと stealth 用のスクリプト注入として働くため `UNSUPPORTED_PARAMS` で拒否する（`SEC-2`・`CDP-6`）。
///
/// 検証順: `sessionId` なしは `UNSUPPORTED_PARAMS` → `params` が非オブジェクトは `INVALID_PARAMS` →
/// `source`・`worldName` 以外のキー（`includeCommandLineAPI`・`runImmediately` 等）は `UNSUPPORTED_PARAMS` →
/// `source` が無い・文字列でないは `INVALID_PARAMS` → 非空 `source` は `UNSUPPORTED_PARAMS` →
/// `worldName` が文字列でない・NUL を含むは `INVALID_PARAMS` → 長さ超過（`MAX_WORLD_NAME_LEN`）と
/// セッションあたりの登録数超過は `-32000`（`CDP-7`）→ 未知・閉じたセッションは `INVALID_PARAMS`。
/// 失敗時は状態を変えない。`worldName` の文字種は絞らない（Playwright は `@<hash>` 付きの名前を送る）。
///
/// 応答は `{"identifier": "<10 進連番>"}` だけでイベントは送らない。identifier は registry 全体の連番
/// （接続スコープより広いが、採番を原子的にでき同一接続での再登録が別 identifier になる。セッション ID と
/// 同じ性質で新しい情報漏えいは無い）。`worldName` は保持しない。止めたターゲットでも受理する。
/// 差異（`REPAIR-3`）: 登録内容は件数だけを持ち、`Page.removeScriptToEvaluateOnNewDocument` は未実装。
/// 将来仕様はページ JS 実行と繋いだ新文書での実行（`JS-4`・TASK-112）。UA・ヘッダ・`navigator` には触れない。
pub(crate) struct PageAddScriptToEvaluateOnNewDocument;

impl CommandHandler for PageAddScriptToEvaluateOnNewDocument {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if obj.keys().any(|k| k != "source" && k != "worldName") {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let source = obj
                .get("source")
                .and_then(Value::as_str)
                .ok_or(CdpError::INVALID_PARAMS)?;
            if !source.is_empty() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            if let Some(w) = obj.get("worldName") {
                let w = w.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                if w.contains('\0') {
                    return Err(CdpError::INVALID_PARAMS);
                }
                if w.len() > crate::target::MAX_WORLD_NAME_LEN {
                    return Err(CdpError::WORLD_NAME_TOO_LONG);
                }
            }
            let id = ctx
                .state
                .registry()
                .add_init_script(sid)
                .map_err(|e| match e {
                    crate::target::CdpStateError::TooManyInitScripts { .. } => {
                        CdpError::INIT_SCRIPT_LIMIT_EXCEEDED
                    }
                    crate::target::CdpStateError::IdExhausted => CdpError::SERVER_ERROR,
                    _ => CdpError::INVALID_PARAMS,
                })?;
            Ok(HandlerOutput::result(
                json!({ "identifier": id.to_string() }),
            ))
        })
    }
}

/// `Emulation.setFocusEmulationEnabled`（`CDP-2`・TASK-43.3m・#1041）。
///
/// `sessionId` 付きで `{"enabled": true}`（Playwright 既定値。AGENTS.md の CDP-6 例外）だけを受け、
/// 受理した印をセッション状態として保持して `{}` を返す（冪等）。検証順は [`PageSetLifecycleEventsEnabled`] と
/// 同じ: `sessionId` なしは `UNSUPPORTED_PARAMS`、`params` が非オブジェクトは `INVALID_PARAMS`、`enabled` 以外の
/// キーは `UNSUPPORTED_PARAMS`、`enabled` が無い・bool でないは `INVALID_PARAMS`、`enabled:false`（既定値以外）は
/// `UNSUPPORTED_PARAMS`、未知・閉じたセッションは `INVALID_PARAMS`。いずれも状態を変えない。止めたターゲット（`Runtime.runIfWaitingForDebugger` より前）でも
/// 受理する（Playwright は再開前に送る）。
///
/// 差異（`REPAIR-3`）: 保持値はどの API からも読まれず、今は観測できる効果が無い（フォーカス状態の
/// エミュレーションはしない）。将来は render（TASK-38.5）のフォーカス管理と接続する（別 issue）。
/// この成功は検証済みの値を実際に保持する状態変更で、検出回避のための一律成功ではない（`SEC-2`・`CDP-6`）。
/// UA・ヘッダ・`navigator`・画面・locale・タイムゾーンには触れない。
pub(crate) struct EmulationSetFocusEmulationEnabled;

impl CommandHandler for EmulationSetFocusEmulationEnabled {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if obj.keys().any(|k| k != "enabled") {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let enabled = obj
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or(CdpError::INVALID_PARAMS)?;
            // 既定値（true）以外は受理しない（CDP-6 例外は Playwright 既定値に限る）。
            if !enabled {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            ctx.state
                .registry()
                .set_focus_emulation(sid, enabled)
                .map_err(|_| CdpError::INVALID_PARAMS)?;
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// `Emulation.setEmulatedMedia` で受理する media feature の Playwright 既定値（`playwright-core` 1.63.0 の
/// `newPage()` が送る 4 件。TASK-43.3m・#1041）。これ以外の name・value は受理しない。
const DEFAULT_MEDIA_FEATURES: [(&str, &str); 4] = [
    ("prefers-color-scheme", "light"),
    ("prefers-reduced-motion", "no-preference"),
    ("forced-colors", "none"),
    ("prefers-contrast", "no-preference"),
];

/// `Emulation.setEmulatedMedia` の `params` を検証し、Playwright 既定値と一致するときだけ `Ok` を返す。
///
/// トップレベルのキーは `media`・`features` の両方が必須（欠落・他のキーは `UNSUPPORTED_PARAMS`）。`media` は
/// 空文字のみ、`features` は既定の 4 件をちょうど 1 回ずつ（順不同）含む配列のみ受理する。型の不正は
/// `INVALID_PARAMS`、既定値以外・未知の name・重複・欠落・余分なキーは `UNSUPPORTED_PARAMS`。
/// 配列は 5 件以上を走査せず拒否する（無制限の走査を避ける）。
fn validate_default_emulated_media(obj: &serde_json::Map<String, Value>) -> Result<(), CdpError> {
    if obj.keys().any(|k| k != "media" && k != "features") {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    let media = obj
        .get("media")
        .ok_or(CdpError::UNSUPPORTED_PARAMS)?
        .as_str()
        .ok_or(CdpError::INVALID_PARAMS)?;
    let features = obj
        .get("features")
        .ok_or(CdpError::UNSUPPORTED_PARAMS)?
        .as_array()
        .ok_or(CdpError::INVALID_PARAMS)?;
    if !media.is_empty() {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    if features.len() > DEFAULT_MEDIA_FEATURES.len() {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    let mut seen = [false; DEFAULT_MEDIA_FEATURES.len()];
    for f in features {
        let fo = f.as_object().ok_or(CdpError::INVALID_PARAMS)?;
        let name = fo
            .get("name")
            .and_then(Value::as_str)
            .ok_or(CdpError::INVALID_PARAMS)?;
        let value = fo
            .get("value")
            .and_then(Value::as_str)
            .ok_or(CdpError::INVALID_PARAMS)?;
        if fo.len() != 2 {
            return Err(CdpError::UNSUPPORTED_PARAMS);
        }
        let idx = DEFAULT_MEDIA_FEATURES
            .iter()
            .position(|(n, v)| *n == name && *v == value)
            .ok_or(CdpError::UNSUPPORTED_PARAMS)?;
        let slot = seen.get_mut(idx).ok_or(CdpError::UNSUPPORTED_PARAMS)?;
        if *slot {
            return Err(CdpError::UNSUPPORTED_PARAMS);
        }
        *slot = true;
    }
    if seen.iter().all(|s| *s) {
        Ok(())
    } else {
        Err(CdpError::UNSUPPORTED_PARAMS)
    }
}

/// `Emulation.setEmulatedMedia`（`CDP-2`・TASK-43.3m・#1041）。
///
/// `sessionId` 付きで、Playwright 既定値（`media:""` と 4 件の既定 feature）だけを受理し、受理済みの印を
/// セッション状態に保持して `{}` を返す（冪等）。検証は `sessionId` なし（`UNSUPPORTED_PARAMS`）→ `params` が
/// 非オブジェクト（`INVALID_PARAMS`）→ 値の検証（[`validate_default_emulated_media`]）→ 未知・閉じたセッション
/// （`INVALID_PARAMS`）の順で、エラー時は状態を変えない。止めたターゲットでも受理する。
///
/// 差異（`REPAIR-3`）: 実際の media エミュレーションはしない。保持値は読まれず観測できる効果が無い。
/// 既定値以外（`print`・`prefers-color-scheme:dark` 等）は `-32602` で拒否する。将来は CSSOM の media 評価と
/// 接続する（別 issue）。この成功は検証済みの値を保持する状態変更で、一律成功ではない（`SEC-2`・`CDP-6`）。
/// UA・ヘッダ・`navigator`・画面・locale・タイムゾーンには触れない。
pub(crate) struct EmulationSetEmulatedMedia;

impl CommandHandler for EmulationSetEmulatedMedia {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            validate_default_emulated_media(obj)?;
            ctx.state
                .registry()
                .set_emulated_media_default(sid)
                .map_err(|_| CdpError::INVALID_PARAMS)?;
            Ok(HandlerOutput::result(json!({})))
        })
    }
}
/// `Log.enable`・`Runtime.enable`・`Network.enable` の共通本体（`CDP-2`・TASK-43.3j・#1038）。
///
/// 検証順は [`PageEnable`] と同じ。`sessionId` なしは `UNSUPPORTED_PARAMS`、未知・閉じたセッションは
/// `INVALID_PARAMS`、`params` にキーがあれば黙って無視せず `UNSUPPORTED_PARAMS`（フラグは立てない）。
/// 成功時はセッションの有効フラグを立てて `{}` だけを返し、イベントは 0 件（`REPAIR-3`）。
/// 止めたターゲット（`Runtime.runIfWaitingForDebugger` より前）でも受理する。Playwright の `newPage()` が
/// 再開前に送るため。フラグは fetch・ヘッダ・UA に一切影響しない（`SEC-2`）。
fn enable_session_domain(
    ctx: CommandContext<'_>,
    params: &Value,
    d: SessionDomain,
) -> Result<HandlerOutput, CdpError> {
    let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
    let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
    if !obj.is_empty() {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    ctx.state
        .registry()
        .enable_domain(sid, d)
        .map_err(|_| CdpError::INVALID_PARAMS)?;
    Ok(HandlerOutput::result(json!({})))
}

/// `Log.enable`（`CDP-2`・TASK-43.3j・#1038）。
///
/// セッションに有効フラグを立てて `{}` を返す（冪等）。差異（`REPAIR-3`）: `Log.entryAdded` は送らない。
/// 将来仕様は JS コンソール・fetch 失敗などのログ送出。この成功は実際の状態変更であり、
/// 検出回避の一律成功ではない（`SEC-2`・`CDP-6`）。検証は [`enable_session_domain`]。
pub(crate) struct LogEnable;

impl CommandHandler for LogEnable {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move { enable_session_domain(ctx, params, SessionDomain::Log) })
    }
}

/// `Runtime.enable`（`CDP-2`・TASK-43.3j・#1038）。
///
/// セッションに有効フラグを立てて `{}` を返す（冪等）。差異（`REPAIR-3`）: `Runtime.executionContextCreated`・
/// `consoleAPICalled` は送らない。`Runtime.evaluate` 等が未実装で、使えない `context.id` を見せると
/// 偽の応答になるため。将来仕様は実行コンテキストのモデルを作ったうえでの送出（TASK-44）。
/// 実際の状態変更であり検出回避の一律成功ではない（`SEC-2`・`CDP-6`）。検証は [`enable_session_domain`]。
pub(crate) struct RuntimeEnable;

impl CommandHandler for RuntimeEnable {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move { enable_session_domain(ctx, params, SessionDomain::Runtime) })
    }
}

/// `Network.enable`（`CDP-2`・TASK-43.3j・#1038）。
///
/// セッションに有効フラグを立てて `{}` を返す（冪等）。差異（`REPAIR-3`）: `Network.requestWillBeSent`・
/// `responseReceived`・`loadingFinished`・`loadingFailed` は送らない（将来仕様は `Page.navigate` の fetch に
/// 対する送出。後続 issue で追跡）。バッファ系 params（`maxTotalBufferSize`・`maxResourceBufferSize`・
/// `maxPostDataSize`・`reportDirectSocketTraffic` 等）は未実装のため `UNSUPPORTED_PARAMS`。
/// フラグは fetch の挙動・ヘッダ・UA を何も変えない（`SEC-2`）。検証は [`enable_session_domain`]。
pub(crate) struct NetworkEnable;

impl CommandHandler for NetworkEnable {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move { enable_session_domain(ctx, params, SessionDomain::Network) })
    }
}

/// `Page.getFrameTree`（`CDP-2`・TASK-43.3i・#1037）。
///
/// `sessionId` 付きで、メインフレームだけの `frameTree` を返す。`Page.enable` は前提にしない（Chromium と同じ）。
/// 読み取り専用で、遷移も状態作成もしない。`frame.id` は targetId（`Page.frameNavigated` と同じ値。TASK-43.3o・#1050）。
/// 止めたターゲットでも応答し、`url` は `about:blank`、`loaderId` は再開前後で同じ値
/// （[`crate::navigation::TargetNavigations::frame_snapshot`]）。
///
/// 差異（`REPAIR-3`）: `childFrames` は返さない（子フレームのモデルがない）。値を持たない
/// `securityOrigin`・`parentId`・`name` 等は偽の値で埋めず出さない。`mimeType` は固定値。遷移中・取得失敗後の
/// 値は `frame_snapshot` の doc を参照。`sessionId` なし・`params` にキーありは `UNSUPPORTED_PARAMS`、
/// 未知・閉じたセッションは `INVALID_PARAMS`。
pub(crate) struct PageGetFrameTree;

impl CommandHandler for PageGetFrameTree {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            let sid = ctx.session_id.ok_or(CdpError::UNSUPPORTED_PARAMS)?;
            let obj = params.as_object().ok_or(CdpError::INVALID_PARAMS)?;
            if !obj.is_empty() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let state = ctx.state;
            let registry = state.registry();
            let tid = registry
                .session_target(sid)
                .ok_or(CdpError::INVALID_PARAMS)?;
            let snap = state
                .navigations()
                .frame_snapshot(registry, &tid)
                .ok_or(CdpError::INVALID_PARAMS)?;
            Ok(HandlerOutput::result(json!({
                "frameTree": {
                    "frame": {
                        "id": tid.as_str(),
                        "loaderId": snap.loader_id,
                        "url": snap.url,
                        "mimeType": MAIN_FRAME_MIME_TYPE,
                    },
                },
            })))
        })
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
/// `deny`（`eventsEnabled` は省略・`true`・`false` のいずれも）と `default`（`eventsEnabled` 省略または
/// `false`）だけを受理して保持する。`allow`・`allowAndName` と `default` + `eventsEnabled:true` は
/// 成功を装わず `-32602`（`UNSUPPORTED_PARAMS`）で拒否する。
///
/// `deny` + `eventsEnabled:true` は Playwright の `newContext({acceptDownloads:false})` が送る組
/// （playwright 1.63.0 `crBrowser.ts`。`connectOverCDP` の `noDefaults:true` なら接続直後には送らない）。
/// `deny` ではダウンロードが起きないため、`eventsEnabled:true` を受理してもイベントは送出しない
/// （Browser.pdl の `behavior`・`eventsEnabled`。TASK-43.3g・#942）。`allow*` は既定の
/// `acceptDownloads` が送る `allowAndName` を含め拒否のままで、Playwright の既定のオプションでは通らない。
/// 設定は接続スコープで、切断で破棄する（プロファイルへ永続化しない）。
///
/// 検証: `behavior` は `deny`・`allow`・`allowAndName`・`default` のみ（`allow*` は上記のとおり拒否）。
/// `downloadPath` を与える場合の検証は`downloadPath` は絶対パス・`..` なし・NUL なし・長さ上限内で、サーバーは
/// そのパスに対して一切のファイル操作をしない（プロファイル境界・パストラバーサルを持ち込まない）。
/// `browserContextId`（TASK-43.3d・#909）を与えると設定はその context 単位で registry に保持する
/// （Chromium と同じく接続単位ではない。context の破棄で消える）。文字列でない・書式不正・未知の ID は
/// `-32602`（`INVALID_PARAMS`）。省略は従来どおり接続スコープ。受理する組み合わせは同じ。
/// `eventsEnabled` は型検証して保持するがイベントは送出しない。
/// `sessionId` 付き（ブラウザドメインのコマンド）は拒否。接続なしは `NO_CONNECTION`。
pub(crate) struct BrowserSetDownloadBehavior;

impl CommandHandler for BrowserSetDownloadBehavior {
    fn handle<'a>(
        &'a self,
        ctx: CommandContext<'a>,
        params: &'a Value,
    ) -> BoxFuture<'a, Result<HandlerOutput, CdpError>> {
        Box::pin(async move {
            if ctx.session_id.is_some() {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            let context_id = match params.get("browserContextId") {
                None => None,
                Some(v) => {
                    let s = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    Some(BrowserContextId::parse(s).map_err(|_| CdpError::INVALID_PARAMS)?)
                }
            };
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
            // 成功を捏造せず拒否する（`CDP-6`・`REPAIR-3`）。受理するのは `deny`（`eventsEnabled` 不問。
            // ダウンロードが起きないので送出すべきイベントが無い）と、`eventsEnabled` なしの `default` のみ。
            let accepted = match behavior {
                DownloadBehavior::Deny => true,
                DownloadBehavior::Default => !events_enabled,
                _ => false,
            };
            if !accepted {
                return Err(CdpError::UNSUPPORTED_PARAMS);
            }
            // 不正な `downloadPath` は従来どおり拒否する。`deny` では保存しないためパスは保持しない。
            let download_path = match params.get("downloadPath") {
                None => None,
                Some(v) => {
                    let s = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
                    let p = validate_download_path(s).ok_or(CdpError::INVALID_PARAMS)?;
                    (behavior != DownloadBehavior::Deny).then_some(p)
                }
            };
            let conn = ctx.conn.ok_or(CdpError::NO_CONNECTION)?;
            let settings = DownloadSettings {
                behavior,
                download_path,
                events_enabled,
            };
            match context_id {
                // 存在確認と設定は registry の 1 回のロックで行う（破棄との競合を防ぐ）。
                Some(id) => ctx
                    .state
                    .registry()
                    .set_context_download(&id, settings)
                    .map_err(|_| CdpError::INVALID_PARAMS)?,
                None => ctx
                    .state
                    .conn_scoped()
                    .set_download(conn, settings)
                    .map_err(conn_error)?,
            }
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// registry 上のターゲットの `targetInfo`。`browserContextId` は所属 context の実値
/// （Playwright の `CRBrowser._onAttachedToTarget` が必須とする。`CDP-2`）。`openerId` は実体がないため出さない。
fn page_target_info(t: &TargetInfo, attached: bool) -> Value {
    json!({
        "targetId": t.target_id().as_str(),
        "type": t.kind().as_str(),
        "title": t.url(),
        "url": t.url(),
        "attached": attached,
        "canAccessOpener": false,
        "browserContextId": t.browser_context_id().as_str(),
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
        use crate::target::{BrowserContextId, MAX_SESSIONS, SessionId, TargetKind};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        /// 一時ディレクトリ（drop で再帰削除）。
        ///
        /// `new` が非 recursive の `create_dir`（既存パスなら失敗）で排他的に新規作成できた
        /// ディレクトリだけを保持するため、実行前から存在したパスを開いたり削除したりしない（PROF-1）。
        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new() -> Self {
                let base = std::env::temp_dir()
                    .canonicalize()
                    .unwrap_or_else(|_| std::env::temp_dir());
                for _ in 0..64 {
                    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                    let nanos = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0);
                    let candidate = base.join(format!(
                        "fandhe-cdp-compat-test-{}-{nanos}-{n}",
                        std::process::id()
                    ));
                    match std::fs::create_dir(&candidate) {
                        Ok(()) => return Self(candidate),
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) => panic!("create temp dir: {e}"),
                    }
                }
                panic!("failed to create a unique temp dir");
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
                        "attached": true, "canAccessOpener": false,
                        "browserContextId": st.registry().default_browser_context().as_str()
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
            // セッション付きは TASK-43.3l（#1040）で受理に変わった。未知のセッションは invalid params
            // （従来の unsupported params は「セッション付きは未実装」を意味しており、仕様の変更であって
            // 検査の弱体化ではない）。
            let with_session = r#"{"id":1,"method":"Target.setAutoAttach","sessionId":"S1","params":{"autoAttach":true,"flatten":true}}"#;
            assert_eq!(
                call(&st, Some(c), with_session).await,
                vec![
                    json!({"id":1,"sessionId":"S1","error":{"code":-32602,"message":"invalid params"}})
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
                    r#""behavior":"allow","eventsEnabled":true"#.into(),
                    unsupported.clone(),
                ),
                (
                    r#""behavior":"default","eventsEnabled":true"#.into(),
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
                // 書式は正しいが未知の browserContextId は invalid params（Chromium も失敗を返す）。
                (
                    r#""behavior":"deny","browserContextId":"X""#.into(),
                    invalid.clone(),
                ),
                (
                    r#""behavior":"deny","browserContextId":5"#.into(),
                    invalid.clone(),
                ),
                (
                    r#""behavior":"deny","browserContextId":"../x""#.into(),
                    invalid.clone(),
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

        /// CDP-2・CDP-6・REPAIR-3: Playwright の `acceptDownloads:false` が送る `deny` + `eventsEnabled:true`
        /// を受理する。イベントは送出せず、`deny` では `downloadPath` を保持しない。
        #[tokio::test]
        async fn cdp2_set_download_behavior_accepts_deny_with_events_enabled() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &dl(1, r#""behavior":"deny","eventsEnabled":true"#)
                )
                .await,
                vec![json!({"id":1,"result":{}})]
            );
            let s = st.conn_scoped().download(c).unwrap();
            assert_eq!(
                (s.behavior, s.download_path, s.events_enabled),
                (DownloadBehavior::Deny, None, true)
            );
            // 存在しない絶対パスでも成功し、パスは保持せず、ディレクトリも作らない。
            let missing = std::env::temp_dir().join("fandhe-cdp-no-such-dir-942");
            let p = serde_json::to_string(missing.to_str().unwrap()).unwrap();
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &dl(
                        2,
                        &format!(r#""behavior":"deny","downloadPath":{p},"eventsEnabled":false"#)
                    )
                )
                .await,
                vec![json!({"id":2,"result":{}})]
            );
            let s = st.conn_scoped().download(c).unwrap();
            assert_eq!(
                (s.behavior, s.download_path, s.events_enabled),
                (DownloadBehavior::Deny, None, false)
            );
            assert!(!missing.exists());
        }

        /// CDP-6: `deny` 受理後も `allow*` は拒否され、保持済みの設定は変わらない。
        #[tokio::test]
        async fn cdp6_set_download_behavior_rejects_allow_after_deny() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &dl(1, r#""behavior":"deny","eventsEnabled":true"#),
            )
            .await;
            for b in ["allow", "allowAndName"] {
                let extra = format!(r#""behavior":"{b}","eventsEnabled":true"#);
                assert_eq!(
                    call(&st, Some(c), &dl(2, &extra)).await,
                    vec![err(-32602, "unsupported params", 2)],
                    "{b}"
                );
            }
            let s = st.conn_scoped().download(c).unwrap();
            assert_eq!(
                (s.behavior, s.download_path, s.events_enabled),
                (DownloadBehavior::Deny, None, true)
            );
        }

        /// CDP-2・CDP-7: `waitForDebuggerOnStart:true` を受理して保持する。既存ターゲットは
        /// `waitingForDebugger:false` で通知される（止める対象は以後の新ターゲットのみ）。
        #[tokio::test]
        async fn cdp2_set_auto_attach_accepts_wait_for_debugger_on_start() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            new_page(&st, "about:blank");
            let f = call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            assert_eq!(f.len(), 2);
            assert_eq!(f[0], json!({"id":1,"result":{}}));
            assert_eq!(f[1]["method"], "Target.attachedToTarget");
            assert_eq!(f[1]["params"]["waitingForDebugger"], false);
            assert!(st.conn_scoped().wait_for_debugger_on_start(c));
            // 再度の有効化は値を置き換える。
            call(
                &st,
                Some(c),
                &auto_attach(
                    2,
                    r#""autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true"#,
                ),
            )
            .await;
            assert!(!st.conn_scoped().wait_for_debugger_on_start(c));
            call(
                &st,
                Some(c),
                &auto_attach(
                    3,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            assert!(st.conn_scoped().wait_for_debugger_on_start(c));
            // autoAttach:false で戻る。
            call(&st, Some(c), &auto_attach(4, r#""autoAttach":false"#)).await;
            assert!(!st.conn_scoped().wait_for_debugger_on_start(c));
        }

        /// CDP-7: セッション上限で失敗した場合、`waitForDebuggerOnStart` の保持値は変わらない。
        #[tokio::test]
        async fn cdp7_set_auto_attach_failure_keeps_wait_for_debugger_value() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t1 = new_page(&st, "about:blank");
            new_page(&st, "about:blank");
            for _ in 0..(MAX_SESSIONS - 1) {
                st.registry().attach(&t1).unwrap();
            }
            let f = call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            assert_eq!(f, vec![err(-32000, "server error", 1)]);
            assert!(!st.conn_scoped().wait_for_debugger_on_start(c));
        }

        fn create_ctx(id: i64) -> String {
            format!(
                r#"{{"id":{id},"method":"Target.createBrowserContext","params":{{"disposeOnDetach":true}}}}"#
            )
        }

        async fn make_context(st: &Arc<CdpState>, c: ConnId) -> BrowserContextId {
            let f = call(st, Some(c), &create_ctx(1)).await;
            BrowserContextId::parse(f[0]["result"]["browserContextId"].as_str().unwrap()).unwrap()
        }

        #[tokio::test]
        async fn cdp2_create_browser_context_returns_id_and_registers_owned_context() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let f = call(&st, Some(c), &create_ctx(10)).await;
            assert_eq!(
                f,
                vec![
                    json!({"id":10,"result":{"browserContextId":"00000000000000000000000000000002"}})
                ]
            );
            let id = BrowserContextId::parse("00000000000000000000000000000002").unwrap();
            assert!(st.registry().has_browser_context(&id));
            assert_eq!(st.conn_scoped().owned_contexts(c), vec![id]);
            // 2 つ目は別 ID。
            let f = call(&st, Some(c), &create_ctx(11)).await;
            assert_eq!(
                f[0]["result"]["browserContextId"],
                "00000000000000000000000000000003"
            );
        }

        #[tokio::test]
        async fn cdp2_create_browser_context_rejects_unsupported_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let unsupported = err(-32602, "unsupported params", 1);
            let invalid = err(-32602, "invalid params", 1);
            let cases = [
                (
                    r#""disposeOnDetach":true,"proxyServer":"http://p:1""#,
                    &unsupported,
                ),
                (
                    r#""disposeOnDetach":true,"proxyBypassList":"a""#,
                    &unsupported,
                ),
                (
                    r#""disposeOnDetach":true,"originsWithUniversalNetworkAccess":[]"#,
                    &unsupported,
                ),
                ("", &unsupported),
                (r#""disposeOnDetach":false"#, &unsupported),
                (r#""disposeOnDetach":"true""#, &invalid),
                (r#""disposeOnDetach":1"#, &invalid),
            ];
            for (extra, want) in cases {
                let req = format!(
                    r#"{{"id":1,"method":"Target.createBrowserContext","params":{{{extra}}}}}"#
                );
                assert_eq!(
                    call(&st, Some(c), &req).await,
                    vec![want.clone()],
                    "{extra}"
                );
            }
            let with_session = r#"{"id":1,"method":"Target.createBrowserContext","sessionId":"S1","params":{"disposeOnDetach":true}}"#;
            assert_eq!(
                call(&st, Some(c), with_session).await,
                vec![
                    json!({"id":1,"sessionId":"S1","error":{"code":-32602,"message":"unsupported params"}})
                ]
            );
            assert_eq!(
                call(&st, None, &create_ctx(1)).await,
                vec![err(-32000, "no connection", 1)]
            );
            assert_eq!(st.registry().browser_context_count(), 0);
        }

        #[tokio::test]
        async fn cdp7_create_browser_context_limit_returns_server_error() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            for i in 0..crate::target::MAX_BROWSER_CONTEXTS {
                let f = call(&st, Some(c), &create_ctx(i as i64)).await;
                assert!(f[0].get("result").is_some(), "{i}");
            }
            assert_eq!(
                call(&st, Some(c), &create_ctx(99)).await,
                vec![err(-32000, "server error", 99)]
            );
            assert_eq!(
                st.registry().browser_context_count(),
                crate::target::MAX_BROWSER_CONTEXTS
            );
        }

        #[tokio::test]
        async fn cdp7_release_connection_disposes_owned_contexts_and_their_targets() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (c1, c2) = (open_conn(&st, 1), open_conn(&st, 2));
            let keep = new_page(&st, "about:blank");
            let ctx = make_context(&st, c1).await;
            let t = st
                .registry()
                .create_target_in(&ctx, TargetKind::Page, "about:blank")
                .unwrap();
            // c2 は自動 attach で context 内のターゲットにも attach する。
            let on = call(&st, Some(c2), &auto_attach(1, ON)).await;
            assert_eq!(on.len(), 3);
            let sid = on
                .iter()
                .find(|e| e["params"]["targetInfo"]["targetId"] == t.as_str())
                .unwrap()["params"]["sessionId"]
                .as_str()
                .unwrap()
                .to_owned();
            st.release_connection(c1);
            assert!(!st.registry().has_browser_context(&ctx));
            assert!(st.registry().target(&t).is_none());
            assert!(st.registry().target(&keep).is_some());
            // c2 は次のコマンドで detach を通知される。
            let f = call(
                &st,
                Some(c2),
                r#"{"id":2,"method":"Nope.nothing","params":{}}"#,
            )
            .await;
            assert_eq!(f.len(), 2);
            assert_eq!(
                f[1],
                json!({"method": "Target.detachedFromTarget", "params": {"sessionId": sid, "targetId": t.as_str()}})
            );
            assert_eq!(st.registry().session_count(), 1);
        }

        #[tokio::test]
        async fn cdp2_target_info_includes_browser_context_id() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let existing = new_page(&st, "about:blank");
            let ctx = make_context(&st, c).await;
            let created = st
                .registry()
                .create_target_in(&ctx, TargetKind::Page, "about:blank")
                .unwrap();
            let default = st.registry().default_browser_context();
            for (t, want) in [(&existing, default.as_str()), (&created, ctx.as_str())] {
                let req = format!(
                    r#"{{"id":3,"method":"Target.getTargetInfo","params":{{"targetId":"{}"}}}}"#,
                    t.as_str()
                );
                let f = call(&st, Some(c), &req).await;
                assert_eq!(f[0]["result"]["targetInfo"]["browserContextId"], want);
            }
            let f = call(&st, Some(c), &auto_attach(4, ON)).await;
            assert_eq!(f.len(), 3);
            assert_eq!(
                f[1]["params"]["targetInfo"]["browserContextId"],
                default.as_str()
            );
            assert_eq!(
                f[2]["params"]["targetInfo"]["browserContextId"],
                ctx.as_str()
            );
            // ブラウザ自身の targetInfo には載せない。
            let f = call(
                &st,
                Some(c),
                r#"{"id":5,"method":"Target.getTargetInfo","params":{}}"#,
            )
            .await;
            assert!(
                f[0]["result"]["targetInfo"]
                    .get("browserContextId")
                    .is_none()
            );
        }

        /// CDP-2・CDP-6・REPAIR-3: Playwright の `newContext({acceptDownloads:false})` が送る
        /// `deny` + `eventsEnabled:true` + 絶対 `downloadPath` + `browserContextId` を受理し、context に保持する。
        #[tokio::test]
        async fn cdp2_set_download_behavior_with_browser_context_id_accepts_deny() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let ctx = make_context(&st, c).await;
            let missing = std::env::temp_dir().join("fandhe-cdp-no-such-dir-909");
            let p = serde_json::to_string(missing.to_str().unwrap()).unwrap();
            let extra = format!(
                r#""behavior":"deny","browserContextId":"{}","downloadPath":{p},"eventsEnabled":true"#,
                ctx.as_str()
            );
            assert_eq!(
                call(&st, Some(c), &dl(2, &extra)).await,
                vec![json!({"id":2,"result":{}})]
            );
            let s = st.registry().context_download(&ctx).unwrap();
            assert_eq!(
                (s.behavior, s.download_path, s.events_enabled),
                (DownloadBehavior::Deny, None, true)
            );
            // 接続単位の設定には影響しない。ディレクトリも作らない。
            assert!(st.conn_scoped().download(c).is_none());
            assert!(!missing.exists());
            // 破棄で設定も消える。
            st.release_connection(c);
            assert!(st.registry().context_download(&ctx).is_none());
        }

        #[tokio::test]
        async fn cdp6_set_download_behavior_with_browser_context_id_rejects_allow() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let ctx = make_context(&st, c).await;
            for b in ["allow", "allowAndName"] {
                let extra = format!(
                    r#""behavior":"{b}","browserContextId":"{}","eventsEnabled":true"#,
                    ctx.as_str()
                );
                assert_eq!(
                    call(&st, Some(c), &dl(2, &extra)).await,
                    vec![err(-32602, "unsupported params", 2)],
                    "{b}"
                );
            }
            assert!(st.registry().context_download(&ctx).is_none());
            // 破棄済み context への設定は invalid params。
            st.release_connection(c);
            let c2 = open_conn(&st, 2);
            let extra = format!(r#""behavior":"deny","browserContextId":"{}""#, ctx.as_str());
            assert_eq!(
                call(&st, Some(c2), &dl(3, &extra)).await,
                vec![err(-32602, "invalid params", 3)]
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

        fn close_req(id: i64, tid: &str) -> String {
            format!(
                r#"{{"id":{id},"method":"Target.closeTarget","params":{{"targetId":"{tid}"}}}}"#
            )
        }

        fn detached(t: &TargetId, s: &str) -> Value {
            json!({"method":"Target.detachedFromTarget","params":{"sessionId":s,"targetId":t.as_str()}})
        }

        #[tokio::test]
        async fn cdp2_close_target_returns_success_then_detaches_once() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            let f = call(&st, Some(c), &auto_attach(1, ON)).await;
            let sid = f[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            let f = call(&st, Some(c), &close_req(2, t.as_str())).await;
            assert_eq!(
                f,
                vec![
                    json!({"id":2,"result":{"success":true}}),
                    detached(&t, &sid)
                ]
            );
            assert!(st.registry().target(&t).is_none());
            assert_eq!(st.registry().session_count(), 0);
            // 二重送出しない。
            let f = call(&st, Some(c), r#"{"id":3,"method":"Browser.getVersion"}"#).await;
            assert_eq!(f.len(), 1);
            // 閉じたターゲットは getTargetInfo で引けない。
            let req = format!(
                r#"{{"id":4,"method":"Target.getTargetInfo","params":{{"targetId":"{}"}}}}"#,
                t.as_str()
            );
            assert_eq!(
                call(&st, Some(c), &req).await,
                vec![err(-32602, "invalid params", 4)]
            );
            // 接続・ブラウザ ID は使い続けられる。
            let f = call(&st, Some(c), r#"{"id":5,"method":"Target.getTargetInfo"}"#).await;
            assert_eq!(
                f[0]["result"]["targetInfo"]["targetId"],
                st.browser_id().as_str()
            );
            assert!(st.navigations().get(&t).is_none());
        }

        #[tokio::test]
        async fn cdp2_close_target_without_attach_emits_only_response() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            let f = call(&st, Some(c), &close_req(1, t.as_str())).await;
            assert_eq!(f, vec![json!({"id":1,"result":{"success":true}})]);
        }

        #[tokio::test]
        async fn sec2_close_target_rejects_unknown_invalid_and_browser_id() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            let bad = err(-32602, "invalid params", 1);
            let ghost = "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
            for req in [
                close_req(1, ghost),
                close_req(1, "bad id!"),
                close_req(1, st.browser_id().as_str()),
                r#"{"id":1,"method":"Target.closeTarget","params":{"targetId":5}}"#.to_owned(),
                r#"{"id":1,"method":"Target.closeTarget","params":{}}"#.to_owned(),
            ] {
                assert_eq!(call(&st, Some(c), &req).await, vec![bad.clone()], "{req}");
            }
            assert!(st.registry().target(&t).is_some());
        }

        /// 新規構築直後の最初のページ ID がブラウザ ID と衝突せず、閉じられて
        /// `detachedFromTarget` が送られる一方、ブラウザ ID は拒否される（`CDP-2`・`SEC-2`）。
        #[tokio::test]
        async fn cdp2_close_target_first_page_on_fresh_state_is_closable() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            assert_ne!(t.as_str(), st.browser_id().as_str());
            let f = call(&st, Some(c), &auto_attach(1, ON)).await;
            let sid = f[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            assert_eq!(
                call(&st, Some(c), &close_req(2, t.as_str())).await,
                vec![
                    json!({"id":2,"result":{"success":true}}),
                    detached(&t, &sid)
                ]
            );
            assert!(st.registry().target(&t).is_none());
            assert_eq!(
                call(&st, Some(c), &close_req(3, st.browser_id().as_str())).await,
                vec![err(-32602, "invalid params", 3)]
            );
        }

        #[tokio::test]
        async fn cdp7_close_target_rejects_other_connections_context() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (a, b) = (open_conn(&st, 1), open_conn(&st, 2));
            let ctx = make_context(&st, a).await;
            let t = st
                .registry()
                .create_target_in(&ctx, TargetKind::Page, "about:blank")
                .unwrap();
            assert_eq!(
                call(&st, Some(b), &close_req(1, t.as_str())).await,
                vec![err(-32602, "invalid params", 1)]
            );
            assert!(st.registry().target(&t).is_some());
            assert_eq!(
                call(&st, Some(a), &close_req(2, t.as_str())).await,
                vec![json!({"id":2,"result":{"success":true}})]
            );
            assert!(st.registry().target(&t).is_none());
        }

        #[tokio::test]
        async fn cdp7_close_target_notifies_other_connection_once() {
            let dir = TempDir::new();
            let st = state(&dir);
            let (a, b) = (open_conn(&st, 1), open_conn(&st, 2));
            let t = new_page(&st, "about:blank");
            call(&st, Some(a), &auto_attach(1, ON)).await;
            let fb = call(&st, Some(b), &auto_attach(1, ON)).await;
            let sb = fb[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            let f = call(&st, Some(a), &close_req(2, t.as_str())).await;
            assert_eq!(f.len(), 2);
            let ver = r#"{"id":3,"method":"Browser.getVersion"}"#;
            let f = call(&st, Some(b), ver).await;
            assert_eq!(f.len(), 2);
            assert_eq!(f[1], detached(&t, &sb));
            assert_eq!(call(&st, Some(b), ver).await.len(), 1);
        }

        #[tokio::test]
        async fn cdp2_close_target_rejects_session_and_requires_connection() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            let with_session = format!(
                r#"{{"id":1,"method":"Target.closeTarget","sessionId":"S1","params":{{"targetId":"{}"}}}}"#,
                t.as_str()
            );
            assert_eq!(
                call(&st, Some(c), &with_session).await,
                vec![
                    json!({"id":1,"sessionId":"S1","error":{"code":-32602,"message":"unsupported params"}})
                ]
            );
            assert_eq!(
                call(&st, None, &close_req(2, t.as_str())).await,
                vec![err(-32000, "no connection", 2)]
            );
            assert!(st.registry().target(&t).is_some());
        }

        // ---- Target.createTarget / Runtime.runIfWaitingForDebugger（TASK-43.3e・#910） ----

        fn create_req(id: i64, params: &str) -> String {
            format!(r#"{{"id":{id},"method":"Target.createTarget","params":{params}}}"#)
        }

        fn run_req(id: i64, sid: &str) -> String {
            format!(
                r#"{{"id":{id},"method":"Runtime.runIfWaitingForDebugger","sessionId":"{sid}"}}"#
            )
        }

        fn get_doc(id: i64, sid: &str) -> String {
            format!(r#"{{"id":{id},"method":"DOM.getDocument","sessionId":"{sid}"}}"#)
        }

        const BLANK: &str = r#"{"url":"about:blank"}"#;

        #[tokio::test]
        async fn cdp2_create_target_emits_attached_before_response() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(&st, Some(c), &auto_attach(1, ON)).await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            // 応答より前に attachedToTarget。他にイベントは無い（二重 attach しない）。
            assert_eq!(f.len(), 2, "{f:?}");
            assert_eq!(f[0]["method"], "Target.attachedToTarget");
            assert!(f[0].get("id").is_none());
            assert_eq!(f[0]["params"]["waitingForDebugger"], false);
            let tid = f[1]["result"]["targetId"].as_str().unwrap().to_owned();
            assert_eq!(f[1]["id"], 2);
            assert_eq!(f[0]["params"]["targetInfo"]["targetId"], tid);
            assert_eq!(f[0]["params"]["targetInfo"]["type"], "page");
            assert_eq!(f[0]["params"]["targetInfo"]["url"], "about:blank");
            assert_eq!(
                f[0]["params"]["targetInfo"]["browserContextId"],
                st.registry().default_browser_context().as_str()
            );
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            // 止めていないので初期 about:blank 文書は確定済み。
            let d = call(&st, Some(c), &get_doc(3, &sid)).await;
            assert_eq!(d[0]["result"]["root"]["nodeName"], "#document", "{d:?}");
            // ブラウザ ID とは衝突しない。
            assert_ne!(tid, st.browser_id().as_str());
            // 次のコマンドでも二重に attach しない。
            let again = call(&st, Some(c), &auto_attach(4, ON)).await;
            assert_eq!(again, vec![json!({"id":4,"result":{}})]);
        }

        #[tokio::test]
        async fn cdp2_create_target_without_auto_attach_returns_only_response() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let f = call(&st, Some(c), &create_req(1, BLANK)).await;
            assert_eq!(f.len(), 1);
            let tid = TargetId::parse(f[0]["result"]["targetId"].as_str().unwrap()).unwrap();
            assert_eq!(f[0]["result"].as_object().unwrap().len(), 1);
            assert!(st.registry().target(&tid).is_some());
            assert_eq!(st.registry().session_count(), 0);
            assert!(st.navigations().get(&tid).unwrap().latest().is_some());
        }

        #[tokio::test]
        async fn cdp2_create_target_id_never_equals_browser_id() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let ctx = make_context(&st, c).await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            let tid = f[0]["result"]["targetId"].as_str().unwrap();
            assert_ne!(tid, st.browser_id().as_str());
            assert_ne!(tid, ctx.as_str());
        }

        #[tokio::test]
        async fn cdp2_create_target_waits_for_debugger_until_run_if_waiting() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            assert_eq!(f.len(), 2, "{f:?}");
            assert_eq!(f[0]["params"]["waitingForDebugger"], true);
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            let tid = TargetId::parse(f[1]["result"]["targetId"].as_str().unwrap()).unwrap();
            assert!(st.registry().is_paused(&tid));
            // 再開前は文書が無く、遷移も拒否される。
            assert_eq!(
                call(&st, Some(c), &get_doc(3, &sid)).await,
                vec![
                    json!({"id":3,"sessionId":sid,"error":{"code":-32000,"message":"document not available for target"}})
                ]
            );
            let nav = format!(
                r#"{{"id":4,"method":"Page.navigate","sessionId":"{sid}","params":{{"url":"about:blank"}}}}"#
            );
            assert_eq!(
                call(&st, Some(c), &nav).await,
                vec![
                    json!({"id":4,"sessionId":sid,"error":{"code":-32000,"message":"target is waiting for debugger"}})
                ]
            );
            // 再開で初期文書が確定する。
            assert_eq!(
                call(&st, Some(c), &run_req(5, &sid)).await,
                vec![json!({"id":5,"sessionId":sid,"result":{}})]
            );
            assert!(!st.registry().is_paused(&tid));
            let d = call(&st, Some(c), &get_doc(6, &sid)).await;
            assert_eq!(d[0]["result"]["root"]["nodeName"], "#document", "{d:?}");
            // 2 回目も成功（no-op）。
            assert_eq!(
                call(&st, Some(c), &run_req(7, &sid)).await,
                vec![json!({"id":7,"sessionId":sid,"result":{}})]
            );
        }

        #[tokio::test]
        async fn cdp2_create_target_without_wait_loads_immediately() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            assert_eq!(f[0]["params"]["waitingForDebugger"], false);
            let tid = TargetId::parse(f[1]["result"]["targetId"].as_str().unwrap()).unwrap();
            assert!(!st.registry().is_paused(&tid));
            assert!(st.navigations().get(&tid).unwrap().latest().is_some());
        }

        #[tokio::test]
        async fn cdp2_run_if_waiting_validates_session_and_is_noop_on_running_target() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            new_page(&st, "about:blank");
            let f = call(&st, Some(c), &auto_attach(1, ON)).await;
            let sid = f[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            assert_eq!(
                call(&st, Some(c), &run_req(2, &sid)).await,
                vec![json!({"id":2,"sessionId":sid,"result":{}})]
            );
            // 実際には何も確定しない（no-op）。
            assert!(st.navigations().len() == 0);
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    r#"{"id":3,"method":"Runtime.runIfWaitingForDebugger"}"#
                )
                .await,
                vec![err(-32602, "unsupported params", 3)]
            );
            assert_eq!(
                call(&st, Some(c), &run_req(4, "NoSuch-1")).await,
                vec![
                    json!({"id":4,"sessionId":"NoSuch-1","error":{"code":-32602,"message":"invalid params"}})
                ]
            );
        }

        #[tokio::test]
        async fn cdp7_create_target_rejects_foreign_and_unknown_context() {
            let dir = TempDir::new();
            let st = state(&dir);
            let a = open_conn(&st, 1);
            let b = open_conn(&st, 2);
            let ctx_a = make_context(&st, a).await;
            let own = format!(
                r#"{{"url":"about:blank","browserContextId":"{}"}}"#,
                ctx_a.as_str()
            );
            // 他接続の context は拒否（表は変わらない）。
            assert_eq!(
                call(&st, Some(b), &create_req(2, &own)).await,
                vec![err(-32602, "invalid params", 2)]
            );
            assert_eq!(st.registry().target_count(), 0);
            // 自接続の context には作れ、所属 context が載る。
            let f = call(&st, Some(a), &create_req(3, &own)).await;
            let tid = TargetId::parse(f[0]["result"]["targetId"].as_str().unwrap()).unwrap();
            assert_eq!(
                st.registry().target(&tid).unwrap().browser_context_id(),
                &ctx_a
            );
            // 既定 context も指定できる。
            let def = format!(
                r#"{{"url":"about:blank","browserContextId":"{}"}}"#,
                st.registry().default_browser_context().as_str()
            );
            assert!(call(&st, Some(b), &create_req(4, &def)).await[0]["result"].is_object());
            // 未知・書式不正・型不正は同じ固定文言。
            for bad in [
                r#"{"url":"about:blank","browserContextId":"9999999999999999"}"#,
                r#"{"url":"about:blank","browserContextId":"bad id!"}"#,
                r#"{"url":"about:blank","browserContextId":5}"#,
            ] {
                assert_eq!(
                    call(&st, Some(a), &create_req(5, bad)).await,
                    vec![err(-32602, "invalid params", 5)],
                    "{bad}"
                );
            }
        }

        #[tokio::test]
        async fn cdp6_create_target_rejects_non_blank_url_extra_params_and_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            for p in [
                r#"{"url":"https://example.test/"}"#,
                r#"{"url":"file:///etc/passwd"}"#,
                r#"{"url":"about:blank","newWindow":true}"#,
                r#"{"url":"about:blank","width":100}"#,
                r#"{"url":"about:blank","background":false}"#,
            ] {
                assert_eq!(
                    call(&st, Some(c), &create_req(1, p)).await,
                    vec![err(-32602, "unsupported params", 1)],
                    "{p}"
                );
            }
            for p in ["{}", r#"{"url":5}"#] {
                assert_eq!(
                    call(&st, Some(c), &create_req(2, p)).await,
                    vec![err(-32602, "invalid params", 2)],
                    "{p}"
                );
            }
            assert_eq!(
                call(&st, None, &create_req(3, BLANK)).await,
                vec![err(-32000, "no connection", 3)]
            );
            let with_session = format!(
                r#"{{"id":4,"method":"Target.createTarget","sessionId":"S-1","params":{BLANK}}}"#
            );
            assert_eq!(
                call(&st, Some(c), &with_session).await,
                vec![
                    json!({"id":4,"sessionId":"S-1","error":{"code":-32602,"message":"unsupported params"}})
                ]
            );
            assert_eq!(st.registry().target_count(), 0);
        }

        #[tokio::test]
        async fn cdp7_create_target_limits_roll_back() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            // ターゲット上限。
            for _ in 0..crate::target::MAX_TARGETS {
                new_page(&st, "about:blank");
            }
            assert_eq!(
                call(&st, Some(c), &create_req(1, BLANK)).await,
                vec![err(-32000, "server error", 1)]
            );
            assert_eq!(st.registry().target_count(), crate::target::MAX_TARGETS);
            // セッション上限: ターゲットも作らない。
            let dir2 = TempDir::new();
            let st2 = state(&dir2);
            let c2 = open_conn(&st2, 1);
            let t = new_page(&st2, "about:blank");
            call(&st2, Some(c2), &auto_attach(1, ON)).await;
            while st2.registry().session_count() < MAX_SESSIONS {
                st2.registry().attach(&t).unwrap();
            }
            let before = st2.registry().target_count();
            assert_eq!(
                call(&st2, Some(c2), &create_req(2, BLANK)).await,
                vec![err(-32000, "server error", 2)]
            );
            assert_eq!(st2.registry().target_count(), before);
        }

        #[tokio::test]
        async fn cdp7_paused_target_resumes_when_creating_connection_releases() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            let tid = TargetId::parse(f[1]["result"]["targetId"].as_str().unwrap()).unwrap();
            assert!(st.navigations().get(&tid).is_none());
            st.release_connection(c);
            // 既定 context のターゲットは残り、初期文書が確定する（永久停止しない）。
            assert!(st.registry().target(&tid).is_some());
            assert!(!st.registry().is_paused(&tid));
            assert!(st.navigations().get(&tid).unwrap().latest().is_some());
        }

        #[tokio::test]
        async fn cdp7_paused_target_resumes_when_auto_attach_disabled() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            let tid = TargetId::parse(f[1]["result"]["targetId"].as_str().unwrap()).unwrap();
            call(&st, Some(c), &auto_attach(3, r#""autoAttach":false"#)).await;
            assert!(!st.registry().is_paused(&tid));
            assert!(st.navigations().get(&tid).unwrap().latest().is_some());
        }

        #[tokio::test]
        async fn cdp7_owned_context_target_is_disposed_on_release() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let ctx = make_context(&st, c).await;
            let p = format!(
                r#"{{"url":"about:blank","browserContextId":"{}"}}"#,
                ctx.as_str()
            );
            let f = call(&st, Some(c), &create_req(2, &p)).await;
            let tid = TargetId::parse(f[0]["result"]["targetId"].as_str().unwrap()).unwrap();
            st.release_connection(c);
            assert!(st.registry().target(&tid).is_none());
        }

        #[tokio::test]
        async fn cdp2_create_target_notifies_other_connection_without_waiting_and_closes() {
            let dir = TempDir::new();
            let st = state(&dir);
            let a = open_conn(&st, 1);
            let b = open_conn(&st, 2);
            call(&st, Some(b), &auto_attach(1, ON)).await;
            let f = call(&st, Some(a), &create_req(2, BLANK)).await;
            assert_eq!(f.len(), 1);
            let tid = f[0]["result"]["targetId"].as_str().unwrap().to_owned();
            // 他接続は次のコマンドで追従 attach（waitingForDebugger:false）。
            let g = call(&st, Some(b), &auto_attach(3, ON)).await;
            assert_eq!(g.len(), 2);
            assert_eq!(g[1]["method"], "Target.attachedToTarget");
            assert_eq!(g[1]["params"]["waitingForDebugger"], false);
            assert_eq!(g[1]["params"]["targetInfo"]["targetId"], tid);
            // 作成したターゲットは Target.closeTarget で閉じられる。
            let sid = g[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            let _ = sid;
            let h = call(&st, Some(a), &close_req(4, &tid)).await;
            assert_eq!(h, vec![json!({"id":4,"result":{"success":true}})]);
        }

        #[tokio::test]
        async fn cdp2_create_target_close_emits_detached_for_own_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(&st, Some(c), &auto_attach(1, ON)).await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            let tid = TargetId::parse(f[1]["result"]["targetId"].as_str().unwrap()).unwrap();
            assert_eq!(
                call(&st, Some(c), &close_req(3, tid.as_str())).await,
                vec![
                    json!({"id":3,"result":{"success":true}}),
                    detached(&tid, &sid)
                ]
            );
        }
        #[tokio::test]
        async fn cdp1_commands_to_closed_session_are_rejected() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let t = new_page(&st, "about:blank");
            let f = call(&st, Some(c), &auto_attach(1, ON)).await;
            let sid = f[1]["params"]["sessionId"].as_str().unwrap().to_owned();
            call(&st, Some(c), &close_req(2, t.as_str())).await;
            for (i, m) in [
                ("DOM.getDocument", "{}"),
                ("Page.navigate", r#"{"url":"about:blank"}"#),
            ]
            .iter()
            .enumerate()
            {
                let req = format!(
                    r#"{{"id":{},"method":"{}","sessionId":"{sid}","params":{}}}"#,
                    10 + i,
                    m.0,
                    m.1
                );
                let id = 10 + i as i64;
                assert_eq!(
                    call(&st, Some(c), &req).await,
                    vec![
                        json!({"id":id,"sessionId":sid,"error":{"code":-32602,"message":"invalid params"}})
                    ],
                    "{}",
                    m.0
                );
            }
        }

        // ---- Page.enable / Page.getFrameTree（TASK-43.3i・#1037） ----

        fn sreq(id: i64, method: &str, sid: &str, params: &str) -> String {
            format!(r#"{{"id":{id},"method":"{method}","sessionId":"{sid}","params":{params}}}"#)
        }

        fn serr(code: i64, msg: &str, id: i64, sid: &str) -> Value {
            json!({"id": id, "sessionId": sid, "error": {"code": code, "message": msg}})
        }

        /// 自動 attach 済みのセッション付きで生きたページを作り、sessionId を返す。
        async fn attached_session(st: &Arc<CdpState>, c: ConnId) -> String {
            new_page(st, "about:blank");
            let f = call(st, Some(c), &auto_attach(1, ON)).await;
            f[1]["params"]["sessionId"].as_str().unwrap().to_owned()
        }

        /// セッションのターゲットの targetId 文字列（メインフレームの frame ID と一致するはずの値）。
        fn target_of(st: &Arc<CdpState>, sid: &str) -> String {
            let s = SessionId::parse(sid).unwrap();
            st.registry()
                .session_target(&s)
                .unwrap()
                .as_str()
                .to_owned()
        }

        #[tokio::test]
        async fn cdp2_page_enable_with_session_returns_empty_and_is_idempotent() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            assert!(!st.registry().is_page_enabled(&s));
            for id in [2, 3] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, "Page.enable", &sid, "{}")).await,
                    vec![json!({"id":id,"sessionId":sid,"result":{}})]
                );
            }
            assert!(st.registry().is_page_enabled(&s));
        }

        #[tokio::test]
        async fn cdp2_page_enable_rejects_missing_or_unknown_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            assert_eq!(
                call(&st, Some(c), r#"{"id":1,"method":"Page.enable"}"#).await,
                vec![err(-32602, "unsupported params", 1)]
            );
            assert_eq!(
                call(&st, Some(c), &sreq(2, "Page.enable", "NoSuch-1", "{}")).await,
                vec![serr(-32602, "invalid params", 2, "NoSuch-1")]
            );
        }

        #[tokio::test]
        async fn cdp2_page_enable_rejects_unknown_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            for (id, p) in [
                (2, r#"{"enableFileChooserOpenedEvent":true}"#),
                (3, r#"{"enableFileChooserOpenedEvent":false}"#),
            ] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, "Page.enable", &sid, p)).await,
                    vec![serr(-32602, "unsupported params", id, &sid)]
                );
            }
            assert!(
                !st.registry()
                    .is_page_enabled(&SessionId::parse(&sid).unwrap())
            );
        }

        // ---- Page.addScriptToEvaluateOnNewDocument・セッション付き Target.setAutoAttach（TASK-43.3l・#1040） ----

        #[tokio::test]
        async fn cdp2_add_script_empty_source_returns_increasing_identifiers() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Page.addScriptToEvaluateOnNewDocument";
            let w = r#"{"source":"","worldName":"__playwright_utility_world_page@0123abcd"}"#;
            assert_eq!(
                call(&st, Some(c), &sreq(2, m, &sid, w)).await,
                vec![json!({"id":2,"sessionId":sid,"result":{"identifier":"1"}})]
            );
            assert_eq!(
                call(&st, Some(c), &sreq(3, m, &sid, r#"{"source":""}"#)).await,
                vec![json!({"id":3,"sessionId":sid,"result":{"identifier":"2"}})]
            );
            assert_eq!(st.registry().init_script_count(&s), 2);
        }

        #[tokio::test]
        async fn cdp2_add_script_is_accepted_on_paused_target() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let create =
                r#"{"id":1,"method":"Target.createTarget","params":{"url":"about:blank"}}"#;
            let _ = call(&st, Some(c), create).await;
            let sid = {
                let on = r#""autoAttach":true,"flatten":true,"waitForDebuggerOnStart":true"#;
                let f = call(&st, Some(c), &auto_attach(2, on)).await;
                f[1]["params"]["sessionId"].as_str().unwrap().to_owned()
            };
            let m = "Page.addScriptToEvaluateOnNewDocument";
            assert_eq!(
                call(&st, Some(c), &sreq(3, m, &sid, r#"{"source":""}"#)).await,
                vec![json!({"id":3,"sessionId":sid,"result":{"identifier":"1"}})]
            );
        }

        #[tokio::test]
        async fn cdp6_add_script_rejects_bad_params_without_changing_state() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Page.addScriptToEvaluateOnNewDocument";
            let long = format!(r#"{{"source":"","worldName":"{}"}}"#, "a".repeat(257));
            let cases: Vec<(String, i64, &str)> = vec![
                (
                    r#"{"source":"","runImmediately":true}"#.into(),
                    -32602,
                    "unsupported params",
                ),
                (
                    r#"{"source":"","includeCommandLineAPI":true}"#.into(),
                    -32602,
                    "unsupported params",
                ),
                ("{}".into(), -32602, "invalid params"),
                (r#"{"source":1}"#.into(), -32602, "invalid params"),
                (
                    r#"{"source":"window.x=1"}"#.into(),
                    -32602,
                    "unsupported params",
                ),
                (
                    r#"{"source":"","worldName":1}"#.into(),
                    -32602,
                    "invalid params",
                ),
                (
                    r#"{"source":"","worldName":"a\u0000b"}"#.into(),
                    -32602,
                    "invalid params",
                ),
                (long, -32000, "world name too long"),
            ];
            for (i, (p, code, msg)) in cases.iter().enumerate() {
                let id = 10 + i as i64;
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, p)).await,
                    vec![serr(*code, msg, id, &sid)],
                    "{p}"
                );
            }
            // 非オブジェクトの params はディスパッチャが sessionId なしの invalid params にする。
            assert_eq!(
                call(&st, Some(c), &sreq(29, m, &sid, "[]")).await,
                vec![err(-32602, "invalid params", 29)]
            );
            assert_eq!(
                call(&st, Some(c), r#"{"id":30,"method":"Page.addScriptToEvaluateOnNewDocument","params":{"source":""}}"#).await,
                vec![err(-32602, "unsupported params", 30)]
            );
            assert_eq!(
                call(&st, Some(c), &sreq(31, m, "NoSuch-1", r#"{"source":""}"#)).await,
                vec![serr(-32602, "invalid params", 31, "NoSuch-1")]
            );
            assert_eq!(st.registry().init_script_count(&s), 0);
            // ちょうど 256 バイトは受理する。
            let ok = format!(r#"{{"source":"","worldName":"{}"}}"#, "a".repeat(256));
            assert_eq!(
                call(&st, Some(c), &sreq(32, m, &sid, &ok)).await,
                vec![json!({"id":32,"sessionId":sid,"result":{"identifier":"1"}})]
            );
        }

        #[tokio::test]
        async fn cdp7_add_script_rejects_over_session_limit() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Page.addScriptToEvaluateOnNewDocument";
            for i in 0..crate::target::MAX_INIT_SCRIPTS_PER_SESSION {
                let f = call(&st, Some(c), &sreq(100, m, &sid, r#"{"source":""}"#)).await;
                assert_eq!(f[0]["result"]["identifier"], (i + 1).to_string());
            }
            assert_eq!(
                call(&st, Some(c), &sreq(200, m, &sid, r#"{"source":""}"#)).await,
                vec![serr(-32000, "too many init scripts", 200, &sid)]
            );
            assert_eq!(
                st.registry().init_script_count(&s),
                crate::target::MAX_INIT_SCRIPTS_PER_SESSION
            );
        }

        #[tokio::test]
        async fn cdp2_session_set_auto_attach_records_state_without_children() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let sessions = st.registry().session_count();
            let m = "Target.setAutoAttach";
            let on = r#"{"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true}"#;
            assert_eq!(
                call(&st, Some(c), &sreq(2, m, &sid, on)).await,
                vec![json!({"id":2,"sessionId":sid,"result":{}})]
            );
            assert_eq!(st.registry().child_auto_attach(&s), Some(true));
            assert_eq!(st.registry().session_count(), sessions);
            let off_wait = r#"{"autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true}"#;
            assert_eq!(
                call(&st, Some(c), &sreq(3, m, &sid, off_wait)).await,
                vec![json!({"id":3,"sessionId":sid,"result":{}})]
            );
            assert_eq!(st.registry().child_auto_attach(&s), Some(false));
            // 接続なしの直接 dispatch でもセッション検証が通れば成功する。
            assert_eq!(
                call(&st, None, &sreq(4, m, &sid, on)).await,
                vec![json!({"id":4,"sessionId":sid,"result":{}})]
            );
        }

        #[tokio::test]
        async fn cdp2_session_set_auto_attach_false_clears_state() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Target.setAutoAttach";
            let _ = call(
                &st,
                Some(c),
                &sreq(2, m, &sid, r#"{"autoAttach":true,"flatten":true}"#),
            )
            .await;
            assert_eq!(st.registry().child_auto_attach(&s), Some(false));
            assert_eq!(
                call(&st, Some(c), &sreq(3, m, &sid, r#"{"autoAttach":false}"#)).await,
                vec![json!({"id":3,"sessionId":sid,"result":{}})]
            );
            assert_eq!(st.registry().child_auto_attach(&s), None);
        }

        #[tokio::test]
        async fn cdp2_session_set_auto_attach_rejects_filter_and_non_flatten() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Target.setAutoAttach";
            let cases = [
                (
                    r#"{"autoAttach":true,"flatten":true,"filter":[{"type":"page"}]}"#,
                    "unsupported params",
                ),
                (
                    r#"{"autoAttach":true,"flatten":false}"#,
                    "unsupported params",
                ),
                (r#"{"autoAttach":true}"#, "unsupported params"),
                (r#"{"flatten":true}"#, "invalid params"),
            ];
            for (i, (p, msg)) in cases.iter().enumerate() {
                let id = 2 + i as i64;
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, p)).await,
                    vec![serr(-32602, msg, id, &sid)],
                    "{p}"
                );
            }
            assert_eq!(st.registry().child_auto_attach(&s), None);
        }

        #[tokio::test]
        async fn cdp7_session_state_is_dropped_on_release_connection() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let _ = call(
                &st,
                Some(c),
                &sreq(
                    2,
                    "Target.setAutoAttach",
                    &sid,
                    r#"{"autoAttach":true,"flatten":true}"#,
                ),
            )
            .await;
            let _ = call(
                &st,
                Some(c),
                &sreq(
                    3,
                    "Page.addScriptToEvaluateOnNewDocument",
                    &sid,
                    r#"{"source":""}"#,
                ),
            )
            .await;
            assert_eq!(st.registry().child_auto_attach(&s), Some(false));
            assert_eq!(st.registry().init_script_count(&s), 1);
            st.release_connection(c);
            assert_eq!(st.registry().child_auto_attach(&s), None);
            assert_eq!(st.registry().init_script_count(&s), 0);
        }

        #[tokio::test]
        async fn cdp2_page_get_frame_tree_returns_main_frame_only() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let tid = target_of(&st, &sid);
            let f = call(&st, Some(c), &sreq(2, "Page.getFrameTree", &sid, "{}")).await;
            assert_eq!(
                f,
                vec![
                    json!({"id":2,"sessionId":sid,"result":{"frameTree":{"frame":{
                    "id":tid,"loaderId":"2","url":"about:blank","mimeType":"text/html"}}}})
                ]
            );
            let frame = f[0]["result"]["frameTree"]["frame"].as_object().unwrap();
            for k in ["childFrames", "securityOrigin", "parentId"] {
                assert!(!frame.contains_key(k));
            }
            assert!(f[0]["result"]["frameTree"].get("childFrames").is_none());
            // 読み取り専用で状態を作らない。
            assert_eq!(st.navigations().len(), 0);
        }

        #[tokio::test]
        async fn cdp2_page_get_frame_tree_on_paused_target_keeps_loader_id_across_resume() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            let tid = target_of(&st, &sid);
            let tree = |id: i64, loader: &str, url: &str| {
                json!({"id":id,"sessionId":sid,"result":{"frameTree":{"frame":{
                    "id":tid,"loaderId":loader,"url":url,"mimeType":"text/html"}}}})
            };
            assert_eq!(
                call(&st, Some(c), &sreq(3, "Page.getFrameTree", &sid, "{}")).await,
                vec![tree(3, "2", "about:blank")]
            );
            assert_eq!(st.navigations().len(), 0);
            call(&st, Some(c), &run_req(4, &sid)).await;
            assert_eq!(
                call(&st, Some(c), &sreq(5, "Page.getFrameTree", &sid, "{}")).await,
                vec![tree(5, "2", "about:blank")]
            );
            let nav = call(
                &st,
                Some(c),
                &sreq(6, "Page.navigate", &sid, r#"{"url":"about:blank"}"#),
            )
            .await;
            assert_eq!(nav[0]["result"]["loaderId"], "4", "{nav:?}");
            assert_eq!(nav[1]["method"], "Page.frameNavigated");
            assert_eq!(nav[1]["params"]["frame"]["loaderId"], "4");
            assert_eq!(
                call(&st, Some(c), &sreq(7, "Page.getFrameTree", &sid, "{}")).await,
                vec![tree(7, "4", "about:blank")]
            );
        }

        #[tokio::test]
        async fn cdp2_page_get_frame_tree_rejects_missing_unknown_session_and_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            assert_eq!(
                call(&st, Some(c), r#"{"id":2,"method":"Page.getFrameTree"}"#).await,
                vec![err(-32602, "unsupported params", 2)]
            );
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(3, "Page.getFrameTree", "NoSuch-1", "{}")
                )
                .await,
                vec![serr(-32602, "invalid params", 3, "NoSuch-1")]
            );
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(4, "Page.getFrameTree", &sid, r#"{"x":1}"#)
                )
                .await,
                vec![serr(-32602, "unsupported params", 4, &sid)]
            );
        }

        const DOMAINS: [(&str, SessionDomain); 3] = [
            ("Log.enable", SessionDomain::Log),
            ("Runtime.enable", SessionDomain::Runtime),
            ("Network.enable", SessionDomain::Network),
        ];

        #[tokio::test]
        async fn cdp2_log_runtime_network_enable_with_session_return_empty_and_are_idempotent() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let mut id = 1;
            for (m, d) in DOMAINS {
                assert!(!st.registry().is_domain_enabled(&s, d));
                for _ in 0..2 {
                    id += 1;
                    assert_eq!(
                        call(&st, Some(c), &sreq(id, m, &sid, "{}")).await,
                        vec![json!({"id":id,"sessionId":sid,"result":{}})]
                    );
                }
                assert!(st.registry().is_domain_enabled(&s, d));
            }
        }

        #[tokio::test]
        async fn cdp2_log_runtime_network_enable_reject_missing_or_unknown_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            for (m, _) in DOMAINS {
                assert_eq!(
                    call(&st, Some(c), &format!(r#"{{"id":1,"method":"{m}"}}"#)).await,
                    vec![err(-32602, "unsupported params", 1)]
                );
                assert_eq!(
                    call(&st, Some(c), &sreq(2, m, "NoSuch-1", "{}")).await,
                    vec![serr(-32602, "invalid params", 2, "NoSuch-1")]
                );
            }
        }

        #[tokio::test]
        async fn cdp2_network_enable_rejects_unimplemented_buffer_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            for (id, p) in [
                (2, r#"{"maxTotalBufferSize":1000}"#),
                (3, r#"{"maxResourceBufferSize":1000}"#),
                (4, r#"{"maxPostDataSize":1000}"#),
                (5, r#"{"reportDirectSocketTraffic":true}"#),
            ] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, "Network.enable", &sid, p)).await,
                    vec![serr(-32602, "unsupported params", id, &sid)]
                );
            }
            assert!(!st.registry().is_domain_enabled(&s, SessionDomain::Network));
        }

        #[tokio::test]
        async fn cdp2_log_and_runtime_enable_reject_non_empty_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            for (id, (m, d)) in [(2, DOMAINS[0]), (3, DOMAINS[1])] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, r#"{"x":1}"#)).await,
                    vec![serr(-32602, "unsupported params", id, &sid)]
                );
                assert!(!st.registry().is_domain_enabled(&s, d));
            }
        }

        #[tokio::test]
        async fn cdp2_runtime_enable_does_not_emit_execution_context_created() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let f = call(&st, Some(c), &sreq(2, "Runtime.enable", &sid, "{}")).await;
            assert_eq!(f, vec![json!({"id":2,"sessionId":sid,"result":{}})]);
            assert!(f.iter().all(|v| v.get("method").is_none()));
        }

        #[tokio::test]
        async fn cdp2_enables_accepted_on_paused_target_before_resume() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            assert_eq!(f[0]["params"]["waitingForDebugger"], true, "{f:?}");
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            for (i, (m, _)) in DOMAINS.iter().enumerate() {
                let id = 10 + i as i64;
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, "{}")).await,
                    vec![json!({"id":id,"sessionId":sid,"result":{}})]
                );
            }
        }

        #[tokio::test]
        async fn cdp2_navigate_after_enables_emits_only_page_events() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            for (i, (m, _)) in DOMAINS.iter().enumerate() {
                call(&st, Some(c), &sreq(2 + i as i64, m, &sid, "{}")).await;
            }
            let nav = call(
                &st,
                Some(c),
                &sreq(9, "Page.navigate", &sid, r#"{"url":"about:blank"}"#),
            )
            .await;
            let methods: Vec<&str> = nav.iter().filter_map(|v| v["method"].as_str()).collect();
            assert_eq!(methods, ["Page.frameNavigated", "Page.loadEventFired"]);
        }

        // ---- Page.setLifecycleEventsEnabled（TASK-43.3k・#1039） ----

        #[tokio::test]
        async fn cdp2_set_lifecycle_events_enabled_toggles_flag() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Page.setLifecycleEventsEnabled";
            assert_eq!(
                call(&st, Some(c), &sreq(2, m, &sid, r#"{"enabled":true}"#)).await,
                vec![json!({"id":2,"sessionId":sid,"result":{}})]
            );
            assert!(st.registry().is_lifecycle_events_enabled(&s));
            assert_eq!(
                call(&st, Some(c), &sreq(3, m, &sid, r#"{"enabled":false}"#)).await,
                vec![json!({"id":3,"sessionId":sid,"result":{}})]
            );
            assert!(!st.registry().is_lifecycle_events_enabled(&s));
        }

        #[tokio::test]
        async fn cdp2_set_lifecycle_events_enabled_rejects_invalid_params() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Page.setLifecycleEventsEnabled";
            for (id, p) in [
                (2, "{}"),
                (3, r#"{"enabled":"true"}"#),
                (4, r#"{"enabled":1}"#),
                (5, r#"{"enabled":null}"#),
            ] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, p)).await,
                    vec![serr(-32602, "invalid params", id, &sid)],
                    "{p}"
                );
            }
            assert_eq!(
                call(&st, Some(c), &sreq(7, m, &sid, r#"{"enabled":true,"x":1}"#)).await,
                vec![serr(-32602, "unsupported params", 7, &sid)]
            );
            assert!(!st.registry().is_lifecycle_events_enabled(&s));
        }

        #[tokio::test]
        async fn cdp2_set_lifecycle_events_enabled_rejects_missing_or_unknown_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    r#"{"id":1,"method":"Page.setLifecycleEventsEnabled","params":{"enabled":true}}"#
                )
                .await,
                vec![err(-32602, "unsupported params", 1)]
            );
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(
                        2,
                        "Page.setLifecycleEventsEnabled",
                        "NoSuch-1",
                        r#"{"enabled":true}"#
                    )
                )
                .await,
                vec![serr(-32602, "invalid params", 2, "NoSuch-1")]
            );
        }

        #[tokio::test]
        async fn cdp2_set_lifecycle_events_enabled_accepted_on_paused_target() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            assert_eq!(f[0]["params"]["waitingForDebugger"], true, "{f:?}");
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(
                        10,
                        "Page.setLifecycleEventsEnabled",
                        &sid,
                        r#"{"enabled":true}"#
                    )
                )
                .await,
                vec![json!({"id":10,"sessionId":sid,"result":{}})]
            );
        }

        #[tokio::test]
        async fn cdp2_navigate_with_lifecycle_enabled_emits_lifecycle_events() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            call(&st, Some(c), &sreq(2, "Page.enable", &sid, "{}")).await;
            call(
                &st,
                Some(c),
                &sreq(
                    3,
                    "Page.setLifecycleEventsEnabled",
                    &sid,
                    r#"{"enabled":true}"#,
                ),
            )
            .await;
            let nav = call(
                &st,
                Some(c),
                &sreq(9, "Page.navigate", &sid, r#"{"url":"about:blank"}"#),
            )
            .await;
            let methods: Vec<&str> = nav.iter().filter_map(|v| v["method"].as_str()).collect();
            assert_eq!(
                methods,
                [
                    "Page.lifecycleEvent",
                    "Page.frameNavigated",
                    "Page.loadEventFired",
                    "Page.lifecycleEvent"
                ]
            );
            assert_eq!(nav.len(), 5);
            let loader = nav[0]["result"]["loaderId"].as_str().unwrap();
            assert_eq!(nav[2]["params"]["frame"]["loaderId"], loader);
            let names: Vec<&str> = [&nav[1], &nav[4]]
                .iter()
                .map(|v| v["params"]["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, ["init", "load"]);
            for ev in [&nav[1], &nav[4]] {
                assert_eq!(ev["params"]["frameId"], target_of(&st, &sid).as_str());
                assert_eq!(ev["params"]["loaderId"], loader);
                assert_eq!(ev["sessionId"], sid.as_str());
            }
            assert_eq!(nav[1]["params"]["timestamp"], nav[3]["params"]["timestamp"]);
            assert_eq!(nav[4]["params"]["timestamp"], nav[3]["params"]["timestamp"]);
        }

        #[tokio::test]
        async fn cdp2_navigate_emits_no_lifecycle_unless_both_enabled() {
            let m = "Page.setLifecycleEventsEnabled";
            // (Page.enable を送る, lifecycle を true にする, 直後に false に戻す) の組。
            for (page_enable, lifecycle, disable_again) in [
                (false, true, false),
                (true, false, false),
                (true, true, true),
            ] {
                let dir = TempDir::new();
                let st = state(&dir);
                let c = open_conn(&st, 1);
                let sid = attached_session(&st, c).await;
                if page_enable {
                    call(&st, Some(c), &sreq(2, "Page.enable", &sid, "{}")).await;
                }
                if lifecycle {
                    call(&st, Some(c), &sreq(3, m, &sid, r#"{"enabled":true}"#)).await;
                }
                if disable_again {
                    call(&st, Some(c), &sreq(4, m, &sid, r#"{"enabled":false}"#)).await;
                }
                let nav = call(
                    &st,
                    Some(c),
                    &sreq(9, "Page.navigate", &sid, r#"{"url":"about:blank"}"#),
                )
                .await;
                let methods: Vec<&str> = nav.iter().filter_map(|v| v["method"].as_str()).collect();
                assert_eq!(methods, ["Page.frameNavigated", "Page.loadEventFired"]);
            }
        }

        #[tokio::test]
        async fn cdp2_browser_level_navigate_emits_no_lifecycle() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            call(&st, Some(c), &sreq(2, "Page.enable", &sid, "{}")).await;
            call(
                &st,
                Some(c),
                &sreq(
                    3,
                    "Page.setLifecycleEventsEnabled",
                    &sid,
                    r#"{"enabled":true}"#,
                ),
            )
            .await;
            let nav = call(
                &st,
                Some(c),
                r#"{"id":9,"method":"Page.navigate","params":{"url":"about:blank"}}"#,
            )
            .await;
            let methods: Vec<&str> = nav.iter().filter_map(|v| v["method"].as_str()).collect();
            assert_eq!(methods, ["Page.frameNavigated", "Page.loadEventFired"]);
        }

        // ---- Emulation.setFocusEmulationEnabled・setEmulatedMedia（TASK-43.3m・#1041） ----

        const MEDIA_TRACE: &str = r#"{"media":"","features":[{"name":"prefers-color-scheme","value":"light"},{"name":"prefers-reduced-motion","value":"no-preference"},{"name":"forced-colors","value":"none"},{"name":"prefers-contrast","value":"no-preference"}]}"#;

        #[tokio::test]
        async fn cdp2_emulation_accepts_trace_values_and_is_idempotent() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            assert_eq!(st.registry().emulation_of(&s), None);
            for id in [2, 3] {
                assert_eq!(
                    call(
                        &st,
                        Some(c),
                        &sreq(
                            id,
                            "Emulation.setFocusEmulationEnabled",
                            &sid,
                            r#"{"enabled":true}"#
                        )
                    )
                    .await,
                    vec![json!({"id":id,"sessionId":sid,"result":{}})]
                );
            }
            for id in [4, 5] {
                assert_eq!(
                    call(
                        &st,
                        Some(c),
                        &sreq(id, "Emulation.setEmulatedMedia", &sid, MEDIA_TRACE)
                    )
                    .await,
                    vec![json!({"id":id,"sessionId":sid,"result":{}})]
                );
            }
            assert_eq!(
                st.registry().emulation_of(&s),
                Some(crate::target::SessionEmulation {
                    focus: Some(true),
                    media_default: true
                })
            );
            // `enabled:false` は既定値以外のため拒否され、直前の保持値（true）は変わらない。
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(
                        6,
                        "Emulation.setFocusEmulationEnabled",
                        &sid,
                        r#"{"enabled":false}"#,
                    ),
                )
                .await,
                vec![serr(-32602, "unsupported params", 6, &sid)]
            );
            assert_eq!(
                st.registry().emulation_of(&s).map(|e| e.focus),
                Some(Some(true))
            );
            // features は順不同で受理する。
            let reordered = r#"{"media":"","features":[{"name":"prefers-contrast","value":"no-preference"},{"name":"forced-colors","value":"none"},{"name":"prefers-reduced-motion","value":"no-preference"},{"name":"prefers-color-scheme","value":"light"}]}"#;
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(7, "Emulation.setEmulatedMedia", &sid, reordered)
                )
                .await,
                vec![json!({"id":7,"sessionId":sid,"result":{}})]
            );
        }

        #[tokio::test]
        async fn cdp2_emulation_focus_rejects_invalid_params_without_state_change() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Emulation.setFocusEmulationEnabled";
            for (id, p, code, msg) in [
                (2, r#"{}"#, -32602, "invalid params"),
                (3, r#"{"enabled":"true"}"#, -32602, "invalid params"),
                (4, r#"{"enabled":1}"#, -32602, "invalid params"),
                (6, r#"{"enabled":true,"x":1}"#, -32602, "unsupported params"),
                // 既定値以外（false）は拒否し、状態を作らない。
                (7, r#"{"enabled":false}"#, -32602, "unsupported params"),
            ] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, p)).await,
                    vec![serr(code, msg, id, &sid)],
                    "{p}"
                );
            }
            assert_eq!(st.registry().emulation_of(&s), None);
        }

        #[tokio::test]
        async fn cdp2_emulated_media_rejects_non_default_values_without_state_change() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let m = "Emulation.setEmulatedMedia";
            let f = |name: &str, value: &str| format!(r#"{{"name":"{name}","value":"{value}"}}"#);
            let d1 = f("prefers-color-scheme", "light");
            let d2 = f("prefers-reduced-motion", "no-preference");
            let d3 = f("forced-colors", "none");
            let d4 = f("prefers-contrast", "no-preference");
            let dark = f("prefers-color-scheme", "dark");
            let unknown = f("color-gamut", "p3");
            let extra = r#"{"name":"forced-colors","value":"none","x":1}"#;
            let cases: Vec<(String, i64, &str)> = vec![
                // 既定値以外の media
                (
                    format!(r#"{{"media":"print","features":[{d1},{d2},{d3},{d4}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 既定値以外の value
                (
                    format!(r#"{{"media":"","features":[{dark},{d2},{d3},{d4}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 未知の feature 名
                (
                    format!(r#"{{"media":"","features":[{unknown},{d2},{d3},{d4}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 重複
                (
                    format!(r#"{{"media":"","features":[{d1},{d1},{d3},{d4}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 欠落（3 件）
                (
                    format!(r#"{{"media":"","features":[{d1},{d2},{d3}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 5 件
                (
                    format!(r#"{{"media":"","features":[{d1},{d2},{d3},{d4},{d1}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 要素の余分なキー
                (
                    format!(r#"{{"media":"","features":[{d1},{d2},{extra},{d4}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                // features の欠落・media の欠落・未知のトップレベルキー
                (r#"{"media":""}"#.to_owned(), -32602, "unsupported params"),
                (
                    format!(r#"{{"features":[{d1},{d2},{d3},{d4}]}}"#),
                    -32602,
                    "unsupported params",
                ),
                (
                    format!(r#"{{"media":"","features":[{d1},{d2},{d3},{d4}],"x":1}}"#),
                    -32602,
                    "unsupported params",
                ),
                // 型の不正
                (
                    format!(r#"{{"media":1,"features":[{d1},{d2},{d3},{d4}]}}"#),
                    -32602,
                    "invalid params",
                ),
                (
                    r#"{"media":"","features":"x"}"#.to_owned(),
                    -32602,
                    "invalid params",
                ),
                (
                    r#"{"media":"","features":[1,2,3,4]}"#.to_owned(),
                    -32602,
                    "invalid params",
                ),
                (
                    r#"{"media":"","features":[{"name":1,"value":"x"}]}"#.to_owned(),
                    -32602,
                    "invalid params",
                ),
            ];
            for (i, (p, code, msg)) in cases.iter().enumerate() {
                let id = 2 + i as i64;
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, p)).await,
                    vec![serr(*code, msg, id, &sid)],
                    "{p}"
                );
            }
            assert_eq!(st.registry().emulation_of(&s), None);
        }

        #[tokio::test]
        async fn cdp2_emulation_rejects_missing_or_unknown_session() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            for (m, p) in [
                ("Emulation.setFocusEmulationEnabled", r#"{"enabled":true}"#),
                ("Emulation.setEmulatedMedia", MEDIA_TRACE),
            ] {
                assert_eq!(
                    call(
                        &st,
                        Some(c),
                        &format!(r#"{{"id":1,"method":"{m}","params":{p}}}"#)
                    )
                    .await,
                    vec![err(-32602, "unsupported params", 1)],
                    "{m}"
                );
                assert_eq!(
                    call(&st, Some(c), &sreq(2, m, "NoSuch-1", p)).await,
                    vec![serr(-32602, "invalid params", 2, "NoSuch-1")],
                    "{m}"
                );
            }
        }

        #[tokio::test]
        async fn cdp2_emulation_accepted_on_paused_target() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            call(
                &st,
                Some(c),
                &auto_attach(
                    1,
                    r#""autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true"#,
                ),
            )
            .await;
            let f = call(&st, Some(c), &create_req(2, BLANK)).await;
            assert_eq!(f[0]["params"]["waitingForDebugger"], true, "{f:?}");
            let sid = f[0]["params"]["sessionId"].as_str().unwrap().to_owned();
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(
                        10,
                        "Emulation.setFocusEmulationEnabled",
                        &sid,
                        r#"{"enabled":true}"#
                    )
                )
                .await,
                vec![json!({"id":10,"sessionId":sid,"result":{}})]
            );
            assert_eq!(
                call(
                    &st,
                    Some(c),
                    &sreq(11, "Emulation.setEmulatedMedia", &sid, MEDIA_TRACE)
                )
                .await,
                vec![json!({"id":11,"sessionId":sid,"result":{}})]
            );
        }

        /// `SEC-2`: 保持値は他の API の応答・イベントを変えない。
        #[tokio::test]
        async fn sec2_emulation_does_not_change_other_responses() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            let s = SessionId::parse(&sid).unwrap();
            let target_id = st.registry().session_target(&s).unwrap();
            let probe = |base: i64| {
                vec![
                    format!(r#"{{"id":{base},"method":"Browser.getVersion"}}"#),
                    format!(
                        r#"{{"id":{},"method":"Target.getTargetInfo","params":{{"targetId":"{}"}}}}"#,
                        base + 1,
                        target_id.as_str()
                    ),
                    sreq(base + 2, "Page.getFrameTree", &sid, "{}"),
                ]
            };
            let mut before = Vec::new();
            for t in probe(100) {
                before.push(call(&st, Some(c), &t).await);
            }
            call(
                &st,
                Some(c),
                &sreq(
                    2,
                    "Emulation.setFocusEmulationEnabled",
                    &sid,
                    r#"{"enabled":true}"#,
                ),
            )
            .await;
            call(
                &st,
                Some(c),
                &sreq(3, "Emulation.setEmulatedMedia", &sid, MEDIA_TRACE),
            )
            .await;
            let mut after = Vec::new();
            for t in probe(100) {
                after.push(call(&st, Some(c), &t).await);
            }
            assert_eq!(before, after);
            let nav = call(
                &st,
                Some(c),
                &sreq(9, "Page.navigate", &sid, r#"{"url":"about:blank"}"#),
            )
            .await;
            let methods: Vec<&str> = nav.iter().filter_map(|v| v["method"].as_str()).collect();
            assert_eq!(methods, ["Page.frameNavigated", "Page.loadEventFired"]);
        }

        /// `CDP-6`・`SEC-2`: 識別面・画面系のエミュレーションは受理しない。
        #[tokio::test]
        async fn cdp6_identity_and_screen_emulation_remain_method_not_implemented() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            for (id, m) in [
                (2, "Emulation.setUserAgentOverride"),
                (3, "Emulation.setLocaleOverride"),
                (4, "Emulation.setTimezoneOverride"),
                (5, "Emulation.setDeviceMetricsOverride"),
                (6, "Network.setUserAgentOverride"),
            ] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, "{}")).await,
                    vec![serr(-32601, "method not implemented", id, &sid)],
                    "{m}"
                );
            }
        }

        #[tokio::test]
        async fn cdp6_session_scoped_ua_override_and_disable_remain_method_not_implemented() {
            let dir = TempDir::new();
            let st = state(&dir);
            let c = open_conn(&st, 1);
            let sid = attached_session(&st, c).await;
            for (id, m) in [
                (2, "Network.setUserAgentOverride"),
                (3, "Log.disable"),
                (4, "Emulation.setUserAgentOverride"),
            ] {
                assert_eq!(
                    call(&st, Some(c), &sreq(id, m, &sid, "{}")).await,
                    vec![serr(-32601, "method not implemented", id, &sid)]
                );
            }
        }
    }
}

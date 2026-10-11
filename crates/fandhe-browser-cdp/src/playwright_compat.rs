//! Playwright `connectOverCDP` 互換のための最小ハンドラ群（TASK-43.3・`CDP-2`。段階 1 は 43.3a、段階 2a は 43.3b）。
//!
//! `protocol::builtin_handlers` から登録される。Playwright が接続直後に送る
//! `Browser.getVersion`（43.3a）・`Target.setAutoAttach`・`Browser.setDownloadBehavior`・
//! `Target.getTargetInfo`（43.3b）を実装する。値は fandhe-browser 自身の実値に限り、
//! Chrome / Chromium を装わない（`SEC-2`）。`Target.createBrowserContext` など以降の
//! メソッドは `-32601` のまま残し、実測で必要なものだけを個別に追加する
//! （`CDP-6`・`REPAIR-3`。方針は `docs/design/playwright-compat.md`）。
//!
//! 状態は 2 種を持つ。`Target.setAutoAttach` の設定は接続別（[`AutoAttachStates`]。切断で解放）、
//! `Browser.setDownloadBehavior` の挙動はブラウザ全体（[`DownloadPolicyState`]。固定サイズ 1 件）。
//! どちらも現時点では保持のみで、イベント送出・ダウンロード処理からは参照されない。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use crate::discovery::{PRODUCT, PROTOCOL_VERSION};
use crate::protocol::{BoxFuture, CdpError, CommandContext, CommandHandler, ConnId, HandlerOutput};
use crate::target::TargetId;

/// `Browser.setDownloadBehavior` の `downloadPath` の最大バイト数。不透明な文字列として
/// 長さだけを検査する（ファイルシステムは操作しない。`SEC-2`・PROF 境界）。
const MAX_DOWNLOAD_PATH_LEN: usize = 4096;

/// `Target.setAutoAttach` で保持する設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoAttachConfig {
    /// `autoAttach`。
    pub auto_attach: bool,
    /// `waitForDebuggerOnStart`。
    pub wait_for_debugger_on_start: bool,
}

struct AutoAttachInner {
    /// 生存中の接続。切断済み接続への書き込みを拒否するために持つ（`dom::IssuedDocuments` と同流儀）。
    live: HashSet<ConnId>,
    /// 接続別の設定。`None` キーは WebSocket を介さない直接 dispatch（テスト）専用。
    configs: HashMap<Option<ConnId>, AutoAttachConfig>,
}

/// `Target.setAutoAttach` の接続別設定表（`CDP-2`・TASK-43.3b）。
///
/// `crate::ws` が接続の開始・終了で `CdpState` 経由に
/// [`Self::open_connection`]・[`Self::release_connection`] を呼ぶ。保持件数は生存接続数以下で、
/// 値は固定サイズ。`Target.attachedToTarget` の送出は 2c（未実装）で、現状は保持のみ。
pub(crate) struct AutoAttachStates {
    inner: Mutex<AutoAttachInner>,
}

impl AutoAttachStates {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(AutoAttachInner {
                live: HashSet::new(),
                configs: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, AutoAttachInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 接続を生存中として登録する。
    pub(crate) fn open_connection(&self, conn: ConnId) {
        self.lock().live.insert(conn);
    }

    /// 設定を保存する。`Some(conn)` が生存していなければ保存せず `false`。
    pub(crate) fn set(&self, conn: Option<ConnId>, cfg: AutoAttachConfig) -> bool {
        let mut st = self.lock();
        if conn.is_some_and(|c| !st.live.contains(&c)) {
            return false;
        }
        st.configs.insert(conn, cfg);
        true
    }

    /// 接続の終了で設定を解放する。
    pub(crate) fn release_connection(&self, conn: ConnId) {
        let mut st = self.lock();
        st.live.remove(&conn);
        st.configs.remove(&Some(conn));
    }

    /// 現在の設定（テスト用）。
    #[cfg(test)]
    pub(crate) fn get(&self, conn: Option<ConnId>) -> Option<AutoAttachConfig> {
        self.lock().configs.get(&conn).copied()
    }

    /// 保持件数（テスト用）。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().configs.len()
    }
}

/// `Browser.setDownloadBehavior` の `behavior`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum DownloadBehavior {
    /// 既定（未指定時の初期値、または `default`）。
    #[default]
    Default,
    /// `deny`。
    Deny,
    /// `allow`。
    Allow,
    /// `allowAndName`。
    AllowAndName,
}

/// 保持するダウンロード挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DownloadPolicy {
    /// `behavior`。
    pub behavior: DownloadBehavior,
    /// `eventsEnabled`（欠落は `false`）。
    pub events_enabled: bool,
}

/// ブラウザ全体のダウンロード挙動（固定サイズ 1 件）。ダウンロード機能は未実装で、
/// 保持した値は現時点でどの処理からも参照されない（`CDP-2`・`REPAIR-3`）。
pub(crate) struct DownloadPolicyState {
    inner: Mutex<DownloadPolicy>,
}

impl DownloadPolicyState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(DownloadPolicy::default()),
        }
    }

    pub(crate) fn set(&self, p: DownloadPolicy) {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner) = p;
    }

    /// 現在の挙動。
    pub(crate) fn get(&self) -> DownloadPolicy {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

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

/// `Target.setAutoAttach`（`CDP-2`・TASK-43.3b）。
///
/// ルート（`sessionId` なし）・`flatten:true` のみ受理し、接続別に設定を保持して `{}` を返す。
/// `Target.attachedToTarget` の送出と既存ターゲットへの自動 attach は 2c で実装する
/// （2a は保持のみで送出しない。実装済みを装わない。`REPAIR-3`）。`filter`・非 flatten・
/// `sessionId` 付きは `-32602`（`UNSUPPORTED_PARAMS`）で明示拒否する。
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
            let cfg = parse_set_auto_attach(params)?;
            if !ctx.state.auto_attach().set(ctx.conn, cfg) {
                return Err(CdpError::SERVER_ERROR);
            }
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// `Target.setAutoAttach` の params 検査。
fn parse_set_auto_attach(params: &Value) -> Result<AutoAttachConfig, CdpError> {
    let auto_attach = params
        .get("autoAttach")
        .and_then(Value::as_bool)
        .ok_or(CdpError::INVALID_PARAMS)?;
    let wait = params
        .get("waitForDebuggerOnStart")
        .and_then(Value::as_bool)
        .ok_or(CdpError::INVALID_PARAMS)?;
    let flatten = match params.get("flatten") {
        None => false,
        Some(v) => v.as_bool().ok_or(CdpError::INVALID_PARAMS)?,
    };
    if !flatten || params.get("filter").is_some() {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    Ok(AutoAttachConfig {
        auto_attach,
        wait_for_debugger_on_start: wait,
    })
}

/// `Browser.setDownloadBehavior`（`CDP-2`・TASK-43.3b）。
///
/// 挙動をブラウザ全体で保持して `{}` を返す。ダウンロード機能は未実装で、保持した挙動は
/// どの処理からも参照されず `Browser.downloadWillBegin`・`downloadProgress` も送出しない。
/// `downloadPath` は不透明な文字列として長さだけ検査し、ファイルシステムを触らない。
/// `browserContextId` 付きは 2b の範囲のため `-32602` で拒否する。
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
            let policy = parse_download_behavior(params)?;
            ctx.state.download_policy().set(policy);
            Ok(HandlerOutput::result(json!({})))
        })
    }
}

/// `Browser.setDownloadBehavior` の params 検査。
fn parse_download_behavior(params: &Value) -> Result<DownloadPolicy, CdpError> {
    let behavior = match params.get("behavior").and_then(Value::as_str) {
        Some("default") => DownloadBehavior::Default,
        Some("deny") => DownloadBehavior::Deny,
        Some("allow") => DownloadBehavior::Allow,
        Some("allowAndName") => DownloadBehavior::AllowAndName,
        _ => return Err(CdpError::INVALID_PARAMS),
    };
    if params.get("browserContextId").is_some() {
        return Err(CdpError::UNSUPPORTED_PARAMS);
    }
    let path = match params.get("downloadPath") {
        None => None,
        Some(v) => {
            let s = v.as_str().ok_or(CdpError::INVALID_PARAMS)?;
            if s.len() > MAX_DOWNLOAD_PATH_LEN {
                return Err(CdpError::INVALID_PARAMS);
            }
            Some(s)
        }
    };
    if matches!(
        behavior,
        DownloadBehavior::Allow | DownloadBehavior::AllowAndName
    ) && path.is_none()
    {
        return Err(CdpError::INVALID_PARAMS);
    }
    let events_enabled = match params.get("eventsEnabled") {
        None => false,
        Some(v) => v.as_bool().ok_or(CdpError::INVALID_PARAMS)?,
    };
    Ok(DownloadPolicy {
        behavior,
        events_enabled,
    })
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
                    json!({
                        "targetId": t.target_id().as_str(),
                        "type": t.kind().as_str(),
                        "title": t.url(),
                        "url": t.url(),
                        "attached": attached,
                        "canAccessOpener": false,
                    })
                }
            };
            Ok(HandlerOutput::result(json!({ "targetInfo": info })))
        })
    }
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

    fn cfg(a: bool, w: bool) -> AutoAttachConfig {
        AutoAttachConfig {
            auto_attach: a,
            wait_for_debugger_on_start: w,
        }
    }

    #[test]
    fn cdp2_auto_attach_states_are_per_connection_and_released() {
        let st = AutoAttachStates::new();
        let (a, b) = (ConnId::new(1), ConnId::new(2));
        st.open_connection(a);
        st.open_connection(b);
        assert!(st.set(Some(a), cfg(true, true)));
        assert_eq!(st.get(Some(a)), Some(cfg(true, true)));
        assert_eq!(st.get(Some(b)), None);
        st.release_connection(a);
        assert_eq!(st.get(Some(a)), None);
        assert_eq!(st.len(), 0);
        // 解放済み・未登録の接続への書き込みは保存しない。
        assert!(!st.set(Some(a), cfg(true, true)));
        assert!(!st.set(Some(ConnId::new(9)), cfg(true, true)));
        assert_eq!(st.len(), 0);
        // 直接 dispatch（None）は常に保存できる。
        assert!(st.set(None, cfg(false, false)));
        assert_eq!(st.get(None), Some(cfg(false, false)));
    }

    #[test]
    fn cdp2_set_auto_attach_params() {
        let ok = json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true});
        assert_eq!(parse_set_auto_attach(&ok), Ok(cfg(true, true)));
        let off = json!({"autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true});
        assert_eq!(parse_set_auto_attach(&off), Ok(cfg(false, false)));
        for p in [
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": false}),
            json!({"autoAttach": true, "waitForDebuggerOnStart": true}),
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true, "filter": []}),
        ] {
            assert_eq!(parse_set_auto_attach(&p), Err(CdpError::UNSUPPORTED_PARAMS));
        }
        for p in [
            json!({"waitForDebuggerOnStart": true, "flatten": true}),
            json!({"autoAttach": true, "flatten": true}),
            json!({"autoAttach": "yes", "waitForDebuggerOnStart": true, "flatten": true}),
            json!({"autoAttach": true, "waitForDebuggerOnStart": 1, "flatten": true}),
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": "x"}),
        ] {
            assert_eq!(parse_set_auto_attach(&p), Err(CdpError::INVALID_PARAMS));
        }
    }

    #[test]
    fn cdp2_set_download_behavior_params() {
        let dp = |b, e| DownloadPolicy {
            behavior: b,
            events_enabled: e,
        };
        assert_eq!(
            parse_download_behavior(
                &json!({"behavior": "allowAndName", "downloadPath": "<ARTIFACTS_DIR>", "eventsEnabled": true})
            ),
            Ok(dp(DownloadBehavior::AllowAndName, true))
        );
        assert_eq!(
            parse_download_behavior(&json!({"behavior": "deny"})),
            Ok(dp(DownloadBehavior::Deny, false))
        );
        assert_eq!(
            parse_download_behavior(&json!({"behavior": "default"})),
            Ok(dp(DownloadBehavior::Default, false))
        );
        assert_eq!(
            parse_download_behavior(&json!({"behavior": "allow", "downloadPath": "../../etc"})),
            Ok(dp(DownloadBehavior::Allow, false))
        );
        let max = "a".repeat(MAX_DOWNLOAD_PATH_LEN);
        assert!(
            parse_download_behavior(&json!({"behavior": "allow", "downloadPath": max})).is_ok()
        );
        let over = "a".repeat(MAX_DOWNLOAD_PATH_LEN + 1);
        for p in [
            json!({"behavior": "Allow", "downloadPath": "x"}),
            json!({"behavior": "bogus"}),
            json!({"behavior": 1}),
            json!({}),
            json!({"behavior": "allow", "downloadPath": 1}),
            json!({"behavior": "allow", "downloadPath": over}),
            json!({"behavior": "allowAndName"}),
            json!({"behavior": "deny", "eventsEnabled": "yes"}),
        ] {
            assert_eq!(parse_download_behavior(&p), Err(CdpError::INVALID_PARAMS));
        }
        for p in [
            json!({"behavior": "deny", "browserContextId": "X"}),
            json!({"behavior": "deny", "browserContextId": null}),
        ] {
            assert_eq!(
                parse_download_behavior(&p),
                Err(CdpError::UNSUPPORTED_PARAMS)
            );
        }
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
}

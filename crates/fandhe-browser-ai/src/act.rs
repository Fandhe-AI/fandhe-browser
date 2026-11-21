//! `POST /ai/act` の本体（TASK-128・`AISNAP-17`・`AISNAP-21`）。
//!
//! HTTP の受け口（Host・Origin・Content-Type の検査と応答）は [`crate::api`]。本モジュールは本文の検査と、
//! 操作（直前の取り直し → 写しの上での解決 → ホストへの操作）を行う。ブロッキングするため、呼び出し元は
//! スレッドへ移して呼ぶ（[`crate::blocking`]）。
//!
//! # 2026-11-19 のオーナー判断（TASK-128・#1617）
//!
//! - 最初は `click` と `fill` だけを入れる（段階導入）。`select`・`press` は [`ActError::NotImplemented`]（501）
//! - `sensitive`: `sensitive` の `fill`、または解決した時点で伏せる対象だった要素への操作があった世代では、以後
//!   その世代の間、すべての入力欄の値を要約・CDP に出さない（ホストの番号は取得ごとに変わり、次の取得で同じ
//!   要素を特定できないため）。印（core の `NavigationState::mark_redact_all`）は操作をホストへ送る前に付ける
//!   （失敗・時間切れでも途中まで入力されうるため）
//! - `busy`: 操作の実行中に来た `/ai/snapshot` の同期と別の操作は、待たせずに `busy`（409）
//!
//! # spec に未記載の挙動（spec へは後追いで足す）
//!
//! - `/ai/snapshot` が操作モードの同期をしている間に来た操作も `busy`（同じ排他を読み側で取るため）
//! - 成功応答は `{"ok":true,"generation","navigated"}`。取り直し・ホストへの操作の失敗は `act_failed`（502）、
//!   ナビゲート結果が無ければ `no_navigation`（409。`/ai/snapshot` と同じ）
//!
//! # 既知の制約
//!
//! - 全部伏せた世代では、`<input type=submit value=…>` の表示名や checkbox の `checked` も消えるため、要約の
//!   ボタン名・ref が伏せる前と変わりうる（spec の「すべての入力値を伏せる」どおり）
//! - 要求が固定するのはナビゲーション世代だけで、同じ世代の中でページが別の文書へ遷移していても、取り直した
//!   文書の上で解決して操作する（クライアントが見た文書とは照合しない）
//! - CDP からの見え方は [`crate::navigate`] の既知の制約と同じ（操作の結果はセッション付きの `DOM.*` から
//!   見えない。TASK-130）
//!
//! # 流れ
//!
//! 本文の検査 → 操作の排他（取れなければ `busy`）→ 確定結果の確認（無い・静的・世代違い）→ 直前の取り直し
//! （最小間隔によらない。`AISNAP-17`）→ 写しの上で一意に解決（`not_found`・`ambiguous`）→ 必要なら伏せる印 →
//! ホストへ操作 → 同期の記録を消す（成否によらず。次の `/ai/snapshot` で取り直させる）。`stale` の自動の
//! 再試行はしない（設計 §14.9 の未決。呼び出し元が `/ai/snapshot` を取り直して再度送る）。

use std::fmt;
use std::sync::TryLockError;

use fandhe_browser_core::NavigationGeneration;
use fandhe_browser_core::interactive::is_redaction_target;
use fandhe_browser_core::render::{ActAction, ActRequest, MAX_FILL_TEXT_BYTES, RenderError};
use fandhe_browser_core::state::PageMode;
use serde_json::{Map, Value};

use crate::interactive::{
    MAX_TARGET_BYTES, ResolveError, SyncFailure, Target, resolve_target, sync_interactive_now,
};
use crate::plugin_api::AiState;

/// 本文の上限（バイト）。`fill` の文字列（8192 バイト。JSON のエスケープで最大 6 倍）と対象の指定に余裕を足す。
pub const MAX_ACT_BODY_BYTES: usize = 64 * 1024;

/// 操作の成功結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActOk {
    generation: NavigationGeneration,
    navigated: bool,
}

impl ActOk {
    /// 操作したページのナビゲーション世代。
    pub fn generation(&self) -> NavigationGeneration {
        self.generation
    }
    /// 操作で遷移が始まったか（最善努力。設計 §14.4）。
    pub fn navigated(&self) -> bool {
        self.navigated
    }
}

/// `POST /ai/act` の失敗（`AISNAP-17`・`AISNAP-21`）。[`Self::status_and_code`] が HTTP へ写す。
/// `Display` は固定の英文で、入力文字列・URL を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActError {
    /// 本文の不正（JSON・未知のキー・型・ref とセレクタの両方か無し・長さ・セレクタの構文）。
    InvalidParams,
    /// `select`・`press`（2026-11-19 の判断で後続）。
    NotImplemented,
    /// 別の操作の実行中。
    Busy,
    /// 直近のナビゲート結果が無い。
    NoNavigation,
    /// 確定結果が静的モード。
    NotInteractive,
    /// 世代が最新と違う・取得の後に対象が変わった。
    Stale,
    /// 一致する要素が無い。
    NotFound,
    /// 2 つ以上の要素が一致した。
    Ambiguous,
    /// 操作できない（不可視・`disabled`・隠れている・フォーカスが外れた・現在の文書が http(s) でない）。
    NotActionable,
    /// 新しいウィンドウ・ファイル選択を要求した。
    Unsupported,
    /// 描画層が無効。
    RenderingDisabled,
    /// ホストが破棄・再起動された（新しい navigate まで戻らない）。
    SessionLost,
    /// ページが操作モードの条件を満たさなくなった（`navigator.webdriver`。SEC-5）。
    Unavailable,
    /// 直前の取り直し・ホストへの操作に失敗した。
    Failed,
}

impl ActError {
    /// HTTP ステータスとエラーコード（2026-11-19 のオーナー判断の表。`no_navigation` は既存の `/ai/snapshot` と同じ）。
    pub fn status_and_code(self) -> (u16, &'static str) {
        match self {
            Self::InvalidParams => (400, "invalid_params"),
            Self::NotImplemented => (501, "not_implemented"),
            Self::NotFound => (404, "not_found"),
            Self::Busy => (409, "busy"),
            Self::NoNavigation => (409, "no_navigation"),
            Self::NotInteractive => (409, "not_interactive"),
            Self::Stale => (409, "stale"),
            Self::Ambiguous => (409, "ambiguous"),
            Self::NotActionable => (409, "not_actionable"),
            Self::Unsupported => (409, "unsupported"),
            Self::RenderingDisabled => (409, "rendering_disabled"),
            Self::SessionLost => (409, "session_lost"),
            Self::Unavailable => (409, "interactive_unavailable"),
            Self::Failed => (502, "act_failed"),
        }
    }

    /// 同期の失敗を写す（[`crate::wait`] も使う）。
    pub(crate) fn from_sync(e: SyncFailure) -> Self {
        match e {
            SyncFailure::SessionLost => Self::SessionLost,
            SyncFailure::RenderingDisabled => Self::RenderingDisabled,
            SyncFailure::Stale => Self::Stale,
            SyncFailure::Unavailable => Self::Unavailable,
            _ => Self::Failed,
        }
    }

    fn from_render(e: &RenderError) -> Self {
        match e {
            RenderError::Stale | RenderError::PageNotLoaded => Self::Stale,
            // 現在の URL が http(s) でない文書（`about:blank` 等）は操作できない。
            RenderError::NotActionable | RenderError::InvalidState => Self::NotActionable,
            RenderError::Unsupported => Self::Unsupported,
            RenderError::InvalidParams => Self::InvalidParams,
            RenderError::SessionLost => Self::SessionLost,
            RenderError::RenderingDisabled => Self::RenderingDisabled,
            RenderError::NotInteractive => Self::NotInteractive,
            _ => Self::Failed,
        }
    }
}

impl fmt::Display for ActError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidParams => "invalid act parameters",
            Self::NotImplemented => "this action is not implemented yet",
            Self::Busy => "another operation is in progress",
            Self::NoNavigation => "no navigation has been performed yet",
            Self::NotInteractive => "page is not in interactive mode",
            Self::Stale => "page changed since the snapshot",
            Self::NotFound => "target not found",
            Self::Ambiguous => "target matches more than one element",
            Self::NotActionable => "target is not actionable",
            Self::Unsupported => "action requested an unsupported feature",
            Self::RenderingDisabled => "rendering is disabled",
            Self::SessionLost => "rendering host session was lost",
            Self::Unavailable => "page does not meet interactive mode requirements",
            Self::Failed => "failed to perform the action",
        })
    }
}

impl std::error::Error for ActError {}

/// 本文を検査した結果。`Debug` は入力文字列を出さない（[`ActAction`] の `Debug`）。
#[derive(Debug, PartialEq, Eq)]
struct Request {
    target: Target,
    generation: u64,
    action: ActAction,
}

/// 本文を検査する（副作用なし）。`{"ref" | "selector": <文字列>, "generation": <整数>, "action": {...}}`。
fn parse(body: &[u8]) -> Result<Request, ActError> {
    if body.len() > MAX_ACT_BODY_BYTES {
        return Err(ActError::InvalidParams);
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| ActError::InvalidParams)?;
    let obj = v.as_object().ok_or(ActError::InvalidParams)?;
    only_keys(obj, &["ref", "selector", "generation", "action"])?;
    let target = match (obj.get("ref"), obj.get("selector")) {
        (Some(Value::String(r)), None) => Target::Ref(r.clone()),
        (None, Some(Value::String(s))) => Target::Selector(s.clone()),
        _ => return Err(ActError::InvalidParams),
    };
    if let Target::Ref(t) | Target::Selector(t) = &target
        && (t.is_empty() || t.len() > MAX_TARGET_BYTES)
    {
        return Err(ActError::InvalidParams);
    }
    let generation = obj
        .get("generation")
        .and_then(Value::as_u64)
        .ok_or(ActError::InvalidParams)?;
    let action = obj
        .get("action")
        .and_then(Value::as_object)
        .ok_or(ActError::InvalidParams)?;
    let action = match action.get("type").and_then(Value::as_str) {
        Some("click") => {
            only_keys(action, &["type"])?;
            ActAction::Click
        }
        Some("fill") => {
            only_keys(action, &["type", "text", "sensitive"])?;
            let text = action
                .get("text")
                .and_then(Value::as_str)
                .ok_or(ActError::InvalidParams)?;
            if text.len() > MAX_FILL_TEXT_BYTES {
                return Err(ActError::InvalidParams);
            }
            let sensitive = match action.get("sensitive") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(ActError::InvalidParams),
            };
            ActAction::Fill {
                text: text.to_owned(),
                sensitive,
            }
        }
        Some("select" | "press") => return Err(ActError::NotImplemented),
        _ => return Err(ActError::InvalidParams),
    };
    Ok(Request {
        target,
        generation,
        action,
    })
}

fn only_keys(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), ActError> {
    if obj.keys().all(|k| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(ActError::InvalidParams)
    }
}

/// `POST /ai/act` を処理する（ブロッキングする。呼び出し元はスレッドへ移す）。
pub fn act(state: &AiState, body: &[u8]) -> Result<ActOk, ActError> {
    let request = parse(body)?;
    let _guard = match state.operation_lock().try_write() {
        Ok(g) => g,
        Err(TryLockError::WouldBlock) => return Err(ActError::Busy),
        // 書き込みは `()` だけで壊れる状態が無いため、poison は回復して使う。
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
    };
    let app = state.app();
    let nav = app.navigation();
    let generation = check_current(state, request.generation)?;
    // 直前の取り直し（最小間隔によらない。`AISNAP-17`）。失敗の扱いは `/ai/snapshot` と同じ（無効化を含む）。
    let synced = sync_interactive_now(app, state.sync_tracker()).map_err(ActError::from_sync);
    let result = synced.and_then(|()| {
        let (g, latest) = nav.latest_with_generation().ok_or(ActError::Stale)?;
        if g != generation {
            return Err(ActError::Stale);
        }
        let doc = latest.interactive().ok_or(ActError::Stale)?;
        let resolved = resolve_target(doc, &request.target).map_err(|e| match e {
            ResolveError::NotFound => ActError::NotFound,
            ResolveError::Ambiguous => ActError::Ambiguous,
            ResolveError::InvalidTarget => ActError::InvalidParams,
            _ => ActError::Failed,
        })?;
        let document = doc.document();
        let node = resolved.node();
        let tag = document.local_name(node).ok_or(ActError::Failed)?;
        let attrs = ["id", "name", "type"].map(|a| document.attribute(node, a));
        let req = ActRequest::new(
            doc.page().clone(),
            doc.snapshot(),
            resolved.number(),
            tag,
            attrs,
            request.action.clone(),
        )
        .map_err(|e| ActError::from_render(&e))?;
        let sensitive = matches!(
            request.action,
            ActAction::Fill {
                sensitive: true,
                ..
            }
        ) || is_redaction_target(doc, node);
        if sensitive {
            nav.mark_redact_all(generation)
                .map_err(|_| ActError::Stale)?;
        }
        let out = app.act(&req).map_err(|e| {
            let err = ActError::from_render(&e);
            if err == ActError::SessionLost {
                // ホストが失われた。古い内容を今のページと取り違えないよう確定結果を無効にする（`AISNAP-20`）。
                let _ = nav.invalidate_interactive(generation);
            }
            err
        });
        // 成否によらず、次の `/ai/snapshot` で取り直させる（途中まで入力されうるため）。
        state.sync_tracker().clear();
        out
    })?;
    Ok(ActOk {
        generation,
        navigated: result.navigated(),
    })
}

/// 確定結果が操作モードで、要求の世代が最新かを確かめる（[`crate::wait`] も使う）。
pub(crate) fn check_current(
    state: &AiState,
    requested: u64,
) -> Result<NavigationGeneration, ActError> {
    let nav = state.app().navigation();
    let Some((generation, latest)) = nav.latest_with_generation() else {
        return Err(if nav.interactive_lost() {
            ActError::SessionLost
        } else {
            ActError::NoNavigation
        });
    };
    if latest.mode() != PageMode::Interactive {
        return Err(ActError::NotInteractive);
    }
    if generation.get() != requested {
        return Err(ActError::Stale);
    }
    Ok(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AISNAP-17`・2026-11-19 の判断: 本文の検査。ref とセレクタは片方だけ、`select`・`press` は 501。
    #[test]
    fn aisnap17_parse_body() {
        let p = |s: &str| parse(s.as_bytes());
        assert_eq!(
            p(r##"{"selector":"#a","generation":3,"action":{"type":"click"}}"##),
            Ok(Request {
                target: Target::Selector("#a".into()),
                generation: 3,
                action: ActAction::Click
            })
        );
        assert_eq!(
            p(
                r#"{"ref":"e1","generation":1,"action":{"type":"fill","text":"hi","sensitive":true}}"#
            ),
            Ok(Request {
                target: Target::Ref("e1".into()),
                generation: 1,
                action: ActAction::Fill {
                    text: "hi".into(),
                    sensitive: true
                }
            })
        );
        for body in [
            r#"{"ref":"e1","generation":1,"action":{"type":"select","values":["a"]}}"#,
            r#"{"ref":"e1","generation":1,"action":{"type":"press","key":"Enter"}}"#,
        ] {
            assert_eq!(p(body), Err(ActError::NotImplemented), "{body}");
        }
        for body in [
            r#"{"ref":"e1","selector":"a","generation":1,"action":{"type":"click"}}"#,
            r#"{"generation":1,"action":{"type":"click"}}"#,
            r#"{"ref":"","generation":1,"action":{"type":"click"}}"#,
            r#"{"ref":"e1","action":{"type":"click"}}"#,
            r#"{"ref":"e1","generation":-1,"action":{"type":"click"}}"#,
            r#"{"ref":"e1","generation":1,"action":{"type":"click","x":1}}"#,
            r#"{"ref":"e1","generation":1,"action":{"type":"fill"}}"#,
            r#"{"ref":"e1","generation":1,"action":{"type":"fill","text":"a","sensitive":"yes"}}"#,
            r#"{"ref":"e1","generation":1,"action":{"type":"hover"}}"#,
            r#"{"ref":"e1","generation":1,"action":{"type":"click"},"x":1}"#,
            "[]",
        ] {
            assert_eq!(p(body), Err(ActError::InvalidParams), "{body}");
        }
        let long = format!(
            r#"{{"ref":"e1","generation":1,"action":{{"type":"fill","text":"{}"}}}}"#,
            "a".repeat(MAX_FILL_TEXT_BYTES + 1)
        );
        assert_eq!(p(&long), Err(ActError::InvalidParams));
        let long_ref = format!(
            r#"{{"ref":"{}","generation":1,"action":{{"type":"click"}}}}"#,
            "a".repeat(MAX_TARGET_BYTES + 1)
        );
        assert_eq!(p(&long_ref), Err(ActError::InvalidParams));
    }

    /// 2026-11-19 の判断の HTTP の表（`not_found` 404・状態は 409・失敗は 502）。
    #[test]
    fn aisnap21_status_table() {
        use ActError::*;
        assert_eq!(NotFound.status_and_code(), (404, "not_found"));
        assert_eq!(Busy.status_and_code(), (409, "busy"));
        assert_eq!(InvalidParams.status_and_code(), (400, "invalid_params"));
        assert_eq!(NotImplemented.status_and_code(), (501, "not_implemented"));
        assert_eq!(Failed.status_and_code(), (502, "act_failed"));
        for e in [
            NotInteractive,
            Stale,
            Ambiguous,
            NotActionable,
            Unsupported,
            RenderingDisabled,
            SessionLost,
            Unavailable,
        ] {
            assert_eq!(e.status_and_code().0, 409, "{e:?}");
        }
    }
}

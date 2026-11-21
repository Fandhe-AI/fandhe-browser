//! `POST /ai/wait` の本体（TASK-129・`AISNAP-18`・`AISNAP-21`・MS-11）。
//!
//! HTTP の受け口（Host・Origin・Content-Type の検査と応答）は [`crate::api`]。本モジュールは本文の検査と、
//! 待機（操作の排他 → 直前の取り直し → ホストへの `wait`）を行う。ブロッキングするため、呼び出し元はスレッドへ移して
//! 呼ぶ（[`crate::blocking`]）。`timeout_ms` はホストでの条件の待機だけに効き、API 全体はそれに直前の取り直し
//! （DOM の取得。既定で最大 35 秒）と描画ゲートの待ち・応答待ちの余裕（既定 5 秒）がかかる。呼び出し元の期限はその分を見込む。
//!
//! # spec に未記載の挙動（spec へは後追いで足す）
//!
//! - 本文は `{"generation": <整数>, "condition": {...}, "timeout_ms": <整数>}`。`condition` は
//!   `{"type": "selector_present" | "selector_absent", "selector": <文字列>}`・`{"type": "text_present", "text": <文字列>}`・
//!   `{"type": "load_complete"}`（`/ai/act` の `action` と同じく `type` で選ぶ）
//! - 成功応答は `{"ok": true, "generation"}`。上限時間までに成立しなければ `timeout`（409。状態によるものと同じ扱い。
//!   ホストは保つ）、取り直し・ホストへの待機の失敗は `wait_failed`（502）
//! - 伏せる対象の入力欄がある文書・全部伏せた世代では、selector 系の条件を `unsupported`（409）で断る
//!   （TASK-130 の指摘 S-8。2026-11-21 の利用者の判断。`load_complete`・`text_present` は通す）
//! - 待つ前に DOM を取り直す（最小間隔によらない）。直前の `/ai/act` で遷移していても、いまの文書から待てるようにするため
//!   （ホストは要求の文書番号が現在と違えば `stale` で断る）
//!
//! # `busy`
//!
//! 待機の間は `/ai/act` と同じ排他を書き込みで持つ。待機中の別の待機・操作・操作モードの `/ai/snapshot` は、待たせずに
//! `busy`（409）になる（`AISNAP-18`）。撮影の入口（TASK-134）を足すときは、同じ排他を読み取りで取って `busy` を返すこと。
//!
//! # 既知の制約
//!
//! - 待機中の `/ai/navigate` は `busy` にならず、描画層のゲートで待機の終わりまで待つ（最大で上限時間と応答待ちの余裕）。
//!   その間に古いページで条件が成立しても、世代が進んでいれば `stale` で返す
//! - 要求が固定するのはナビゲーション世代だけ（`/ai/act` と同じ）。待機の開始の文書は直前の取り直しで決まる

use std::fmt;
use std::sync::TryLockError;

use fandhe_browser_core::NavigationGeneration;
use fandhe_browser_core::interactive::has_redaction_target;
use fandhe_browser_core::render::{
    MAX_WAIT_CONDITION_BYTES, MAX_WAIT_TIMEOUT_MS, RenderError, WaitCondition, WaitRequest,
};
use serde_json::{Map, Value};

use crate::act::{ActError, check_current};
use crate::interactive::sync_interactive_now_at;
use crate::plugin_api::AiState;

/// 本文の上限（バイト）。条件の文字列（1024 バイト。JSON のエスケープで最大 6 倍）に余裕を足す。
pub const MAX_WAIT_BODY_BYTES: usize = 8 * 1024;

/// 待機の成功結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaitOk {
    generation: NavigationGeneration,
}

impl WaitOk {
    /// 待機したページのナビゲーション世代。
    pub fn generation(&self) -> NavigationGeneration {
        self.generation
    }
}

/// `POST /ai/wait` の失敗（`AISNAP-18`・`AISNAP-21`）。[`Self::status_and_code`] が HTTP へ写す。
/// `Display` は固定の英文で、条件の文字列を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WaitError {
    /// 本文の不正（JSON・未知のキー・型・長さ・上限時間の範囲・ホストが解釈できないセレクタ）。
    InvalidParams,
    /// 別の待機・操作の実行中。
    Busy,
    /// 直近のナビゲート結果が無い。
    NoNavigation,
    /// 確定結果が静的モード。
    NotInteractive,
    /// 世代が最新と違う・待機中に文書が変わった（`load_complete` を除く）。
    Stale,
    /// 上限時間までに条件が成立しなかった（ホストは保つ）。
    Timeout,
    /// このページでは使えない条件（伏せる対象の入力欄がある文書・全部伏せた世代での selector 系の条件。TASK-130 の
    /// 指摘 S-8。2026-11-21 の利用者の判断）。
    Unsupported,
    /// 描画層が無効。
    RenderingDisabled,
    /// ホストが破棄・再起動された（新しい navigate まで戻らない）。
    SessionLost,
    /// ページが操作モードの条件を満たさなくなった（`navigator.webdriver`。SEC-5）。
    Unavailable,
    /// 直前の取り直し・ホストへの待機に失敗した。
    Failed,
}

impl WaitError {
    /// HTTP ステータスとエラーコード（`/ai/act` と同じ表に `timeout` 409 と `wait_failed` 502 を足す）。
    pub fn status_and_code(self) -> (u16, &'static str) {
        match self {
            Self::InvalidParams => (400, "invalid_params"),
            Self::Busy => (409, "busy"),
            Self::NoNavigation => (409, "no_navigation"),
            Self::NotInteractive => (409, "not_interactive"),
            Self::Stale => (409, "stale"),
            Self::Timeout => (409, "timeout"),
            Self::Unsupported => (409, "unsupported"),
            Self::RenderingDisabled => (409, "rendering_disabled"),
            Self::SessionLost => (409, "session_lost"),
            Self::Unavailable => (409, "interactive_unavailable"),
            Self::Failed => (502, "wait_failed"),
        }
    }

    fn from_render(e: &RenderError) -> Self {
        match e {
            RenderError::Stale | RenderError::PageNotLoaded => Self::Stale,
            RenderError::Timeout => Self::Timeout,
            RenderError::InvalidParams => Self::InvalidParams,
            RenderError::SessionLost => Self::SessionLost,
            RenderError::RenderingDisabled => Self::RenderingDisabled,
            RenderError::NotInteractive => Self::NotInteractive,
            _ => Self::Failed,
        }
    }
}

/// 確定結果の確認と直前の取り直し（[`crate::act`] と共有）の失敗を写す。
impl From<ActError> for WaitError {
    fn from(e: ActError) -> Self {
        match e {
            ActError::NoNavigation => Self::NoNavigation,
            ActError::NotInteractive => Self::NotInteractive,
            ActError::Stale => Self::Stale,
            ActError::RenderingDisabled => Self::RenderingDisabled,
            ActError::SessionLost => Self::SessionLost,
            ActError::Unavailable => Self::Unavailable,
            _ => Self::Failed,
        }
    }
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidParams => "invalid wait parameters",
            Self::Busy => "another operation is in progress",
            Self::NoNavigation => "no navigation has been performed yet",
            Self::NotInteractive => "page is not in interactive mode",
            Self::Stale => "page changed since the snapshot",
            Self::Timeout => "condition was not met within the timeout",
            Self::Unsupported => "condition is not supported on this page",
            Self::RenderingDisabled => "rendering is disabled",
            Self::SessionLost => "rendering host session was lost",
            Self::Unavailable => "page does not meet interactive mode requirements",
            Self::Failed => "failed to wait for the condition",
        })
    }
}

impl std::error::Error for WaitError {}

/// 本文を検査した結果。`Debug` は条件の文字列を出さない（[`WaitCondition`] の `Debug`）。
#[derive(Debug, PartialEq, Eq)]
struct Request {
    generation: u64,
    condition: WaitCondition,
    timeout_ms: u32,
}

/// 本文を検査する（副作用なし）。
fn parse(body: &[u8]) -> Result<Request, WaitError> {
    if body.len() > MAX_WAIT_BODY_BYTES {
        return Err(WaitError::InvalidParams);
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| WaitError::InvalidParams)?;
    let obj = v.as_object().ok_or(WaitError::InvalidParams)?;
    only_keys(obj, &["generation", "condition", "timeout_ms"])?;
    let generation = obj
        .get("generation")
        .and_then(Value::as_u64)
        .ok_or(WaitError::InvalidParams)?;
    let timeout_ms = obj
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| (1..=MAX_WAIT_TIMEOUT_MS).contains(n))
        .ok_or(WaitError::InvalidParams)?;
    let c = obj
        .get("condition")
        .and_then(Value::as_object)
        .ok_or(WaitError::InvalidParams)?;
    let text = |key: &str| -> Result<String, WaitError> {
        only_keys(c, &["type", key])?;
        c.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= MAX_WAIT_CONDITION_BYTES)
            .map(str::to_owned)
            .ok_or(WaitError::InvalidParams)
    };
    let condition = match c.get("type").and_then(Value::as_str) {
        Some("selector_present") => WaitCondition::SelectorPresent(text("selector")?),
        Some("selector_absent") => WaitCondition::SelectorAbsent(text("selector")?),
        Some("text_present") => WaitCondition::TextPresent(text("text")?),
        Some("load_complete") => {
            only_keys(c, &["type"])?;
            WaitCondition::LoadComplete
        }
        _ => return Err(WaitError::InvalidParams),
    };
    Ok(Request {
        generation,
        condition,
        timeout_ms,
    })
}

fn only_keys(obj: &Map<String, Value>, allowed: &[&str]) -> Result<(), WaitError> {
    if obj.keys().all(|k| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(WaitError::InvalidParams)
    }
}

/// `POST /ai/wait` を処理する（ブロッキングする。呼び出し元はスレッドへ移す）。
pub fn wait(state: &AiState, body: &[u8]) -> Result<WaitOk, WaitError> {
    let request = parse(body)?;
    let _guard = match state.operation_lock().try_write() {
        Ok(g) => g,
        Err(TryLockError::WouldBlock) => return Err(WaitError::Busy),
        // 書き込みは `()` だけで壊れる状態が無いため、poison は回復して使う。
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
    };
    let app = state.app();
    let nav = app.navigation();
    let generation = check_current(state, request.generation)?;
    sync_interactive_now_at(app, state.sync_tracker(), generation)
        .map_err(|e| WaitError::from(ActError::from_sync(e)))?;
    let (g, latest) = nav.latest_with_generation().ok_or(WaitError::Stale)?;
    if g != generation {
        return Err(WaitError::Stale);
    }
    let doc = latest.interactive().ok_or(WaitError::Stale)?;
    // 伏せる対象の入力欄がある文書・全部伏せた世代では、ホストの生の DOM で評価する selector 系の条件を断る
    // （`AISNAP-18`・TASK-130 の指摘 S-8・2026-11-21 の利用者の判断。`load_complete`・`text_present` は通す）。
    if matches!(
        request.condition,
        WaitCondition::SelectorPresent(_) | WaitCondition::SelectorAbsent(_)
    ) && (nav.redacts_all(generation) || has_redaction_target(doc))
    {
        return Err(WaitError::Unsupported);
    }
    let req = WaitRequest::new(doc.page().clone(), request.condition, request.timeout_ms)
        .map_err(|e| WaitError::from_render(&e))?;
    let out = app.wait(generation, &req).map_err(|e| {
        let err = WaitError::from_render(&e);
        if matches!(err, WaitError::SessionLost | WaitError::Failed) {
            // ホストが失われた・待機の途中でホストがページを捨てた（クラッシュ等）。古い内容を今のページと
            // 取り違えないよう確定結果を無効にする（`AISNAP-20`・`AISNAP-21`。失敗の種類を区別できないため
            // 安全側に倒す）。
            let _ = nav.invalidate_interactive(generation);
        }
        err
    });
    // 成否によらず、次の `/ai/snapshot` で取り直させる（待機の間にページが変わりうるため。`AISNAP-18`・TASK-129・MS-11。
    // 設計 §14.6）。
    state.sync_tracker().clear();
    out?;
    // 待機の間に新しい navigate が始まっていたら（navigate はこの排他を取らない）、成立したのは古いページの条件。
    // 無効になった世代で成功を返さない。
    if nav.latest_with_generation().map(|(g, _)| g) != Some(generation) {
        return Err(WaitError::Stale);
    }
    Ok(WaitOk { generation })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AISNAP-18`: 本文の検査。条件は `type` で選び、文字列は 1〜1024 バイト、上限時間は 1〜60,000 ms で必須。
    #[test]
    fn aisnap18_parse_body() {
        let p = |s: &str| parse(s.as_bytes());
        assert_eq!(
            p(
                r##"{"generation":2,"condition":{"type":"selector_present","selector":"#a"},"timeout_ms":500}"##
            ),
            Ok(Request {
                generation: 2,
                condition: WaitCondition::SelectorPresent("#a".into()),
                timeout_ms: 500
            })
        );
        assert_eq!(
            p(r#"{"generation":1,"condition":{"type":"load_complete"},"timeout_ms":60000}"#),
            Ok(Request {
                generation: 1,
                condition: WaitCondition::LoadComplete,
                timeout_ms: 60_000
            })
        );
        assert_eq!(
            p(r#"{"generation":1,"condition":{"type":"text_present","text":"done"},"timeout_ms":1}"#)
                .map(|r| r.condition),
            Ok(WaitCondition::TextPresent("done".into()))
        );
        assert_eq!(
            p(r#"{"generation":1,"condition":{"type":"selector_absent","selector":".x"},"timeout_ms":1}"#)
                .map(|r| r.condition),
            Ok(WaitCondition::SelectorAbsent(".x".into()))
        );
        let at_limit = format!("a{}", "あ".repeat(341));
        let with_text = |t: &str| {
            format!(
                r#"{{"generation":1,"condition":{{"type":"text_present","text":"{t}"}},"timeout_ms":1}}"#
            )
        };
        assert!(p(&with_text(&at_limit)).is_ok());
        for body in [
            with_text(&format!("{at_limit}a")),
            with_text(""),
            r#"{"generation":1,"condition":{"type":"load_complete"}}"#.into(),
            r#"{"generation":1,"condition":{"type":"load_complete"},"timeout_ms":0}"#.into(),
            r#"{"generation":1,"condition":{"type":"load_complete"},"timeout_ms":60001}"#.into(),
            r#"{"generation":1,"condition":{"type":"load_complete"},"timeout_ms":1.5}"#.into(),
            r#"{"generation":1,"condition":{"type":"load_complete"},"timeout_ms":4294967297}"#
                .into(),
            r#"{"condition":{"type":"load_complete"},"timeout_ms":1}"#.into(),
            r#"{"generation":-1,"condition":{"type":"load_complete"},"timeout_ms":1}"#.into(),
            r#"{"generation":1,"condition":{"type":"load_complete","selector":"a"},"timeout_ms":1}"#
                .into(),
            r#"{"generation":1,"condition":{"type":"selector_present","text":"a"},"timeout_ms":1}"#
                .into(),
            r#"{"generation":1,"condition":{"type":"ref_present","ref":"e1"},"timeout_ms":1}"#
                .into(),
            r#"{"generation":1,"condition":{"type":"load_complete"},"timeout_ms":1,"x":1}"#.into(),
            "[]".into(),
        ] {
            assert_eq!(p(&body), Err(WaitError::InvalidParams), "{body}");
        }
        assert_eq!(
            p(&"x".repeat(MAX_WAIT_BODY_BYTES + 1)),
            Err(WaitError::InvalidParams)
        );
    }

    /// `AISNAP-18`・`AISNAP-21`: 写像の表（`timeout`・状態は 409、`invalid_params` 400、失敗は 502）。
    #[test]
    fn aisnap18_status_table() {
        use WaitError::*;
        assert_eq!(InvalidParams.status_and_code(), (400, "invalid_params"));
        assert_eq!(Timeout.status_and_code(), (409, "timeout"));
        assert_eq!(Failed.status_and_code(), (502, "wait_failed"));
        for e in [
            Busy,
            Unsupported,
            NoNavigation,
            NotInteractive,
            Stale,
            RenderingDisabled,
            SessionLost,
            Unavailable,
        ] {
            assert_eq!(e.status_and_code().0, 409, "{e:?}");
        }
    }
}

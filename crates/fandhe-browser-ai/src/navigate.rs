//! `POST /ai/navigate` の本体（TASK-127・`AISNAP-16`・`AISNAP-21`）。
//!
//! HTTP の受け口（Host・Origin・Content-Type の検査と応答）は [`crate::api`]。本モジュールは本文の検査と、
//! 操作モードの読み込み（core の [`AppState::load_interactive`] → 検査 → [`AppState::commit_interactive`]）を行う。
//! ブロッキングするため、呼び出し元はスレッドへ移して呼ぶ（[`crate::blocking`]）。
//!
//! # 2026-11-19 のオーナー判断（TASK-127・#1616）
//!
//! - 今回は `mode: "interactive"` だけを入れる。`mode: "static"` と省略時は [`NavigateError::NotImplemented`]
//!   （501）。静的モードの取得とページ JS の処理は cdp crate の `Page.navigate` の経路にあり、ai から呼べない
//!   （ai は cdp に依存しない）。core へ移すのは別 issue
//! - ホストが主フレームの `navigator.webdriver` の確認で断った場合（SEC-5・#1609）は、`interactive_unavailable`
//!   （409）で返し、message で区別する
//! - 操作モードは、設定（テストの入口 `allow_interactive_for_tests`。設定キーは TASK-131）で許されていて、
//!   ホストの実際の隔離状態が `Isolated` で、内部アドレスを許さない設定のときだけ始める。満たさなければ
//!   `interactive_unavailable` で、静的モードへ黙って落とさない
//!
//! # 既知の制約（TASK-130 へ引き継ぐ）
//!
//! - セッション付きの CDP の `DOM.*`（Playwright・Puppeteer の通常の経路）は target ごとのナビゲーション状態を
//!   読むため、`/ai/navigate` の結果は見えない（target の前のページが返る）。CDP がこの結果を読むのは
//!   セッション無しの経路だけ（2026-11-19 のオーナー判断で、既知の制約として記録する）

use std::fmt;

use fandhe_browser_core::interactive::{MAX_INTERACTIVE_URL_BYTES, build_interactive_document};
use fandhe_browser_core::render::{PageRef, RenderError};
use fandhe_browser_core::{
    AppState, InteractiveLoadError, NavigationGeneration, StateError, check_network_target_url,
};
use serde_json::Value;

use crate::interactive::SyncTracker;

/// 本文の上限（バイト）。URL の上限（2048）に余裕を足したもの。
pub const MAX_NAVIGATE_BODY_BYTES: usize = 8 * 1024;

/// 読み込みの成功結果（`AISNAP-16`「結果（最終 URL・状態・モード）を返す」）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NavigateOk {
    url: String,
    generation: NavigationGeneration,
}

impl NavigateOk {
    /// 最終 URL（読み込みと同時に取った DOM の URL。保存する文書・`/ai/snapshot` と同じ）。
    pub fn url(&self) -> &str {
        &self.url
    }
    /// ナビゲーション世代（`/ai/snapshot` の `generation` と同じ値）。
    pub fn generation(&self) -> NavigationGeneration {
        self.generation
    }
}

/// `POST /ai/navigate` の失敗（`AISNAP-16`・`AISNAP-21`）。[`Self::status_and_code`] が HTTP へ写す。
/// `Display` は固定の英文で、URL などの入力値を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NavigateError {
    /// 本文が JSON でない・未知のキー・型の違い・URL の不正（scheme・内部アドレス・長さ）。
    InvalidParams,
    /// `mode` が `static` か省略（2026-11-19 のオーナー判断で当面は未実装）。
    NotImplemented,
    /// 操作モードを始められない（設定で許されていない・隔離が成立しない・内部アドレスを許す設定・
    /// ホストが未対応）。
    Unavailable,
    /// ページが操作モードの条件を満たさない（主フレームの `navigator.webdriver` が `true` でない。SEC-5）。
    PageUnsupported,
    /// 読み込みの途中で新しい navigate が始まった。
    Stale,
    /// 描画層が無効。
    RenderingDisabled,
    /// 読み込みの直後にホストが失われた。
    SessionLost,
    /// 読み込み・DOM の取得・検査に失敗した。
    LoadFailed,
}

impl NavigateError {
    /// HTTP ステータスとエラーコード（2026-11-19 のオーナー判断の表）。
    pub fn status_and_code(self) -> (u16, &'static str) {
        match self {
            Self::InvalidParams => (400, "invalid_params"),
            Self::NotImplemented => (501, "not_implemented"),
            Self::Unavailable | Self::PageUnsupported => (409, "interactive_unavailable"),
            Self::Stale => (409, "stale"),
            Self::RenderingDisabled => (409, "rendering_disabled"),
            Self::SessionLost => (409, "session_lost"),
            Self::LoadFailed => (502, "load_failed"),
        }
    }
}

impl fmt::Display for NavigateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidParams => "invalid navigate parameters",
            Self::NotImplemented => "static mode navigation is not implemented yet",
            Self::Unavailable => "interactive mode is unavailable",
            Self::PageUnsupported => "page does not meet interactive mode requirements",
            Self::Stale => "navigation superseded by a newer navigation",
            Self::RenderingDisabled => "rendering is disabled",
            Self::SessionLost => "rendering host session was lost",
            Self::LoadFailed => "failed to load page in interactive mode",
        })
    }
}

impl std::error::Error for NavigateError {}

/// 本文を検査した結果（読み込む URL）。
#[derive(Debug, PartialEq, Eq)]
struct Request {
    url: String,
}

/// 本文を検査する（副作用なし）。`{"url": <文字列>, "mode": "static" | "interactive"}`。未知のキーは拒否する。
fn parse(body: &[u8]) -> Result<Request, NavigateError> {
    if body.len() > MAX_NAVIGATE_BODY_BYTES {
        return Err(NavigateError::InvalidParams);
    }
    let v: Value = serde_json::from_slice(body).map_err(|_| NavigateError::InvalidParams)?;
    let obj = v.as_object().ok_or(NavigateError::InvalidParams)?;
    if obj.keys().any(|k| k != "url" && k != "mode") {
        return Err(NavigateError::InvalidParams);
    }
    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .ok_or(NavigateError::InvalidParams)?;
    match obj.get("mode") {
        None => return Err(NavigateError::NotImplemented),
        Some(Value::String(m)) if m == "static" => return Err(NavigateError::NotImplemented),
        Some(Value::String(m)) if m == "interactive" => {}
        Some(_) => return Err(NavigateError::InvalidParams),
    }
    if url.len() > MAX_INTERACTIVE_URL_BYTES {
        return Err(NavigateError::InvalidParams);
    }
    // Page.navigate と同じ検査（scheme・IP リテラル・`localhost`。SSRF）。操作モードは内部アドレスを許す
    // 設定では始めないため、常に許さない側で検査する（`AISNAP-16`）。
    let url = check_network_target_url(url, false).map_err(|_| NavigateError::InvalidParams)?;
    Ok(Request { url })
}

/// `POST /ai/navigate` を処理する（ブロッキングする。呼び出し元はスレッドへ移す）。
///
/// 検査 → 許可の確認 → 世代の払い出し → 読み込みと DOM の取得 → 検査 → 保存。世代を払い出す前の失敗では
/// 状態を変えない。払い出した後の失敗では、確定結果は無いまま（CDP の `Page.navigate` の失敗と同じ）。
pub fn navigate(
    app: &AppState,
    tracker: &SyncTracker,
    body: &[u8],
) -> Result<NavigateOk, NavigateError> {
    let request = parse(body)?;
    if !app.interactive_allowed() {
        return Err(NavigateError::Unavailable);
    }
    let nav = app.navigation();
    // 操作モード同士の navigate はホストを使い続ける（止めない開始。失敗したら下で止める。#1638）。
    let generation = nav
        .begin_interactive_navigation()
        .map_err(|_| NavigateError::LoadFailed)?;
    // 前のページの JS 実行・評価コンテキストを打ち切る（CDP の `Page.navigate` と同じ。`JS-4`・`JS-6`）。
    // 評価コンテキストを失効できなければ、CDP と同じく読み込まずに失敗にする（fail-closed）。
    app.page_scripts().begin_generation(nav.scope(), generation);
    app.script_contexts()
        .begin_generation(nav.scope(), generation)
        .map_err(|_| NavigateError::LoadFailed)?;
    // 読み込みの途中・後の失敗では、操作モードのホストを残さない（TASK-130 の指摘 S-1・`AISNAP-16`・`AISNAP-20`・MS-11・#1638）。
    let release = || app.release_interactive_if_current(generation);
    let (_load, snapshot) = app
        .load_interactive(generation, &PageRef::new(&request.url))
        .inspect_err(|_| release())
        .map_err(|e| match e {
            InteractiveLoadError::NotAllowed => NavigateError::Unavailable,
            InteractiveLoadError::Superseded => NavigateError::Stale,
            InteractiveLoadError::Render(r) => from_render(&r),
            _ => NavigateError::LoadFailed,
        })?;
    let sync = build_interactive_document(snapshot).map_err(|_| {
        release();
        NavigateError::LoadFailed
    })?;
    // 応答の URL は保存する文書と同じ取得時の URL にする（読み込みから取得までにページが自分で
    // 遷移しても `/ai/snapshot` と食い違わない）。保存後に共有状態を読み直すと別の navigate と競合する。
    let url = sync.url().to_owned();
    app.commit_interactive(generation, sync)
        .inspect_err(|_| release())
        .map_err(|e| match e {
            StateError::Superseded { .. } => NavigateError::Stale,
            StateError::InteractiveNotAllowed => NavigateError::Unavailable,
            _ => NavigateError::LoadFailed,
        })?;
    tracker.record(generation);
    Ok(NavigateOk { url, generation })
}

fn from_render(e: &RenderError) -> NavigateError {
    match e {
        RenderError::Unsupported => NavigateError::PageUnsupported,
        RenderError::InteractiveUnavailable => NavigateError::Unavailable,
        RenderError::RenderingDisabled => NavigateError::RenderingDisabled,
        RenderError::SessionLost => NavigateError::SessionLost,
        _ => NavigateError::LoadFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AISNAP-16`・2026-11-19 の判断: 本文の検査。`mode` の省略・`static` は未実装（501）、内部アドレス・
    /// 不正な scheme・未知のキー・長すぎる URL は `invalid_params`。
    #[test]
    fn aisnap16_parse_body() {
        let p = |s: &str| parse(s.as_bytes());
        assert_eq!(
            p(r#"{"url":"https://a.example/","mode":"interactive"}"#),
            Ok(Request {
                url: "https://a.example/".into()
            })
        );
        assert_eq!(
            p(r#"{"url":"https://a.example/"}"#),
            Err(NavigateError::NotImplemented)
        );
        assert_eq!(
            p(r#"{"url":"https://a.example/","mode":"static"}"#),
            Err(NavigateError::NotImplemented)
        );
        for bad in [
            r#"{"url":"https://a.example/","mode":"x"}"#,
            r#"{"url":"https://a.example/","mode":"interactive","x":1}"#,
            r#"{"mode":"interactive"}"#,
            r#"{"url":1,"mode":"interactive"}"#,
            r#"{"url":"file:///etc/passwd","mode":"interactive"}"#,
            r#"{"url":"http://127.0.0.1/","mode":"interactive"}"#,
            r#"{"url":"http://localhost/","mode":"interactive"}"#,
            "[]",
            "not json",
        ] {
            assert_eq!(p(bad), Err(NavigateError::InvalidParams), "{bad}");
        }
        let long = format!(
            r#"{{"url":"https://a.example/{}","mode":"interactive"}}"#,
            "a".repeat(MAX_INTERACTIVE_URL_BYTES)
        );
        assert_eq!(p(&long), Err(NavigateError::InvalidParams));
    }

    /// 2026-11-19 の判断の HTTP の表。
    #[test]
    fn aisnap21_status_table() {
        use NavigateError::*;
        let got: Vec<_> = [
            InvalidParams,
            NotImplemented,
            Unavailable,
            PageUnsupported,
            Stale,
            RenderingDisabled,
            SessionLost,
            LoadFailed,
        ]
        .into_iter()
        .map(NavigateError::status_and_code)
        .collect();
        assert_eq!(
            got,
            vec![
                (400, "invalid_params"),
                (501, "not_implemented"),
                (409, "interactive_unavailable"),
                (409, "interactive_unavailable"),
                (409, "stale"),
                (409, "rendering_disabled"),
                (409, "session_lost"),
                (502, "load_failed"),
            ]
        );
        assert_eq!(
            PageUnsupported.to_string(),
            "page does not meet interactive mode requirements"
        );
    }
}

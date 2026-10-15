//! 要素可視性判定のスクリプト生成と結果解釈（Servo の WebView に触れない純粋な部分）。
//!
//! 役割: `page::ServoHandler::visibility` が現在ページで JS を評価する前後の処理を担う。評価前は
//! セレクタを検証して固定スクリプトへ JSON 文字列リテラルとして埋め込み、評価後は返ってきた
//! `servo::JSValue` を許可リストの理由だけに絞って [`VisibilityResult`] へ変換する
//! （`RENDER-3`・`SEC-2`・TASK-38.4・#233。設計 §2.2・§2.4・§2.5）。
//!
//! 取得手段の確認結果: Servo 0.6.0 の `WebView` に要素矩形・computed style を直接返す埋め込み API は
//! 無く（アクセシビリティツリーはセレクタで引けない）、`WebView::evaluate_javascript` を採用する。
//! スクリプトは読み取り専用（`querySelector`・`getComputedStyle`・`getBoundingClientRect`・
//! `innerWidth/innerHeight` のみ）で、DOM 変更・スクロール・フォーカスをしない。
//!
//! 可視の定義: 現在のスクロール位置で固定ビューポート（1280x800）と交差する矩形を持ち、
//! `display:none`・`visibility:hidden|collapse` でない要素。Playwright はビューポート外を可視とするが、
//! 本ホストのスクリーンショットはビューポートのみでスクロールもしないため、描画されるかどうかに揃える。
//!
//! 将来仕様: `opacity:0` や他要素による被覆はスキーマの既知値に無いため判定しない（可視扱い。
//! `REPAIR-3`）。ページが `getComputedStyle` 等を上書きすると判定は偽れるが、応答に載るのは
//! 許可リストの理由と真偽値だけで、ページ由来の文字列は流れない。

use std::time::Duration;

use servo::JSValue;

use crate::server::{ErrorKind, MAX_SELECTOR_BYTES, QUERY_TIMEOUT_MS, VisibilityResult};

/// 要素クエリの待機上限（5 秒。`RENDER-4`）。
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_millis(QUERY_TIMEOUT_MS);

/// スキーマ `VisibilityResult.reason` の既知値。戻り値の `&'static str` はここからしか作らない。
const KNOWN_REASONS: [&str; 4] = [
    "display_none",
    "visibility_hidden",
    "zero_size",
    "offscreen",
];

/// セレクタを引数に取る関数式。セレクタは呼び出し側が JSON 文字列リテラルとして後ろに付ける。
const VISIBILITY_FN: &str = r#"(function(sel){
var el;
try { el = document.querySelector(sel); } catch (e) { return {error: "invalid_selector"}; }
if (el === null) { return {found: false}; }
for (var e = el; e !== null; e = e.parentElement) {
  if (getComputedStyle(e).display === "none") { return {found: true, visible: false, reason: "display_none"}; }
}
var v = getComputedStyle(el).visibility;
if (v === "hidden" || v === "collapse") { return {found: true, visible: false, reason: "visibility_hidden"}; }
var r = el.getBoundingClientRect();
if (!(r.width > 0) || !(r.height > 0)) { return {found: true, visible: false, reason: "zero_size"}; }
if (r.right <= 0 || r.bottom <= 0 || r.left >= window.innerWidth || r.top >= window.innerHeight) {
  return {found: true, visible: false, reason: "offscreen"};
}
return {found: true, visible: true};
})"#;

/// 可視性判定の失敗。`server::ErrorKind` への対応は [`LayoutError::kind`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum LayoutError {
    /// セレクタの長さ・文字種の違反、またはセレクタの構文エラー。
    InvalidSelector,
    /// セレクタに一致する要素が無い。
    NotFound,
    /// スクリプトの結果が想定した形でない。
    MalformedResult,
}

impl LayoutError {
    pub(crate) fn kind(self) -> ErrorKind {
        match self {
            Self::InvalidSelector => ErrorKind::InvalidParams,
            Self::NotFound => ErrorKind::NotFound,
            Self::MalformedResult => ErrorKind::Internal,
        }
    }
}

/// セレクタの長さ（1〜1024 バイト）と文字種を検証する。入口（`request.rs`）は長さだけ見るため、
/// ホスト側で制御文字（C0・C1）と U+2028・U+2029（JSON リテラルでエスケープされない）も拒否する。
pub(crate) fn validate_selector(selector: &str) -> Result<(), LayoutError> {
    if selector.is_empty() || selector.len() > MAX_SELECTOR_BYTES {
        return Err(LayoutError::InvalidSelector);
    }
    if selector
        .chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return Err(LayoutError::InvalidSelector);
    }
    Ok(())
}

/// 検証済みセレクタを JSON 文字列リテラルとして固定の関数式へ渡すスクリプトを組む。
/// 生のセレクタを連結しない（インジェクション対策。`SEC-2`）。
pub(crate) fn visibility_script(selector: &str) -> Result<String, LayoutError> {
    validate_selector(selector)?;
    let literal = serde_json::to_string(selector).map_err(|_| LayoutError::InvalidSelector)?;
    Ok(format!("{VISIBILITY_FN}({literal})"))
}

/// スクリプトの結果を厳密に解釈する。未知のキーは無視し、理由は許可リストの定数だけを返す。
pub(crate) fn parse_visibility(value: JSValue) -> Result<VisibilityResult, LayoutError> {
    let JSValue::Object(map) = value else {
        return Err(LayoutError::MalformedResult);
    };
    if let Some(err) = map.get("error") {
        return match err {
            JSValue::String(s) if s == "invalid_selector" => Err(LayoutError::InvalidSelector),
            _ => Err(LayoutError::MalformedResult),
        };
    }
    match map.get("found") {
        Some(JSValue::Boolean(false)) => return Err(LayoutError::NotFound),
        Some(JSValue::Boolean(true)) => {}
        _ => return Err(LayoutError::MalformedResult),
    }
    match map.get("visible") {
        Some(JSValue::Boolean(true)) => Ok(VisibilityResult {
            visible: true,
            reason: None,
        }),
        Some(JSValue::Boolean(false)) => {
            let Some(JSValue::String(r)) = map.get("reason") else {
                return Err(LayoutError::MalformedResult);
            };
            let reason = KNOWN_REASONS
                .iter()
                .find(|k| **k == r.as_str())
                .ok_or(LayoutError::MalformedResult)?;
            Ok(VisibilityResult {
                visible: false,
                reason: Some(reason),
            })
        }
        _ => Err(LayoutError::MalformedResult),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn obj(pairs: &[(&str, JSValue)]) -> JSValue {
        JSValue::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<HashMap<_, _>>(),
        )
    }

    fn s(v: &str) -> JSValue {
        JSValue::String(v.to_string())
    }

    /// RENDER-3: 長さと文字種の境界。
    #[test]
    fn validate_selector_enforces_length_and_charset() {
        assert_eq!(validate_selector(""), Err(LayoutError::InvalidSelector));
        assert_eq!(
            validate_selector(&"a".repeat(1025)),
            Err(LayoutError::InvalidSelector)
        );
        assert_eq!(validate_selector(&"a".repeat(1024)), Ok(()));
        for bad in [
            "a\0",
            "a\n",
            "a\t",
            "a\u{7f}",
            "a\u{85}",
            "a\u{2028}",
            "a\u{2029}",
        ] {
            assert_eq!(
                validate_selector(bad),
                Err(LayoutError::InvalidSelector),
                "{bad:?}"
            );
        }
        for ok in ["div[data-k=\"x'y\"]", "#日本語", "a > b:nth-child(2)"] {
            assert_eq!(validate_selector(ok), Ok(()), "{ok:?}");
        }
    }

    /// SEC-2: 破壊的なセレクタでも固定部 + JSON リテラル以外を含まない。
    #[test]
    fn visibility_script_embeds_selector_as_json_literal_only() {
        for sel in ["\"); throw 1; (\"", "\\", "\"", "a'b\"c"] {
            let script = visibility_script(sel).expect("script");
            let literal = serde_json::to_string(sel).expect("json");
            assert_eq!(script, format!("{VISIBILITY_FN}({literal})"));
            let tail = script.strip_prefix(VISIBILITY_FN).expect("prefix");
            let inner = tail
                .strip_prefix('(')
                .and_then(|t| t.strip_suffix(')'))
                .expect("parens");
            assert_eq!(serde_json::from_str::<String>(inner).expect("literal"), sel);
        }
        assert_eq!(visibility_script("a\nb"), Err(LayoutError::InvalidSelector));
    }

    #[test]
    fn parse_visibility_accepts_known_shapes() {
        let r = parse_visibility(obj(&[
            ("found", JSValue::Boolean(true)),
            ("visible", JSValue::Boolean(true)),
        ]))
        .expect("visible");
        assert!(r.visible);
        assert_eq!(r.reason, None);
        for reason in KNOWN_REASONS {
            let r = parse_visibility(obj(&[
                ("found", JSValue::Boolean(true)),
                ("visible", JSValue::Boolean(false)),
                ("reason", s(reason)),
            ]))
            .expect("hidden");
            assert!(!r.visible);
            assert_eq!(r.reason, Some(reason));
        }
        assert_eq!(
            parse_visibility(obj(&[("found", JSValue::Boolean(false))])).err(),
            Some(LayoutError::NotFound)
        );
        assert_eq!(
            parse_visibility(obj(&[("error", s("invalid_selector"))])).err(),
            Some(LayoutError::InvalidSelector)
        );
    }

    #[test]
    fn parse_visibility_rejects_malformed_results() {
        let hidden = |extra: &[(&str, JSValue)]| {
            let mut v = vec![
                ("found", JSValue::Boolean(true)),
                ("visible", JSValue::Boolean(false)),
            ];
            v.extend_from_slice(extra);
            obj(&v)
        };
        let cases = [
            JSValue::Null,
            JSValue::Boolean(true),
            obj(&[]),
            obj(&[("error", s("other"))]),
            obj(&[("found", s("yes"))]),
            obj(&[("found", JSValue::Boolean(true)), ("visible", s("true"))]),
            hidden(&[]),
            hidden(&[("reason", s("secret page text"))]),
            hidden(&[("reason", JSValue::Boolean(true))]),
        ];
        for c in cases {
            assert_eq!(
                parse_visibility(c.clone()).err(),
                Some(LayoutError::MalformedResult),
                "{c:?}"
            );
        }
    }

    #[test]
    fn layout_error_maps_to_error_kind() {
        assert_eq!(
            LayoutError::InvalidSelector.kind(),
            ErrorKind::InvalidParams
        );
        assert_eq!(LayoutError::NotFound.kind(), ErrorKind::NotFound);
        assert_eq!(LayoutError::MalformedResult.kind(), ErrorKind::Internal);
    }

    #[test]
    fn query_timeout_is_five_seconds() {
        assert_eq!(QUERY_TIMEOUT, Duration::from_secs(5));
        assert_eq!(QUERY_TIMEOUT_MS, 5_000);
    }
}

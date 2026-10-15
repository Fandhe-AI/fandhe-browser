//! 要素の可視性判定と境界ボックス取得のスクリプト生成・結果解釈（Servo の WebView に触れない純粋な部分）。
//!
//! 役割: `page::ServoHandler::visibility`・`bounding_box` が現在ページで JS を評価する前後の処理を担う。
//! 評価前はセレクタを検証して固定スクリプトへ JSON 文字列リテラルとして埋め込み、評価後は返ってきた
//! `servo::JSValue` を [`VisibilityResult`]（許可リストの理由だけ）・[`BoundingBoxResult`]（検証済みの
//! 有限な数値 4 つだけ）へ変換する（`RENDER-3`・`SEC-2`・TASK-38.4・#233、TASK-38.5・#234。設計 §2.2・§2.4・§2.5）。
//! 境界ボックスも #233 の結論（JS 評価）に合わせ、同じ検証・埋め込み規則と 5 秒のタイムアウトを使う。
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
//! 境界ボックスの座標系: `getBoundingClientRect()` のビューポート相対（CSS px）。ホストはスクロールせず
//! ビューポートも固定（1280x800・DPR 1 前提）なので、スクリーンショットのピクセル座標と一致する。
//! ビューポート外の要素は負値や範囲外の座標をそのまま返す。`display:none` など描画されない要素は
//! DOMRect の仕様どおり (0,0,0,0) を返す（プロトコルに null の結果が無く、`not_found` と偽らないため。
//! 区別が必要な呼び出し元は `visibility` を使う。`REPAIR-3`）。
//!
//! 将来仕様: `opacity:0` や他要素による被覆はスキーマの既知値に無いため判定しない（可視扱い。
//! `REPAIR-3`）。ページが `getComputedStyle` 等を上書きすると判定は偽れるが、応答に載るのは
//! 許可リストの理由と真偽値だけで、ページ由来の文字列は流れない。

use std::collections::HashMap;
use std::time::Duration;

use servo::JSValue;

use crate::server::{
    BoundingBoxResult, ErrorKind, MAX_SELECTOR_BYTES, QUERY_TIMEOUT_MS, VisibilityResult,
};

/// 要素クエリの待機上限（5 秒。`RENDER-4`）。
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_millis(QUERY_TIMEOUT_MS);

/// スキーマ `VisibilityResult.reason` の既知値。戻り値の `&'static str` はここからしか作らない。
const KNOWN_REASONS: [&str; 4] = [
    "display_none",
    "visibility_hidden",
    "zero_size",
    "offscreen",
];

/// 関数式の共通の前半（セレクタ解決）。可視性と境界ボックスで同一にし、挙動がずれないようにする。
/// 構文エラーは `{error}`、一致なしは `{found: false}` を返す。
macro_rules! selector_prelude {
    () => {
        r#"(function(sel){
var el;
try { el = document.querySelector(sel); } catch (e) { return {error: "invalid_selector"}; }
if (el === null) { return {found: false}; }
"#
    };
}

/// セレクタを引数に取る関数式。セレクタは呼び出し側が JSON 文字列リテラルとして後ろに付ける。
const VISIBILITY_FN: &str = concat!(
    selector_prelude!(),
    r#"for (var e = el; e !== null; e = e.parentElement) {
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
})"#
);

/// 境界ボックス取得の関数式。読み取り専用で、DOM 変更・スクロール・フォーカスをしない。
const BOUNDING_BOX_FN: &str = concat!(
    selector_prelude!(),
    r#"var r = el.getBoundingClientRect();
return {found: true, x: r.left, y: r.top, width: r.width, height: r.height};
})"#
);

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
fn selector_script(fn_src: &str, selector: &str) -> Result<String, LayoutError> {
    validate_selector(selector)?;
    let literal = serde_json::to_string(selector).map_err(|_| LayoutError::InvalidSelector)?;
    Ok(format!("{fn_src}({literal})"))
}

/// 可視性判定のスクリプトを組む（`RENDER-3`・TASK-38.4）。
pub(crate) fn visibility_script(selector: &str) -> Result<String, LayoutError> {
    selector_script(VISIBILITY_FN, selector)
}

/// 境界ボックス取得のスクリプトを組む（`RENDER-3`・TASK-38.5・#234）。
pub(crate) fn bounding_box_script(selector: &str) -> Result<String, LayoutError> {
    selector_script(BOUNDING_BOX_FN, selector)
}

/// 結果オブジェクトの `error`・`found` を解釈し、要素が見つかった場合だけ `Ok` を返す。
fn expect_found(map: &HashMap<String, JSValue>) -> Result<(), LayoutError> {
    if let Some(err) = map.get("error") {
        return match err {
            JSValue::String(s) if s == "invalid_selector" => Err(LayoutError::InvalidSelector),
            _ => Err(LayoutError::MalformedResult),
        };
    }
    match map.get("found") {
        Some(JSValue::Boolean(false)) => Err(LayoutError::NotFound),
        Some(JSValue::Boolean(true)) => Ok(()),
        _ => Err(LayoutError::MalformedResult),
    }
}

/// 有限な数値だけを取り出す（ページが `getBoundingClientRect` を上書きしても NaN・inf・非数値は通さない）。
fn finite_number(map: &HashMap<String, JSValue>, key: &str) -> Result<f64, LayoutError> {
    match map.get(key) {
        Some(JSValue::Number(n)) if n.is_finite() => Ok(*n),
        _ => Err(LayoutError::MalformedResult),
    }
}

/// 境界ボックスの結果を厳密に解釈する。width・height は 0 以上、`-0.0` は `0.0` に正規化する。
pub(crate) fn parse_bounding_box(value: JSValue) -> Result<BoundingBoxResult, LayoutError> {
    let JSValue::Object(map) = value else {
        return Err(LayoutError::MalformedResult);
    };
    expect_found(&map)?;
    let x = finite_number(&map, "x")?;
    let y = finite_number(&map, "y")?;
    let width = finite_number(&map, "width")?;
    let height = finite_number(&map, "height")?;
    if width < 0.0 || height < 0.0 {
        return Err(LayoutError::MalformedResult);
    }
    // `-0.0 + 0.0 == 0.0`。JSON に `-0.0` を出さない。
    Ok(BoundingBoxResult {
        x: x + 0.0,
        y: y + 0.0,
        width: width + 0.0,
        height: height + 0.0,
    })
}

/// スクリプトの結果を厳密に解釈する。未知のキーは無視し、理由は許可リストの定数だけを返す。
pub(crate) fn parse_visibility(value: JSValue) -> Result<VisibilityResult, LayoutError> {
    let JSValue::Object(map) = value else {
        return Err(LayoutError::MalformedResult);
    };
    expect_found(&map)?;
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

    /// RENDER-3（TASK-38.4・#233）: 既知の結果形状を受理する。
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

    /// RENDER-3（TASK-38.4・#233）: 不正な結果や未知 reason を MalformedResult で拒否する。
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

    /// RENDER-3（TASK-38.4・#233）: LayoutError を ErrorKind へ写像する。
    #[test]
    fn layout_error_maps_to_error_kind() {
        assert_eq!(
            LayoutError::InvalidSelector.kind(),
            ErrorKind::InvalidParams
        );
        assert_eq!(LayoutError::NotFound.kind(), ErrorKind::NotFound);
        assert_eq!(LayoutError::MalformedResult.kind(), ErrorKind::Internal);
    }

    /// RENDER-4（TASK-38.4・#233）: 要素クエリの待機上限が 5 秒である。
    #[test]
    fn query_timeout_is_five_seconds() {
        assert_eq!(QUERY_TIMEOUT, Duration::from_secs(5));
        assert_eq!(QUERY_TIMEOUT_MS, 5_000);
    }

    fn n(v: f64) -> JSValue {
        JSValue::Number(v)
    }

    fn bbox(x: JSValue, y: JSValue, w: JSValue, h: JSValue) -> JSValue {
        obj(&[
            ("found", JSValue::Boolean(true)),
            ("x", x),
            ("y", y),
            ("width", w),
            ("height", h),
        ])
    }

    /// SEC-2: 境界ボックスのスクリプトも固定部 + JSON リテラルだけで構成される。
    #[test]
    fn bounding_box_script_embeds_selector_as_json_literal_only() {
        for sel in ["\"); throw 1; (\"", "\\", "\"", "a'b\"c"] {
            let script = bounding_box_script(sel).expect("script");
            let literal = serde_json::to_string(sel).expect("json");
            assert_eq!(script, format!("{BOUNDING_BOX_FN}({literal})"));
        }
        assert_eq!(
            bounding_box_script("a\nb"),
            Err(LayoutError::InvalidSelector)
        );
        assert!(VISIBILITY_FN.starts_with(selector_prelude!()));
        assert!(BOUNDING_BOX_FN.starts_with(selector_prelude!()));
    }

    /// RENDER-3（TASK-38.5・#234）: 既知の結果形状を具体値で受理する。
    #[test]
    fn parse_bounding_box_accepts_known_shapes() {
        let r = parse_bounding_box(bbox(n(10.0), n(20.0), n(30.0), n(40.0))).expect("int");
        assert_eq!((r.x, r.y, r.width, r.height), (10.0, 20.0, 30.0, 40.0));
        let r = parse_bounding_box(bbox(n(5.5), n(-1000.0), n(12.5), n(3.75))).expect("frac");
        assert_eq!((r.x, r.y, r.width, r.height), (5.5, -1000.0, 12.5, 3.75));
        let r = parse_bounding_box(bbox(n(-0.0), n(-0.0), n(-0.0), n(0.0))).expect("zero");
        for v in [r.x, r.y, r.width, r.height] {
            assert_eq!(v, 0.0);
            assert!(v.is_sign_positive());
        }
        assert_eq!(
            parse_bounding_box(obj(&[("found", JSValue::Boolean(false))])).err(),
            Some(LayoutError::NotFound)
        );
        assert_eq!(
            parse_bounding_box(obj(&[("error", s("invalid_selector"))])).err(),
            Some(LayoutError::InvalidSelector)
        );
    }

    /// RENDER-3（TASK-38.5・#234）: 不正な結果は MalformedResult で拒否する。
    #[test]
    fn parse_bounding_box_rejects_malformed_results() {
        let ok = || n(1.0);
        let cases = [
            JSValue::Null,
            obj(&[]),
            obj(&[("found", s("yes"))]),
            obj(&[("error", s("other"))]),
            obj(&[("found", JSValue::Boolean(true))]),
            bbox(s("1"), ok(), ok(), ok()),
            bbox(ok(), JSValue::Null, ok(), ok()),
            bbox(n(f64::NAN), ok(), ok(), ok()),
            bbox(ok(), n(f64::INFINITY), ok(), ok()),
            bbox(ok(), ok(), n(f64::NEG_INFINITY), ok()),
            bbox(ok(), ok(), n(-1.0), ok()),
            bbox(ok(), ok(), ok(), n(-0.5)),
        ];
        for c in cases {
            assert_eq!(
                parse_bounding_box(c.clone()).err(),
                Some(LayoutError::MalformedResult),
                "{c:?}"
            );
        }
    }
}

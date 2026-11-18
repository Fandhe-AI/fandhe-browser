//! 操作モードの DOM の取得（`dom_snapshot`）の直列化スクリプトと結果の解釈（Servo の WebView に触れない部分）。
//!
//! 役割: `page::ServoHandler::dom_snapshot` が現在ページで評価する固定のスクリプトと、返ってきた
//! `servo::JSValue` を検証済みの [`Serialized`] へ変換する処理を担う（`RENDER-11`・TASK-122。
//! 設計 `docs/design/servo-host-design.md` §14.3・§14.4）。
//!
//! 直列化の規則（スクリプト側）:
//! - 主フレームの文書を前順に歩き、各要素の開始タグの先頭に `data-fandhe-n="<番号>"` を付ける。番号は
//!   スクリプトの整数のカウンタから作り、ページの DOM には書き込まない。ページ自身の同名の属性は出さない
//! - `input` の `value`・`checked`、`option` の `selected`、`textarea` の中身は現在値を書く。伏せる対象
//!   （`type=password`、`autocomplete` のトークンが `current-password`・`new-password`・`one-time-code`・
//!   `cc-*`）の値は書かず、数を返す（第 1 層。本体の第 2 層は TASK-125）。伏せる対象の `select` は配下の
//!   `option` の `selected` を書かない。`type=file` の値は書かない
//! - iframe の中身は含めない（子の文書は歩かない）。`srcdoc` と、解決後の scheme が http / https 以外の
//!   `src`（`object` は `data`、`embed` は `src`）は書かない（`data:`・`blob:` の中身を出さない。#1581 の
//!   引き継ぎ。相対 URL は文書の URL で解決して判定する）。shadow root の中身も含めない
//!   （開いた shadow root を持つ要素の数を返す）。`template` の中身も含めない
//! - 空要素・生テキスト要素・`template`・`textarea` の扱いは HTML 名前空間の要素だけに適用する（SVG・MathML の
//!   同名要素は通常の要素として子を書く）。`pre`・`textarea`・`listing` の中身が改行で始まるときは、再パースで
//!   1 つ消えるのを補うため改行を 1 つ足す（HTML の直列化の規則）
//! - HTML の UTF-16 の長さが上限（8 MiB）を超えた時点で打ち切り、`tooLarge` を返す（UTF-8 のバイト数は
//!   UTF-16 の長さ以上のため、超えた時点で上限超過が確定する）。上限内でも UTF-8 のバイト数は Rust 側で数え直す
//! - 番号を振った要素そのものを配列で返す。Servo はこれを `JSValue::Element`（要素ごとの ID）に変換し、
//!   ホストは番号の順に保持する（`act` が `window.webdriverElement(id)` で引き直す。TASK-126）
//!
//! スクリプトは即時関数で、ページのグローバルへ書き込まない。ページは prototype を書き換えて出力を
//! 変えられるため、結果は自己申告として扱う（本体の検査が前提。§14.3）。

use servo::JSValue;

use crate::server::{ErrorKind, MAX_DOM_SNAPSHOT_HTML_BYTES};

/// 直列化の関数式。引数は HTML の上限（UTF-16 の長さ）で、呼び出し側が整数から整形して渡す。
const SERIALIZE_FN: &str = r#"(function(LIMIT){
var out = [], len = 0, big = false, els = [], ifr = 0, shd = 0, red = 0, hideSel = false;
var XHTML = "http://www.w3.org/1999/xhtml";
var VOID = {area:1,base:1,br:1,col:1,embed:1,hr:1,img:1,input:1,link:1,meta:1,source:1,track:1,wbr:1,param:1,keygen:1,basefont:1,bgsound:1,frame:1};
var RAW = {style:1,script:1,xmp:1,iframe:1,noembed:1,noframes:1,plaintext:1,noscript:1};
var LF = {pre:1,textarea:1,listing:1};
var EMBED = {iframe:"src",frame:"src",object:"data",embed:"src"};
function push(s) { if (big) { return; } len += s.length; if (len > LIMIT) { big = true; return; } out.push(s); }
function escT(s) { return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/\u00a0/g, "&nbsp;"); }
function escA(s) { return s.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/\u00a0/g, "&nbsp;"); }
function attr(n, v) { push(" " + n + "=\"" + escA(v) + "\""); }
function redacted(el) {
  if (el.localName === "input" && String(el.type).toLowerCase() === "password") { return true; }
  var ac = String(el.getAttribute("autocomplete") || "").toLowerCase().split(/\s+/);
  for (var i = 0; i < ac.length; i++) {
    var t = ac[i];
    if (t === "current-password" || t === "new-password" || t === "one-time-code" || t.indexOf("cc-") === 0) { return true; }
  }
  return false;
}
function httpUrl(v) {
  try { var p = new URL(v, document.baseURI).protocol; return p === "http:" || p === "https:"; }
  catch (e) { return false; }
}
function element(el) {
  var name = el.localName, n = els.length, html = el.namespaceURI === XHTML;
  els.push(el);
  if (el.shadowRoot) { shd++; }
  var isInput = html && name === "input", isOption = html && name === "option", isSelect = html && name === "select";
  var isArea = html && name === "textarea", embed = html ? EMBED[name] : undefined;
  if (html && (name === "iframe" || name === "frame")) { ifr++; }
  var hide = (isInput || isArea || isSelect) && redacted(el);
  if (hide) { red++; }
  push("<" + name + " data-fandhe-n=\"" + n + "\"");
  var as = el.attributes;
  for (var i = 0; i < as.length; i++) {
    var a = as[i], an = a.name;
    if (an === "data-fandhe-n") { continue; }
    if (isInput && (an === "value" || an === "checked")) { continue; }
    if (isOption && an === "selected") { continue; }
    if (embed && (an === "srcdoc" || (an === embed && !httpUrl(a.value)))) { continue; }
    attr(an, a.value);
  }
  if (isInput) {
    var t = String(el.type).toLowerCase();
    if (t === "checkbox" || t === "radio") { if (el.checked) { push(" checked"); } }
    else if (t !== "file" && !hide) { attr("value", String(el.value)); }
  }
  if (isOption && el.selected && !hideSel) { push(" selected"); }
  push(">");
  if (html && VOID[name]) { return; }
  if (isArea) {
    var v = hide ? "" : String(el.value);
    if (v.charAt(0) === "\n") { push("\n"); }
    push(escT(v));
  } else if (html && RAW[name]) {
    var c = el.childNodes;
    for (var j = 0; j < c.length; j++) { if (c[j].nodeType === 3) { push(c[j].data); } }
  } else if (!(html && name === "template")) {
    var f = el.firstChild;
    if (html && LF[name] && f && f.nodeType === 3 && f.data.charAt(0) === "\n") { push("\n"); }
    var outer = hideSel;
    if (isSelect && hide) { hideSel = true; }
    children(el);
    hideSel = outer;
  }
  push("</" + name + ">");
}
function node(x) {
  if (big) { return; }
  switch (x.nodeType) {
    case 1: element(x); break;
    case 3: push(escT(x.data)); break;
    case 8: push("<!--" + x.data + "-->"); break;
    case 10: push("<!DOCTYPE " + x.name + ">"); break;
  }
}
function children(p) { var c = p.childNodes; for (var i = 0; i < c.length && !big; i++) { node(c[i]); } }
children(document);
if (big) { return {tooLarge: true}; }
return [out.join(""), els, ifr, shd, red];
})"#;

/// 直列化スクリプト（上限を整数から整形して渡す）。ページ由来の値を含まない固定の文字列。
pub(crate) fn serialize_script() -> String {
    format!("{SERIALIZE_FN}({MAX_DOM_SNAPSHOT_HTML_BYTES})")
}

/// 検証済みの直列化結果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Serialized {
    pub html: String,
    /// 番号の順の要素 ID（`JSValue::Element`）。長さが要素数。
    pub element_ids: Vec<String>,
    pub iframes: u64,
    pub shadow_roots: u64,
    pub redacted: u64,
}

/// 結果の解釈の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DomError {
    /// HTML が上限（8 MiB）を超えた。
    TooLarge,
    /// 結果が想定した形でない。
    Malformed,
}

impl DomError {
    pub(crate) fn kind(self) -> ErrorKind {
        match self {
            Self::TooLarge => ErrorKind::TooLarge,
            Self::Malformed => ErrorKind::Internal,
        }
    }
}

/// スクリプトの戻り値を検証して [`Serialized`] にする。UTF-8 のバイト数で上限を数え直す。
pub(crate) fn parse_serialized(value: JSValue) -> Result<Serialized, DomError> {
    let items = match value {
        JSValue::Array(items) => items,
        JSValue::Object(map) => {
            return Err(match map.get("tooLarge") {
                Some(JSValue::Boolean(true)) => DomError::TooLarge,
                _ => DomError::Malformed,
            });
        }
        _ => return Err(DomError::Malformed),
    };
    let mut it = items.into_iter();
    let (
        Some(JSValue::String(html)),
        Some(JSValue::Array(els)),
        Some(ifr),
        Some(shd),
        Some(red),
        None,
    ) = (
        it.next(),
        it.next(),
        it.next(),
        it.next(),
        it.next(),
        it.next(),
    )
    else {
        return Err(DomError::Malformed);
    };
    if html.len() > MAX_DOM_SNAPSHOT_HTML_BYTES {
        return Err(DomError::TooLarge);
    }
    let element_ids = els
        .into_iter()
        .map(|v| match v {
            JSValue::Element(id) => Ok(id),
            _ => Err(DomError::Malformed),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Serialized {
        html,
        element_ids,
        iframes: count(ifr)?,
        shadow_roots: count(shd)?,
        redacted: count(red)?,
    })
}

/// 0 以上の整数の数（f64）を `u64` にする。小数・負数・非有限は不正。
fn count(v: JSValue) -> Result<u64, DomError> {
    match v {
        JSValue::Number(n)
            if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= u32::MAX as f64 =>
        {
            Ok(n as u64)
        }
        _ => Err(DomError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn ok_value(html: &str, ids: &[&str]) -> JSValue {
        JSValue::Array(vec![
            JSValue::String(html.to_owned()),
            JSValue::Array(
                ids.iter()
                    .map(|i| JSValue::Element((*i).to_owned()))
                    .collect(),
            ),
            JSValue::Number(1.0),
            JSValue::Number(0.0),
            JSValue::Number(2.0),
        ])
    }

    /// RENDER-11: 正常な結果は要素 ID を番号の順に保ち、数を整数で返す。
    #[test]
    fn parses_well_formed_result() {
        let s =
            parse_serialized(ok_value("<html data-fandhe-n=\"0\"></html>", &["a", "b"])).unwrap();
        assert_eq!(s.html, "<html data-fandhe-n=\"0\"></html>");
        assert_eq!(s.element_ids, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!((s.iframes, s.shadow_roots, s.redacted), (1, 0, 2));
    }

    /// RENDER-11: 上限超過はスクリプトの打ち切りでも、UTF-8 で数え直した場合でも too_large。
    #[test]
    fn too_large_is_reported_without_truncation() {
        let mut m = HashMap::new();
        m.insert("tooLarge".to_owned(), JSValue::Boolean(true));
        assert_eq!(
            parse_serialized(JSValue::Object(m)),
            Err(DomError::TooLarge)
        );
        // UTF-16 では上限内でも UTF-8 では超える（3 バイトの文字）。
        let html = "あ".repeat(MAX_DOM_SNAPSHOT_HTML_BYTES / 3 + 1);
        assert_eq!(
            parse_serialized(ok_value(&html, &[])),
            Err(DomError::TooLarge)
        );
        assert_eq!(DomError::TooLarge.kind(), ErrorKind::TooLarge);
    }

    /// 想定外の形（要素でない ID・小数の数・余分な項目・別の型）は internal に倒す。
    #[test]
    fn malformed_results_are_rejected() {
        let mut bad_id = ok_value("", &[]);
        if let JSValue::Array(v) = &mut bad_id {
            v[1] = JSValue::Array(vec![JSValue::String("x".into())]);
        }
        let mut frac = ok_value("", &[]);
        if let JSValue::Array(v) = &mut frac {
            v[2] = JSValue::Number(1.5);
        }
        let mut extra = ok_value("", &[]);
        if let JSValue::Array(v) = &mut extra {
            v.push(JSValue::Null);
        }
        for v in [
            bad_id,
            frac,
            extra,
            JSValue::Null,
            JSValue::Object(HashMap::new()),
        ] {
            assert_eq!(parse_serialized(v), Err(DomError::Malformed));
        }
        assert_eq!(DomError::Malformed.kind(), ErrorKind::Internal);
    }

    /// スクリプトは固定で、上限は整数から整形して渡す（ページ由来の値を含まない。確定条件 8）。
    #[test]
    fn script_is_fixed_and_passes_integer_limit() {
        let s = serialize_script();
        assert!(s.ends_with(&format!("({MAX_DOM_SNAPSHOT_HTML_BYTES})")));
        assert!(s.starts_with("(function(LIMIT){"));
    }
}

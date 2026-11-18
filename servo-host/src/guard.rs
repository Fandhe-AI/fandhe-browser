//! 読み込み先の許可リスト検査（`RENDER-3`・`SEC-2`）。
//!
//! 役割: Servo の全ての読み込み（主フレーム・iframe・サブリソース・Worker）は、ハンドラ選択の前に
//! 埋め込み側の `load_web_resource` へ問い合わせられる。ここでは URL を許可リストで判定し、
//! 許可外は空の応答で終わらせる。`page` の `WebViewDelegate` と `engine` が設定する
//! `ServoDelegate`（WebView に紐づかない要求）の両方がこの判定を使う。
//!
//! 方針: 許可と判定したときだけ読み込みを素通し（drop）にする。drop は許可になるため、
//! それ以外の分岐は必ず拒否へ倒す（fail-closed）。拒否は `cancel()` ではなく空の 200 で終える
//! （`cancel()` だと iframe・主フレームが Complete に届かず撮影できない）。
//! 検査点に渡る URL は最初の URL で、リダイレクト先は見えない（リダイレクト先の scheme は Servo 側が別途 http(s) に限る）。

use std::rc::Rc;

use servo::{ServoDelegate, WebResourceLoad, WebResourceResponse};

/// URL 全体を受け取り、許可するか判定する。許可は `http`・`https`・`data`・`blob` と、
/// `about:blank`・`about:srcdoc` の完全一致だけ。未知の scheme・scheme なしは拒否する。
/// `&str` の純関数にして Servo 型なしで真理値表を検証する（`Url::scheme()` 版にしない理由）。
/// `blob:` は内側の URL の出所に関わらず許可する（blob の作成元は同じページのスクリプトで、リスク評価済み）。
/// `data:`・`blob:` を読み込みで許す根拠は RENDER-10（2026-11-18 の判断。#1581）。主フレームの遷移は
/// [`navigation_allowed`] が別に止める。
pub(crate) fn url_allowed(url: &str) -> bool {
    navigation_allowed(url)
        || url.split_once(':').is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("data") || scheme.eq_ignore_ascii_case("blob")
        })
}

/// ページから始まる遷移（`request_navigation`）を許すか判定する（`RENDER-10`・TASK-120）。
/// 許可は `http`・`https` と、`about:blank`・`about:srcdoc` の完全一致だけ。[`url_allowed`] と違い
/// `data:`・`blob:` は許さない（主フレームの `data:`・`blob:` への遷移を止める。#1581）。
///
/// 検査点の範囲（TASK-117 の試作。`docs/design/servo-interactive-poc.md`）: `request_navigation` は
/// 主フレームと iframe の script 起点の遷移（`location` 代入・フォーム送信・リンク）で呼ばれ、
/// どちらのフレームかは区別できない。iframe の `src` 属性による読み込みとリダイレクト先は通らず、
/// 前者は [`url_allowed`]（iframe・サブリソースの `data:`・`blob:` は読み込みを許し、操作と
/// DOM 取得の対象にしない。RENDER-10）、後者は Servo 内部の制限が受け持つ。
pub(crate) fn navigation_allowed(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" => true,
        "about" => url == "about:blank" || url == "about:srcdoc",
        _ => false,
    }
}

/// 読み込みを検査し、許可でなければ空の応答で終わらせる。
///
/// WebSocket は scheme が http(s) に書き換わって届くため URL では見分けられない。ハンドシェイクの
/// `Upgrade`・`Sec-WebSocket-Key` ヘッダ（ページのスクリプトは付けられない禁止ヘッダ）で見分けて拒否し、
/// egress 隔離ありで WebSocket が使えない挙動と揃える（`RENDER-3`）。
pub(crate) fn check(load: WebResourceLoad) {
    let req = load.request();
    let is_websocket =
        req.headers.contains_key("upgrade") || req.headers.contains_key("sec-websocket-key");
    if url_allowed(req.url.as_str()) && !is_websocket {
        return; // drop は許可
    }
    let url = req.url.clone();
    load.intercept(WebResourceResponse::new(url)).finish();
}

/// WebView に紐づかない読み込み（Worker 等）の検査用 delegate。
pub(crate) struct GuardDelegate;

impl GuardDelegate {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self)
    }
}

impl ServoDelegate for GuardDelegate {
    fn load_web_resource(&self, load: WebResourceLoad) {
        check(load);
    }
}

#[cfg(test)]
mod tests {
    use super::{navigation_allowed, url_allowed};

    /// RENDER-3・SEC-2: 許可リストの真理値表。
    #[test]
    fn url_allowlist_truth_table() {
        for (u, want) in [
            ("http://a/", true),
            ("https://a/", true),
            ("HTTP://a/", true),
            ("data:text/html,x", true),
            ("blob:http://a/x", true),
            ("about:blank", true),
            ("about:srcdoc", true),
            ("about:memory", false),
            ("about:blank#x", false),
            ("file:///x", false),
            ("FILE:///x", false),
            ("view-source:file:///x", false),
            ("chrome:allowcert", false),
            ("javascript:1", false),
            ("ftp://a/", false),
            ("resource://a", false),
            ("", false),
            ("x", false),
        ] {
            assert_eq!(url_allowed(u), want, "{u}");
        }
    }

    /// RENDER-10（TASK-120）: 遷移の許可は http(s) と `about:blank`・`about:srcdoc` だけで、
    /// 読み込みでは許す `data:`・`blob:` を許さない。
    #[test]
    fn navigation_allowlist_truth_table() {
        for (u, want) in [
            ("http://a/", true),
            ("https://a/", true),
            ("HTTPS://a/", true),
            ("about:blank", true),
            ("about:srcdoc", true),
            ("data:text/html,x", false),
            ("DATA:text/html,x", false),
            ("blob:http://a/x", false),
            ("about:blank#x", false),
            ("about:memory", false),
            ("file:///x", false),
            ("javascript:1", false),
            ("view-source:http://a/", false),
            ("ws://a/", false),
            ("", false),
            ("x", false),
        ] {
            assert_eq!(navigation_allowed(u), want, "{u}");
        }
    }
}

//! `urlParse` op の実体: WHATWG URL の解析を行う純粋関数（TASK-108・Issue #1144・ビヘイビア
//! `JS-5`・`JS-6`・`SEC-2`）。
//!
//! 呼び出し元は [`super`]（`dom_bridge` の `execute` の `urlParse` 分岐）で、shim の `url.js` が
//! `new URL(input, base)` / `URL.canParse` / setter の再解析に使う。解析は `reqwest::Url`
//! （url クレート。WHATWG 準拠）に任せ、ネットワークには出ない。ページの状態も変えず、診断も
//! 記録しない（URL はクエリにトークンを含み得る。`page_env` と同じ規則）。
//!
//! # 戻り値の形
//!
//! `JsValue` は配列を持てないため、11 個の構成要素を固定順
//! （href・origin・protocol・username・password・host・hostname・port・pathname・search・hash）に
//! `\n` で連結した 1 文字列で返す。区切りに `\n` を使える根拠: WHATWG の解析は入力から ASCII の
//! タブ・改行を取り除き、どの構成要素でも C0 制御文字をパーセントエンコードするため、直列化結果に
//! 生の `\n` は現れない。念のためどれかが `\n` を含めば失敗扱いにする（fail-closed）。
//!
//! # 入力長
//!
//! 入力長の上限検証は呼び出し側（`string_arg`）が解析より前に行う。この関数は検証済みの入力だけを受ける。

use reqwest::Url;

use super::{LocationMember, location_member};

/// 構成要素の個数（出力を `\n` で分割したときの要素数）。
#[cfg(test)]
pub(super) const COMPONENT_COUNT: usize = 11;

/// `input`（`base` があればそれを基準に解決）を解析し、構成要素を `\n` で連結して返す。
/// 構文エラー・base の解析失敗は `None`。
pub(super) fn parse_url_components(input: &str, base: Option<&str>) -> Option<String> {
    let url = match base {
        Some(b) => Url::parse(b).ok()?.join(input).ok()?,
        None => Url::parse(input).ok()?,
    };
    let parts = [
        location_member(Some(&url), LocationMember::Href),
        location_member(Some(&url), LocationMember::Origin),
        location_member(Some(&url), LocationMember::Protocol),
        url.username().to_owned(),
        url.password().unwrap_or("").to_owned(),
        location_member(Some(&url), LocationMember::Host),
        location_member(Some(&url), LocationMember::Hostname),
        location_member(Some(&url), LocationMember::Port),
        location_member(Some(&url), LocationMember::Pathname),
        location_member(Some(&url), LocationMember::Search),
        location_member(Some(&url), LocationMember::Hash),
    ];
    if parts.iter().any(|p| p.contains('\n')) {
        return None;
    }
    Some(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<&str> {
        s.split('\n').collect()
    }

    /// JS-5: 絶対 URL の 11 構成要素が固定順に並ぶ。
    #[test]
    fn js_5_absolute_url_components() {
        let out = parse_url_components("https://user:pw@example.com:8080/a/b?x=1#h", None)
            .expect("parses");
        assert_eq!(
            split(&out),
            vec![
                "https://user:pw@example.com:8080/a/b?x=1#h",
                "https://example.com:8080",
                "https:",
                "user",
                "pw",
                "example.com:8080",
                "example.com",
                "8080",
                "/a/b",
                "?x=1",
                "#h",
            ]
        );
    }

    /// JS-5: 相対 URL は base で解決し、既定ポートの port は空になる。
    #[test]
    fn js_5_relative_with_base_and_default_port() {
        let out = parse_url_components("../c?q", Some("https://example.com/a/b/")).expect("ok");
        assert_eq!(split(&out)[0], "https://example.com/a/c?q");
        let out = parse_url_components("https://example.com:443/", None).expect("ok");
        assert_eq!(split(&out)[5], "example.com");
        assert_eq!(split(&out)[7], "");
    }

    /// JS-5: 構文エラーと base の失敗は None。
    #[test]
    fn js_5_failures_are_none() {
        assert_eq!(parse_url_components("not a url", None), None);
        assert_eq!(parse_url_components("/x", Some("also bad")), None);
        assert_eq!(parse_url_components("x", Some("mailto:a@b")), None);
    }

    /// JS-6: タブ・改行は WHATWG が取り除くため、出力は常にちょうど 11 要素。
    #[test]
    fn js_6_control_characters_never_break_separator() {
        let out = parse_url_components("https://ex\nample.com/a\tb", None).expect("ok");
        assert_eq!(split(&out).len(), COMPONENT_COUNT);
        assert_eq!(split(&out)[0], "https://example.com/ab");
        let out = parse_url_components("https://a/p?q=\u{1}\u{2}", None).expect("ok");
        assert_eq!(split(&out).len(), COMPONENT_COUNT);
    }
}

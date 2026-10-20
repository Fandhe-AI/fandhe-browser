//! `isFocusable` op の実体: 要素が `focus()` でフォーカスできるかの簡略判定
//! （TASK-108・Issue #1185・ビヘイビア `JS-5`・`JS-6`・`REPAIR-3`）。
//!
//! 呼び出し元は [`super`]（`dom_bridge` の `execute` の `isFocusable` 分岐）で、shim の `dom.js` が
//! `Element.prototype.focus()` の可否と `document.activeElement` の再確認（切り離し・`disabled` 付与・
//! `tabindex` 除去の検出）に使う。`Document` を読むだけで状態を変えず、診断も記録しない。
//!
//! # 判定（HTML の「focusable area」の一部）
//!
//! 要素であり文書に接続していて、さらに次のいずれかに当たれば true。
//!
//! - HTML 名前空間の `button` / `select` / `textarea`（`disabled` なし）、`input`（`disabled` なし・
//!   `type` が `hidden` でない）、`a` / `area`（`href` あり）、`iframe`
//! - `contenteditable` が空文字・`true`・`plaintext-only`（ASCII 大文字小文字を区別しない）
//! - `tabindex` が HTML の「valid integer」として解釈できる（負値も可。`focus()` は負値でも成功する）
//!
//! disabled のフォームコントロールは `tabindex` があっても false。
//!
//! # 未判定（REPAIR-3）
//!
//! レンダリングに基づく判定（`display:none`・`hidden`・`inert`）、祖先の `fieldset[disabled]`、
//! `summary`、`controls` 付きの `audio` / `video`、SVG の `a[href]`、`autofocus`。将来は
//! レンダリング層と連動して判定する。

use crate::dom::{Document, HTML_NAMESPACE_URI, NodeId};

/// `node` がフォーカスできる要素なら true。範囲外・要素以外・切り離し済みは false。
pub(super) fn is_focusable(doc: &Document, node: NodeId) -> bool {
    if !doc.is_element(node) || !is_connected(doc, node) {
        return false;
    }
    let is_html = doc.namespace_url(node) == Some(HTML_NAMESPACE_URI);
    let name = doc.local_name(node).unwrap_or("");
    let disabled = doc.attribute(node, "disabled").is_some();
    if is_html {
        match name {
            "button" | "select" | "textarea" | "input" if disabled => return false,
            "button" | "select" | "textarea" | "iframe" => return true,
            "input" => {
                let hidden = doc
                    .attribute(node, "type")
                    .is_some_and(|t| t.eq_ignore_ascii_case("hidden"));
                return !hidden;
            }
            "a" | "area" if doc.attribute(node, "href").is_some() => return true,
            _ => {}
        }
    }
    let editable = doc.attribute(node, "contenteditable").is_some_and(|v| {
        v.is_empty() || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("plaintext-only")
    });
    editable
        || doc
            .attribute(node, "tabindex")
            .is_some_and(|v| parse_html_integer(v).is_some())
}

/// 文書のルートまで祖先を辿れるか（`ancestors` は `node_count` 歩で打ち切るので有限）。
fn is_connected(doc: &Document, node: NodeId) -> bool {
    let root = doc.root();
    node != root && doc.ancestors(node).any(|a| a == root)
}

/// HTML の「valid integer」の簡略解釈（先頭の ASCII 空白を飛ばし、任意の符号と 1 桁以上の数字を読む。
/// 数字列の後ろに続く文字は無視し、数字が無い・桁あふれは None）。
fn parse_html_integer(value: &str) -> Option<i64> {
    let trimmed = value.trim_start_matches([' ', '\t', '\n', '\u{000C}', '\r']);
    let (negative, digits) = match trimmed.as_bytes().first()? {
        b'-' => (true, trimmed.get(1..)?),
        b'+' => (false, trimmed.get(1..)?),
        _ => (false, trimmed),
    };
    let mut acc: i64 = 0;
    let mut seen = false;
    for b in digits.bytes() {
        if !b.is_ascii_digit() {
            break;
        }
        seen = true;
        acc = acc.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    if !seen {
        return None;
    }
    Some(if negative { -acc } else { acc })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{ParseOptions, parse_document};

    fn doc(body: &str) -> Document {
        parse_document(
            &format!("<html><head></head><body>{body}</body></html>"),
            &ParseOptions::default(),
        )
        .expect("テスト入力は必ず成功する")
        .document
    }

    fn by_id(d: &Document, id: &str) -> NodeId {
        (0..d.node_count())
            .map(NodeId::new)
            .find(|n| d.attribute(*n, "id") == Some(id))
            .expect("id が存在する")
    }

    /// JS-5: フォーカスできる要素の具体例。
    #[test]
    fn js_5_focusable_elements() {
        let d = doc(concat!(
            "<input id=a><button id=b></button><select id=c></select><textarea id=d></textarea>",
            "<a id=e href=x></a><area id=f href=x><iframe id=g></iframe>",
            "<div id=h tabindex=0></div><div id=i tabindex=-1></div><div id=j tabindex=' 3'></div>",
            "<div id=k contenteditable></div><div id=l contenteditable=TRUE></div>",
            "<div id=m contenteditable=plaintext-only></div>",
        ));
        for id in [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
        ] {
            assert!(is_focusable(&d, by_id(&d, id)), "{id}");
        }
    }

    /// JS-5: フォーカスできない要素・ノードの具体例。
    #[test]
    fn js_5_non_focusable_elements() {
        let d = doc(concat!(
            "<div id=a></div><a id=b></a><input id=c type=HIDDEN><button id=d disabled></button>",
            "<button id=e disabled tabindex=0></button><div id=f tabindex=abc></div>",
            "<div id=g tabindex=''></div><div id=h contenteditable=false></div>",
            "<input id=i disabled>",
        ));
        for id in ["a", "b", "c", "d", "e", "f", "g", "h", "i"] {
            assert!(!is_focusable(&d, by_id(&d, id)), "{id}");
        }
        assert!(!is_focusable(&d, d.root()));
        assert!(!is_focusable(&d, NodeId::new(usize::MAX / 2)));
    }

    /// JS-5: 切り離した要素とテキストノードは false。
    #[test]
    fn js_5_detached_and_text_are_not_focusable() {
        let mut d = doc("<p id=p>text</p>");
        let loose = d.create_element("input").expect("create");
        assert!(!is_focusable(&d, loose));
        let p = by_id(&d, "p");
        let text = d.first_child(p).expect("text");
        assert!(!is_focusable(&d, text));
    }

    /// JS-5: tabindex の整数解釈。
    #[test]
    fn js_5_parse_html_integer() {
        assert_eq!(parse_html_integer(" 3"), Some(3));
        assert_eq!(parse_html_integer("-1"), Some(-1));
        assert_eq!(parse_html_integer("+7x"), Some(7));
        assert_eq!(parse_html_integer(""), None);
        assert_eq!(parse_html_integer("abc"), None);
        assert_eq!(parse_html_integer("-"), None);
        assert_eq!(parse_html_integer("99999999999999999999"), None);
    }
}

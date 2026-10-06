//! `AISNAP-5` の分母となる「生 DOM シリアライズ」を作るモジュール
//! （TASK-14.5・Issue #96・`MS-2`）。
//!
//! 役割: PoC-4 `reduce.mjs` の `rawDomSerialize` の移植。`script` / `style` /
//! `noscript` / `svg` / `link` / `meta` を部分木ごと除去し、`body`（無ければルート
//! 要素）の outerHTML 相当の文字列を返す。呼び出し元は `huge_static.rs`
//! （巨大静的ページ単体の測定）で、TASK-23（`AISNAP-15`）も再利用する見込み。
//!
//! 測定専用の簡易シリアライザで core の公開 API ではない。jsdom の `outerHTML` との
//! 完全一致は保証しない（`REPAIR-3`）。未対応: `<template>` の template contents
//! （辿らない）、名前空間付き属性の接頭辞（local name のみ出力）。
//! 深いネストでスタックを溢れさせないよう、明示スタックで反復実装している。

use fandhe_browser_core::dom::{Document, NodeData, NodeId};

/// 部分木ごと除去する要素（PoC-4 `rawDomSerialize` と同一集合）。
const REMOVED_TAGS: [&str; 6] = ["script", "style", "noscript", "svg", "link", "meta"];

/// 終了タグを出さない void 要素。
const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

enum Step {
    Open(NodeId),
    Close(NodeId),
}

fn is_in(list: &[&str], name: &str) -> bool {
    list.iter().any(|t| t.eq_ignore_ascii_case(name))
}

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            _ => out.push(c),
        }
    }
}

/// 子要素のうち local name が `name` の最初のものを返す。
fn find_child(doc: &Document, parent: NodeId, name: &str) -> Option<NodeId> {
    doc.children(parent).find(|&c| {
        doc.local_name(c)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// `body`（無ければルート要素）を生 DOM 文字列へシリアライズする。どちらも無ければ空文字列。
pub fn serialize_raw_dom(doc: &Document) -> String {
    let mut out = String::new();
    let Some(html) = doc.children(doc.root()).find(|&c| doc.is_element(c)) else {
        return out;
    };
    let start = find_child(doc, html, "body").unwrap_or(html);
    let mut stack = vec![Step::Open(start)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Close(id) => {
                if let Some(name) = doc.local_name(id) {
                    out.push_str("</");
                    out.push_str(name);
                    out.push('>');
                }
            }
            Step::Open(id) => match doc.node_data(id) {
                Some(NodeData::Element { .. }) => {
                    let Some(name) = doc.local_name(id) else {
                        continue;
                    };
                    if is_in(&REMOVED_TAGS, name) {
                        continue;
                    }
                    out.push('<');
                    out.push_str(name);
                    for attr in doc.attributes(id) {
                        out.push(' ');
                        out.push_str(&attr.name.local);
                        out.push_str("=\"");
                        escape_attr(&attr.value, &mut out);
                        out.push('"');
                    }
                    out.push('>');
                    if is_in(&VOID_TAGS, name) {
                        continue;
                    }
                    stack.push(Step::Close(id));
                    let kids: Vec<NodeId> = doc.children(id).collect();
                    stack.extend(kids.into_iter().rev().map(Step::Open));
                }
                Some(NodeData::Text { contents }) => escape_text(contents, &mut out),
                Some(NodeData::Comment { contents }) => {
                    out.push_str("<!--");
                    out.push_str(contents);
                    out.push_str("-->");
                }
                _ => {}
            },
        }
    }
    out
}

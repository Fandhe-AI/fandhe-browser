//! REPAIR-5（TASK-6・MS-7）の破壊的変更検出ゲート。
//!
//! spec は「意図的な破壊的変更（オフバイワン・checkbox 判定反転・仕様後退）を
//! 注入すると `cargo test` が必ず失敗として検出する」ことを求める。成果物欄は
//! ルートの `tests/break_detection.rs` を挙げているが、ルートの `Cargo.toml`
//! は `[package]` を持たない仮想マニフェスト（workspace のみ）であり、
//! ルート直下の `tests/` は `cargo test --workspace` でコンパイル対象になら
//! ない。そのため本ファイルは `crates/fandhe-browser-core/tests/` に置き、
//! `cargo test --workspace`（ci.yml の 3 OS matrix）で確実に実行されるように
//! する。
//!
//! セクション構成（兄弟 Issue が並行して追記する前提。既存セクションの中身
//! には手を入れず、末尾に自分のセクションを追加すること）:
//! - TASK-6.1（本ファイル）: オフバイワン検出
//! - TASK-6.2（#331）: checkbox 判定反転検出
//! - TASK-6.3（#332）: 仕様後退検出

use fandhe_browser_core::{
    Document, Error, NodeId, ParseError, ParseOptions, parse_document, parse_document_bytes,
};

/// 既定の `ParseOptions` で HTML をパースする。テスト入力は固定のダミー
/// HTML であり外部入力ではないため、失敗時に `expect` で即座に落として良い。
fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

/// `root` の子孫から `local_name` に一致する最初の要素を探す。
fn find_by_local_name(doc: &Document, root: NodeId, local_name: &str) -> NodeId {
    doc.descendants(root)
        .find(|&id| doc.local_name(id) == Some(local_name))
        .unwrap_or_else(|| panic!("要素 {local_name} が見つからない"))
}

/// `parent` の直接の子の `text_content` を出現順に集める。
fn child_texts(doc: &Document, parent: NodeId) -> Vec<String> {
    doc.children(parent)
        .filter_map(|id| doc.text_content(id))
        .collect()
}

// ---- TASK-6.1: オフバイワン（REPAIR-5） ----
//
// 対象 API は `dom` モジュール（ビヘイビア CORE-1）の走査系と、
// `parse` モジュール（CORE-1）の入力サイズ上限検査。
// 本セクションのテストは、以下の境界を 1 つずらす変更を本番コードへ注入
// すると red になることを確認済み（PR 本文の「注入検証記録」参照）:
// - `Document::children` の `front` 初期値（0 → 1 で先頭の子が落ちる）
// - `parse_document`/`parse_document_bytes` の
//   `input.len() > options.max_input_bytes`（`>=` にすると上限ちょうどの
//   入力が誤って拒否される）

/// REPAIR-5・TASK-6（6.1）・CORE-1: `Document::children` が先頭・末尾を含む
/// 全ての子を欠落なく返す。`front` の初期値がオフバイワンでずれる
///（0 → 1 相当）と先頭要素 `"1"` が失われて検出される。
#[test]
fn repair_5_off_by_one_children_keeps_first_and_last() {
    let doc = parse("<ul><li>1</li><li>2</li><li>3</li></ul>");
    let ul = find_by_local_name(&doc, doc.root(), "ul");

    assert_eq!(child_texts(&doc, ul), vec!["1", "2", "3"]);
    assert_eq!(doc.children(ul).len(), 3);

    let reversed: Vec<String> = doc
        .children(ul)
        .rev()
        .filter_map(|id| doc.text_content(id))
        .collect();
    assert_eq!(reversed, vec!["3", "2", "1"]);

    let first = doc.first_child(ul).expect("先頭の子が存在する");
    assert_eq!(doc.text_content(first).as_deref(), Some("1"));
    let last = doc.last_child(ul).expect("末尾の子が存在する");
    assert_eq!(doc.text_content(last).as_deref(), Some("3"));
}

/// REPAIR-5・TASK-6（6.1）・CORE-1: `next_sibling`/`prev_sibling` の端の
/// 挙動（先頭の `prev_sibling` は `None`、末尾の `next_sibling` は `None`）
/// と中間要素の往復が、オフバイワンで隣を飛ばしたり端で値を返したりせず
/// 正しく動くことを確認する。
#[test]
fn repair_5_off_by_one_sibling_navigation_boundaries() {
    let doc = parse("<ul><li>1</li><li>2</li><li>3</li></ul>");
    let ul = find_by_local_name(&doc, doc.root(), "ul");
    let children: Vec<NodeId> = doc.children(ul).collect();
    assert_eq!(children.len(), 3, "テスト入力の子要素数の前提");
    let (li1, li2, li3) = (children[0], children[1], children[2]);

    assert_eq!(doc.prev_sibling(li1), None);
    assert_eq!(doc.next_sibling(li3), None);
    assert_eq!(doc.next_sibling(li1), Some(li2));
    assert_eq!(doc.prev_sibling(li3), Some(li2));
    assert_eq!(doc.next_sibling(li2), Some(li3));
    assert_eq!(doc.prev_sibling(li2), Some(li1));
}

/// REPAIR-5・TASK-6（6.1）・CORE-1: `parse_document` の `max_input_bytes`
/// 境界（`&str` 版）。上限ちょうどの入力は `Ok`、上限を 1 バイト超える入力は
/// `InputTooLarge` になる。比較が `>` から `>=` にオフバイワンでずれると、
/// 上限ちょうどの入力が誤って拒否されて検出される。
#[test]
fn repair_5_off_by_one_max_input_bytes_boundary_str() {
    const LIMIT: usize = 64;
    let prefix = "<p>";
    let filler_len = LIMIT - prefix.len();
    let exact: String = format!("{prefix}{}", "a".repeat(filler_len));
    assert_eq!(
        exact.len(),
        LIMIT,
        "テスト入力そのものの作り間違いを防ぐ前提"
    );

    let options = ParseOptions::default().with_max_input_bytes(LIMIT);
    assert!(
        parse_document(&exact, &options).is_ok(),
        "上限ちょうどの入力は Ok であるべき"
    );

    let over = format!("{exact}a");
    match parse_document(&over, &options) {
        Err(Error::Parse(ParseError::InputTooLarge { len, limit })) => {
            assert_eq!(len, LIMIT + 1);
            assert_eq!(limit, LIMIT);
        }
        other => panic!("InputTooLarge を期待したが {other:?} だった"),
    }
}

/// REPAIR-5・TASK-6（6.1）・CORE-1: `parse_document_bytes` 側でも同じ
/// `max_input_bytes` 境界を確認する（本番コードで比較式が `parse_document`
/// と `parse_document_bytes` の 2 か所に重複しているため、両方を固定する）。
#[test]
fn repair_5_off_by_one_max_input_bytes_boundary_bytes() {
    const LIMIT: usize = 64;
    let prefix = b"<p>";
    let filler_len = LIMIT - prefix.len();
    let mut exact: Vec<u8> = prefix.to_vec();
    exact.extend(std::iter::repeat_n(b'a', filler_len));
    assert_eq!(
        exact.len(),
        LIMIT,
        "テスト入力そのものの作り間違いを防ぐ前提"
    );

    let options = ParseOptions::default().with_max_input_bytes(LIMIT);
    assert!(
        parse_document_bytes(&exact, &options).is_ok(),
        "上限ちょうどの入力は Ok であるべき"
    );

    let mut over = exact.clone();
    over.push(b'a');
    match parse_document_bytes(&over, &options) {
        Err(Error::Parse(ParseError::InputTooLarge { len, limit })) => {
            assert_eq!(len, LIMIT + 1);
            assert_eq!(limit, LIMIT);
        }
        other => panic!("InputTooLarge を期待したが {other:?} だった"),
    }
}

// ---- TASK-6.3: 仕様後退（REPAIR-5） ----
//
// オフバイワン（6.1）や真偽判定反転（6.2）と異なり、「対応済みの経路
// （match の腕・分岐）を丸ごと削除する」変更を仕様後退として注入する。
// 対象は `selector`・`query` モジュール（CORE-1）で、以下の 3 か所を
// それぞれ削除すると red になることを確認済み（PR 本文の
// 「注入検証記録」参照）:
// - `selector::parse_attribute_selector` の
//   `Some('\'') | Some('"') => parse_quoted_string(cursor)?,` の腕
//   （属性値の引用符対応が消え、空白を含む値が解析できなくなる）
// - `query::compound_matches` の
//   `Some(local_name) if is_html => html_local_name_eq(local_name, type_name),`
//   の腕（HTML 名前空間での型名の大文字小文字無視が消える）
// - `query::id_or_class_eq` の
//   `if document.quirks_mode() == QuirksMode::Quirks { ... }` の分岐
//   （quirks mode での ID・クラスの大文字小文字無視が消える）

/// REPAIR-5・TASK-6（6.3）・CORE-1: 属性セレクタの引用符付き値
/// （`[attr="a b"]`・`[attr='c']`）が解析・照合できる。
/// `parse_attribute_selector` から引用符分岐を削除する仕様後退を注入すると、
/// 空白を含む値は識別子の経路では解析できず失敗として検出される。
#[test]
fn repair_5_spec_regression_quoted_attribute_value() {
    let doc = parse(r#"<div><a data-x="a b">1</a><a data-x='c'>2</a><a data-x="ab">3</a></div>"#);
    let root = doc.root();

    let double_quoted =
        fandhe_browser_core::query_selector_all_str(&doc, root, r#"a[data-x="a b"]"#)
            .expect("引用符付き属性セレクタは解析・照合に成功するべき");
    assert_eq!(double_quoted.len(), 1);
    assert_eq!(doc.text_content(double_quoted[0]).as_deref(), Some("1"));

    let single_quoted = fandhe_browser_core::query_selector_all_str(&doc, root, "a[data-x='c']")
        .expect("シングルクォート付き属性セレクタは解析・照合に成功するべき");
    assert_eq!(single_quoted.len(), 1);
    assert_eq!(doc.text_content(single_quoted[0]).as_deref(), Some("2"));
}

/// REPAIR-5・TASK-6（6.3）・CORE-1: HTML 名前空間の型セレクタは大文字小文字を
/// 無視して一致するが、SVG 名前空間（`foreignObject`）は区別する。
/// `compound_matches` から HTML 名前空間の大文字小文字無視の腕を削除すると、
/// 大文字の型名（`DIV`・`Div`）が一致しなくなり検出される。
#[test]
fn repair_5_spec_regression_html_type_selector_case_insensitive() {
    let doc = parse("<div><p>x</p></div><svg><foreignObject></foreignObject></svg>");
    let root = doc.root();
    let expected_div = find_by_local_name(&doc, root, "div");

    for selector in ["DIV", "Div", "div"] {
        let matched = fandhe_browser_core::query_selector_all_str(&doc, root, selector)
            .unwrap_or_else(|e| panic!("selector {selector} の解析・照合に成功するべき: {e:?}"));
        assert_eq!(
            matched.len(),
            1,
            "selector {selector} は 1 件に一致するべき"
        );
        assert_eq!(matched[0], expected_div);
    }

    // 対照: SVG 名前空間は大文字小文字を区別するため、HTML 側の緩和が
    // SVG 側へ漏れていないことも併せて固定する。
    let case_sensitive = fandhe_browser_core::query_selector_all_str(&doc, root, "foreignObject")
        .expect("foreignObject（正しい大文字小文字）は解析・照合に成功するべき");
    assert_eq!(case_sensitive.len(), 1);

    let case_mismatched = fandhe_browser_core::query_selector_all_str(&doc, root, "foreignobject")
        .expect("foreignobject（誤った大文字小文字）も解析自体は成功するべき");
    assert_eq!(
        case_mismatched.len(),
        0,
        "SVG 名前空間では大文字小文字が異なると一致しないべき"
    );
}

/// REPAIR-5・TASK-6（6.3）・CORE-1: quirks mode では ID・クラスセレクタが
/// 大文字小文字を無視して一致するが、no-quirks（`<!DOCTYPE html>` あり）
/// では区別する。`id_or_class_eq` から quirks mode 分岐を削除すると、
/// quirks mode でも大文字小文字を区別するようになり検出される。
#[test]
fn repair_5_spec_regression_quirks_mode_id_class_case_insensitive() {
    let quirks_doc = parse(r#"<p class="foo" id="bar">q</p>"#);
    assert_eq!(
        quirks_doc.quirks_mode(),
        fandhe_browser_core::QuirksMode::Quirks,
        "DOCTYPE なしの入力は quirks mode になるべき（テスト前提）"
    );
    let quirks_root = quirks_doc.root();
    let expected_p = find_by_local_name(&quirks_doc, quirks_root, "p");

    for selector in [".FOO", "#BAR"] {
        let matched =
            fandhe_browser_core::query_selector_all_str(&quirks_doc, quirks_root, selector)
                .unwrap_or_else(|e| {
                    panic!("selector {selector} の解析・照合に成功するべき: {e:?}")
                });
        assert_eq!(
            matched.len(),
            1,
            "quirks mode では {selector} が 1 件に一致するべき"
        );
        assert_eq!(matched[0], expected_p);
        assert_eq!(quirks_doc.text_content(matched[0]).as_deref(), Some("q"));
    }

    // 対照: no-quirks では大文字小文字を区別する。
    let no_quirks_doc = parse(r#"<!DOCTYPE html><p class="foo" id="bar">q</p>"#);
    assert_eq!(
        no_quirks_doc.quirks_mode(),
        fandhe_browser_core::QuirksMode::NoQuirks,
        "DOCTYPE ありの入力は no-quirks mode になるべき（テスト前提）"
    );
    let no_quirks_root = no_quirks_doc.root();

    let upper_in_no_quirks =
        fandhe_browser_core::query_selector_all_str(&no_quirks_doc, no_quirks_root, ".FOO")
            .expect(".FOO の解析自体は成功するべき");
    assert_eq!(
        upper_in_no_quirks.len(),
        0,
        "no-quirks mode では大文字小文字が異なると一致しないべき"
    );

    let lower_in_no_quirks =
        fandhe_browser_core::query_selector_all_str(&no_quirks_doc, no_quirks_root, ".foo")
            .expect(".foo の解析・照合に成功するべき");
    assert_eq!(lower_in_no_quirks.len(), 1);
}

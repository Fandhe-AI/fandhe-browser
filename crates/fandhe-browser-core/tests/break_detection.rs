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
//! - TASK-6.1（#330）: オフバイワン検出
//! - TASK-6.2（本ファイル・#331）: checkbox 判定反転検出
//! - TASK-6.3（#332）: 仕様後退検出
//!
//! 本ファイルは Issue #331 着手時点で TASK-6.1（#330・PR #445）がまだ
//! `origin/main` に merge されていなかったため新規作成した。#445 が先に
//! merge された場合は add/add の衝突が起きる見込みで、後から merge する側が
//! rebase して両セクション（共通のヘッダ・`parse`/`find_by_local_name` と、
//! それぞれのセクション本体）を残せば解消できる。

use fandhe_browser_core::{Document, NodeId, ParseOptions, parse_document};

/// 既定の `ParseOptions` で HTML をパースする。テスト入力は固定のダミー
/// HTML であり外部入力ではないため、失敗時に `expect` で即座に落として良い。
fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("テスト入力は必ず成功する")
        .document
}

// ---- TASK-6.2: checkbox 判定反転（REPAIR-5） ----
//
// 対象は「checkbox が checked かどうか」を実際に決めている本番コードの
// 真偽判定 3 箇所（`query` モジュール・`dom` モジュール。ビヘイビア
// `CORE-1`）。PoC-10（`docs/spec/03-poc/ai-self-repair/`）の 2 番目の注入は
// 「フォーム値を組み立てる処理で checkbox の checked 判定を `!` で反転
// させる」ものだったが、本リポの core にはまだ PoC の
// `build_form_values` 相当（フォーム値の組み立て）は実装されていない
// （フォーム機能の新設は本テスト Issue のスコープ外）。そのため
// 「query API による checked 判定」に置き換えて検証する
// （REPAIR-3: 実装済みを装わない）。
//
// - `query::match_attribute` の `AttributeMatcher::Exists` 腕
//   （`document.attribute(..).is_some()`）: `input[checked]` のように
//   「checked 属性があるか」を決める。
// - 同関数の `AttributeMatcher::Equals` 腕
//   （`document.attribute(..) == Some(expected)`）: `[type=checkbox]` で
//   checkbox だけを選ぶ判定。
// - `dom::Document::attribute` の `find` クロージャ
//   （HTML 要素での `eq_ignore_ascii_case` による属性名の大文字小文字
//   無視一致）: `CHECKED` のような表記揺れも同じ属性として扱うかどうかを
//   決める。
//
// 本セクションのテストは、上記 3 箇所をそれぞれ反転する変更を本番コードへ
// 注入すると red になることを確認済み（PR 本文の「注入検証記録」参照）。

/// TASK-6.2 検証用の固定ダミー HTML。checked・未チェック・値ありの
/// checked・属性名の大文字表記に加え、type が checkbox でない紛らわしい
/// 要素（radio・text）を並べる。
const CHECKBOX_FORM_HTML: &str = r#"<form>
<input type="checkbox" name="a" value="a" checked>
<input type="checkbox" name="b" value="b">
<input type="checkbox" name="c" value="c" checked="checked">
<input type="checkbox" name="d" value="d" CHECKED>
<input type="radio" name="r" value="r" checked>
<input type="text" name="t" value="t">
</form>"#;

/// `ids` の各要素の `value` 属性値を文書順に集める（属性なしは空文字列）。
fn checkbox_values_of(doc: &Document, ids: &[NodeId]) -> Vec<String> {
    ids.iter()
        .map(|&id| doc.attribute(id, "value").unwrap_or_default().to_string())
        .collect()
}

/// REPAIR-5・TASK-6（6.2）・CORE-1: `input[type=checkbox][checked]` は
/// checked な checkbox（`a`・`c`・`d`）だけを文書順で返し、未チェックの
/// `b`・checkbox でない `r`（radio）・`t`（text）は含まない。
///
/// `match_attribute` の `Exists` 腕（checked 属性の有無）を
/// `.is_some()` → `.is_none()` に反転すると未チェックの `b` だけが残り、
/// `Equals` 腕（`type=checkbox` の一致）を `==` → `!=` に反転すると
/// checkbox 以外（radio・text）が混ざるため、どちらの反転でも検出される。
#[test]
fn repair_5_checkbox_inversion_checked_selector_returns_only_checked() {
    let doc = parse(CHECKBOX_FORM_HTML);
    let ids = fandhe_browser_core::query_selector_all_str(
        &doc,
        doc.root(),
        "input[type=checkbox][checked]",
    )
    .expect("セレクタは解析・照合とも成功する");

    assert_eq!(
        checkbox_values_of(&doc, &ids),
        vec!["a".to_string(), "c".to_string(), "d".to_string()]
    );
}

/// REPAIR-5・TASK-6（6.2）・CORE-1: `input[type=checkbox]` の各要素に
/// `[checked]` を `element_matches` で個別に照合すると、checked
/// （`a`・`c`・`d`）は `true`、未チェックの `b` は `false` になる。
/// true 側・false 側の両方を具体値で固定するため、`Exists` 腕の反転で
/// true/false が入れ替わると検出される。
#[test]
fn repair_5_checkbox_inversion_element_matches_per_checkbox() {
    let doc = parse(CHECKBOX_FORM_HTML);
    let checkbox_ids =
        fandhe_browser_core::query_selector_all_str(&doc, doc.root(), "input[type=checkbox]")
            .expect("セレクタは解析・照合とも成功する");
    let checked_selector = fandhe_browser_core::selector::parse_selector_list("[checked]")
        .expect("[checked] は解析に成功する");

    let pairs: Vec<(String, bool)> = checkbox_ids
        .iter()
        .map(|&id| {
            let value = doc.attribute(id, "value").unwrap_or_default().to_string();
            let matched = fandhe_browser_core::element_matches(&doc, id, &checked_selector)
                .expect("element_matches は成功する");
            (value, matched)
        })
        .collect();

    assert_eq!(
        pairs,
        vec![
            ("a".to_string(), true),
            ("b".to_string(), false),
            ("c".to_string(), true),
            ("d".to_string(), true),
        ]
    );
}

/// REPAIR-5・TASK-6（6.2）・CORE-1: `Document::attribute` が `checked`
/// 属性の値を要素ごとに正しく返す。値なしの `checked` は空文字列
/// （`Some("")`）、値ありの `checked="checked"` は `Some("checked")`、
/// 大文字表記 `CHECKED`（HTML 要素なので大文字小文字を無視して一致）も
/// `Some("")`、未チェックの `b` は `None` になる。`find` クロージャの
/// `eq_ignore_ascii_case` の結果を `!` で反転すると、別の属性を拾うか
/// `None` になり検出される。
#[test]
fn repair_5_checkbox_inversion_attribute_presence_accessor() {
    let doc = parse(CHECKBOX_FORM_HTML);
    let checkbox_ids =
        fandhe_browser_core::query_selector_all_str(&doc, doc.root(), "input[type=checkbox]")
            .expect("セレクタは解析・照合とも成功する");
    assert_eq!(checkbox_ids.len(), 4, "テスト入力の checkbox 数の前提");
    let (a, b, c, d) = (
        checkbox_ids[0],
        checkbox_ids[1],
        checkbox_ids[2],
        checkbox_ids[3],
    );

    assert_eq!(doc.attribute(a, "checked"), Some(""));
    assert_eq!(doc.attribute(b, "checked"), None);
    assert_eq!(doc.attribute(c, "checked"), Some("checked"));
    assert_eq!(doc.attribute(d, "checked"), Some(""));
}

/// REPAIR-5・TASK-6（6.2）・CORE-1: `input[type=checkbox]` は checkbox
/// 4 件（`a`〜`d`）だけを返し、radio（`r`）・text（`t`）は含まない。
/// `Equals` 腕の反転（`==` → `!=`）の検出をテスト 1 から切り離して
/// 単独で明確にする。
#[test]
fn repair_5_checkbox_inversion_type_equals_excludes_radio_and_text() {
    let doc = parse(CHECKBOX_FORM_HTML);
    let ids = fandhe_browser_core::query_selector_all_str(&doc, doc.root(), "input[type=checkbox]")
        .expect("セレクタは解析・照合とも成功する");

    assert_eq!(
        checkbox_values_of(&doc, &ids),
        vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]
    );
}

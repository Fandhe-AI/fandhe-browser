//! 代表的な静的 HTML に対する `build_snapshot` の結果を、role・name・ref・state の
//! 具体値で固定する結合テスト（`AISNAP-1`・`AISNAP-10`・TASK-11.8・Issue #77・`MS-2`）。
//!
//! 呼び出し文脈: core の `parse_document` で得た `Document` を ai の
//! `build_snapshot` へ渡し、返る `Snapshot` 全体を期待木と `assert_eq!` で一致比較する。
//! `src/snapshot/build.rs` のユニットテストが個別の性質（除外・深さ上限等）を
//! 確かめるのに対し、本ファイルはページ全体の構造の回帰を検出する。
//!
//! ref を固定リテラルで持つ根拠: ref は FNV-1a ベースで、リリースをまたいで
//! 黙って変わらないよう意図的に `DefaultHasher` を使っていない
//! （`element_ref.rs` のモジュール doc）。したがってリテラルが変われば
//! `AISNAP-10`（ref の安定性）の回帰として検出すべきである。
//!
//! フィクスチャの入力はコンパイル時定数で、値はすべてダミー（外部通信・実資格情報なし）。
//! 暫定挙動（`generic` へのフォールバック）になる landmark 等の role は
//! 固定しないよう、`nav`・`main`・`form`・`img` 等は使わない。

use std::collections::HashSet;

use fandhe_browser_ai::snapshot::{
    CheckedState, Node, Snapshot, State, build_snapshot, ref_signature,
};
use fandhe_browser_core::parse::{ParseOptions, parse_document};

/// 静的な記事ページ。
const ARTICLE: &str = r#"<!DOCTYPE html><html><head><title>Rust 入門記事</title><style>p{}</style></head><body><header>サイトヘッダ</header><h1>はじめに</h1><p>詳細は<a href="https://example.com/docs">公式ドキュメント</a>を参照。</p><h2>要点</h2><ul><li>所有権</li><li>借用</li></ul><footer>連絡先</footer></body></html>"#;

/// フォームページ（hidden 入力の値は Snapshot に現れてはならない）。
const FORM: &str = r#"<!DOCTYPE html><html><head><title>登録フォーム</title></head><body><div><label for="user">ユーザー名</label><input type="text" id="user" name="user"></div><div><label><input type="checkbox" name="agree" checked>規約に同意</label></div><div><input type="radio" name="plan" aria-label="無料プラン"></div><select name="lang" aria-label="言語"></select><input type="hidden" name="token" value="dummy-hidden-value"><button type="submit">送信</button><button disabled>取消</button></body></html>"#;

/// 表ページ。
const TABLE: &str = r#"<!DOCTYPE html><html><head><title>成績表</title></head><body><table><thead><tr><th>名前</th><th>点数</th></tr></thead><tbody><tr><td>太郎</td><td>80</td></tr></tbody></table></body></html>"#;

/// HTML から `Snapshot` を構築する。
fn snap(html: &str) -> Snapshot {
    let parsed =
        parse_document(html, &ParseOptions::default()).expect("フィクスチャのパースは成功する");
    build_snapshot(&parsed.document).expect("フィクスチャの構築は成功する")
}

/// ref 付きノードの期待値。
fn n(role: &str, name: &str, r: &str, children: Vec<Node>) -> Node {
    Node::new(role, name).with_ref(r).with_children(children)
}

/// `children` の `i` 番目（添字 `[]` を使わない）。
fn child(node: &Node, i: usize) -> &Node {
    node.children.get(i).expect("期待した位置に子ノードがある")
}

/// 先行順（明示スタックの反復）で全ノードを列挙する。
fn all_nodes(root: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        out.push(node);
        stack.extend(node.children.iter().rev());
    }
    out
}

/// ref が `e` + 16 桁の小文字 16 進 + 任意の `-<n>` の形か（本フィクスチャで現れる形）。
fn is_ref_shaped(r: &str) -> bool {
    let Some(rest) = r.strip_prefix('e') else {
        return false;
    };
    let hex_len = rest
        .chars()
        .take_while(|c| matches!(c, '0'..='9' | 'a'..='f'))
        .count();
    if hex_len != 16 {
        return false;
    }
    let tail: String = rest.chars().skip(16).collect();
    tail.is_empty()
        || tail
            .strip_prefix('-')
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

/// 全フィクスチャ共通の性質（ルート・決定性・ref の形状と一意性）。
fn assert_common(html: &str, s: &Snapshot) {
    assert_eq!(s.tree.role, "document");
    assert_eq!(s.tree.r#ref, None);
    assert!(!s.truncated);
    assert_eq!(&snap(html), s, "同じ HTML は同じ Snapshot になる");
    let refs: Vec<&str> = all_nodes(&s.tree)
        .into_iter()
        .skip(1)
        .map(|nd| nd.r#ref.as_deref().expect("ルート以外は ref を持つ"))
        .collect();
    assert!(
        refs.iter().all(|r| is_ref_shaped(r)),
        "ref の形状: {refs:?}"
    );
    let uniq: HashSet<&str> = refs.iter().copied().collect();
    assert_eq!(uniq.len(), refs.len(), "ref は一意");
}

/// `AISNAP-1`（TASK-11.8・Issue #77・MS-2）: 記事ページの構造を具体値で固定する。
#[test]
fn aisnap_1_article_snapshot_structure() {
    let s = snap(ARTICLE);
    let expected = Snapshot::new(Node::new("document", "Rust 入門記事").with_children(vec![n(
        "generic",
        "",
        "e65477c205c50fefb",
        vec![n(
            "generic",
            "",
            "e9c1602d3222315df",
            vec![
                n("banner", "", "e02d16e1795403182", vec![]),
                n("heading", "はじめに", "e300ae11bc252b0fd", vec![]),
                n(
                    "generic",
                    "",
                    "edf7a99064530f860",
                    vec![n("link", "公式ドキュメント", "ed45c1d22b4e186f4", vec![])],
                ),
                n("heading", "要点", "ee0714a175d20c8b2", vec![]),
                n(
                    "list",
                    "",
                    "ecc842c965dec143a",
                    vec![
                        n("listitem", "", "ef17dbedadbdb41bc", vec![]),
                        n("listitem", "", "ef17dbedadbdb41bc-2", vec![]),
                    ],
                ),
                n("contentinfo", "", "ec0aa17bccd809080", vec![]),
            ],
        )],
    )]));
    assert_eq!(s, expected);
    assert_common(ARTICLE, &s);

    // 要所の個別確認（失敗時の読みやすさのため）。
    let body = child(child(&s.tree, 0), 0);
    assert_eq!(child(body, 1).role, "heading");
    assert_eq!(child(body, 1).name, "はじめに");
    let link = child(child(body, 2), 0);
    assert_eq!(link.role, "link");
    assert_eq!(link.r#ref.as_deref(), Some("ed45c1d22b4e186f4"));
    // 同 role・同 name の listitem は `-2` で区別される（AISNAP-10）。
    assert_eq!(
        child(child(body, 4), 1).r#ref.as_deref(),
        Some("ef17dbedadbdb41bc-2")
    );
}

/// `AISNAP-1`（TASK-11.8・Issue #77・MS-2）: フォームページの構造・state を具体値で固定し、
/// hidden 入力の値が漏れないことを確認する。
#[test]
fn aisnap_1_form_snapshot_structure() {
    let s = snap(FORM);
    let checked = State::default().with_checked(Some(CheckedState::Checked));
    let unchecked = State::default().with_checked(Some(CheckedState::Unchecked));
    let disabled = State::default().with_disabled(true);
    let expected = Snapshot::new(Node::new("document", "登録フォーム").with_children(vec![n(
        "generic",
        "",
        "e65477c205c50fefb",
        vec![n(
            "generic",
            "",
            "e9c1602d3222315df",
            vec![
                n(
                    "generic",
                    "",
                    "edf7a99064530f860",
                    vec![
                        n("generic", "", "ec49e5925d2775d07", vec![]),
                        n("textbox", "ユーザー名", "e60f2465f276d6230", vec![]),
                    ],
                ),
                n(
                    "generic",
                    "",
                    "edf7a99064530f860-2",
                    vec![n(
                        "generic",
                        "",
                        "ec49e5925d2775d07-2",
                        vec![
                            n("checkbox", "規約に同意", "eb4e606a8d0b0322c", vec![])
                                .with_state(checked),
                        ],
                    )],
                ),
                n(
                    "generic",
                    "",
                    "edf7a99064530f860-3",
                    vec![
                        n("radio", "無料プラン", "e3ca491e4e71deece", vec![]).with_state(unchecked),
                    ],
                ),
                n("combobox", "言語", "ea907649b0395e5e0", vec![]),
                n("button", "送信", "e764e1c46ab9a2bd6", vec![]),
                n("button", "取消", "ebc8665358ad3ea30", vec![]).with_state(disabled),
            ],
        )],
    )]));
    assert_eq!(s, expected);
    assert_common(FORM, &s);

    let form_root = child(child(&s.tree, 0), 0);
    let cb = child(child(child(form_root, 1), 0), 0);
    assert_eq!(cb.role, "checkbox");
    assert_eq!(cb.state.checked, Some(CheckedState::Checked));
    let cancel = child(form_root, 5);
    assert_eq!(cancel.name, "取消");
    assert!(cancel.state.disabled);

    // hidden 入力の値・name は木のどこにも現れない。
    for nd in all_nodes(&s.tree) {
        assert!(
            !nd.name.contains("dummy-hidden-value"),
            "hidden の値が漏れた: {nd:?}"
        );
        assert!(
            !nd.name.contains("token"),
            "hidden の name が漏れた: {nd:?}"
        );
    }
}

/// `AISNAP-1`（TASK-11.8・Issue #77・MS-2）: 表ページの構造を具体値で固定する。
#[test]
fn aisnap_1_table_snapshot_structure() {
    let s = snap(TABLE);
    let expected = Snapshot::new(Node::new("document", "成績表").with_children(vec![n(
        "generic",
        "",
        "e65477c205c50fefb",
        vec![n(
            "generic",
            "",
            "e9c1602d3222315df",
            vec![n(
                "table",
                "",
                "e7c96b5162ee5821b",
                vec![
                    n(
                        "rowgroup",
                        "",
                        "e562e6ae373000aaa",
                        vec![n(
                            "row",
                            "名前点数",
                            "e3b9ec063bec3ce0d",
                            vec![
                                n("columnheader", "名前", "e895cf72c839b312e", vec![]),
                                n("columnheader", "点数", "e4c3266d420a99a2c", vec![]),
                            ],
                        )],
                    ),
                    n(
                        "rowgroup",
                        "",
                        "e562e6ae373000aaa-2",
                        vec![n(
                            "row",
                            "太郎80",
                            "e603d30fc95b8b612",
                            vec![
                                n("cell", "太郎", "e8c54b60bbb2904e5", vec![]),
                                n("cell", "80", "eb3df38593dc1377c", vec![]),
                            ],
                        )],
                    ),
                ],
            )],
        )],
    )]));
    assert_eq!(s, expected);
    assert_common(TABLE, &s);

    let table = child(child(child(&s.tree, 0), 0), 0);
    assert_eq!(table.role, "table");
    assert_eq!(child(child(child(table, 0), 0), 1).name, "点数");
}

/// `AISNAP-10`（TASK-11.8・Issue #77）: 修飾子（id・name・href・親 ref）を持たない
/// ルート直下の `html` 要素の ref は `ref_signature(role, name)` のダイジェストと一致する。
/// 固定リテラルがハッシュ実装と整合していることの独立確認。
#[test]
fn aisnap_10_root_child_ref_matches_signature_digest() {
    let s = snap(ARTICLE);
    let html = child(&s.tree, 0);
    assert_eq!(
        html.r#ref,
        Some(format!("e{:016x}", ref_signature("generic", "")))
    );
    assert_eq!(html.r#ref.as_deref(), Some("e65477c205c50fefb"));
}

/// `AISNAP-10`（TASK-11.8・Issue #77）: 異なるページでも同じシグネチャの要素
/// （`html`・`body`）の ref は同一で、ページ内容に依存しない。
#[test]
fn aisnap_10_ref_is_stable_across_pages() {
    let a = snap(ARTICLE);
    let b = snap(TABLE);
    assert_eq!(child(&a.tree, 0).r#ref, child(&b.tree, 0).r#ref);
    assert_eq!(
        child(child(&a.tree, 0), 0).r#ref,
        child(child(&b.tree, 0), 0).r#ref
    );
}

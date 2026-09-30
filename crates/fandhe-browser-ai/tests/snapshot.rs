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
//! `AISNAP-3`（TASK-13.3・Issue #88）: 表セル・価格クラス要素の `data_leaf` 反映も固定する。
//!
//! 暫定挙動（`generic` へのフォールバック）になる landmark 等の role は
//! 固定しないよう、`nav`・`main`・`form`・`img` 等は使わない。

use std::collections::HashSet;

use fandhe_browser_ai::snapshot::{
    CheckedState, DataLeafKind, HeaderCell, Node, Snapshot, State, TableRow, TableSummary,
    build_snapshot, ref_signature,
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
    let nodes = all_nodes(&s.tree);
    let mut refs: Vec<&str> = nodes
        .iter()
        .skip(1)
        .map(|nd| nd.r#ref.as_deref().expect("ルート以外は ref を持つ"))
        .collect();
    // 圧縮した表のヘッダ ref も一意性の検査対象に含める（AISNAP-2・TASK-12.5）。
    for nd in &nodes {
        if let Some(t) = &nd.table {
            refs.extend(t.header.iter().map(|h| h.r#ref.as_str()));
        }
    }
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
                n("list", "", "ecc842c965dec143a", vec![]).with_table(TableSummary::new(
                    vec![],
                    vec![TableRow::new("所有権", false), TableRow::new("借用", false)],
                    0,
                )),
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
    // 規則的な ul は子孫を展開せず 1 ノードへ圧縮される（AISNAP-2・TASK-12.5）。
    assert_eq!(child(body, 4).children.len(), 0);
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

/// `AISNAP-2`（TASK-12.5・Issue #83・MS-2）: 規則的な表は子孫を展開せず、ヘッダ
/// （個別 ref）・圧縮行・超過行数を持つ 1 ノードになる（受入基準）。
/// ヘッダ ref の scope が rowgroup/row から table に変わったため、TASK-11.8 当時と
/// リテラルが異なる（AISNAP-10）。rowgroup・row・cell は圧縮で消える。
#[test]
fn aisnap_2_table_snapshot_is_compressed() {
    let s = snap(TABLE);
    let expected = Snapshot::new(Node::new("document", "成績表").with_children(vec![n(
        "generic",
        "",
        "e65477c205c50fefb",
        vec![n(
            "generic",
            "",
            "e9c1602d3222315df",
            vec![
                n("table", "", "e7c96b5162ee5821b", vec![]).with_table(TableSummary::new(
                    vec![
                        HeaderCell::new("columnheader", "名前", "ed7fea8519e468d01"),
                        HeaderCell::new("columnheader", "点数", "ee748caeae6097f8b"),
                    ],
                    vec![TableRow::new("太郎 | 80", false)],
                    0,
                )),
            ],
        )],
    )]));
    assert_eq!(s, expected);
    assert_common(TABLE, &s);
    let table = child(child(child(&s.tree, 0), 0), 0);
    assert_eq!(table.role, "table");
    assert_eq!(table.data_leaf, None);
}

/// 規則的でない表（rowspan 付き）。従来どおり子孫へ展開される。
const IRREGULAR_TABLE: &str = r#"<!DOCTYPE html><html><head><title>成績表</title></head><body><table><thead><tr><th>名前</th><th>点数</th></tr></thead><tbody><tr><td rowspan="2">太郎</td><td>80</td></tr></tbody></table></body></html>"#;

/// `AISNAP-1`・`AISNAP-2`（TASK-11.8・TASK-12.5・Issue #83）: 規則的でない表は
/// 圧縮されず（`table == None`）、rowgroup・row・cell が具体値で展開される。
#[test]
fn aisnap_2_irregular_table_snapshot_is_expanded() {
    let s = snap(IRREGULAR_TABLE);
    let tc = DataLeafKind::TableCell;
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
                                n("columnheader", "名前", "e895cf72c839b312e", vec![])
                                    .with_data_leaf(tc),
                                n("columnheader", "点数", "e4c3266d420a99a2c", vec![])
                                    .with_data_leaf(tc),
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
                                n("cell", "太郎", "e8c54b60bbb2904e5", vec![]).with_data_leaf(tc),
                                n("cell", "80", "eb3df38593dc1377c", vec![]).with_data_leaf(tc),
                            ],
                        )],
                    ),
                ],
            )],
        )],
    )]));
    assert_eq!(s, expected);
    assert_common(IRREGULAR_TABLE, &s);
    let table = child(child(child(&s.tree, 0), 0), 0);
    assert_eq!(table.table, None);
}

/// 表を取り出す（`body` 直下の最初の子）。
fn first_table(s: &Snapshot) -> &Node {
    child(child(child(&s.tree, 0), 0), 0)
}

fn summary(s: &Snapshot) -> &TableSummary {
    first_table(s).table.as_ref().expect("圧縮された表である")
}

/// ページ全体の HTML を作る。
fn page(body: &str) -> String {
    format!("<!DOCTYPE html><html><head><title>t</title></head><body>{body}</body></html>")
}

/// `AISNAP-2`（TASK-12.5・Issue #83）: thead なし 25 行の表は 20 行に圧縮され、
/// 超過 5 行が `truncated_rows` になる。
#[test]
fn aisnap_2_large_table_reports_truncated_rows() {
    let rows: String = (1..=25)
        .map(|i| format!("<tr><td>r{i}</td><td>{i}</td></tr>"))
        .collect();
    let html = page(&format!("<table>{rows}</table>"));
    let s = snap(&html);
    let t = summary(&s);
    assert_eq!(t.header, vec![]);
    assert_eq!(t.rows.len(), 20);
    assert_eq!(t.rows.first(), Some(&TableRow::new("r1 | 1", false)));
    assert_eq!(t.rows.get(19), Some(&TableRow::new("r20 | 20", false)));
    assert_eq!(t.truncated_rows, 5);
    assert!(!s.truncated, "行の省略では Snapshot::truncated を立てない");
    assert!(first_table(&s).children.is_empty());
}

/// `AISNAP-2`（TASK-12.5・Issue #83）: hidden / aria-hidden の表は Node にならない。
#[test]
fn aisnap_2_hidden_tables_are_excluded() {
    let html = page(
        "<table hidden><tr><td>秘密1</td></tr></table>\
         <table aria-hidden=\"true\"><tr><td>秘密2</td></tr></table><p>本文</p>",
    );
    let s = snap(&html);
    let nodes = all_nodes(&s.tree);
    assert!(
        nodes
            .iter()
            .all(|nd| nd.table.is_none() && nd.role != "table")
    );
    assert!(nodes.iter().all(|nd| !nd.name.contains("秘密")));
}

/// `AISNAP-2`・`AISNAP-10`（TASK-12.5・Issue #83）: id が異なる同形の表 2 つで
/// ヘッダ ref が互いに異なる。
#[test]
fn aisnap_10_headers_of_distinct_tables_have_distinct_refs() {
    let one = |id: &str| {
        format!(
            "<table id=\"{id}\"><thead><tr><th>名前</th></tr></thead><tr><td>a</td></tr></table>"
        )
    };
    let html = page(&format!("{}{}", one("t1"), one("t2")));
    let s = snap(&html);
    let tables: Vec<&Node> = all_nodes(&s.tree)
        .into_iter()
        .filter(|nd| nd.table.is_some())
        .collect();
    assert_eq!(tables.len(), 2);
    let refs: Vec<&str> = tables
        .iter()
        .flat_map(|nd| {
            let t = nd.table.as_ref().expect("圧縮済み");
            t.header.iter().map(|h| h.r#ref.as_str())
        })
        .collect();
    assert_eq!(refs.len(), 2);
    assert_ne!(refs.first(), refs.get(1));
    assert_common(&html, &s);
}

/// `AISNAP-2`（TASK-12.5・Issue #83）: 圧縮した行の中の hidden 入力の値は行テキストへ漏れない。
#[test]
fn aisnap_2_hidden_input_value_does_not_leak_into_row() {
    let html = page(
        "<table><tr><td>a<input type=\"hidden\" value=\"dummy-secret\"></td><td>b</td></tr></table>",
    );
    let s = snap(&html);
    let t = summary(&s);
    assert_eq!(t.rows, vec![TableRow::new("a | b", false)]);
}

/// `AISNAP-2`（TASK-12.5・Issue #83）: ヘッダ name の打ち切りは `Snapshot::truncated` を
/// 立てる。短いセルだけの表では立たない。
#[test]
fn aisnap_2_header_name_truncation_marks_snapshot() {
    let long = "あ".repeat(5000);
    let html = page(&format!(
        "<table><thead><tr><th>{long}</th></tr></thead><tr><td>x</td></tr></table>"
    ));
    assert!(snap(&html).truncated);
    let short = page("<table><thead><tr><th>名</th></tr></thead><tr><td>x</td></tr></table>");
    assert!(!snap(&short).truncated);
}

/// `AISNAP-2`（TASK-12.5・Issue #83）: 入れ子の表・列数の食い違う表は圧縮されない。
#[test]
fn aisnap_2_irregular_tables_are_not_compressed() {
    let nested = page("<table><tr><td><table><tr><td>x</td></tr></table></td></tr></table>");
    let mismatch = page("<table><tr><td>a</td><td>b</td></tr><tr><td>c</td></tr></table>");
    for html in [nested, mismatch] {
        let s = snap(&html);
        assert_eq!(first_table(&s).table, None);
        assert!(!first_table(&s).children.is_empty());
    }
}

/// `AISNAP-2`（TASK-12.5・Issue #83）: ul/ol の各 li が 1 行になり、header は空。
#[test]
fn aisnap_2_lists_are_compressed_to_rows() {
    let html =
        page("<ul><li>りんご</li><li>みかん</li></ul><ol><li>一</li><li>二</li><li>三</li></ol>");
    let s = snap(&html);
    let lists: Vec<&TableSummary> = all_nodes(&s.tree)
        .into_iter()
        .filter_map(|nd| nd.table.as_ref())
        .collect();
    assert_eq!(lists.len(), 2);
    let first = lists.first().expect("ul がある");
    assert_eq!(first.header, vec![]);
    assert_eq!(
        first.rows,
        vec![
            TableRow::new("りんご", false),
            TableRow::new("みかん", false)
        ]
    );
    let second = lists.get(1).expect("ol がある");
    assert_eq!(second.rows.len(), 3);
    assert_eq!(second.truncated_rows, 0);
}

/// `AISNAP-2`・`AISNAP-10`（TASK-12.5・Issue #83）: 圧縮で ref の発行数が変わっても、
/// 後続要素の ref は決定的（同じ HTML の 2 回構築で一致）。
#[test]
fn aisnap_10_refs_after_compressed_table_are_deterministic() {
    let html = page("<table><tr><td>a</td></tr></table><button>送信</button>");
    let (a, b) = (snap(&html), snap(&html));
    assert_eq!(a, b);
    let btn = all_nodes(&a.tree)
        .into_iter()
        .find(|nd| nd.role == "button")
        .expect("button がある");
    assert!(btn.r#ref.is_some());
    assert_common(&html, &a);
}

/// 価格クラス要素を含むページ（landmark 系は使わない）。
const PRICE: &str = r#"<!DOCTYPE html><html><head><title>商品</title></head><body><h1>商品</h1><p>価格: <span class="price">¥1,980</span></p><button>カートに追加</button></body></html>"#;

/// `AISNAP-3`（TASK-13.3・Issue #88）: 価格 span だけが `PriceClass` になる。
#[test]
fn aisnap_3_price_snapshot_marks_price_leaf() {
    let s = snap(PRICE);
    assert_common(PRICE, &s);
    let nodes = all_nodes(&s.tree);
    let marked: Vec<&&Node> = nodes.iter().filter(|nd| nd.data_leaf.is_some()).collect();
    assert_eq!(marked.len(), 1);
    let price = marked.first().expect("価格ノードが 1 件ある");
    assert_eq!(price.role, "generic");
    assert_eq!(price.name, "");
    assert_eq!(price.data_leaf, Some(DataLeafKind::PriceClass));
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

/// 全ノードを先行順で集める（反復）。
fn collect_nodes(root: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        out.push(n);
        stack.extend(n.children.iter().rev());
    }
    out
}

/// AISNAP-2: 操作要素（リンク・ボタン・入力欄）を含む表・一覧は圧縮せず、ref を保持する。
#[test]
fn aisnap_2_interactive_rows_are_not_compressed() {
    let html = r#"<body><table><thead><tr><th>名前</th><th>操作</th></tr></thead><tbody><tr><td>太郎</td><td><a href="/u/1">詳細</a> <button>削除</button></td></tr></tbody></table><ul><li><input type="checkbox" aria-label="選択"></li><li>b</li></ul></body>"#;
    let s = snap(html);
    let all = collect_nodes(&s.tree);
    assert!(all.iter().all(|n| n.table.is_none()));
    for (role, name) in [("link", "詳細"), ("button", "削除"), ("checkbox", "選択")] {
        let n = all
            .iter()
            .find(|n| n.role == role && n.name == name)
            .expect("操作要素が展開されている");
        assert!(n.r#ref.is_some());
    }
}

/// AISNAP-2: tfoot を持つ表は圧縮せず、フッターの可視情報を保持する。
#[test]
fn aisnap_2_table_with_tfoot_is_not_compressed() {
    let html = "<body><table><thead><tr><th>品名</th><th>金額</th></tr></thead><tbody><tr><td>A</td><td>100</td></tr></tbody><tfoot><tr><td>合計</td><td>100</td></tr></tfoot></table></body>";
    let s = snap(html);
    let all = collect_nodes(&s.tree);
    assert!(all.iter().all(|n| n.table.is_none()));
    assert!(all.iter().any(|n| n.name == "合計"));
}

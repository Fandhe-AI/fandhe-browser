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

use fandhe_browser_ai::compress_table::MAX_TABLE_ROWS;
use fandhe_browser_ai::snapshot::{
    CheckedState, DataLeafKind, HeaderCell, MAX_ROW_CONTROLS, MAX_TABLE_CONTROLS, Node, Snapshot,
    State, TableRow, TableSummary, build_snapshot, ref_signature,
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
    // 圧縮した表のヘッダ ref・行内操作要素の ref も一意性の検査対象に含める
    // （AISNAP-2・TASK-12.5・Issue #632）。
    for nd in &nodes {
        if let Some(t) = &nd.table {
            refs.extend(t.header.iter().map(|h| h.r#ref.as_str()));
            refs.extend(
                t.rows
                    .iter()
                    .flat_map(|r| r.controls.iter())
                    .map(|c| c.r#ref.as_str()),
            );
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
                        HeaderCell::new("columnheader", "名前", "ed7fea8519e468d01")
                            .with_data_leaf(DataLeafKind::TableCell),
                        HeaderCell::new("columnheader", "点数", "ee748caeae6097f8b")
                            .with_data_leaf(DataLeafKind::TableCell),
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

/// `AISNAP-2`（Issue #631）: title 付きセルだけを理由に表の圧縮は拒否されない。
#[test]
fn aisnap_2_titled_cells_table_is_compressed() {
    let html = page(
        "<table><tr><th>名前</th><th>値</th></tr>\
         <tr><td title=\"tip\">A</td><td title=\"t2\">80</td></tr></table>",
    );
    let s = snap(&html);
    let t = summary(&s);
    assert_eq!(t.header.len(), 2);
    assert_eq!(t.rows.first(), Some(&TableRow::new("A | 80", false)));
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

/// 圧縮された表・一覧（`Node::table` を持つノード）を先行順で返す。
fn tables(s: &Snapshot) -> Vec<&TableSummary> {
    collect_nodes(&s.tree)
        .into_iter()
        .filter_map(|n| n.table.as_ref())
        .collect()
}

/// 先頭の圧縮表。無ければテスト失敗。
fn first_summary(s: &Snapshot) -> &TableSummary {
    tables(s).into_iter().next().expect("圧縮された表がある")
}

/// 圧縮された表・一覧が 1 つも無い（展開された）か。
fn is_expanded(s: &Snapshot) -> bool {
    tables(s).is_empty()
}

/// `(role, name, disabled)` の列。
fn control_shapes(row: &TableRow) -> Vec<(&str, &str, bool)> {
    row.controls
        .iter()
        .map(|c| (c.role.as_str(), c.name.as_str(), c.state.disabled))
        .collect()
}

fn links_row(n: usize) -> String {
    (0..n)
        .map(|i| format!("<a href=\"/p/{i}\">l{i}</a>"))
        .collect()
}

/// AISNAP-2・AISNAP-13・Issue #632: リンク・ボタンを含む表は圧縮され、行内の操作要素が
/// role・name・state・ref 付きで `TableRow::controls` に入る。子ノードへは展開されない。
#[test]
fn aisnap_2_link_and_button_rows_are_compressed_with_controls() {
    let html = r#"<body><table><thead><tr><th>名前</th><th>操作</th></tr></thead><tbody><tr><td>太郎</td><td><a href="/u/1">詳細</a> <button>削除</button></td></tr><tr><td>花子</td><td><a href="/u/2">詳細</a> <button disabled>削除</button></td></tr></tbody></table></body>"#;
    let s = snap(html);
    assert_common(html, &s);
    let all = collect_nodes(&s.tree);
    let table_node = all.iter().find(|n| n.table.is_some()).expect("圧縮表");
    assert!(table_node.children.is_empty());
    assert!(all.iter().all(|n| n.role != "link" && n.role != "button"));
    let t = first_summary(&s);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(
        control_shapes(t.rows.first().expect("1 行目")),
        vec![("link", "詳細", false), ("button", "削除", false)]
    );
    assert_eq!(
        control_shapes(t.rows.get(1).expect("2 行目")),
        vec![("link", "詳細", false), ("button", "削除", true)]
    );
    assert!(t.rows.iter().all(|r| !r.controls_truncated));
    // 行文字列は従来どおりセル文字列で、control の ref は全て形式が正しい。
    assert_eq!(
        t.rows.first().map(|r| r.text.as_str()),
        Some("太郎 | 詳細 削除")
    );
}

/// AISNAP-2・Issue #632: `ul`・`ol` もリンクだけなら圧縮され、行内リンクが control になる。
#[test]
fn aisnap_2_link_lists_are_compressed_with_controls() {
    let html = r#"<body><ul><li><a href="/a">記事A</a></li><li><a href="/b">記事B</a></li></ul><ol><li><a href="/c">記事C</a></li><li>プレーン</li></ol></body>"#;
    let s = snap(html);
    assert_common(html, &s);
    let ts = tables(&s);
    assert_eq!(ts.len(), 2);
    let ul = ts.first().expect("ul");
    assert_eq!(
        ul.rows.iter().flat_map(control_shapes).collect::<Vec<_>>(),
        vec![("link", "記事A", false), ("link", "記事B", false)]
    );
    let ol = ts.get(1).expect("ol");
    assert_eq!(
        control_shapes(ol.rows.first().expect("1 行目")),
        vec![("link", "記事C", false)]
    );
    assert!(ol.rows.get(1).is_some_and(|r| r.controls.is_empty()));
}

/// AISNAP-2・Issue #632: `input` を含む一覧は従来どおり展開され、ref を持つ。
#[test]
fn aisnap_2_input_rows_are_not_compressed() {
    let html =
        r#"<body><ul><li><input type="checkbox" aria-label="選択"></li><li>b</li></ul></body>"#;
    let s = snap(html);
    assert!(is_expanded(&s));
    let all = collect_nodes(&s.tree);
    let cb = all
        .iter()
        .find(|n| n.role == "checkbox" && n.name == "選択")
        .expect("操作要素が展開されている");
    assert!(cb.r#ref.is_some());
}

/// AISNAP-13・Issue #632: 同一セル内の複数リンクは先頭 1 件に絞らず、全件を文書順で保持する。
#[test]
fn aisnap_13_multiple_links_in_one_cell_are_all_kept_in_order() {
    let html = r#"<body><table><thead><tr><th>タグ</th></tr></thead><tbody><tr><td><a href="/t/1">一</a> <a href="/t/2">二</a> <a href="/t/3">三</a></td></tr><tr><td>x</td></tr></tbody></table></body>"#;
    let s = snap(html);
    let t = first_summary(&s);
    let row = t.rows.first().expect("1 行目");
    assert_eq!(
        control_shapes(row),
        vec![
            ("link", "一", false),
            ("link", "二", false),
            ("link", "三", false)
        ]
    );
    assert!(!row.controls_truncated);
    assert!(
        t.rows
            .get(1)
            .is_some_and(|r| r.controls.is_empty() && !r.controls_truncated)
    );
}

/// AISNAP-10・Issue #632: 行内 control の ref は、同名でも href が違えば別、同名・同 href でも
/// 出現順で別になり、2 回構築で一致し、表外へのバナー挿入で変わらない。
#[test]
fn aisnap_10_row_control_refs_are_unique_and_stable() {
    let table = r#"<table><thead><tr><th>n</th></tr></thead><tbody><tr><td><a href="/x">詳細</a> <a href="/y">詳細</a> <a href="/x">詳細</a></td></tr></tbody></table>"#;
    let html = format!("<body>{table}</body>");
    let s = snap(&html);
    assert_common(&html, &s);
    let refs: Vec<&str> = first_summary(&s)
        .rows
        .iter()
        .flat_map(|r| r.controls.iter())
        .map(|c| c.r#ref.as_str())
        .collect();
    assert_eq!(refs.len(), 3);
    let uniq: HashSet<&str> = refs.iter().copied().collect();
    assert_eq!(uniq.len(), 3, "{refs:?}");
    let bannered = snap(&format!("<body><div>お知らせ</div>{table}</body>"));
    let refs_after: Vec<&str> = first_summary(&bannered)
        .rows
        .iter()
        .flat_map(|r| r.controls.iter())
        .map(|c| c.r#ref.as_str())
        .collect();
    // 兄弟が増えても表（scope）の ref は変わらないため control の ref も変わらない。
    assert_eq!(refs, refs_after);
}

/// AISNAP-10・Issue #632: 行内 control の ref のリテラル固定（ハッシュ・scope 規則の回帰検出）。
#[test]
fn aisnap_10_row_control_ref_literal_is_pinned() {
    let html = r#"<body><ul><li><a href="/a">記事A</a></li><li>b</li></ul></body>"#;
    let s = snap(html);
    let c = first_summary(&s)
        .rows
        .first()
        .and_then(|r| r.controls.first())
        .expect("control がある");
    assert_eq!(c.r#ref, "eca61c2b53fd46633");
}

/// AISNAP-2・Issue #632: 維持される拒否条件（ref・name・分類を失う構造）では、従来どおり
/// 圧縮せず展開し、対象ノードが ref 付きで残る。
#[test]
fn aisnap_2_lossy_structures_with_links_stay_expanded() {
    let wrap = |cell: &str| {
        format!(
            "<body><table><thead><tr><th>n</th></tr></thead><tbody><tr><td>{cell}</td></tr><tr><td>b</td></tr></tbody></table></body>"
        )
    };
    let cases: Vec<(&str, String)> = vec![
        ("caption", "<body><table><caption>表題</caption><thead><tr><th>n</th></tr></thead><tbody><tr><td><a href=\"/a\">A</a></td></tr><tr><td>b</td></tr></tbody></table></body>".to_string()),
        ("tfoot", "<body><table><thead><tr><th>n</th></tr></thead><tbody><tr><td><a href=\"/a\">A</a></td></tr></tbody><tfoot><tr><td>合計</td></tr></tfoot></table></body>".to_string()),
        ("heading", wrap("<h3>見出し</h3><a href=\"/a\">A</a>")),
        ("img alt", wrap("<a href=\"/a\">A</a><img src=\"/i.png\" alt=\"画像\">")),
        ("aria-label", wrap("<a href=\"/a\" aria-label=\"ラベル\">A</a>")),
        ("aria-labelledby", wrap("<a href=\"/a\" aria-labelledby=\"x\">A</a>")),
        ("title only", wrap("<a href=\"/a\" title=\"題名\"></a>")),
        ("price leaf", wrap("<span class=\"price\">1</span><a href=\"/a\">A</a>")),
        ("input", wrap("<input type=\"text\" aria-label=\"入力\"><a href=\"/a\">A</a>")),
        ("select", wrap("<select aria-label=\"選択\"></select><a href=\"/a\">A</a>")),
        ("textarea", wrap("<textarea aria-label=\"本文\"></textarea><a href=\"/a\">A</a>")),
        ("tabindex", wrap("<a href=\"/a\" tabindex=\"0\">A</a>")),
        ("onclick", wrap("<button onclick=\"f()\">B</button>")),
        ("role", wrap("<a href=\"/a\" role=\"tab\">A</a>")),
        ("header link", "<body><table><thead><tr><th><a href=\"/sort\">名前</a></th></tr></thead><tbody><tr><td>a</td></tr><tr><td>b</td></tr></tbody></table></body>".to_string()),
    ];
    for (label, html) in cases {
        let s = snap(&html);
        assert!(is_expanded(&s), "{label} は展開される");
        // 展開された操作要素（リンク・ボタン）は ref を持つ。
        assert!(
            collect_nodes(&s.tree)
                .iter()
                .filter(|n| n.role == "link" || n.role == "button")
                .all(|n| n.r#ref.is_some()),
            "{label}"
        );
    }
}

/// AISNAP-13・AISNAP-12・Issue #632: 行あたり上限を超えると `MAX_ROW_CONTROLS` 件に絞り
/// `controls_truncated` を立てる。超過行末尾のページネーションリンクは優先して残す。
#[test]
fn aisnap_13_row_control_cap_keeps_pagination_and_flags_truncation() {
    let over = MAX_ROW_CONTROLS + 1;
    let cell = format!(
        "{}<a href=\"/next\" rel=\"next\">Next</a>",
        links_row(MAX_ROW_CONTROLS)
    );
    let html = format!(
        "<body><table><thead><tr><th>n</th></tr></thead><tbody><tr><td>{cell}</td></tr><tr><td>{}</td></tr></tbody></table></body>",
        links_row(MAX_ROW_CONTROLS)
    );
    let s = snap(&html);
    assert_common(&html, &s);
    let t = first_summary(&s);
    let row = t.rows.first().expect("1 行目");
    assert_eq!(over, MAX_ROW_CONTROLS + 1);
    assert_eq!(row.controls.len(), MAX_ROW_CONTROLS);
    assert!(row.controls_truncated);
    let names: Vec<&str> = row.controls.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names.last(), Some(&"Next"));
    assert_eq!(names.first(), Some(&"l0"));
    // 上限ちょうどの行は省略なし。
    let exact = t.rows.get(1).expect("2 行目");
    assert_eq!(exact.controls.len(), MAX_ROW_CONTROLS);
    assert!(!exact.controls_truncated);
}

/// AISNAP-13・Issue #632: コンテナ全体の上限を超える表では合計が上限に等しく、超過した行の
/// フラグが立つ。上限までに収まった行は立たない。
#[test]
fn aisnap_13_table_control_cap_flags_rows_beyond_budget() {
    let rows: String = (0..20)
        .map(|_| format!("<tr><td>{}</td></tr>", links_row(MAX_ROW_CONTROLS)))
        .collect();
    let html = format!(
        "<body><table><thead><tr><th>n</th></tr></thead><tbody>{rows}</tbody></table></body>"
    );
    let s = snap(&html);
    assert_common(&html, &s);
    let t = first_summary(&s);
    let total: usize = t.rows.iter().map(|r| r.controls.len()).sum();
    assert_eq!(total, MAX_TABLE_CONTROLS);
    let full = MAX_TABLE_CONTROLS / MAX_ROW_CONTROLS;
    for (i, r) in t.rows.iter().enumerate() {
        if i < full {
            assert_eq!(r.controls.len(), MAX_ROW_CONTROLS, "行 {i}");
            assert!(!r.controls_truncated, "行 {i}");
        } else {
            assert!(r.controls.is_empty(), "行 {i}");
            assert!(r.controls_truncated, "行 {i}");
        }
    }
}

/// AISNAP-12・Issue #632: 20 行超の表で、キャップ外のページネーション行も圧縮行として残り、
/// その control が ref を持つ。省略行の分は保持しない。
#[test]
fn aisnap_12_pagination_row_beyond_cap_keeps_control_with_ref() {
    let rows: String = (0..40)
        .map(|i| {
            if i == 30 {
                "<tr><td><a href=\"/next\" rel=\"next\">Next</a></td></tr>".to_string()
            } else {
                format!("<tr><td>row{i}</td></tr>")
            }
        })
        .collect();
    let html = format!(
        "<body><table><thead><tr><th>n</th></tr></thead><tbody>{rows}</tbody></table></body>"
    );
    let s = snap(&html);
    assert_common(&html, &s);
    let t = first_summary(&s);
    assert_eq!(t.rows.len(), 20);
    assert_eq!(t.truncated_rows, 20);
    let nexts: Vec<_> = t
        .rows
        .iter()
        .flat_map(|r| r.controls.iter())
        .filter(|c| c.name == "Next")
        .collect();
    assert_eq!(nexts.len(), 1);
    assert!(nexts.iter().all(|c| c.r#ref.starts_with('e')));
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
// ---------------------------------------------------------------------------
// Hacker News 相当フィクスチャの回帰テスト（TASK-12.6・Issue #84・`AISNAP-2`・`MS-2`）
// ---------------------------------------------------------------------------

/// Hacker News のレイアウトを踏襲した合成ページ（値はすべてダミー。実ページの複写ではない）。
///
/// 構造: 入れ子のレイアウト表・ヘッダ表・記事表（題名行・subtext 行・spacer 行の繰り返しと
/// 末尾の More 行）・フッタ。`with_titles` が true のとき、実ページ同様に
/// `div.votearrow` と `span.age` へ `title` 属性を付ける。この 2 つは `generic` の
/// accessible name になり、現行実装では圧縮を拒否させる（`build.rs` の `can_compress`
/// の「既知の制約」）。
fn hn_page(stories: usize, with_titles: bool) -> String {
    let (vote_title, age_title) = if with_titles {
        (" title=\"upvote\"", " title=\"2026-01-01T00:00:00\"")
    } else {
        ("", "")
    };
    let mut rows = String::new();
    for i in 1..=stories {
        rows.push_str(&format!(
            "<tr class=\"athing\" id=\"{i}\"><td align=\"right\" valign=\"top\" class=\"title\"><span class=\"rank\">{i}.</span></td>\
<td valign=\"top\" class=\"votelinks\"><center><a id=\"up_{i}\" href=\"vote?id={i}&amp;how=up\"><div class=\"votearrow\"{vote_title}></div></a></center></td>\
<td class=\"title\"><span class=\"titleline\"><a href=\"https://example.com/s/{i}\">Story headline number {i}</a>\
<span class=\"sitebit comhead\"> (<a href=\"from?site=example.com\"><span class=\"sitestr\">example.com</span></a>)</span></span></td></tr>\
<tr><td colspan=\"2\"></td><td class=\"subtext\"><span class=\"subline\"><span class=\"score\" id=\"score_{i}\">{i} points</span> by \
<a href=\"user?id=user{i}\" class=\"hnuser\">user{i}</a> <span class=\"age\"{age_title}><a href=\"item?id={i}\">{i} hours ago</a></span> \
<span id=\"unv_{i}\"></span> | <a href=\"hide?id={i}\">hide</a> | <a href=\"item?id={i}\">{i} comments</a></span></td></tr>\
<tr class=\"spacer\" style=\"height:5px\"></tr>"
        ));
    }
    let body = format!(
        "<center><table id=\"hnmain\" border=\"0\" cellpadding=\"0\" cellspacing=\"0\" width=\"85%\" bgcolor=\"#f6f6ef\">\
<tr><td bgcolor=\"#ff6600\"><table border=\"0\" cellpadding=\"0\" cellspacing=\"0\" width=\"100%\" style=\"padding:2px\"><tr>\
<td style=\"width:18px;padding-right:4px\"><a href=\"https://example.com\"><img src=\"logo.gif\" width=\"18\" height=\"18\" alt=\"Y\"></a></td>\
<td style=\"line-height:12pt; height:10px;\"><span class=\"pagetop\"><b class=\"hnname\"><a href=\"news\">Sample News</a></b>\
<a href=\"newest\">new</a> | <a href=\"front\">past</a> | <a href=\"newcomments\">comments</a> | <a href=\"ask\">ask</a> | <a href=\"show\">show</a> | <a href=\"jobs\">jobs</a> | <a href=\"submit\">submit</a></span></td>\
<td style=\"text-align:right;padding-right:4px;\"><span class=\"pagetop\"><a href=\"login?goto=news\">login</a></span></td></tr></table></td></tr>\
<tr id=\"pagespace\" title=\"\" style=\"height:10px\"></tr>\
<tr><td><table border=\"0\" cellpadding=\"0\" cellspacing=\"0\">{rows}\
<tr class=\"morespace\" style=\"height:10px\"></tr><tr><td colspan=\"2\"></td><td class=\"title\"><a href=\"?p=2\" class=\"morelink\" rel=\"next\">More</a></td></tr>\
</table></td></tr>\
<tr><td><table width=\"100%\" cellspacing=\"0\" cellpadding=\"1\"><tr><td bgcolor=\"#ff6600\"></td></tr></table><br>\
<center><span class=\"yclinks\"><a href=\"newsguidelines.html\">Guidelines</a> | <a href=\"newsfaq.html\">FAQ</a> | <a href=\"lists\">Lists</a> | <a href=\"security.html\">Security</a></span><br><br>\
<form method=\"get\" action=\"//search.example.com/\">Search: <input type=\"text\" name=\"q\" size=\"17\" autocorrect=\"off\" spellcheck=\"false\" autocapitalize=\"off\" autocomplete=\"off\"></form></center></td></tr>\
</table></center>"
    );
    page(&body)
}

/// テスト内の量の代理指標: Snapshot を 1 ノード 1 行（role・name・ref）で描画した文字列。
///
/// これは決定的な「バイト長の代理指標」でありトークン数ではない。正規のシリアライズ形式は
/// TASK-19（`AISNAP-6`）、トークン実測は TASK-14・TASK-23 が担う。圧縮表は header・行テキスト・
/// 行内操作要素（role・name・ref）・`truncated_rows` を全て出力し、情報を落として有利に見せない。
/// 明示スタックの反復で走査する（再帰・添字なし）。
fn render_text(s: &Snapshot) -> String {
    let mut out = String::new();
    let mut stack = vec![(&s.tree, 0usize)];
    while let Some((node, depth)) = stack.pop() {
        let pad = "  ".repeat(depth);
        out.push_str(&format!(
            "{pad}{} {:?} {}\n",
            node.role,
            node.name,
            node.r#ref.as_deref().unwrap_or("-")
        ));
        if let Some(t) = &node.table {
            for h in &t.header {
                out.push_str(&format!(
                    "{pad}  header {} {:?} {}\n",
                    h.role, h.name, h.r#ref
                ));
            }
            for row in &t.rows {
                out.push_str(&format!("{pad}  row {:?}\n", row.text));
                for c in &row.controls {
                    out.push_str(&format!("{pad}    {} {:?} {}\n", c.role, c.name, c.r#ref));
                }
            }
            out.push_str(&format!("{pad}  truncated_rows {}\n", t.truncated_rows));
        }
        stack.extend(node.children.iter().rev().map(|c| (c, depth + 1)));
    }
    out
}

/// 行数が `min_rows` 以上の圧縮表のうち行数最大のもの。
fn largest_table(s: &Snapshot, min_rows: usize) -> Option<&TableSummary> {
    tables(s)
        .into_iter()
        .filter(|t| t.rows.len() >= min_rows)
        .max_by_key(|t| t.rows.len())
}

/// 圧縮表の行内操作要素の ref を文書順で全て集める。
fn row_control_refs(t: &TableSummary) -> Vec<&str> {
    t.rows
        .iter()
        .flat_map(|r| r.controls.iter().map(|c| c.r#ref.as_str()))
        .collect()
}

/// `AISNAP-2`・`AISNAP-10`・`AISNAP-12`・`AISNAP-13`・TASK-12.6・Issue #84:
/// 現行実装が圧縮できる HN 形状のサブセット（記事数 9・`title` 属性なし）では、
/// 記事表が行へ圧縮され、簡約表現が生 HTML より小さい。
///
/// 忠実な HN ページとの差分は `title` 属性 2 種（`votearrow`・`age`）の除去と、記事数が
/// `MAX_TABLE_ROWS` 以内に収まる点。両者の理由と現状は
/// `aisnap_2_faithful_hn_page_stays_expanded_known_gap` を参照。
#[test]
fn aisnap_2_hn_like_list_is_compressed_and_smaller_than_html() {
    let stories = (MAX_TABLE_ROWS - 1) / 2;
    let html = hn_page(stories, false);
    let s = snap(&html);

    let t = largest_table(&s, 2).expect("記事表は圧縮される");
    assert_eq!(t.rows.len(), 2 * stories + 1);
    assert_eq!(t.truncated_rows, 0);
    assert!(t.header.is_empty());

    // 題名行: 投票リンク（空名）・題名リンク・サイトリンクの 3 操作要素。
    let first = t.rows.first().expect("先頭行がある");
    assert_eq!(
        control_shapes(first),
        vec![
            ("link", "", false),
            ("link", "Story headline number 1", false),
            ("link", "example.com", false),
        ]
    );
    // subtext 行: ユーザー・経過時間・hide・コメントの 4 リンク。
    let second = t.rows.get(1).expect("2 行目がある");
    assert_eq!(
        control_shapes(second),
        vec![
            ("link", "user1", false),
            ("link", "1 hours ago", false),
            ("link", "hide", false),
            ("link", "1 comments", false),
        ]
    );
    // 末尾の More（ページング）リンクが ref 付きで残る（AISNAP-12）。
    let last = t.rows.last().expect("末尾行がある");
    assert_eq!(control_shapes(last), vec![("link", "More", false)]);

    // 操作要素は全件保持され、ref は形式どおりで一意（AISNAP-10・AISNAP-13）。
    let refs = row_control_refs(t);
    assert_eq!(refs.len(), 7 * stories + 1);
    assert!(refs.iter().all(|r| is_ref_shaped(r)));
    let unique: HashSet<&str> = refs.iter().copied().collect();
    assert_eq!(unique.len(), refs.len());

    // 決定性。
    assert_eq!(s, snap(&html));

    // 量: 簡約表現は生 HTML より小さい（バイト長の代理指標。トークン数ではない）。
    let rendered = render_text(&s);
    assert!(
        rendered.len() < html.len(),
        "rendered {} bytes must be smaller than html {} bytes",
        rendered.len(),
        html.len()
    );
}

/// `AISNAP-2`・`AISNAP-13`・TASK-12.6・Issue #84: 既知のギャップの固定。
/// 忠実な HN 形状（30 記事・`title` 属性あり）の記事表は現行実装では圧縮されず、
/// 全ノードが展開される。原因は独立に 3 つあり、いずれか 1 つで圧縮が拒否される。
///
/// 1. `div.votearrow[title]`: `generic` の `title` が accessible name になり、圧縮が
///    情報を落とすと判定される（`build.rs` の `can_compress`）。
/// 2. `span.age[title]`: 同上。
/// 3. 非空行が `MAX_TABLE_ROWS` を超え、省略行にリンクがあると圧縮は拒否される
///    （`AISNAP-13`・Issue #632）。
///
/// 本テストは望ましくない現状を期待値にしている。解消した時点で期待値を反転させること。
/// この形状では生 HTML 比の削減は未達（展開表現は生 HTML より大きい）のため、
/// 量の assert はしない。
#[test]
fn aisnap_2_faithful_hn_page_stays_expanded_known_gap() {
    let stories = 30;
    let faithful = snap(&hn_page(stories, true));
    assert!(largest_table(&faithful, 2).is_none());

    // 切り分け: title を外しても（行数上限＋操作要素が原因で）未圧縮。
    assert!(largest_table(&snap(&hn_page(stories, false)), 2).is_none());
    // 切り分け: 記事数を上限内にしても（title が原因で）未圧縮。
    let within_cap = (MAX_TABLE_ROWS - 1) / 2;
    assert!(largest_table(&snap(&hn_page(within_cap, true)), 2).is_none());

    // 展開されても情報は失われない: 記事リンク・More リンクが ref 付きで残る。
    let nodes = collect_nodes(&faithful.tree);
    for name in [
        "Story headline number 1",
        "Story headline number 30",
        "More",
    ] {
        let node = nodes
            .iter()
            .find(|n| n.role == "link" && n.name == name)
            .unwrap_or_else(|| panic!("link {name:?} must remain"));
        assert!(node.r#ref.as_deref().is_some_and(is_ref_shaped));
    }
}

//! PoC-4 の情報保持チェック（代表タスク 7 種）を Rust の結合テストへ移植し、
//! 簡約表現（`Snapshot`）から対象要素を判別できるタスク数が 7 件中 6 件以上である
//! ことを機械的に確認する（`AISNAP-3`・TASK-13.4・Issue #89・`MS-2`）。
//!
//! 呼び出し文脈: core の `parse_document` で得た `Document` を ai の `build_snapshot`
//! へ渡し、`Snapshot` 内で対象要素が判別できるかを検査する。`data_leaf` 判定の
//! 個別ロジックは `src/data_leaf.rs`・`src/snapshot/build.rs` のユニットテスト
//! （TASK-13.1〜13.3）が担い、本ファイルは受入基準の集計だけを担う。
//!
//! PoC の `task_check.mjs` との対応: `Node` は DOM の `NodeId` を持たず ref から DOM
//! への公開リゾルバも無いため、DOM 上の対象要素と Snapshot 上のノードをテスト側の
//! 添字対応付け（`map_to_snapshot`）で結び付け、role・name・data_leaf の一致で
//! 対応付けの正しさを確認する。判別可能の条件は次の 3 点。
//! 1. 対応ノードが ref を持つ（PoC の `found`）。
//! 2. その ref が形式上妥当で木全体で一意。ref から DOM を復元できること（PoC の
//!    `restoreOk`）は検証しない（下記「未検証の契約」）。
//! 3. snapshot の情報だけで作る述語の「先行順で最初の一致」が対象ノードと一致する。
//!
//! 値の確認（`expected_text`）の範囲: Snapshot は価格等のテキスト値を保持しないため、
//! 値そのものの保持ではなく「Snapshot 木の添字位置から DOM を再特定して値に到達できる
//! こと」を検証する。これは ref ではなく子添字の位置による再特定であり、ref の復元性は
//! 保証しない。name を持つノード（リンク・表見出し）は name も照合する。
//!
//! 未検証の契約（ref から対象 DOM 要素を解決できること）: ref から DOM への公開リゾルバが
//! 未提供のため本ファイルでは検証せず、`restoreOk` 相当を満たしたとは主張しない。リゾルバ
//! 提供後に ref 経由の解決テストへ差し替える（`AISNAP-3` は判別可能性の集計、ref 復元性は
//! `AISNAP-10` 側の契約）。
//!
//! フィクスチャ制約: `<body>` 内に `build_snapshot` が除外する要素（`script`・`style`・
//! `noscript`・`template`・`hidden`・`aria-hidden="true"`・`input[type=hidden]`）を
//! 置かない。除外されると添字対応付けがずれるため。
//!
//! フィクスチャは PoC の 7 ページの構造を模した合成 HTML で、値はすべてダミー
//! （実サイトの HTML・外部通信・実資格情報なし。`docs/spec` も参照しない）。
//! ref のリテラルは固定しない（ref の安定性 `AISNAP-10` は `tests/snapshot.rs` の担当）。

use std::collections::HashSet;

use fandhe_browser_ai::data_leaf::classify_data_leaf;
use fandhe_browser_ai::snapshot::{
    CheckedState, DataLeafKind, Node, Snapshot, build_snapshot, compute_name, compute_role,
};
use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_str;

/// 判別可能とみなす最小タスク数（`AISNAP-3` の受入基準: 7 件中 6 件以上）。
const REQUIRED_DISCRIMINABLE: usize = 6;

/// 1. ログインフォーム。
const LOGIN_FORM: &str = r#"<!DOCTYPE html><html><head><title>Login</title></head><body><h2>Login Page</h2><form><label for="u">Username</label><input type="text" id="u" name="username"><label for="p">Password</label><input type="password" id="p" name="password"><button type="submit">Login</button></form></body></html>"#;

/// 2. EC 商品一覧（価格クラス要素）。
const EC_PRODUCT_LIST: &str = r#"<!DOCTYPE html><html><head><title>Books</title></head><body><ol><li><article class="product_pod"><h3><a href="/item-1.html">Sample Book One</a></h3><p class="price_color">10.00</p><button type="button">Add to basket</button></article></li><li><article class="product_pod"><h3><a href="/item-2.html">Sample Book Two</a></h3><p class="price_color">20.50</p><button type="button">Add to basket</button></article></li><li><article class="product_pod"><h3><a href="/item-3.html">Sample Book Three</a></h3><p class="price_color">30.99</p><button type="button">Add to basket</button></article></li></ol></body></html>"#;

/// 3. 数値入力フォーム。
const INPUTS_FORM: &str = r#"<!DOCTYPE html><html><head><title>Inputs</title></head><body><h3>Inputs</h3><div><input type="number"></div></body></html>"#;

/// 4. ダッシュボード表（後続に `#table2` のディストラクタ）。
const DASHBOARD_TABLE: &str = r#"<!DOCTYPE html><html><head><title>Tables</title></head><body><h3>Data Tables</h3><table id="table1"><thead><tr><th><span>Last Name</span></th><th><span>First Name</span></th><th><span>Due</span></th></tr></thead><tbody><tr><td>Doe</td><td>Jane</td><td>50.00</td></tr><tr><td>Roe</td><td>Rick</td><td>12.00</td></tr></tbody></table><table id="table2"><thead><tr><th><span>Surname</span></th><th><span>Given</span></th></tr></thead><tbody><tr><td>Poe</td><td>Pat</td></tr></tbody></table></body></html>"#;

/// 表見出しのネガティブコントロール用。表より前に同じ role・name の非データ葉を置く。
const HEADING_AMBIGUOUS: &str = r#"<!DOCTYPE html><html><head><title>Tables</title></head><body><div role="columnheader">Last Name</div><table><thead><tr><th>Last Name</th></tr></thead><tbody><tr><td>Doe</td></tr></tbody></table></body></html>"#;

/// 5. ドロップダウン。
const DROPDOWN_FORM: &str = r#"<!DOCTYPE html><html><head><title>Dropdown</title></head><body><h3>Dropdown List</h3><select id="dropdown"><option value="">Please select an option</option><option value="1">Option 1</option><option value="2">Option 2</option></select></body></html>"#;

/// 6. HN 風リスト（レイアウト用入れ子テーブル・ナビリンクのディストラクタ）。
const HN_LIST: &str = r#"<!DOCTYPE html><html><head><title>News</title></head><body><table><tr><td><table><tr><td><a href="https://example.com/">Home</a></td><td><a href="https://example.com/new">new</a> | <a href="https://example.com/past">past</a></td></tr></table></td></tr><tr><td><table><tr class="athing"><td class="title"><span class="titleline"><a href="https://example.com/a">Sample Story Alpha</a></span></td></tr><tr class="athing"><td class="title"><span class="titleline"><a href="https://example.com/b">Sample Story Beta</a></span></td></tr><tr class="athing"><td class="title"><span class="titleline"><a href="https://example.com/c">Sample Story Gamma</a></span></td></tr></table></td></tr></table></body></html>"#;

/// 7. チェックボックス。
const CHECKBOXES_FORM: &str = r#"<!DOCTYPE html><html><head><title>Checkboxes</title></head><body><h3>Checkboxes</h3><form id="checkboxes"><input type="checkbox"> checkbox 1<br><input type="checkbox" checked> checkbox 2</form></body></html>"#;

/// 代表タスクの判別結果（真偽値だけにしない。REPAIR-4）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// snapshot から対象要素を判別できた。
    Discriminable,
    /// 対応ノードが ref を持たない、または ref の形が不正。
    NoRef,
    /// 対応ノードの ref が木全体で一意でない。
    RefNotUnique,
    /// 述語の最初の一致が対象ノードと異なる（または一致なし）。
    PredicateMismatch,
    /// Snapshot の name、または木の添字位置で再特定した DOM の値が期待と異なる（ref 経由ではない）。
    ValueMismatch(String),
}

/// 代表タスクの定義。
struct Task {
    name: &'static str,
    html: &'static str,
    /// 対象要素の CSS セレクタ。
    selector: &'static str,
    /// snapshot の情報だけでエージェントが対象を特定する述語。
    predicate: fn(&Node) -> bool,
    /// データ系タスクで、対象要素の text_content の期待値。
    expected_text: Option<&'static str>,
}

fn is_login_button(n: &Node) -> bool {
    n.role == "button" && n.name == "Login"
}
fn is_first_price(n: &Node) -> bool {
    n.data_leaf == Some(DataLeafKind::PriceClass)
}
fn is_number_input(n: &Node) -> bool {
    n.role == "spinbutton"
}
fn is_table_heading(n: &Node) -> bool {
    n.data_leaf == Some(DataLeafKind::TableCell) && n.role == "columnheader"
}
fn is_dropdown(n: &Node) -> bool {
    n.role == "combobox"
}
fn is_top_story_link(n: &Node) -> bool {
    n.role == "link" && n.name == "Sample Story Alpha"
}
fn is_first_checkbox(n: &Node) -> bool {
    n.role == "checkbox" && n.state.checked == Some(CheckedState::Unchecked)
}

/// PoC-4 の 7 タスク。
fn tasks() -> Vec<Task> {
    vec![
        Task {
            name: "login-button",
            html: LOGIN_FORM,
            selector: "button[type=submit]",
            predicate: is_login_button,
            expected_text: None,
        },
        Task {
            name: "ec-price",
            html: EC_PRODUCT_LIST,
            selector: "p.price_color",
            predicate: is_first_price,
            expected_text: Some("10.00"),
        },
        Task {
            name: "number-input",
            html: INPUTS_FORM,
            selector: "input[type=number]",
            predicate: is_number_input,
            expected_text: None,
        },
        Task {
            name: "table-heading",
            html: DASHBOARD_TABLE,
            selector: "table#table1 th",
            predicate: is_table_heading,
            expected_text: Some("Last Name"),
        },
        Task {
            name: "dropdown",
            html: DROPDOWN_FORM,
            selector: "select#dropdown",
            predicate: is_dropdown,
            expected_text: None,
        },
        Task {
            name: "top-story-link",
            html: HN_LIST,
            selector: ".athing .titleline > a",
            predicate: is_top_story_link,
            expected_text: Some("Sample Story Alpha"),
        },
        Task {
            name: "checkbox",
            html: CHECKBOXES_FORM,
            selector: "input[type=checkbox]",
            predicate: is_first_checkbox,
            expected_text: None,
        },
    ]
}

fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("フィクスチャのパースは成功する")
        .document
}

fn snap(doc: &Document) -> Snapshot {
    build_snapshot(doc).expect("フィクスチャの構築は成功する")
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

/// ref が `ElementRef::to_ref_string` の契約 `e<16hex>[v<n>][-<n>]` の形か。
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
    let all_digits = |d: &str| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit());
    let mut tail: &str = rest.get(16..).unwrap_or("");
    if let Some(after_v) = tail.strip_prefix('v') {
        let end = after_v.find('-').unwrap_or(after_v.len());
        let (variant, remain) = after_v.split_at(end);
        if !all_digits(variant) {
            return false;
        }
        tail = remain;
    }
    tail.is_empty() || tail.strip_prefix('-').is_some_and(all_digits)
}

/// `build_snapshot` が `Node` 化する子か（フィクスチャ制約下では要素かつ `head` でない）。
fn is_snapshot_child(doc: &Document, id: NodeId) -> bool {
    doc.is_element(id) && doc.local_name(id) != Some("head")
}

/// DOM 上の `target` に対応する Snapshot 上のノードを、ルートからの添字で辿って返す。
/// role・name・data_leaf の一致まで確認し、ずれは対応付け自体の不備として失敗させる。
fn map_to_snapshot<'a>(doc: &Document, snapshot: &'a Snapshot, target: NodeId) -> &'a Node {
    // target からルートへ向かって、各段での「Snapshot 上の子としての添字」を集める。
    let mut path = Vec::new();
    let mut cur = target;
    while let Some(parent) = doc.parent(cur) {
        let idx = doc
            .children(parent)
            .filter(|c| is_snapshot_child(doc, *c))
            .position(|c| c == cur)
            .expect("対象は親の Snapshot 対象の子に含まれる");
        path.push(idx);
        cur = parent;
    }
    let mut node = &snapshot.tree;
    for idx in path.iter().rev() {
        node = node
            .children
            .get(*idx)
            .expect("添字に対応する Snapshot ノードがある");
    }
    let expected_role = compute_role(doc, target).map_or("generic", |r| r.as_str());
    assert_eq!(node.role, expected_role, "対応付けの role が一致する");
    assert_eq!(
        node.name,
        compute_name(doc, target).text,
        "対応付けの name が一致する"
    );
    assert_eq!(
        node.data_leaf,
        classify_data_leaf(doc, target),
        "対応付けの data_leaf が一致する"
    );
    node
}

/// 述語に最初に一致するノードへの、ルートからの子添字の列を先行順で返す。
fn first_path(root: &Node, pred: fn(&Node) -> bool) -> Option<Vec<usize>> {
    let mut stack: Vec<(&Node, Vec<usize>)> = vec![(root, Vec::new())];
    while let Some((node, path)) = stack.pop() {
        if pred(node) {
            return Some(path);
        }
        for (i, child) in node.children.iter().enumerate().rev() {
            let mut p = path.clone();
            p.push(i);
            stack.push((child, p));
        }
    }
    None
}

/// Snapshot 上の添字列を DOM へ逆引きする（`map_to_snapshot` の逆。フィクスチャ制約が前提）。
fn dom_at_path(doc: &Document, path: &[usize]) -> Option<NodeId> {
    let mut cur = doc.root();
    for idx in path {
        cur = doc
            .children(cur)
            .filter(|c| is_snapshot_child(doc, *c))
            .nth(*idx)?;
    }
    Some(cur)
}

/// 1 タスクの判別結果を返す。
fn evaluate(task: &Task) -> Outcome {
    let doc = parse(task.html);
    let snapshot = snap(&doc);
    let target = query_selector_str(&doc, doc.root(), task.selector)
        .expect("セレクタは有効")
        .expect("フィクスチャに対象要素がある");
    let mapped = map_to_snapshot(&doc, &snapshot, target);

    let Some(target_ref) = mapped.r#ref.as_deref().filter(|r| is_ref_shaped(r)) else {
        return Outcome::NoRef;
    };
    let nodes = all_nodes(&snapshot.tree);
    let same_ref = nodes
        .iter()
        .filter(|n| n.r#ref.as_deref() == Some(target_ref))
        .count();
    if same_ref != 1 {
        return Outcome::RefNotUnique;
    }
    let first = nodes.iter().find(|n| (task.predicate)(n));
    if first.and_then(|n| n.r#ref.as_deref()) != Some(target_ref) {
        return Outcome::PredicateMismatch;
    }
    if let Some(expected) = task.expected_text {
        // Snapshot は価格などのテキスト値を持たない（`Node` に text フィールドは無い）。
        // そのため値の確認は次の 2 段に分ける。
        // (a) Snapshot 側: name が空でなければ（リンク・表見出し）name が期待値と一致する。
        // (b) 添字位置による DOM 再特定: 述語が選んだ Snapshot 上の子添字位置から DOM を
        //     逆引きし、その要素の text_content が期待値と一致する。ref を使った復元では
        //     なく（リゾルバ未提供）、`restoreOk` 相当の検証ではない。値そのものが Snapshot に
        //     残ることではなく、Snapshot の構造だけで値の在処へ辿り着けることを検証する。
        let name = first.map(|n| n.name.as_str()).unwrap_or_default();
        if !name.is_empty() && name != expected {
            return Outcome::ValueMismatch(name.to_string());
        }
        let actual = first_path(&snapshot.tree, task.predicate)
            .and_then(|path| dom_at_path(&doc, &path))
            .and_then(|id| doc.text_content(id))
            .unwrap_or_default();
        if actual != expected {
            return Outcome::ValueMismatch(actual);
        }
    }
    Outcome::Discriminable
}

/// AISNAP-3 回帰固定: 現状 7 タスクすべてが `Discriminable` であることを具体値で固定する。
///
/// 受入基準（7 件中 6 件以上）より意図的に厳しい。基準の判定は直後の
/// `aisnap_3_discriminable_tasks_at_least_6_of_7` が担い、本テストは 1 件でも退行したら
/// 気付くための固定であり、許容する非判別タスクは無い。
#[test]
fn aisnap_3_all_seven_tasks_currently_discriminable_regression_pin() {
    let got: Vec<(&str, Outcome)> = tasks().iter().map(|t| (t.name, evaluate(t))).collect();
    let expected: Vec<(&str, Outcome)> = [
        "login-button",
        "ec-price",
        "number-input",
        "table-heading",
        "dropdown",
        "top-story-link",
        "checkbox",
    ]
    .into_iter()
    .map(|n| (n, Outcome::Discriminable))
    .collect();
    assert_eq!(got, expected);
}

/// AISNAP-3: 判別可能タスク数が 7 件中 6 件以上（PoC-4 実測 6/7 と同水準）。
#[test]
fn aisnap_3_discriminable_tasks_at_least_6_of_7() {
    let results: Vec<(&str, Outcome)> = tasks().iter().map(|t| (t.name, evaluate(t))).collect();
    assert_eq!(results.len(), 7, "代表タスクは 7 種");
    let ok = results
        .iter()
        .filter(|(_, o)| *o == Outcome::Discriminable)
        .count();
    assert!(
        ok >= REQUIRED_DISCRIMINABLE,
        "判別可能 {ok}/7（要 {REQUIRED_DISCRIMINABLE} 以上）: {results:?}"
    );
}

/// AISNAP-3 ネガティブコントロール: 価格・表見出しは role/name の述語だと別ノードを選び、
/// `data_leaf` を併用して初めて対象を選べる（同 role・name の別ノードの存在を検証）。
#[test]
fn aisnap_3_data_tasks_rely_on_data_leaf() {
    // 価格: 対象は (generic, "") で、同じ組の非価格ノードが木に存在する。
    let doc = parse(EC_PRODUCT_LIST);
    let snapshot = snap(&doc);
    let target = query_selector_str(&doc, doc.root(), "p.price_color")
        .expect("セレクタは有効")
        .expect("価格要素がある");
    let mapped = map_to_snapshot(&doc, &snapshot, target);
    assert_eq!(
        (mapped.role.as_str(), mapped.name.as_str()),
        ("generic", "")
    );
    let nodes = all_nodes(&snapshot.tree);
    let same_shape = nodes
        .iter()
        .filter(|n| n.role == "generic" && n.name.is_empty())
        .count();
    assert!(
        same_shape > 1,
        "role/name だけでは価格を特定できない: {same_shape}"
    );
    let price_leaves = nodes
        .iter()
        .filter(|n| n.data_leaf == Some(DataLeafKind::PriceClass))
        .count();
    assert_eq!(price_leaves, 3, "価格葉は 3 件で、先頭が対象");

    // 価格: (generic, "") の最初の一致は対象ではなく、data_leaf なら対象を選べる。
    let first_shape = nodes
        .iter()
        .find(|n| n.role == "generic" && n.name.is_empty())
        .expect("同形ノードがある");
    assert_ne!(
        first_shape.r#ref, mapped.r#ref,
        "role/name では別ノードを選ぶ"
    );
    let first_price = nodes
        .iter()
        .find(|n| n.data_leaf == Some(DataLeafKind::PriceClass))
        .expect("価格葉がある");
    assert_eq!(
        first_price.r#ref, mapped.r#ref,
        "data_leaf なら対象を選べる"
    );

    // 表見出し: 同じ role・name を持つ非データ葉（role 上書きの div）が表より前にあり、
    // role/name の述語はそれを選ぶ。data_leaf 併用なら表の th を選べる。
    let doc = parse(HEADING_AMBIGUOUS);
    let snapshot = snap(&doc);
    let th = query_selector_str(&doc, doc.root(), "table th")
        .expect("セレクタは有効")
        .expect("th がある");
    let decoy = query_selector_str(&doc, doc.root(), "div[role=columnheader]")
        .expect("セレクタは有効")
        .expect("ディストラクタがある");
    let th_node = map_to_snapshot(&doc, &snapshot, th);
    let decoy_node = map_to_snapshot(&doc, &snapshot, decoy);
    assert_eq!(
        (th_node.role.as_str(), th_node.name.as_str()),
        (decoy_node.role.as_str(), decoy_node.name.as_str()),
        "対象とディストラクタは role・name が同じ"
    );
    assert_ne!(th_node.r#ref, decoy_node.r#ref, "別ノード");
    assert_eq!(decoy_node.data_leaf, None);
    let nodes = all_nodes(&snapshot.tree);
    let by_role_name = nodes
        .iter()
        .find(|n| n.role == "columnheader" && n.name == "Last Name")
        .expect("role/name の一致がある");
    assert_eq!(
        by_role_name.r#ref, decoy_node.r#ref,
        "role/name はディストラクタを選ぶ"
    );
    let by_data_leaf = nodes
        .iter()
        .find(|n| is_table_heading(n) && n.name == "Last Name")
        .expect("data_leaf 併用の一致がある");
    assert_eq!(
        by_data_leaf.r#ref, th_node.r#ref,
        "data_leaf なら対象を選べる"
    );
}

/// AISNAP-3: 同一 HTML から 2 回構築した Snapshot は一致し、全 ref は木内で一意。
#[test]
fn aisnap_3_snapshot_is_deterministic_and_refs_unique() {
    for task in tasks() {
        let doc = parse(task.html);
        let a = snap(&doc);
        let b = snap(&doc);
        assert_eq!(a, b, "{} は決定的", task.name);
        let refs: Vec<&str> = all_nodes(&a.tree)
            .iter()
            .filter_map(|n| n.r#ref.as_deref())
            .collect();
        let unique: HashSet<&str> = refs.iter().copied().collect();
        assert_eq!(refs.len(), unique.len(), "{} の ref は一意", task.name);
    }
}

/// AISNAP-3: `is_ref_shaped` は `e<16hex>[v<n>][-<n>]` の全組み合わせを受け入れ、不正形を弾く。
#[test]
fn aisnap_3_is_ref_shaped_follows_ref_contract() {
    let h = "0123456789abcdef";
    for ok in [
        format!("e{h}"),
        format!("e{h}-2"),
        format!("e{h}v1"),
        format!("e{h}v12-3"),
    ] {
        assert!(is_ref_shaped(&ok), "{ok}");
    }
    for ng in [
        format!("e{h}v"),
        format!("e{h}vx"),
        format!("e{h}v1-"),
        format!("e{h}-"),
        format!("e{h}x"),
        "e0123".to_string(),
        format!("x{h}"),
    ] {
        assert!(!is_ref_shaped(&ng), "{ng}");
    }
}

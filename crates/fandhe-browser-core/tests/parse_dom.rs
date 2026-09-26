//! `parse`（TASK-24.4・#38）→ `dom`（TASK-24.5・#39）の結合を、crate 外から
//! 代表的な HTML 断片の**具体値**で検証する結合テスト（TASK-24.6・#40・
//! ビヘイビア `CORE-1`・MS-1）。
//!
//! `tests/parse.rs`（`parse_document`/`parse_document_bytes` の `Ok`/`Err`
//! 契約）・`tests/dom.rs`（走査 API を crate 外から呼べることの最小確認）に
//! 続き、本ファイルは PoC-2（`docs/spec/03-poc/non-rendering-core/core-proto`）の
//! parse/dom テストが検証していた観点（代表的な HTML パターンごとの DOM 構造・
//! 属性・テキストの具体値）を本 crate の公開 API 越しに移植・拡充する。
//!
//! # スコープ外
//!
//! セレクタ照合（`query_selector(_all)` 相当）・PoC の `get_text`/
//! `get_all_texts`/`get_attr`/`build_form_values` のような高レベル helper は
//! `selector`（TASK-24.7・#41・実装済み）を使った `query`（TASK-24.10・#418）・
//! `fandhe-browser-ai` 側の簡約 DOM 生成（#42）が担当するスコープであり、本
//! ファイルでは扱わない。ここでは `dom` の走査 API（`descendants`・
//! `children`・`local_name`・`attribute`・`class_names`・`text_content`・
//! `parent`/`ancestors`）のみで同等の観点を検証する。
//!
//! フィクスチャは PoC-2 の `core-proto/fixtures/NN-*.html` 由来（プロジェクト
//! 自作コンテンツでありサードパーティ資産ではないため `NOTICE` 追記は不要）。
//! `.html` ファイルとして追加せず、テスト内の `const` raw 文字列として
//! インライン化する（パス・OS 差異吸収が不要になり、`harness/compat_fixtures/`
//! （TASK-26.1・#136）との配置衝突も避けられる）。

use fandhe_browser_core::{
    Document, Error, NodeData, NodeId, ParseError, ParseErrorPolicy, ParseOptions, QuirksMode,
    parse_document, parse_document_bytes,
};

// ---------------------------------------------------------------------------
// フィクスチャ（PoC-2 `core-proto/fixtures/` 由来）
// ---------------------------------------------------------------------------

/// PoC-2 fixture 01: 静的記事ページ（`meta`・見出し・複数段落）。
const FIXTURE_01_STATIC_ARTICLE: &str = r#"<!DOCTYPE html>
<html lang="ja">
<head>
<title>記事タイトル</title>
<meta name="author" content="山田太郎">
</head>
<body>
<article>
<h1 class="headline">Rust で作る軽量ブラウザ基盤</h1>
<p class="byline">著者: 山田太郎</p>
<p>本文の 1 段落目です。</p>
<p>本文の 2 段落目です。</p>
</article>
</body>
</html>"#;

/// PoC-2 fixture 02: 価格テーブル（`thead`/`tbody`・クラス付き `td`）。
const FIXTURE_02_TABLE: &str = r#"<!DOCTYPE html>
<html>
<body>
<table id="prices">
<thead>
<tr><th>商品</th><th>価格</th></tr>
</thead>
<tbody>
<tr><td class="name">りんご</td><td class="price">150</td></tr>
<tr><td class="name">バナナ</td><td class="price">100</td></tr>
<tr><td class="name">メロン</td><td class="price">500</td></tr>
</tbody>
</table>
</body>
</html>"#;

/// PoC-2 fixture 03: ログインフォーム（text/password/checkbox 入力）。
///
/// `password` 値の `secret123` は PoC 由来のダミー値であり実資格情報では
/// ない（security.md「秘密情報の混入防止」）。
const FIXTURE_03_FORM_LOGIN: &str = r#"<!DOCTYPE html>
<html>
<body>
<form id="login" action="/login" method="post">
<input type="text" name="username" value="alice">
<input type="password" name="password" value="secret123">
<input type="checkbox" name="remember" checked>
<input type="submit" value="ログイン">
</form>
</body>
</html>"#;

/// PoC-2 fixture 04: 商品一覧（`data-sku` 属性付き `li`）。
const FIXTURE_04_LIST: &str = r#"<!DOCTYPE html>
<html>
<body>
<ul class="products">
<li data-sku="A1">商品A - 1000円</li>
<li data-sku="A2">商品B - 2000円</li>
<li data-sku="A3">商品C - 3000円</li>
<li data-sku="A4">商品D - 4000円</li>
</ul>
</body>
</html>"#;

/// PoC-2 fixture 05: ナビゲーションリンク集。
const FIXTURE_05_NAV: &str = r#"<!DOCTYPE html>
<html>
<body>
<nav>
<a class="nav-link" href="/home">ホーム</a>
<a class="nav-link" href="/about">概要</a>
<a class="nav-link" href="/contact">連絡先</a>
</nav>
</body>
</html>"#;

/// PoC-2 fixture 06: SSR ブログ風（`#__next` 配下に `article` を複数）。
const FIXTURE_06_SSR_BLOG: &str = r#"<!DOCTYPE html>
<html>
<body>
<div id="__next">
<div class="post-list">
<article class="post"><h2>投稿1</h2><span class="date">2026-01-01</span></article>
<article class="post"><h2>投稿2</h2><span class="date">2026-01-02</span></article>
<article class="post"><h2>投稿3</h2><span class="date">2026-01-03</span></article>
</div>
</div>
</body>
</html>"#;

/// PoC-2 fixture 07: SPA シェル（`#root` は空・`script` の `src` のみ）。
///
/// JS を実行しないため（本 crate は `js_stub`。ビヘイビア `JS-2`・TASK-30 で
/// `fandhe-browser-js` に置換予定）、`#root` の中身が JS によって描画される
/// 想定でも、パース時点では常に空のまま観測される。
const FIXTURE_07_SPA_SHELL: &str = r#"<!DOCTYPE html>
<html>
<body>
<div id="root"></div>
<script src="/static/js/bundle.js"></script>
</body>
</html>"#;

/// PoC-2 fixture 08: ダッシュボード風テーブル（`<tr>` を `<table>` 直下へ
/// 直接記述し、暗黙の `tbody` 挿入を確認する）。
const FIXTURE_08_DASHBOARD: &str = r#"<!DOCTYPE html>
<html>
<body>
<table data-testid="dashboard">
<tr><td data-col="metric">CPU</td><td data-col="value">42%</td></tr>
<tr><td data-col="metric">MEM</td><td data-col="value">67%</td></tr>
</table>
</body>
</html>"#;

/// PoC-2 fixture 09: checkbox/radio/textarea を含むフォーム。
const FIXTURE_09_CHECKBOX_RADIO: &str = r#"<!DOCTYPE html>
<html>
<body>
<form id="prefs">
<input type="checkbox" name="opt_a" value="a">
<input type="checkbox" name="opt_b" value="b" checked>
<input type="radio" name="plan" value="basic" checked>
<input type="radio" name="plan" value="pro">
<textarea name="notes">特になし</textarea>
</form>
</body>
</html>"#;

/// PoC-2 fixture 10: 画像 2 件（void 要素）。
const FIXTURE_10_IMAGES: &str = r#"<!DOCTYPE html>
<html>
<body>
<img src="/img/a.png" alt="画像A" width="100">
<img src="/img/b.png" alt="画像B" width="200">
</body>
</html>"#;

/// PoC-2 fixture 11: Unicode（絵文字・多言語・実体参照）。
const FIXTURE_11_UNICODE: &str = r#"<!DOCTYPE html>
<html>
<body>
<h1>絵文字テスト &#x1F389;&#x1F680; 日本語 中文 한국어</h1>
<p>特殊文字: &amp; &lt; &gt; &quot; &copy;&#xFE0F; &#x1F1EF;&#x1F1F5;</p>
</body>
</html>"#;

/// PoC-2 fixture 12: 崩れた HTML（DOCTYPE なし・未閉じ `<span>`/`<b>`）。
/// `<ul>` 配下の 3 つの `<li>` は同一 `<ul>` の兄弟のまま観測される
/// （html5ever の tree construction により、未閉じ要素は `<ul>` の
/// 開始前に暗黙的に閉じられるため）。
const FIXTURE_12_MALFORMED: &str = r#"<html>
<body>
<p>段落<span><b>強調
<ul><li>項目1</li><li>項目2</li><li>項目3</li></ul>
</body>
</html>"#;

// ---------------------------------------------------------------------------
// ヘルパ
// ---------------------------------------------------------------------------

/// `ParseOptions::default()`（Recover ポリシー）でパースする。
fn parse(html: &str) -> Document {
    parse_document(html, &ParseOptions::default())
        .expect("フィクスチャは既定オプションで必ず成功する")
        .document
}

/// `root` の子孫のうち local name が `name` に一致する要素を文書順に返す。
fn elements_by_local_name(doc: &Document, root: NodeId, name: &str) -> Vec<NodeId> {
    doc.descendants(root)
        .filter(|&id| doc.local_name(id) == Some(name))
        .collect()
}

/// `root` の子孫のうち local name が `name` に一致する最初の要素を返す。
/// 見つからなければ `panic`（テスト用ヘルパであり、外部入力を扱う本体
/// コードではないため許容する）。
fn first_by_local_name(doc: &Document, root: NodeId, name: &str) -> NodeId {
    elements_by_local_name(doc, root, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("要素 {name} が見つからない"))
}

/// `id` の要素の子のうち要素ノードのみを local name の列として返す
/// （テキストノード・コメントは除外する）。
fn element_children_names(doc: &Document, id: NodeId) -> Vec<&str> {
    doc.children(id)
        .filter_map(|child| doc.local_name(child))
        .collect()
}

/// `ids` それぞれの `text_content` を具体値（`String`）の列として返す。
/// 該当なしは空文字列として扱う（フィクスチャは常に要素ノードを渡すため
/// 発生しない想定だが、テストヘルパとして `unwrap_or_default` で防御する）。
fn texts(doc: &Document, ids: &[NodeId]) -> Vec<String> {
    ids.iter()
        .map(|&id| doc.text_content(id).unwrap_or_default())
        .collect()
}

/// `root` 配下の要素だけを深さ優先・文書順で辿り、
/// `name(child1,child2(...),...)` 形式の 1 行文字列に整形する
/// （テキスト・コメント・属性は含めない。木構造全体を 1 回の assert で
/// 検証するためのヘルパ）。
fn dump_elements(doc: &Document, id: NodeId) -> String {
    let mut out = String::new();
    dump_elements_into(doc, id, &mut out);
    out
}

fn dump_elements_into(doc: &Document, id: NodeId, out: &mut String) {
    out.push_str(doc.local_name(id).unwrap_or("?"));
    let element_children: Vec<NodeId> = doc
        .children(id)
        .filter(|&child| doc.is_element(child))
        .collect();
    if element_children.is_empty() {
        return;
    }
    out.push('(');
    for (i, &child) in element_children.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        dump_elements_into(doc, child, out);
    }
    out.push(')');
}

/// `root` 配下の全ノード（要素・テキスト・コメント）を深さ優先・文書順で辿り、
/// 要素名・属性（名前でソート済み）・テキスト/コメント内容まで含めた 1 行
/// 文字列に整形する（`dump_elements` と異なり属性値・テキストの復号結果も
/// 比較対象にする。`core_1_bytes_and_str_entrypoints_build_identical_trees`
/// が str/bytes 両エントリポイントの完全な同等性を検証するために使う）。
fn dump_full(doc: &Document, id: NodeId) -> String {
    let mut out = String::new();
    dump_full_into(doc, id, &mut out);
    out
}

fn dump_full_into(doc: &Document, id: NodeId, out: &mut String) {
    match doc.node_data(id) {
        Some(NodeData::Text { contents }) => {
            out.push('"');
            out.push_str(contents);
            out.push('"');
            return;
        }
        Some(NodeData::Comment { contents }) => {
            out.push_str("<!--");
            out.push_str(contents);
            out.push_str("-->");
            return;
        }
        _ => {}
    }

    out.push_str(doc.local_name(id).unwrap_or("?"));

    if doc.is_element(id) {
        let mut attrs: Vec<(&str, &str)> = doc
            .attributes(id)
            .iter()
            .map(|attr| (&*attr.name.local, attr.value.as_str()))
            .collect();
        attrs.sort_unstable_by_key(|(name, _)| *name);
        if !attrs.is_empty() {
            out.push('[');
            for (i, (name, value)) in attrs.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(name);
                out.push('=');
                out.push_str(value);
            }
            out.push(']');
        }
    }

    let children: Vec<NodeId> = doc.children(id).collect();
    if children.is_empty() {
        return;
    }
    out.push('(');
    for (i, &child) in children.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        dump_full_into(doc, child, out);
    }
    out.push(')');
}

// ---------------------------------------------------------------------------
// フィクスチャ別テスト
// ---------------------------------------------------------------------------

/// CORE-1: fixture 01（静的記事）の DOM 木・`meta`/`h1`/`p` の属性・
/// テキストが具体値どおりになる（PoC-2 t01 相当）。
#[test]
fn core_1_fixture_01_static_article_structure_and_text() {
    let doc = parse(FIXTURE_01_STATIC_ARTICLE);
    let html = first_by_local_name(&doc, doc.root(), "html");

    assert_eq!(
        dump_elements(&doc, html),
        "html(head(title,meta),body(article(h1,p,p,p)))"
    );
    assert_eq!(doc.attribute(html, "lang"), Some("ja"));
    assert_eq!(doc.quirks_mode(), QuirksMode::NoQuirks);

    let title = first_by_local_name(&doc, html, "title");
    assert_eq!(doc.text_content(title).as_deref(), Some("記事タイトル"));

    let meta = first_by_local_name(&doc, html, "meta");
    assert_eq!(doc.attribute(meta, "name"), Some("author"));
    assert_eq!(doc.attribute(meta, "content"), Some("山田太郎"));

    let h1 = first_by_local_name(&doc, html, "h1");
    assert_eq!(doc.class_names(h1).collect::<Vec<_>>(), ["headline"]);
    assert_eq!(
        doc.text_content(h1).as_deref(),
        Some("Rust で作る軽量ブラウザ基盤")
    );

    let byline = first_by_local_name(&doc, html, "p");
    assert_eq!(doc.class_names(byline).collect::<Vec<_>>(), ["byline"]);
    assert_eq!(doc.text_content(byline).as_deref(), Some("著者: 山田太郎"));

    let article = first_by_local_name(&doc, html, "article");
    assert_eq!(
        element_children_names(&doc, article),
        vec!["h1", "p", "p", "p"]
    );
}

/// CORE-1: fixture 02（テーブル）の `thead`/`tbody` 構造と `td` の値が
/// 具体値どおりになる（PoC-2 t02 相当）。
#[test]
fn core_1_fixture_02_table_structure_and_cell_values() {
    let doc = parse(FIXTURE_02_TABLE);
    let table = first_by_local_name(&doc, doc.root(), "table");
    assert_eq!(doc.attribute(table, "id"), Some("prices"));
    assert_eq!(element_children_names(&doc, table), vec!["thead", "tbody"]);

    let th_texts = texts(&doc, &elements_by_local_name(&doc, table, "th"));
    assert_eq!(th_texts, vec!["商品".to_string(), "価格".to_string()]);

    let name_cells: Vec<NodeId> = elements_by_local_name(&doc, table, "td")
        .into_iter()
        .filter(|&id| doc.class_names(id).eq(["name"]))
        .collect();
    assert_eq!(
        texts(&doc, &name_cells),
        vec![
            "りんご".to_string(),
            "バナナ".to_string(),
            "メロン".to_string()
        ]
    );

    let price_cells: Vec<NodeId> = elements_by_local_name(&doc, table, "td")
        .into_iter()
        .filter(|&id| doc.class_names(id).eq(["price"]))
        .collect();
    assert_eq!(
        texts(&doc, &price_cells),
        vec!["150".to_string(), "100".to_string(), "500".to_string()]
    );
}

/// CORE-1: fixture 03（ログインフォーム）の各 `input` の `type`/`name`/
/// `value` と真偽属性 `checked` の有無が具体値どおりになる
/// （真偽属性は空文字列 `Some("")` として観測される。PoC-2 t03 相当）。
#[test]
fn core_1_fixture_03_form_login_input_attributes() {
    let doc = parse(FIXTURE_03_FORM_LOGIN);
    let form = first_by_local_name(&doc, doc.root(), "form");
    assert_eq!(doc.attribute(form, "id"), Some("login"));

    let inputs = elements_by_local_name(&doc, form, "input");
    assert_eq!(inputs.len(), 4);

    let username = inputs[0];
    assert_eq!(doc.attribute(username, "type"), Some("text"));
    assert_eq!(doc.attribute(username, "name"), Some("username"));
    assert_eq!(doc.attribute(username, "value"), Some("alice"));
    assert_eq!(doc.attribute(username, "checked"), None);

    let password = inputs[1];
    assert_eq!(doc.attribute(password, "type"), Some("password"));
    assert_eq!(doc.attribute(password, "name"), Some("password"));
    assert_eq!(doc.attribute(password, "value"), Some("secret123"));

    let remember = inputs[2];
    assert_eq!(doc.attribute(remember, "type"), Some("checkbox"));
    assert_eq!(doc.attribute(remember, "name"), Some("remember"));
    assert_eq!(doc.attribute(remember, "checked"), Some(""));

    let submit = inputs[3];
    assert_eq!(doc.attribute(submit, "type"), Some("submit"));
    assert_eq!(doc.attribute(submit, "value"), Some("ログイン"));
}

/// CORE-1: fixture 04（商品一覧）の `data-sku` 属性とテキストが具体値どおり
/// 4 件揃う（PoC-2 t04 相当）。
#[test]
fn core_1_fixture_04_list_items_data_sku_and_text() {
    let doc = parse(FIXTURE_04_LIST);
    let ul = first_by_local_name(&doc, doc.root(), "ul");
    assert_eq!(doc.class_names(ul).collect::<Vec<_>>(), ["products"]);

    let items = elements_by_local_name(&doc, ul, "li");
    assert_eq!(items.len(), 4);

    let skus: Vec<&str> = items
        .iter()
        .filter_map(|&id| doc.attribute(id, "data-sku"))
        .collect();
    assert_eq!(skus, vec!["A1", "A2", "A3", "A4"]);

    assert_eq!(
        texts(&doc, &items),
        vec![
            "商品A - 1000円".to_string(),
            "商品B - 2000円".to_string(),
            "商品C - 3000円".to_string(),
            "商品D - 4000円".to_string(),
        ]
    );
}

/// CORE-1: fixture 05（ナビゲーション）の `a` 要素 3 件の `href`・
/// `class`・テキストが具体値どおりになる（PoC-2 t05 相当）。
#[test]
fn core_1_fixture_05_nav_links_href_and_class() {
    let doc = parse(FIXTURE_05_NAV);
    let nav = first_by_local_name(&doc, doc.root(), "nav");
    let links = element_children_names(&doc, nav);
    assert_eq!(links, vec!["a", "a", "a"]);

    let link_ids = elements_by_local_name(&doc, nav, "a");
    let hrefs: Vec<&str> = link_ids
        .iter()
        .filter_map(|&id| doc.attribute(id, "href"))
        .collect();
    assert_eq!(hrefs, vec!["/home", "/about", "/contact"]);

    for &id in &link_ids {
        assert_eq!(doc.class_names(id).collect::<Vec<_>>(), ["nav-link"]);
    }

    assert_eq!(
        texts(&doc, &link_ids),
        vec![
            "ホーム".to_string(),
            "概要".to_string(),
            "連絡先".to_string()
        ]
    );
}

/// CORE-1: fixture 06（SSR ブログ風）の `article.post` 3 件それぞれの
/// `h2`/`span.date` 構造とテキストが具体値どおりになる（PoC-2 t06 相当）。
#[test]
fn core_1_fixture_06_ssr_blog_articles_structure_and_text() {
    let doc = parse(FIXTURE_06_SSR_BLOG);
    let root_div = doc
        .descendants(doc.root())
        .find(|&id| doc.attribute(id, "id") == Some("__next"))
        .expect("#__next が見つかる");

    assert_eq!(
        dump_elements(&doc, root_div),
        "div(div(article(h2,span),article(h2,span),article(h2,span)))"
    );

    let articles = elements_by_local_name(&doc, root_div, "article");
    assert_eq!(articles.len(), 3);
    for &article in &articles {
        assert_eq!(doc.class_names(article).collect::<Vec<_>>(), ["post"]);
        assert_eq!(element_children_names(&doc, article), vec!["h2", "span"]);
    }

    let h2_texts = texts(&doc, &elements_by_local_name(&doc, root_div, "h2"));
    assert_eq!(
        h2_texts,
        vec![
            "投稿1".to_string(),
            "投稿2".to_string(),
            "投稿3".to_string()
        ]
    );

    let date_spans: Vec<NodeId> = elements_by_local_name(&doc, root_div, "span")
        .into_iter()
        .filter(|&id| doc.class_names(id).eq(["date"]))
        .collect();
    assert_eq!(
        texts(&doc, &date_spans),
        vec![
            "2026-01-01".to_string(),
            "2026-01-02".to_string(),
            "2026-01-03".to_string(),
        ]
    );
}

/// CORE-1: fixture 07（SPA シェル）は JS 非実行のため `#root` が常に空
/// （子要素 0・`text_content` は空文字列）で観測され、`script` の `src`
/// のみが取得できる（JS-2・TASK-30 で状況が変わる可能性がある。PoC-2 t07
/// 相当）。
#[test]
fn core_1_fixture_07_spa_shell_root_is_empty_without_js_execution() {
    let doc = parse(FIXTURE_07_SPA_SHELL);
    let root_div = doc
        .descendants(doc.root())
        .find(|&id| doc.attribute(id, "id") == Some("root"))
        .expect("#root が見つかる");

    assert_eq!(doc.children(root_div).len(), 0);
    assert_eq!(doc.text_content(root_div).as_deref(), Some(""));

    let script = first_by_local_name(&doc, doc.root(), "script");
    assert_eq!(doc.attribute(script, "src"), Some("/static/js/bundle.js"));
}

/// CORE-1: fixture 08（ダッシュボード）は `<table>` 直下に `<tr>` を
/// 直接書いても、html5ever が暗黙の `tbody` を挿入し
/// `table > tbody > tr` になる（PoC-2 t08 相当）。
#[test]
fn core_1_fixture_08_dashboard_implicit_tbody_insertion() {
    let doc = parse(FIXTURE_08_DASHBOARD);
    let table = first_by_local_name(&doc, doc.root(), "table");
    assert_eq!(doc.attribute(table, "data-testid"), Some("dashboard"));
    assert_eq!(
        dump_elements(&doc, table),
        "table(tbody(tr(td,td),tr(td,td)))"
    );

    let value_cells: Vec<NodeId> = elements_by_local_name(&doc, table, "td")
        .into_iter()
        .filter(|&id| doc.attribute(id, "data-col") == Some("value"))
        .collect();
    assert_eq!(
        texts(&doc, &value_cells),
        vec!["42%".to_string(), "67%".to_string()]
    );
}

/// CORE-1: fixture 09（checkbox/radio/textarea）の真偽属性・値・
/// `textarea` のテキストが具体値どおりになる（PoC-2 t09 相当）。
#[test]
fn core_1_fixture_09_checkbox_radio_textarea_values() {
    let doc = parse(FIXTURE_09_CHECKBOX_RADIO);
    let form = first_by_local_name(&doc, doc.root(), "form");

    let checkboxes: Vec<NodeId> = elements_by_local_name(&doc, form, "input")
        .into_iter()
        .filter(|&id| doc.attribute(id, "type") == Some("checkbox"))
        .collect();
    assert_eq!(checkboxes.len(), 2);
    let opt_a = checkboxes[0];
    let opt_b = checkboxes[1];
    assert_eq!(doc.attribute(opt_a, "name"), Some("opt_a"));
    assert_eq!(doc.attribute(opt_a, "checked"), None);
    assert_eq!(doc.attribute(opt_b, "name"), Some("opt_b"));
    assert_eq!(doc.attribute(opt_b, "checked"), Some(""));

    let radios: Vec<NodeId> = elements_by_local_name(&doc, form, "input")
        .into_iter()
        .filter(|&id| doc.attribute(id, "type") == Some("radio"))
        .collect();
    assert_eq!(radios.len(), 2);
    let basic = radios[0];
    let pro = radios[1];
    assert_eq!(doc.attribute(basic, "value"), Some("basic"));
    assert_eq!(doc.attribute(basic, "checked"), Some(""));
    assert_eq!(doc.attribute(pro, "value"), Some("pro"));
    assert_eq!(doc.attribute(pro, "checked"), None);

    let textarea = first_by_local_name(&doc, form, "textarea");
    assert_eq!(doc.attribute(textarea, "name"), Some("notes"));
    assert_eq!(doc.text_content(textarea).as_deref(), Some("特になし"));
}

/// CORE-1: fixture 10（画像 2 件）は void 要素のため子を持たず、
/// `src`/`alt`/`width` が具体値どおりになる（PoC-2 t10 相当）。
#[test]
fn core_1_fixture_10_images_are_void_elements_with_attributes() {
    let doc = parse(FIXTURE_10_IMAGES);
    let images = elements_by_local_name(&doc, doc.root(), "img");
    assert_eq!(images.len(), 2);

    let a = images[0];
    assert_eq!(doc.attribute(a, "src"), Some("/img/a.png"));
    assert_eq!(doc.attribute(a, "alt"), Some("画像A"));
    assert_eq!(doc.attribute(a, "width"), Some("100"));
    assert_eq!(doc.children(a).len(), 0);

    let b = images[1];
    assert_eq!(doc.attribute(b, "src"), Some("/img/b.png"));
    assert_eq!(doc.attribute(b, "alt"), Some("画像B"));
    assert_eq!(doc.attribute(b, "width"), Some("200"));
    assert_eq!(doc.children(b).len(), 0);
}

/// CORE-1: fixture 11（Unicode）は絵文字・多言語文字列がそのまま保持され、
/// 実体参照（`&amp;`・`&copy;` 等）は復号されたテキストになる
/// （`©️` は不可視の異体字セレクタ U+FE0F を含むため `\u{FE0F}` 明示で
/// 期待値を書く。🇯🇵 は地域指示子 2 文字。PoC-2 t11 相当）。
#[test]
fn core_1_fixture_11_unicode_and_entities_decode_correctly() {
    let doc = parse(FIXTURE_11_UNICODE);
    let h1 = first_by_local_name(&doc, doc.root(), "h1");
    assert_eq!(
        doc.text_content(h1).as_deref(),
        Some("絵文字テスト \u{1F389}\u{1F680} 日本語 中文 한국어")
    );

    let p = first_by_local_name(&doc, doc.root(), "p");
    assert_eq!(
        doc.text_content(p).as_deref(),
        Some("特殊文字: & < > \" \u{00A9}\u{FE0F} \u{1F1EF}\u{1F1F5}")
    );
}

/// CORE-1: fixture 12（崩れた HTML）は DOCTYPE 欠落により Quirks モードに
/// なり、既定（Recover）では `Ok` を返しつつ診断にエラー件数が記録される。
/// 同じ入力を Strict で解析すると `Err(Malformed)` になる。`<ul>` の 3 つの
/// `<li>` は同一 `<ul>` の兄弟としてテキストが順に取得できる
/// （PoC-2 t12 相当）。
#[test]
fn core_1_fixture_12_malformed_html_quirks_mode_and_error_policies() {
    let parsed = parse_document(FIXTURE_12_MALFORMED, &ParseOptions::default())
        .expect("Recover（既定）では Ok を返す");
    assert_eq!(parsed.document.quirks_mode(), QuirksMode::Quirks);
    assert!(parsed.diagnostics.error_count > 0);

    let doc = &parsed.document;
    let ul = first_by_local_name(doc, doc.root(), "ul");
    let items = elements_by_local_name(doc, ul, "li");
    assert_eq!(items.len(), 3);
    assert_eq!(
        texts(doc, &items),
        vec![
            "項目1".to_string(),
            "項目2".to_string(),
            "項目3".to_string()
        ]
    );

    let strict_options = ParseOptions::default().with_error_policy(ParseErrorPolicy::Strict);
    let err = parse_document(FIXTURE_12_MALFORMED, &strict_options)
        .expect_err("Strict ではパースエラーで Err になる");
    assert!(matches!(
        err,
        Error::Parse(ParseError::Malformed { error_count, .. }) if error_count > 0
    ));
}

// ---------------------------------------------------------------------------
// 横断テスト
// ---------------------------------------------------------------------------

/// CORE-1: `html`/`head`/`body` を持たない断片でも、html5ever の tree
/// construction により暗黙的に補完される。
#[test]
fn core_1_fragment_gets_implied_html_head_body() {
    let doc = parse("<p>hi");
    let html = first_by_local_name(&doc, doc.root(), "html");
    assert_eq!(dump_elements(&doc, html), "html(head,body(p))");
}

/// CORE-1: DOCTYPE 宣言は `NodeData::Doctype` としてルートの子に現れ、
/// `name` フィールドが `"html"` になる。
#[test]
fn core_1_doctype_node_data_is_exposed() {
    let doc = parse(FIXTURE_01_STATIC_ARTICLE);
    let root_children: Vec<NodeId> = doc.children(doc.root()).collect();
    let doctype_id = *root_children
        .first()
        .expect("ルートは最低 1 つの子（Doctype）を持つ");
    match doc.node_data(doctype_id) {
        Some(NodeData::Doctype { name, .. }) => assert_eq!(name, "html"),
        other => panic!("Doctype を期待したが {other:?} だった"),
    }
}

/// CORE-1: `parse_document`（`&str` 入力）と `parse_document_bytes`
/// （UTF-8 バイト列入力）は、全フィクスチャについて属性値・テキスト内容まで
/// 同一の木を構築する（`dump_full` の文字列一致で確認する。要素名だけを
/// 比較する `dump_elements` では、属性やテキストの復号結果が経路によって
/// 異なっていても検出できないため使わない）。
#[test]
fn core_1_bytes_and_str_entrypoints_build_identical_trees() {
    let fixtures = [
        FIXTURE_01_STATIC_ARTICLE,
        FIXTURE_02_TABLE,
        FIXTURE_03_FORM_LOGIN,
        FIXTURE_04_LIST,
        FIXTURE_05_NAV,
        FIXTURE_06_SSR_BLOG,
        FIXTURE_07_SPA_SHELL,
        FIXTURE_08_DASHBOARD,
        FIXTURE_09_CHECKBOX_RADIO,
        FIXTURE_10_IMAGES,
        FIXTURE_11_UNICODE,
        FIXTURE_12_MALFORMED,
    ];

    for fixture in fixtures {
        let from_str = parse_document(fixture, &ParseOptions::default())
            .expect("既定オプションで成功する")
            .document;
        let from_bytes = parse_document_bytes(fixture.as_bytes(), &ParseOptions::default())
            .expect("既定オプションで成功する")
            .document;

        assert_eq!(
            dump_full(&from_str, from_str.root()),
            dump_full(&from_bytes, from_bytes.root()),
            "fixture の str/bytes エントリポイントで木が一致しない: {fixture}"
        );
    }
}

/// CORE-1: fixture 06 の `h2` から祖先を辿ると、`article` → `div`
/// （`post-list`） → `div`（`__next`） → `body` → `html` の順で要素名が
/// 得られる（ルート自身は `local_name` が `None` になるため含めない）。
#[test]
fn core_1_ancestors_chain_from_fixture() {
    let doc = parse(FIXTURE_06_SSR_BLOG);
    let h2 = first_by_local_name(&doc, doc.root(), "h2");
    let ancestor_names: Vec<&str> = doc
        .ancestors(h2)
        .filter_map(|id| doc.local_name(id))
        .collect();
    assert_eq!(
        ancestor_names,
        vec!["article", "div", "div", "body", "html"]
    );
}

//! AI エージェント評価ハーネスの代表タスク 25 件と golden answer（TASK-21.1・`AISNAP-8`・Issue #119）。
//!
//! 「クリック対象特定・値抽出・フォーム入力手順・ナビゲーション判断」の 4 種を
//! 7/6/6/6 件で定義する。後続の #120（簡約表現の生成ハーネス）と #121（採点）が
//! `#[path]` でこのファイルを取り込み、[`TASKS`]・[`GOLDEN`] を入力にする。
//! 本ファイルは定義と JSON 直列化のみを担い、簡約表現の生成・ref 解決・採点は行わない。
//!
//! # 正本と生成物
//!
//! 正本はこのファイルの定数。`tasks.json`（エージェントへ渡す側）と
//! `golden-answers.json`（採点側。エージェントへ渡さない）は [`render_tasks_json`]・
//! [`render_golden_json`] の出力で、`tasks_tests.rs` が一致を固定する。
//!
//! # golden が ref ではなくロケータである理由
//!
//! PoC の golden は連番 ref（`e1`〜）だが、本リポの ref は `AISNAP-10` のダイジェスト形式で
//! 移植できない。そのため正解要素は fixture 上のロケータ（selector と文書順 index）で指し、
//! ref への解決は #120、照合は #121 が担う。fixture は `benches/fixtures/` の合成ページで、
//! PoC の実サイトと内容が異なるため、タスク文と正解値は fixture の実内容に合わせて定義し直した。

/// タスクの種別（`AISNAP-8` の 4 種）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// クリック対象特定。
    Click,
    /// 値抽出。
    Extract,
    /// フォーム入力手順。
    Form,
    /// ナビゲーション判断。
    Nav,
}

impl Category {
    /// JSON 等で使う ASCII 識別子。
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Click => "click",
            Category::Extract => "extract",
            Category::Form => "form",
            Category::Nav => "nav",
        }
    }
}

/// fixture 上の要素の位置。`query_selector_all_str` の結果の文書順 `index` 番目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locator {
    pub selector: &'static str,
    pub index: usize,
}

/// エージェントへ渡す 1 タスク。
#[derive(Debug, Clone, Copy)]
pub struct Task {
    pub id: &'static str,
    pub category: Category,
    /// `benches/fixtures/` の拡張子なしファイル名。
    pub page: &'static str,
    /// エージェントへの指示文（日本語。入力データ）。
    pub prompt: &'static str,
}

/// フォーム手順の操作種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Fill,
    Click,
    Select,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Fill => "fill",
            Action::Click => "click",
            Action::Select => "select",
        }
    }
}

/// フォーム手順の 1 操作。`Fill`・`Select` は `value` 必須、`Click` は持たない。
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub action: Action,
    pub target: Locator,
    pub value: Option<&'static str>,
}

/// golden answer の形。
#[derive(Debug, Clone, Copy)]
pub enum Golden {
    /// 正解要素（click・nav）。同じ遷移先の要素が複数ある場合に備え複数許容する。
    Ref { any_of: &'static [Locator] },
    /// 期待値と、その値を持つ要素（extract）。`attr` が `None` なら text、`Some` なら属性値。
    Value {
        value: &'static str,
        source: Locator,
        attr: Option<&'static str>,
    },
    /// 操作列（form）。順序が意味を持つ。
    Steps(&'static [Step]),
}

const fn loc(selector: &'static str, index: usize) -> Locator {
    Locator { selector, index }
}

const fn task(
    id: &'static str,
    category: Category,
    page: &'static str,
    prompt: &'static str,
) -> Task {
    Task {
        id,
        category,
        page,
        prompt,
    }
}

const fn step(action: Action, target: Locator, value: Option<&'static str>) -> Step {
    Step {
        action,
        target,
        value,
    }
}

/// 代表タスク 25 件（click 7・extract 6・form 6・nav 6）。
pub const TASKS: [Task; 25] = [
    task(
        "click-01",
        Category::Click,
        "login-form",
        "Login ボタンをクリックしてください。対象の要素を特定してください。",
    ),
    task(
        "click-02",
        Category::Click,
        "dropdown-form",
        "ドロップダウンから Option 2 を選ぶ対象の要素を特定してください。",
    ),
    task(
        "click-03",
        Category::Click,
        "checkboxes-form",
        "1 つ目のチェックボックスをクリックする対象の要素を特定してください。",
    ),
    task(
        "click-04",
        Category::Click,
        "dashboard-table",
        "Example 1 の表で、2 行目（Last Name が Gamma の行）の edit リンクを特定してください。",
    ),
    task(
        "click-05",
        Category::Click,
        "hn-list",
        "2 番目の記事のタイトルリンクをクリックする対象の要素を特定してください。",
    ),
    task(
        "click-06",
        Category::Click,
        "ec-product-list",
        "1 番目の商品の Add to basket ボタンを特定してください。",
    ),
    task(
        "click-07",
        Category::Click,
        "quotes-list",
        "ヘッダーにある Login リンクを特定してください。",
    ),
    task(
        "extract-01",
        Category::Extract,
        "dashboard-table",
        "Example 1 の表で、3 行目（Last Name が Delta の行）の Email を答えてください。",
    ),
    task(
        "extract-02",
        Category::Extract,
        "ec-product-list",
        "2 番目の商品（Vector channel thread network）の価格を答えてください。",
    ),
    task(
        "extract-03",
        Category::Extract,
        "hn-list",
        "1 位の記事のポイント表記を答えてください。",
    ),
    task(
        "extract-04",
        Category::Extract,
        "large-table",
        "表の 1 行目・15 列目（Column 14）のセルの値を答えてください。",
    ),
    task(
        "extract-05",
        Category::Extract,
        "quotes-list",
        "1 件目の引用の本文を、引用符も含めてそのまま答えてください。",
    ),
    task(
        "extract-06",
        Category::Extract,
        "python-portal",
        "サイト内検索ボックスの placeholder 文字列を答えてください。",
    ),
    task(
        "form-01",
        Category::Form,
        "login-form",
        "ユーザー名 dummy-user、パスワード dummy-pass でログインする操作手順を答えてください。",
    ),
    task(
        "form-02",
        Category::Form,
        "dropdown-form",
        "ドロップダウンで Option 1 を選択する操作手順を答えてください。",
    ),
    task(
        "form-03",
        Category::Form,
        "inputs-form",
        "数値入力欄に 42 を入力する操作手順を答えてください。",
    ),
    task(
        "form-04",
        Category::Form,
        "checkboxes-form",
        "2 つのチェックボックスをどちらも checked の状態にするために必要な操作だけを答えてください。",
    ),
    task(
        "form-05",
        Category::Form,
        "python-portal",
        "検索ボックスに venv と入力して検索する操作手順を答えてください。",
    ),
    task(
        "form-06",
        Category::Form,
        "reddit-list",
        "検索欄に rust と入力して検索を実行する操作手順を答えてください。",
    ),
    task(
        "nav-01",
        Category::Nav,
        "wiki-portal-nav",
        "Language 3 のサイトへ移動するためにクリックするリンクを特定してください。",
    ),
    task(
        "nav-02",
        Category::Nav,
        "mdn-docs",
        "リファレンスの Item 1.3 のページへ移動するためのリンクを特定してください。",
    ),
    task(
        "nav-03",
        Category::Nav,
        "python-portal",
        "ダウンロードページへ移動するためのリンクを特定してください。",
    ),
    task(
        "nav-04",
        Category::Nav,
        "hn-list",
        "上部ナビゲーションにある login ページへ移動するリンクを特定してください。",
    ),
    task(
        "nav-05",
        Category::Nav,
        "quotes-list",
        "次のページへ進むためのリンクを特定してください。",
    ),
    task(
        "nav-06",
        Category::Nav,
        "login-form",
        "フッターの Example Test Site へ移動するリンクを特定してください。",
    ),
];

const CLICK_01: [Locator; 1] = [loc("form#login button[type=submit]", 0)];
const CLICK_02: [Locator; 1] = [loc("select#dropdown option[value=\"2\"]", 0)];
const CLICK_03: [Locator; 1] = [loc("form#checkboxes input[type=checkbox]", 0)];
const CLICK_04: [Locator; 1] = [loc("table#table1 tbody a[href=\"#edit\"]", 1)];
const CLICK_05: [Locator; 1] = [loc("span.titleline > a", 1)];
const CLICK_06: [Locator; 1] = [loc("article.product_pod button", 0)];
const CLICK_07: [Locator; 1] = [loc("a[href=\"/login\"]", 0)];

const FORM_01: [Step; 3] = [
    step(Action::Fill, loc("input#username", 0), Some("dummy-user")),
    step(Action::Fill, loc("input#password", 0), Some("dummy-pass")),
    step(
        Action::Click,
        loc("form#login button[type=submit]", 0),
        None,
    ),
];
const FORM_02: [Step; 1] = [step(Action::Select, loc("select#dropdown", 0), Some("1"))];
const FORM_03: [Step; 1] = [step(Action::Fill, loc("input#quantity", 0), Some("42"))];
// 2 つ目は初期状態で checked のため、1 つ目だけをクリックすれば両方 checked になる。
const FORM_04: [Step; 1] = [step(
    Action::Click,
    loc("form#checkboxes input[type=checkbox]", 0),
    None,
)];
const FORM_05: [Step; 2] = [
    step(Action::Fill, loc("input#id-search-field", 0), Some("venv")),
    step(
        Action::Click,
        loc("form#search-form button[type=submit]", 0),
        None,
    ),
];
const FORM_06: [Step; 2] = [
    step(
        Action::Fill,
        loc("form#search input[name=q]", 0),
        Some("rust"),
    ),
    step(
        Action::Click,
        loc("form#search input[type=submit]", 0),
        None,
    ),
];

/// 中央の featured-box リンク（2 ロケータは同一要素）と、言語一覧の別リンク（異なる要素）。
/// どちらも同じ `//l3.example.org/` へ遷移するため、設問が場所を限定しない以上いずれも正解とする。
const NAV_01: [Locator; 3] = [
    loc("a#js-link-box-x3", 0),
    loc("div.lang3 > a", 0),
    loc("li.lang-item > a[title=\"Language 3\"]", 0),
];
const NAV_02: [Locator; 1] = [loc("a[href=\"/docs/ref/group1/item3\"]", 0)];
const NAV_03: [Locator; 1] = [loc("a[href=\"/downloads/\"]", 0)];
const NAV_04: [Locator; 1] = [loc("span.pagetop > a[href=\"login?goto=news\"]", 0)];
const NAV_05: [Locator; 1] = [loc("li.next > a", 0)];
const NAV_06: [Locator; 1] = [loc("a[href=\"https://example.com/\"]", 0)];

/// [`TASKS`] と同じ id・同じ順序の golden answer。
pub const GOLDEN: [(&str, Golden); 25] = [
    ("click-01", Golden::Ref { any_of: &CLICK_01 }),
    ("click-02", Golden::Ref { any_of: &CLICK_02 }),
    ("click-03", Golden::Ref { any_of: &CLICK_03 }),
    ("click-04", Golden::Ref { any_of: &CLICK_04 }),
    ("click-05", Golden::Ref { any_of: &CLICK_05 }),
    ("click-06", Golden::Ref { any_of: &CLICK_06 }),
    ("click-07", Golden::Ref { any_of: &CLICK_07 }),
    (
        "extract-01",
        Golden::Value {
            value: "user3@example.com",
            source: loc("table#table1 tbody td", 14),
            attr: None,
        },
    ),
    (
        "extract-02",
        Golden::Value {
            value: "\u{a3}13.17",
            source: loc("p.price_color", 1),
            attr: None,
        },
    ),
    (
        "extract-03",
        Golden::Value {
            value: "20 points",
            source: loc("span.score", 0),
            attr: None,
        },
    ),
    (
        "extract-04",
        Golden::Value {
            value: "r0c14-14",
            source: loc("tbody td", 14),
            attr: None,
        },
    ),
    (
        "extract-05",
        Golden::Value {
            value: "\u{201c}River stone beta garden prism record valley vector record theta delta system.\u{201d}",
            source: loc("span.text", 0),
            attr: None,
        },
    ),
    (
        "extract-06",
        Golden::Value {
            value: "Search",
            source: loc("input#id-search-field", 0),
            attr: Some("placeholder"),
        },
    ),
    ("form-01", Golden::Steps(&FORM_01)),
    ("form-02", Golden::Steps(&FORM_02)),
    ("form-03", Golden::Steps(&FORM_03)),
    ("form-04", Golden::Steps(&FORM_04)),
    ("form-05", Golden::Steps(&FORM_05)),
    ("form-06", Golden::Steps(&FORM_06)),
    ("nav-01", Golden::Ref { any_of: &NAV_01 }),
    ("nav-02", Golden::Ref { any_of: &NAV_02 }),
    ("nav-03", Golden::Ref { any_of: &NAV_03 }),
    ("nav-04", Golden::Ref { any_of: &NAV_04 }),
    ("nav-05", Golden::Ref { any_of: &NAV_05 }),
    ("nav-06", Golden::Ref { any_of: &NAV_06 }),
];

/// JSON 文字列リテラル（引用符込み）へエスケープする。
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_loc(l: &Locator) -> String {
    format!(
        "{{\"selector\": {}, \"index\": {}}}",
        json_str(l.selector),
        l.index
    )
}

/// `tasks.json`（エージェントへ渡す側）の内容を返す。2 スペースインデント・LF・末尾改行。
pub fn render_tasks_json() -> String {
    let items: Vec<String> = TASKS
        .iter()
        .map(|t| {
            format!(
                "  {{\n    \"id\": {},\n    \"category\": {},\n    \"page\": {},\n    \"prompt\": {}\n  }}",
                json_str(t.id),
                json_str(t.category.as_str()),
                json_str(t.page),
                json_str(t.prompt)
            )
        })
        .collect();
    format!("[\n{}\n]\n", items.join(",\n"))
}

/// `golden-answers.json`（採点側。エージェントへ渡さない）の内容を返す。
pub fn render_golden_json() -> String {
    let items: Vec<String> = GOLDEN
        .iter()
        .map(|(id, g)| {
            let body = match g {
                Golden::Ref { any_of } => {
                    let locs: Vec<String> = any_of
                        .iter()
                        .map(|l| format!("      {}", json_loc(l)))
                        .collect();
                    format!(
                        "    \"type\": \"ref\",\n    \"any_of\": [\n{}\n    ]",
                        locs.join(",\n")
                    )
                }
                Golden::Value {
                    value,
                    source,
                    attr,
                } => {
                    let attr = match attr {
                        Some(a) => json_str(a),
                        None => "null".to_owned(),
                    };
                    format!(
                        "    \"type\": \"value\",\n    \"value\": {},\n    \"source\": {},\n    \"attr\": {}",
                        json_str(value),
                        json_loc(source),
                        attr
                    )
                }
                Golden::Steps(steps) => {
                    let ss: Vec<String> = steps
                        .iter()
                        .map(|s| {
                            let v = match s.value {
                                Some(v) => json_str(v),
                                None => "null".to_owned(),
                            };
                            format!(
                                "      {{\"action\": {}, \"target\": {}, \"value\": {}}}",
                                json_str(s.action.as_str()),
                                json_loc(&s.target),
                                v
                            )
                        })
                        .collect();
                    format!(
                        "    \"type\": \"steps\",\n    \"steps\": [\n{}\n    ]",
                        ss.join(",\n")
                    )
                }
            };
            format!("  {{\n    \"id\": {},\n{}\n  }}", json_str(id), body)
        })
        .collect();
    format!("[\n{}\n]\n", items.join(",\n"))
}

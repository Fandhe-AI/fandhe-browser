//! compat_tasks: ローカルフィクスチャに対する代表タスク実行・判定ロジック。
//!
//! `examples/compat_tasks/main.rs`（CLI 本体。ローカル/実サイトモードの
//! 分岐・出力）と `tests/compat_fixtures.rs`（結合テスト。
//! `#[path = "../examples/compat_tasks/tasks.rs"] mod tasks;` で本ファイルを
//! 取り込む）の双方から使われる共有ロジックのみをここへ置く。実サイト用の
//! タスク表・fetch ループ・CLI 引数解析は `main.rs`側に置く（`clippy
//! --all-targets -D warnings` がどちらか一方だけで使う項目を dead_code
//! として検出するため。本ファイルに置く項目は example とテストの両方が
//! 使うものに限る）。
//!
//! CORE-1（`core-dom.md`）の期待値「代表タスクの成功率 70% 以上」を、
//! 本実装の core crate で再測定するための道具立て（TASK-26（26.1）・
//! Issue #136・MS-3）。成功率の測定・レポート化そのものは TASK-26.2
//! （Issue #137）が本モジュールを呼び出して行う。
//!
//! フォーム値の組み立て（[`collect_form_values`]）は core の公開 API では
//! ない、計測専用の補助関数である。core には `<form>` の送信値を組み立てる
//! API が無く（`dom-api-scope.md` CORE-5 (4)。未判定）、本モジュールは
//! `dom::Document` の既存アクセサ（`attributes`・`local_name`・
//! `text_content`）だけを使って計測用途に限定した簡易実装を行う
//! （REPAIR-3: 実装済みを装わない。`<select>` は対象外）。

use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::{Error, ParseOptions, parse_document_bytes, query_selector_all_str};

/// フィクスチャの類型（CORE-1 が定める代表タスク類型の一部）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// 静的 HTML（PoC-2 の「静的」類型）。
    Static,
    /// SSR/SPA 静的レンダリング類型（Next.js・Nuxt 風のプリレンダリング済み HTML を含む）。
    SsrSpaStatic,
    /// 成功率の分母に含めない参考枠（CSR シェル等）。
    Excluded,
}

impl Category {
    /// 出力・集計で使う識別子（プログラム出力文字列は英語。japanese-style.md）。
    pub fn label(self) -> &'static str {
        match self {
            Category::Static => "static",
            Category::SsrSpaStatic => "ssr_spa_static",
            Category::Excluded => "excluded",
        }
    }
}

/// タスクが呼び出す `query` API の種別。
///
/// 戻り値の形（[`Outcome`]）を将来拡張しやすいよう、真偽値・フラットな
/// 文字列ではなく列挙型で表す（REPAIR-4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// `query_selector_all_str` で一致した各要素の `text_content` を
    /// 正規化（[`normalize_ws`]）して文書順に集める。
    Texts,
    /// `query_selector_all_str` で一致した各要素の属性値を文書順に集める。
    Attr(&'static str),
    /// `query_selector_all_str` で一致した `<form>` 要素（先頭 1 件）の
    /// 送信値を [`collect_form_values`] で組み立てる。
    Form,
}

/// タスクの期待値。
#[derive(Debug, Clone, Copy)]
pub enum Expected {
    /// [`Outcome::Values`] がこの並びと完全一致することを期待する。
    Values(&'static [&'static str]),
    /// [`Outcome::Pairs`] がこの並びと完全一致することを期待する。
    Pairs(&'static [(&'static str, &'static str)]),
    /// 結果が空でないことだけを期待する（実サイトモード用。HTML の変更に
    /// 対して脆くならないよう、具体値までは固定しない）。
    NonEmpty,
    /// 結果が空であることを期待する（CSR シェル等、対象外タスクの記録用）。
    Empty,
}

/// タスク実行の結果（実際に得られた値）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// [`TaskKind::Texts`] / [`TaskKind::Attr`] の結果。
    Values(Vec<String>),
    /// [`TaskKind::Form`] の結果（`(name, value)` の文書順リスト）。
    Pairs(Vec<(String, String)>),
}

impl Outcome {
    fn is_empty(&self) -> bool {
        match self {
            Outcome::Values(v) => v.is_empty(),
            Outcome::Pairs(v) => v.is_empty(),
        }
    }

    /// 正規化後に内容のある値が 1 件以上存在するかを判定する
    /// （`Expected::NonEmpty` 専用。`is_empty` は要素数のみを見るため、
    /// 実サイトの見出しなどが空文字・空白のみの要素を持つ場合に
    /// `Texts`/`Attr` タスクが誤って `Status::Ok` になり CORE-1 の
    /// 成功率を過大評価してしまう。`Values` は各要素を trim して非空かで
    /// 判定する（`Attr` は [`run_task`] で正規化していないため、ここで
    /// trim する）。
    ///
    /// `Pairs`（`Form` タスク）は要素数のみで判定する。
    /// [`collect_form_values`] は `name` 属性が空の要素を除外して
    /// pair を積むため、`Pairs` に要素があること自体が
    /// 「name 付きフィールドの抽出に成功した」ことを意味する。実サイトの
    /// ログインフォームは典型的に value が空のデフォルト値を持つ
    /// named field を返すため、value の非空を要求すると成功した抽出を
    /// `UnexpectedEmpty` に誤判定し、CORE-1 の成功率を過小評価してしまう。
    fn has_meaningful_content(&self) -> bool {
        match self {
            Outcome::Values(v) => v.iter().any(|s| !s.trim().is_empty()),
            Outcome::Pairs(v) => !v.is_empty(),
        }
    }
}

/// タスク実行の判定結果。将来のステータス追加に備えて `#[non_exhaustive]`
/// にする（REPAIR-4。新しい失敗種別を非破壊で追加できるようにする）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Status {
    /// 期待値どおりの結果が得られた。
    Ok,
    /// 結果が期待値と異なる（`Expected::Values`/`Pairs` との不一致）。
    Mismatch,
    /// 結果が期待どおり空だった（`Expected::Empty`。CSR シェル等）。
    ExpectedEmpty,
    /// 非空を期待したが空の結果しか得られなかった（実サイトモード用）。
    UnexpectedEmpty,
    /// セレクタが本 crate の対応サブセット外だった
    /// （`Error::Unsupported`。harness 側のセレクタ定義ミスであり、
    /// CORE-1 の抽出失敗とは区別する）。
    SelectorUnsupported,
    /// 取得先が 2xx 以外のステータスを返した（実サイトモード用）。
    Http(u16),
    /// `fetch::Fetcher::get` が失敗した（実サイトモード用。エラー種別名の
    /// みを保持し、詳細メッセージは埋め込まない。security.md 秘密情報混入
    /// 防止・出力肥大化の回避）。
    FetchError(&'static str),
    /// HTML のパースに失敗した（実サイトモード用）。
    ParseError,
    /// `query_selector_all_str`/`query_selector_str` が
    /// `Error::Unsupported` 以外の内部エラー（例:
    /// `Error::MatchCacheLimitExceeded`）を返した。`ParseError`
    /// （HTML 自体をパースできない）とは原因が異なるため区別する。
    QueryError,
}

impl Status {
    /// 「成功」として扱うかどうか（[`Summary`] の集計基準）。
    pub fn is_success(&self) -> bool {
        matches!(self, Status::Ok)
    }

    /// この結果を成功率の分母（reachable）に含めるかどうか。取得・パース・
    /// 照合のいずれかに失敗したタスクは「そもそも比較できなかった」ため
    /// 分母から除く（attempted には含める）。
    pub fn is_reachable(&self) -> bool {
        !matches!(
            self,
            Status::Http(_) | Status::FetchError(_) | Status::ParseError | Status::QueryError
        )
    }

    pub fn label(&self) -> String {
        match self {
            Status::Ok => "ok".to_string(),
            Status::Mismatch => "mismatch".to_string(),
            Status::ExpectedEmpty => "expected-empty".to_string(),
            Status::UnexpectedEmpty => "unexpected-empty".to_string(),
            Status::SelectorUnsupported => "selector-unsupported".to_string(),
            Status::Http(code) => format!("http-{code}"),
            Status::FetchError(kind) => format!("fetch-error:{kind}"),
            Status::ParseError => "parse-error".to_string(),
            Status::QueryError => "query-error".to_string(),
        }
    }
}

/// ローカルフィクスチャ 1 件分のタスク定義。
pub struct LocalTask {
    /// フィクスチャ ID（例: `"S01"`）。出力・テストの assert メッセージで使う。
    pub id: &'static str,
    pub category: Category,
    /// `harness/compat_fixtures/` 配下のフィクスチャを `include_bytes!` で
    /// コンパイル時に埋め込んだもの（実行時のパス解決・IO エラーが無く、
    /// ファイル欠落はビルドエラーになる）。
    pub fixture: &'static [u8],
    pub kind: TaskKind,
    pub selector: &'static str,
    pub expected: Expected,
}

/// タスク 1 件の実行結果（ローカル/実サイト共通の出力単位）。
pub struct TaskResult {
    pub id: &'static str,
    pub category: Category,
    pub kind: TaskKind,
    pub status: Status,
    /// 出力サンプル（[`sanitize_sample`] 適用済み）。
    pub sample: String,
}

/// 空白を 1 個の半角スペースへ畳んで前後を trim する（HTML の改行・
/// インデント由来の空白差を吸収するため。DOM `textContent` は改行や
/// インデントをそのまま含むため、比較前にテキストタスクへ適用する）。
pub fn normalize_ws(input: &str) -> String {
    input.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// 制御文字（ESC 等）を除去し、文字境界を保ったまま概ね 80 文字に切り詰める。
///
/// 実サイトモードで取得したテキストには任意の制御文字（ANSI エスケープ等）が
/// 含まれ得るため、端末へ出力する前に必ず通す（security.md
/// 「不安全な設計」・端末インジェクション対策）。
pub fn sanitize_sample(input: &str) -> String {
    const MAX_CHARS: usize = 80;
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .collect();
    if cleaned.chars().count() <= MAX_CHARS {
        return cleaned;
    }
    let truncated: String = cleaned.chars().take(MAX_CHARS).collect();
    format!("{truncated}…")
}

/// フォーム値の組み立て（harness 専用の計測補助関数。core の公開 API では
/// ない。CORE-5 (4)・未判定。`<select>` は対象外）。
///
/// 対象: `input`（`type` が text・password・hidden・email・search・number、
/// または `checked` 付きの checkbox・radio。値の既定は `on`）・`textarea`
/// （`text_content`）。`disabled` 属性付き・`name` 属性なし・
/// submit/button/reset/file/image の `input` は除外する。
///
/// `form` 直下だけでなく子孫全体を対象にする（`document.descendants(form)`
/// を使う。HTML の `<form>` はネストせず、実務上のフォームはラップ用の
/// `<div>`・`<fieldset>` を挟むことが多いため）。
pub fn collect_form_values(document: &Document, form: NodeId) -> Vec<(String, String)> {
    const TEXT_LIKE_TYPES: &[&str] = &["text", "password", "hidden", "email", "search", "number"];

    let mut pairs = Vec::new();
    for node in document.descendants(form) {
        if !document.is_element(node) {
            continue;
        }
        let Some(local_name) = document.local_name(node) else {
            continue;
        };
        if document.attribute(node, "disabled").is_some() {
            continue;
        }
        let Some(name) = document.attribute(node, "name") else {
            continue;
        };
        if name.is_empty() {
            continue;
        }

        match local_name {
            "input" => {
                let input_type = document.attribute(node, "type").unwrap_or("text");
                if input_type.eq_ignore_ascii_case("checkbox")
                    || input_type.eq_ignore_ascii_case("radio")
                {
                    if document.attribute(node, "checked").is_none() {
                        continue;
                    }
                    let value = document.attribute(node, "value").unwrap_or("on");
                    pairs.push((name.to_string(), value.to_string()));
                } else if TEXT_LIKE_TYPES
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(input_type))
                {
                    let value = document.attribute(node, "value").unwrap_or("");
                    pairs.push((name.to_string(), value.to_string()));
                }
                // submit/button/reset/file/image は送信値の組み立て対象外
                // （計測用途では利用者が明示入力する値のみを対象にする）。
            }
            "textarea" => {
                let value = document.text_content(node).unwrap_or_default();
                pairs.push((name.to_string(), value));
            }
            _ => {}
        }
    }
    pairs
}

/// harness 内で完結するタスク実行エラー（`crate::error::Error` をそのまま
/// 上位へ伝播させず、[`Status::SelectorUnsupported`] へ変換するために薄く
/// 包む）。`Core` は `SelectorUnsupported` 以外の内部エラー
/// （`MatchCacheLimitExceeded` 等）をまとめる受け皿で、詳細メッセージは
/// 出力に含めない（呼び出し元は [`Status::QueryError`] として記録するのみ
/// で十分なため。security.md 出力肥大化の回避）。
#[derive(Debug)]
pub enum TaskError {
    SelectorUnsupported,
    Core,
}

impl From<Error> for TaskError {
    fn from(err: Error) -> Self {
        match err {
            Error::Unsupported { .. } => TaskError::SelectorUnsupported,
            _other => TaskError::Core,
        }
    }
}

/// `kind`/`selector` に従い `document` へ問い合わせて [`Outcome`] を返す。
///
/// `cdp`（`DOM.querySelector` 系）・`ai`（簡約 DOM 抽出）が将来 core の
/// query API を呼ぶ際の使い方を、代表タスクの形で模したもの。
pub fn run_task(document: &Document, kind: TaskKind, selector: &str) -> Result<Outcome, TaskError> {
    let scope = document.root();
    match kind {
        TaskKind::Texts => {
            let nodes = query_selector_all_str(document, scope, selector)?;
            let values = nodes
                .into_iter()
                .map(|id| normalize_ws(&document.text_content(id).unwrap_or_default()))
                .collect();
            Ok(Outcome::Values(values))
        }
        TaskKind::Attr(name) => {
            let nodes = query_selector_all_str(document, scope, selector)?;
            let values = nodes
                .into_iter()
                .filter_map(|id| document.attribute(id, name).map(str::to_string))
                .collect();
            Ok(Outcome::Values(values))
        }
        TaskKind::Form => {
            let nodes = query_selector_all_str(document, scope, selector)?;
            let pairs = match nodes.first() {
                Some(&form) => collect_form_values(document, form),
                None => Vec::new(),
            };
            Ok(Outcome::Pairs(pairs))
        }
    }
}

/// `outcome` を `expected` と照合してステータスを返す。
pub fn judge(outcome: &Outcome, expected: Expected) -> Status {
    match expected {
        Expected::Values(want) => match outcome {
            Outcome::Values(got) if got.as_slice() == want => Status::Ok,
            Outcome::Values(_) => Status::Mismatch,
            Outcome::Pairs(_) => Status::Mismatch,
        },
        Expected::Pairs(want) => match outcome {
            Outcome::Pairs(got) => {
                let got_borrowed: Vec<(&str, &str)> =
                    got.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                if got_borrowed.as_slice() == want {
                    Status::Ok
                } else {
                    Status::Mismatch
                }
            }
            Outcome::Values(_) => Status::Mismatch,
        },
        Expected::NonEmpty => {
            if outcome.has_meaningful_content() {
                Status::Ok
            } else {
                Status::UnexpectedEmpty
            }
        }
        Expected::Empty => {
            if outcome.is_empty() {
                Status::ExpectedEmpty
            } else {
                Status::Mismatch
            }
        }
    }
}

/// 類型別の集計（attempted: 試行数・reachable: 取得・パースに成功した数・
/// success: 期待どおりだった数）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub attempted: usize,
    pub reachable: usize,
    pub success: usize,
}

impl Summary {
    /// `reachable` を分母にした成功率（`reachable == 0` なら `0.0`。
    /// ゼロ除算を避けるため。呼び出し側は `attempted == 0` と合わせて
    /// 判定する）。
    pub fn rate(&self) -> f64 {
        if self.reachable == 0 {
            0.0
        } else {
            self.success as f64 / self.reachable as f64
        }
    }
}

/// `results` を類型別に集計する。`Category::Excluded` は分母に含めない
/// （X01 CSR シェルの扱い。PoC-2 の扱いに合わせる）。
pub fn summarize(results: &[TaskResult]) -> (Summary, Summary) {
    let mut static_summary = Summary::default();
    let mut ssr_spa_summary = Summary::default();
    for result in results {
        let summary = match result.category {
            Category::Static => &mut static_summary,
            Category::SsrSpaStatic => &mut ssr_spa_summary,
            Category::Excluded => continue,
        };
        summary.attempted += 1;
        if result.status.is_reachable() {
            summary.reachable += 1;
            if result.status.is_success() {
                summary.success += 1;
            }
        }
    }
    (static_summary, ssr_spa_summary)
}

/// 成功率の目標値（CORE-1・`core-dom.md`: 代表タスクの成功率 70% 以上）。
pub const TARGET_RATE: f64 = 0.70;

macro_rules! fixture {
    ($path:literal) => {
        include_bytes!(concat!("../../../../harness/compat_fixtures/", $path))
    };
}

/// 静的類型・SSR/SPA 静的類型のローカルタスク一覧
/// （`harness/compat_fixtures/README.md` のフィクスチャ表に対応。`main.rs`
/// （`local` サブコマンド）と結合テストの両方から使う）。
pub const LOCAL_TASKS: &[LocalTask] = &[
    LocalTask {
        id: "S01",
        category: Category::Static,
        fixture: fixture!("static/01-article.html"),
        kind: TaskKind::Texts,
        selector: "article h1, a[rel=author]",
        expected: Expected::Values(&["Rust の非同期ランタイム入門", "相田 藍子"]),
    },
    LocalTask {
        id: "S02",
        category: Category::Static,
        fixture: fixture!("static/02-table.html"),
        kind: TaskKind::Texts,
        selector: "#prices tbody td.name",
        expected: Expected::Values(&["キーボード", "マウス", "モニタ"]),
    },
    LocalTask {
        id: "S03",
        category: Category::Static,
        fixture: fixture!("static/03-login-form.html"),
        kind: TaskKind::Form,
        selector: "form#login",
        expected: Expected::Pairs(&[
            ("username", "user"),
            ("password", "dummy-password"),
            ("csrf_token", "tok-123"),
            ("remember", "on"),
        ]),
    },
    LocalTask {
        id: "S04",
        category: Category::Static,
        fixture: fixture!("static/04-product-list.html"),
        kind: TaskKind::Attr("data-sku"),
        selector: "li.product",
        expected: Expected::Values(&["sku-001", "sku-002", "sku-003"]),
    },
    LocalTask {
        id: "S05",
        category: Category::Static,
        fixture: fixture!("static/05-nav-links.html"),
        kind: TaskKind::Attr("href"),
        selector: "nav a",
        expected: Expected::Values(&["/", "/about", "/contact"]),
    },
    LocalTask {
        id: "S06",
        category: Category::Static,
        fixture: fixture!("static/06-images.html"),
        kind: TaskKind::Attr("alt"),
        selector: "figure img",
        expected: Expected::Values(&["ソファで眠る猫", "公園を走る犬"]),
    },
    LocalTask {
        id: "S07",
        category: Category::Static,
        fixture: fixture!("static/07-unicode.html"),
        kind: TaskKind::Texts,
        selector: "p.greeting",
        expected: Expected::Values(&[
            "こんにちは、世界 🌏",
            "Bonjour le monde 🥐",
            "Здравствуй, мир ✨",
        ]),
    },
    LocalTask {
        id: "S08",
        category: Category::Static,
        fixture: fixture!("static/08-malformed.html"),
        kind: TaskKind::Texts,
        selector: "ul.items li",
        // html5ever のツリー構築規則により、2 番目の `<li>` の開始タグは
        // 直前の `<li>` を implied end tag で閉じる一方、続く `<p>` は
        // `<li>` を閉じずにその子として入れ子になる（`</ul>` まで開いた
        // ままになる）。`text_content` は子孫の Text ノードをすべて連結する
        // ため、2 番目の `<li>` には `<p><b><i>` 配下のテキストも含まれる。
        // `</ul>` の外に出た末尾の `<li>ぶどう</li>` は `ul.items` の子孫に
        // ならないため対象外（このテストが確認する「エラー回復」の要点は、
        // 壊れた入力でも panic せず、HTML5 の木構築規則どおりに 2 要素へ
        // 回復することにある）。
        expected: Expected::Values(&["りんご", "みかん 説明文が 閉じタグなしで ネストしている"]),
    },
    LocalTask {
        id: "P01",
        category: Category::SsrSpaStatic,
        fixture: fixture!("ssr_spa_static/01-ssr-blog-list.html"),
        kind: TaskKind::Texts,
        selector: "main article h2",
        expected: Expected::Values(&[
            "サーバーサイドレンダリングの基礎",
            "ハイドレーションとは何か",
            "SPA とプリレンダリングの違い",
        ]),
    },
    LocalTask {
        id: "P02",
        category: Category::SsrSpaStatic,
        fixture: fixture!("ssr_spa_static/02-dashboard-table.html"),
        kind: TaskKind::Texts,
        selector: "td[data-col=revenue]",
        expected: Expected::Values(&["120000", "98000"]),
    },
    LocalTask {
        id: "P03",
        category: Category::SsrSpaStatic,
        fixture: fixture!("ssr_spa_static/03-checkbox-radio-form.html"),
        kind: TaskKind::Form,
        selector: "form#prefs",
        expected: Expected::Pairs(&[
            ("plan", "pro"),
            ("notify_email", "on"),
            ("bio", "よろしくお願いします。"),
        ]),
    },
    LocalTask {
        id: "P04",
        category: Category::SsrSpaStatic,
        fixture: fixture!("ssr_spa_static/04-next-prerendered.html"),
        kind: TaskKind::Texts,
        selector: "#__next h1",
        expected: Expected::Values(&["プリレンダリングされたトップページ"]),
    },
    LocalTask {
        id: "P05",
        category: Category::SsrSpaStatic,
        fixture: fixture!("ssr_spa_static/05-nuxt-hydrated-list.html"),
        kind: TaskKind::Attr("href"),
        selector: "#__nuxt a.item",
        expected: Expected::Values(&["/posts/1", "/posts/2", "/posts/3"]),
    },
    LocalTask {
        id: "X01",
        category: Category::Excluded,
        fixture: fixture!("ssr_spa_static/06-csr-shell.html"),
        kind: TaskKind::Texts,
        selector: "#root li",
        expected: Expected::Empty,
    },
];

/// [`LOCAL_TASKS`] を実行し、フィクスチャのパースと `run_task`/`judge` を
/// 通した [`TaskResult`] 列を返す。ローカルタスクのフィクスチャは
/// コンパイル時に埋め込まれた既知の UTF-8 HTML であり、パース失敗は
/// harness 側の不具合を意味するため `Status::ParseError` として記録しつつ
/// panic はしない（外部入力ではないが、`unwrap` を避ける方針は一貫させる）。
pub fn run_local() -> Vec<TaskResult> {
    LOCAL_TASKS
        .iter()
        .map(|task| {
            let parsed = match parse_document_bytes(task.fixture, &ParseOptions::default()) {
                Ok(parsed) => parsed,
                Err(_) => {
                    return TaskResult {
                        id: task.id,
                        category: task.category,
                        kind: task.kind,
                        status: Status::ParseError,
                        sample: String::new(),
                    };
                }
            };
            let outcome = run_task(&parsed.document, task.kind, task.selector);
            let (status, sample) = match outcome {
                Ok(outcome) => {
                    let sample = sample_of(&outcome);
                    (judge(&outcome, task.expected), sample)
                }
                Err(TaskError::SelectorUnsupported) => (Status::SelectorUnsupported, String::new()),
                Err(TaskError::Core) => (Status::QueryError, String::new()),
            };
            TaskResult {
                id: task.id,
                category: task.category,
                kind: task.kind,
                status,
                sample,
            }
        })
        .collect()
}

/// 出力用のサンプル文字列を [`Outcome`] から組み立てる（先頭 3 件まで。
/// coding-rust.md「結果件数の表示は先頭 N 件に制限する」）。
///
/// [`Outcome::Pairs`]（`Form` タスク）は実サイトモードで取得した実フォームの
/// 送信値（password・hidden 等。CSRF トークン等の秘密情報を含み得る）を
/// 集計するため、値そのものは一切出力せず、件数と項目名（`name` 属性）
/// のみを示す（security.md 秘密情報の混入防止 P0）。項目名も HTML 由来の
/// 外部入力であるため [`sanitize_sample`] で制御文字除去・長さ制限を適用
/// してから出力する（端末インジェクション対策）。
pub fn sample_of(outcome: &Outcome) -> String {
    const MAX_ITEMS: usize = 3;
    match outcome {
        Outcome::Values(values) => values
            .iter()
            .take(MAX_ITEMS)
            .map(|v| sanitize_sample(v))
            .collect::<Vec<_>>()
            .join(" | "),
        Outcome::Pairs(pairs) => {
            let names = pairs
                .iter()
                .take(MAX_ITEMS)
                .map(|(k, _)| sanitize_sample(k))
                .collect::<Vec<_>>()
                .join(", ");
            format!("fields={} names=[{names}]", pairs.len())
        }
    }
}

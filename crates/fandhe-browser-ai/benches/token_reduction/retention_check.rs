//! 情報保持チェック（7 種）の自動判定モジュール（TASK-14.4・`AISNAP-3`・Issue #95・`MS-2`）。
//!
//! 役割: 「非インタラクティブなデータ値を含む代表タスク 7 種について、snapshot から
//! `isDataLeaf` 相当のヒューリスティックで対象要素を判別できるか」を、`build_snapshot` と
//! `data_leaf`（TASK-13）の実装に対して機械判定する。PoC-4 の `task_check.mjs` の Rust 移植で、
//! 達成可否の最終判断（6/7 以上）は人間担当の #97 が行うため、本モジュールは判別可否を
//! タスクごとの具体値で返すだけで合否は決めない。
//!
//! 呼び出し元: ベンチ本体（`token_reduction.rs`）が結果表を stdout へ出し、ユニットテスト
//! （`retention_check_tests.rs`）が具体値を固定する。どちらも `#[path]` で本ファイルを取り込む。
//!
//! # 判定基準（PoC からの読み替え）
//!
//! snapshot には ref から DOM 要素へ戻る API も snapshot のテキスト直列化も無いため、PoC の
//! 「ref がテキストに出現し ref から元ノードへ戻れる」判定はそのまま移植できない。代わりに
//! 次の ref ベースの基準で判定する。
//!
//! 1. フィクスチャの DOM に PoC セレクタの一致要素が 1 件以上ある
//! 2. snapshot を先行順に平坦化した列に、期待シグネチャ（role・name・`data_leaf`）へ一致する
//!    エントリがある
//! 3. 先頭の一致エントリが空でない ref を持つ
//!
//! 「対象を判別できる」ことと「値を読める」ことは別である。たとえば価格（`p.price_color`）は
//! `data_leaf = PriceClass` と ref で判別できるが、role `generic`・name 空で価格文字列は
//! snapshot に入らない。一致エントリの name を出力（`matched_name`）して、この差が見えるようにする。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use fandhe_browser_ai::snapshot::{DataLeafKind, Node, build_snapshot};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;

/// 期待 name の出所。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameExpect {
    /// name を条件にしない。
    Any,
    /// DOM の先頭一致要素の `text_content`（空白正規化）と一致すること。
    DomText,
}

/// 代表タスク 1 件の定義（コンパイル時定数）。
pub struct Task {
    pub id: &'static str,
    pub fixture: &'static str,
    pub selector: &'static str,
    pub role: Option<&'static str>,
    pub name: NameExpect,
    pub data_leaf: Option<DataLeafKind>,
}

/// 7 種の代表タスク（PoC-4 `task_check.mjs` のセレクタと対応）。
pub const TASKS: [Task; 7] = [
    Task {
        id: "login-button",
        fixture: "login-form.html",
        selector: "button[type=submit]",
        role: Some("button"),
        name: NameExpect::DomText,
        data_leaf: None,
    },
    Task {
        id: "price-first",
        fixture: "ec-product-list.html",
        selector: "p.price_color",
        role: None,
        name: NameExpect::Any,
        data_leaf: Some(DataLeafKind::PriceClass),
    },
    Task {
        id: "number-input",
        fixture: "inputs-form.html",
        selector: "input[type=number]",
        role: Some("spinbutton"),
        name: NameExpect::Any,
        data_leaf: None,
    },
    Task {
        id: "table-header-first",
        fixture: "dashboard-table.html",
        selector: "table#table1 th",
        role: Some("columnheader"),
        name: NameExpect::DomText,
        data_leaf: Some(DataLeafKind::TableCell),
    },
    Task {
        id: "dropdown-select",
        fixture: "dropdown-form.html",
        selector: "select#dropdown",
        role: Some("combobox"),
        name: NameExpect::Any,
        data_leaf: None,
    },
    Task {
        id: "top-story-link",
        fixture: "hn-list.html",
        selector: ".athing .titleline > a",
        role: Some("link"),
        name: NameExpect::DomText,
        data_leaf: None,
    },
    Task {
        id: "checkbox-first",
        fixture: "checkboxes-form.html",
        selector: "input[type=checkbox]",
        role: Some("checkbox"),
        name: NameExpect::Any,
        data_leaf: None,
    },
];

/// 判別できなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotIdentifiedReason {
    /// DOM にセレクタ一致が無い（セレクタとフィクスチャの不整合）。
    TargetMissingInDom,
    /// snapshot に期待シグネチャへ一致するエントリが無い。
    NoMatchingNode,
    /// 一致エントリはあるが ref が無い（エージェントが操作・参照できない）。
    MatchWithoutRef,
}

impl NotIdentifiedReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::TargetMissingInDom => "target-missing-in-dom",
            Self::NoMatchingNode => "no-matching-node",
            Self::MatchWithoutRef => "match-without-ref",
        }
    }
}

/// タスク 1 件の判別結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 対象を判別できた（先頭一致エントリの ref）。
    Identified { r#ref: String },
    /// 判別できなかった。
    NotIdentified { reason: NotIdentifiedReason },
}

/// タスク 1 件の判定結果（出力 1 行分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCheck {
    pub id: &'static str,
    pub fixture: &'static str,
    pub selector: &'static str,
    /// DOM 上のセレクタ一致数。
    pub dom_matches: usize,
    /// snapshot 上の期待シグネチャ一致数。
    pub snapshot_matches: usize,
    /// 先頭一致エントリの name（空なら値そのものは snapshot に無い）。
    pub matched_name: String,
    /// 先頭一致エントリの `data_leaf`。
    pub data_leaf: Option<DataLeafKind>,
    /// snapshot が打ち切りされたか。
    pub snapshot_truncated: bool,
    pub verdict: Verdict,
}

/// 情報保持チェックのエラー（I/O・パース等。判別 NG はエラーにせず [`Verdict`] で返す）。
#[derive(Debug)]
pub enum RetentionCheckError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(String),
    Selector(String),
    Snapshot(String),
    /// DOM 先頭一致要素の `text_content` が取れない。
    Dom(String),
}

impl fmt::Display for RetentionCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::Parse(m) => write!(f, "failed to parse fixture: {m}"),
            Self::Selector(m) => write!(f, "selector query failed: {m}"),
            Self::Snapshot(m) => write!(f, "failed to build snapshot: {m}"),
            Self::Dom(m) => write!(f, "dom access failed: {m}"),
        }
    }
}

impl std::error::Error for RetentionCheckError {}

/// snapshot を平坦化した 1 エントリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatEntry {
    pub role: String,
    pub name: String,
    pub r#ref: Option<String>,
    pub data_leaf: Option<DataLeafKind>,
}

/// 連続空白を 1 つにまとめて前後を trim する。
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// snapshot を先行順（明示スタックの反復）で平坦化する。`Node::table` の
/// ヘッダセル・行内操作要素は `Node` の直後に展開する。`folded_rows` は契約上
/// ref・`data_leaf` を持たないため対象外。
pub fn flatten(root: &Node) -> Vec<FlatEntry> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        out.push(FlatEntry {
            role: node.role.clone(),
            name: node.name.clone(),
            r#ref: node.r#ref.clone(),
            data_leaf: node.data_leaf,
        });
        if let Some(table) = &node.table {
            for h in &table.header {
                out.push(FlatEntry {
                    role: h.role.clone(),
                    name: h.name.clone(),
                    r#ref: Some(h.r#ref.clone()),
                    data_leaf: h.data_leaf,
                });
            }
            for row in &table.rows {
                for c in &row.controls {
                    out.push(FlatEntry {
                        role: c.role.clone(),
                        name: c.name.clone(),
                        r#ref: Some(c.r#ref.clone()),
                        data_leaf: None,
                    });
                }
            }
        }
        stack.extend(node.children.iter().rev());
    }
    out
}

/// 平坦化済みエントリから期待シグネチャへの一致を数え、判別結果を決める。
pub fn judge(
    task: &Task,
    dom_matches: usize,
    dom_text: Option<&str>,
    entries: &[FlatEntry],
    snapshot_truncated: bool,
) -> TaskCheck {
    let matches: Vec<&FlatEntry> = entries
        .iter()
        .filter(|e| task.role.is_none_or(|r| e.role == r))
        .filter(|e| match (task.name, dom_text) {
            (NameExpect::DomText, Some(t)) => normalize_ws(&e.name) == t,
            (NameExpect::DomText, None) => false,
            (NameExpect::Any, _) => true,
        })
        .filter(|e| task.data_leaf.is_none_or(|k| e.data_leaf == Some(k)))
        .collect();
    let first = matches.first();
    let verdict = if dom_matches == 0 {
        Verdict::NotIdentified {
            reason: NotIdentifiedReason::TargetMissingInDom,
        }
    } else {
        match first {
            None => Verdict::NotIdentified {
                reason: NotIdentifiedReason::NoMatchingNode,
            },
            Some(e) => match e.r#ref.as_deref() {
                Some(r) if !r.is_empty() => Verdict::Identified {
                    r#ref: r.to_owned(),
                },
                _ => Verdict::NotIdentified {
                    reason: NotIdentifiedReason::MatchWithoutRef,
                },
            },
        }
    };
    TaskCheck {
        id: task.id,
        fixture: task.fixture,
        selector: task.selector,
        dom_matches,
        snapshot_matches: matches.len(),
        matched_name: first.map(|e| e.name.clone()).unwrap_or_default(),
        data_leaf: first.and_then(|e| e.data_leaf),
        snapshot_truncated,
        verdict,
    }
}

/// `dir`（`benches/fixtures`）のフィクスチャで 7 種のタスクを判定し、表の順で返す。
pub fn run_retention_checks(dir: &Path) -> Result<Vec<TaskCheck>, RetentionCheckError> {
    let mut out = Vec::with_capacity(TASKS.len());
    for task in &TASKS {
        let path = dir.join(task.fixture);
        let html = fs::read_to_string(&path).map_err(|source| RetentionCheckError::Io {
            path: path.clone(),
            source,
        })?;
        let parsed = parse_document(&html, &ParseOptions::default())
            .map_err(|e| RetentionCheckError::Parse(e.to_string()))?;
        let doc = &parsed.document;
        let found = query_selector_all_str(doc, doc.root(), task.selector)
            .map_err(|e| RetentionCheckError::Selector(e.to_string()))?;
        let dom_text = match found.first() {
            Some(id) => {
                let text = doc.text_content(*id).ok_or_else(|| {
                    RetentionCheckError::Dom(format!("no text_content for {}", task.selector))
                })?;
                Some(normalize_ws(&text))
            }
            None => None,
        };
        let snapshot =
            build_snapshot(doc).map_err(|e| RetentionCheckError::Snapshot(e.to_string()))?;
        let entries = flatten(&snapshot.tree);
        out.push(judge(
            task,
            found.len(),
            dom_text.as_deref(),
            &entries,
            snapshot.truncated,
        ));
    }
    Ok(out)
}

/// 判別できたタスク数。
pub fn identified_count(checks: &[TaskCheck]) -> usize {
    checks
        .iter()
        .filter(|c| matches!(c.verdict, Verdict::Identified { .. }))
        .count()
}

/// TSV 用に制御文字（タブ・改行）を空白へ置換し、長い値を 80 文字で切る。
fn tsv_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(80)
        .collect()
}

fn leaf_str(k: Option<DataLeafKind>) -> &'static str {
    match k {
        Some(DataLeafKind::TableCell) => "TableCell",
        Some(DataLeafKind::PriceClass) => "PriceClass",
        // DataLeafKind は non_exhaustive。将来の種別は未知として出す。
        Some(_) => "Other",
        None => "-",
    }
}

/// 出力表のヘッダ行（TSV）。
pub const TSV_HEADER: &str = "task\tfixture\tselector\tidentified\tdomMatches\tsnapshotMatches\tref\tdataLeaf\tname\tsnapshotTruncated\treason";

/// 判定結果 1 件を TSV 1 行へ整形する。
pub fn format_row(c: &TaskCheck) -> String {
    let (identified, r, reason) = match &c.verdict {
        Verdict::Identified { r#ref } => ("yes", r#ref.as_str(), "-"),
        Verdict::NotIdentified { reason } => ("no", "-", reason.as_str()),
    };
    let name = if c.matched_name.is_empty() {
        "-".to_owned()
    } else {
        tsv_safe(&c.matched_name)
    };
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        c.id,
        c.fixture,
        c.selector,
        identified,
        c.dom_matches,
        c.snapshot_matches,
        r,
        leaf_str(c.data_leaf),
        name,
        c.snapshot_truncated,
        reason
    )
}

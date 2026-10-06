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
//! 2. snapshot を先行順に平坦化した列に、期待シグネチャ（role・name・`data_leaf`）へ一致し、
//!    かつ ref のダイジェストと出現番号が「DOM 先頭一致要素から再計算した ref」と一致する
//!    エントリがある（DOM 要素と snapshot エントリの同一性。下記）
//! 3. 先頭の一致エントリが空でない ref を持つ
//!
//! # 同一性の検証（`AISNAP-10`）
//!
//! snapshot は NodeId を公開しないため、DOM 先頭一致要素の祖先鎖から `build_snapshot` と同じ規則
//! （role・name・識別属性・親 ref スコープ）で ref ダイジェストを再計算し、さらに文書順で先行する
//! 同一ダイジェスト要素の数から出現番号（`-n`）を求めて、エントリの ref と突き合わせる。
//! これで「対象が snapshot から消えても別要素（兄弟を含む）が同じ role・name を持てば
//! Identified になる」誤判定を防ぐ。出現番号は DOM のみから再現するため、`build_snapshot` が
//! 省略する要素（hidden・圧縮等）が先行すると実番号とずれうるが、その場合は一致しない側
//! （NotIdentified）へ倒れる（fail-closed）。
//!
//! 「対象を判別できる」ことと「値を読める」ことは別である。たとえば価格（`p.price_color`）は
//! `data_leaf = PriceClass` と ref で判別できるが、role `generic`・name 空で価格文字列は
//! snapshot に入らない。一致エントリの name を出力（`matched_name`）して、この差が見えるようにする。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use fandhe_browser_ai::snapshot::element_ref::ElementSignature;
use fandhe_browser_ai::snapshot::{
    DataLeafKind, ElementRef, NameIndex, Node, RefAllocator, build_snapshot,
    compute_name_with_index, compute_role,
};
use fandhe_browser_core::dom::{Document, NodeId};
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

/// `ref` 文字列（`e<16hex>[v<n>][-<n>]`）をダイジェストと出現番号（無印は 1）へ分解する。
/// 形式に合わない文字列は `None`（同一性を確認できないため一致扱いにしない）。
/// agent_eval（TASK-21.2・#120）の ref 解決からも使う。
pub fn parse_ref(r: &str) -> Option<(u64, u32)> {
    let rest = r.strip_prefix('e')?;
    let digest = u64::from_str_radix(rest.get(..16)?, 16).ok()?;
    let mut tail = rest.get(16..)?;
    if let Some(v) = tail.strip_prefix('v') {
        let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
        v.get(..end)?.parse::<u32>().ok()?;
        tail = v.get(end..)?;
    }
    match tail.strip_prefix('-') {
        None if tail.is_empty() => Some((digest, 1)),
        Some(n) => Some((digest, n.parse().ok()?)),
        None => None,
    }
}

/// `build_snapshot` の `discriminator`（`id` → フォーム部品の `name` → `a`/`area` の `href`）と同じ規則。
fn discriminator(doc: &Document, id: NodeId) -> Option<&str> {
    if let Some(v) = doc.attribute(id, "id").filter(|s| !s.is_empty()) {
        return Some(v);
    }
    let local = doc.local_name(id).unwrap_or("");
    if matches!(local, "input" | "select" | "textarea" | "button")
        && let Some(v) = doc.attribute(id, "name").filter(|s| !s.is_empty())
    {
        return Some(v);
    }
    if matches!(local, "a" | "area") {
        return doc.attribute(id, "href").filter(|s| !s.is_empty());
    }
    None
}

/// ルート直下から `id` までの要素鎖の ref を `build_snapshot` と同じ規則で再計算する。
/// role を持たない要素が鎖にあれば `None`（snapshot へ入らない要素）。
/// 出現番号は scope に折り込まれないため、新規アロケータで十分（`AISNAP-10`）。
fn chain_ref(doc: &Document, index: &NameIndex<'_>, id: NodeId) -> Option<ElementRef> {
    let mut path: Vec<NodeId> = doc.ancestors(id).filter(|&a| doc.is_element(a)).collect();
    path.reverse();
    path.push(id);
    let mut scope: Option<ElementRef> = None;
    for e in path {
        let role = compute_role(doc, e)?;
        let name = compute_name_with_index(doc, index, e);
        let mut sig = ElementSignature::new(role.as_str(), &name.text);
        if let Some(d) = discriminator(doc, e) {
            sig = sig.with_discriminator(d);
        }
        if let Some(sc) = scope {
            sig = sig.with_scope(sc);
        }
        scope = Some(RefAllocator::new().allocate_signature(&sig).ok()?);
    }
    scope
}

/// 文書順で `id` より前にある要素のうち、`same` を満たすものの数を返す。
/// `build_snapshot` は同じダイジェストの要素へ先行順に出現番号を振るため、
/// 「先行する同一シグネチャ要素の数 + 1」が対象の出現番号になる。
fn preceding_count(doc: &Document, id: NodeId, same: impl Fn(NodeId) -> bool) -> u32 {
    let mut n: u32 = 0;
    for e in doc.descendants(doc.root()) {
        if e == id {
            break;
        }
        if doc.is_element(e) && same(e) {
            n = n.saturating_add(1);
        }
    }
    n
}

/// DOM 要素 `id` に対応しうる snapshot ref の（ダイジェスト, 出現番号）の集合を返す。
/// 通常要素は祖先鎖の ref と、先行する同一ダイジェスト要素数から求めた出現番号。
/// 表のヘッダセルは圧縮経路が表 ref 直下のスコープで発行する（`id` 属性のみを
/// 識別属性とする）ため、その分も含める。空は snapshot へ入らない要素。
///
/// 出現番号は DOM のみから再現するため、`build_snapshot` が省略する要素（hidden・
/// 深さ超過）が先行すると実際の番号とずれうる。ずれた場合は一致しない側
/// （`NotIdentified`）へ倒れる。圧縮行の操作要素は `entries`（snapshot の平坦化結果）の
/// 件数と突き合わせ、省略があれば候補を出さない（fail-closed）。
pub fn target_refs(doc: &Document, id: NodeId, entries: &[FlatEntry]) -> Vec<(u64, u32)> {
    let index = NameIndex::build(doc);
    let mut out = Vec::new();
    if let Some(r) = chain_ref(doc, &index, id) {
        let role = compute_role(doc, id);
        let before = preceding_count(doc, id, |e| {
            compute_role(doc, e) == role
                && chain_ref(doc, &index, e).is_some_and(|c| c.digest == r.digest)
        });
        out.push((r.digest, before.saturating_add(1)));
    }
    if matches!(doc.local_name(id), Some("th" | "td"))
        && let Some(table) = doc
            .ancestors(id)
            .find(|&a| doc.local_name(a) == Some("table"))
        && let Some(table_ref) = chain_ref(doc, &index, table)
    {
        let scoped = |e: NodeId| -> Option<u64> {
            let role = compute_role(doc, e)?;
            let name = compute_name_with_index(doc, &index, e);
            let mut sig = ElementSignature::new(role.as_str(), &name.text);
            if let Some(d) = doc.attribute(e, "id").filter(|s| !s.is_empty()) {
                sig = sig.with_discriminator(d);
            }
            sig = sig.with_scope(table_ref);
            RefAllocator::new()
                .allocate_signature(&sig)
                .ok()
                .map(|r| r.digest)
        };
        if let Some(d) = scoped(id) {
            let before = preceding_count(doc, id, |e| {
                matches!(doc.local_name(e), Some("th" | "td")) && scoped(e) == Some(d)
            });
            out.push((d, before.saturating_add(1)));
        }
    }
    out.extend(compressed_control_refs(doc, &index, id, entries));
    out
}

/// 圧縮された表・一覧（`table`/`ul`/`ol`）の行内操作要素（`a[href]`・`button`）が
/// `TableRow::controls` で受ける ref の（ダイジェスト, 出現番号）候補を返す。
///
/// `build_row_controls`（`AISNAP-13`）は行内の操作要素を、中間要素ではなくコンテナ
/// （圧縮された表・一覧）の ref 直下のスコープで発行するため、祖先鎖から再計算する
/// 通常の ref とはダイジェストが一致しない。そのため祖先の `table`/`ul`/`ol` ごとに
/// 「コンテナ scope・role（role 無しは `a`→link・他→button）・name・識別属性」で再計算する。
/// 展開されて圧縮されなかった場合は実在しない ref になり、snapshot 側と一致しない
/// 候補が増えるだけ（fail-closed）。出現番号は同一ダイジェストの先行要素数から求める。
fn compressed_control_refs(
    doc: &Document,
    index: &NameIndex<'_>,
    id: NodeId,
    entries: &[FlatEntry],
) -> Vec<(u64, u32)> {
    let is_control = |e: NodeId| {
        let local = doc.local_name(e).unwrap_or("");
        (local == "a" && doc.attribute(e, "href").is_some()) || local == "button"
    };
    if !is_control(id) {
        return Vec::new();
    }
    let control_sig = |e: NodeId, scope: ElementRef| {
        let role = compute_role(doc, e)
            .map(|r| r.as_str().to_string())
            .unwrap_or_else(|| {
                if doc.local_name(e) == Some("a") {
                    "link".to_string()
                } else {
                    "button".to_string()
                }
            });
        let name = compute_name_with_index(doc, index, e);
        let mut sig = ElementSignature::new(&role, &name.text).with_scope(scope);
        if let Some(d) = discriminator(doc, e) {
            sig = sig.with_discriminator(d);
        }
        RefAllocator::new()
            .allocate_signature(&sig)
            .ok()
            .map(|r| r.digest)
    };
    let mut out = Vec::new();
    for container in doc
        .ancestors(id)
        .filter(|&a| matches!(doc.local_name(a), Some("table" | "ul" | "ol")))
    {
        let Some(scope) = chain_ref(doc, index, container) else {
            continue;
        };
        let Some(digest) = control_sig(id, scope) else {
            continue;
        };
        // 行・表の上限と優先保持で省略された要素は ref を持たず、DOM 上の先行数から求めた
        // 出現番号が後続の保持要素の番号とずれる。保持計画は非公開のため、同ダイジェストの
        // DOM 上の件数と snapshot 上の件数が食い違う場合は番号を確定できないとして
        // 候補を出さない（fail-closed。別要素の ref を誤って解決しない）。
        // 同じシグネチャ（= 同じ ref ダイジェスト）のコンテナが複数あると、それぞれの
        // 操作要素が同じダイジェストを共有し、snapshot 側の件数・出現番号は文書全体で
        // 通しになる。DOM 側も同じ範囲（scope ダイジェストが一致するコンテナ配下すべて）で
        // 数えて snapshot_total と比較・採番する。
        let in_container = |e: NodeId| {
            doc.ancestors(e).any(|a| {
                matches!(doc.local_name(a), Some("table" | "ul" | "ol"))
                    && chain_ref(doc, index, a).is_some_and(|c| c.digest == scope.digest)
            })
        };
        let dom_total = doc
            .descendants(doc.root())
            .filter(|&e| {
                doc.is_element(e)
                    && is_control(e)
                    && in_container(e)
                    && control_sig(e, scope) == Some(digest)
            })
            .count();
        let snapshot_total = entries
            .iter()
            .filter_map(|e| e.r#ref.as_deref().and_then(parse_ref))
            .filter(|&(d, _)| d == digest)
            .count();
        if dom_total != snapshot_total {
            continue;
        }
        let before = preceding_count(doc, id, |e| {
            is_control(e) && in_container(e) && control_sig(e, scope) == Some(digest)
        });
        out.push((digest, before.saturating_add(1)));
    }
    out
}

/// 平坦化済みエントリから期待シグネチャへの一致を数え、判別結果を決める。
///
/// `target_refs` は DOM 先頭一致要素から再計算した ref の（ダイジェスト, 出現番号）
/// （[`target_refs`]）。ref を持つエントリは、シグネチャに加えて ref のダイジェストと
/// 出現番号の組がこの集合に含まれる場合だけ一致とする（同じ親・同じシグネチャの兄弟を
/// 出現番号で区別する）（DOM 要素と snapshot エントリの同一性）。ref を持たないエントリは同一性を
/// 検証できないため、原因を `MatchWithoutRef` として報告できるようシグネチャ一致のみで残す。
pub fn judge(
    task: &Task,
    dom_matches: usize,
    dom_text: Option<&str>,
    target_refs: &[(u64, u32)],
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
        .filter(|e| match e.r#ref.as_deref() {
            None | Some("") => true,
            Some(r) => parse_ref(r).is_some_and(|p| target_refs.contains(&p)),
        })
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
        let target = found
            .first()
            .map(|id| target_refs(doc, *id, &entries))
            .unwrap_or_default();
        out.push(judge(
            task,
            found.len(),
            dom_text.as_deref(),
            &target,
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
        Some(DataLeafKind::ProseClass) => "ProseClass",
        Some(DataLeafKind::Quote) => "Quote",
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

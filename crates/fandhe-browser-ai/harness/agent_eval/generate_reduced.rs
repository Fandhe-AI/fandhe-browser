//! 簡約表現生成ハーネス（TASK-21.2・`AISNAP-8`・Issue #120・`MS-2`）。
//!
//! 役割: 代表タスク 25 件（`tasks.rs`）が指す fixture ページごとに、エージェントへ渡す
//! 簡約表現テキストを `build_snapshot`（TASK-11〜13）と `snapshot_text::render_snapshot` で生成する。
//! あわせて golden のロケータを snapshot の ref へ解決し、#121（採点）が照合できる形で返す。
//! PoC の `generate_reduced.mjs` に相当する準備工程。
//!
//! 呼び出し元: `generate_reduced_tests.rs`（`[[test]] agent_eval_generate_reduced`）。#121 も
//! `#[path]` で取り込む。本ファイルは `crate::tasks`・`crate::snapshot_text`・`crate::retention_check`
//! を参照するため、取り込み側のルートが同名の mod を宣言していること。
//!
//! 暫定（REPAIR-3）: 簡約表現の形式は `snapshot_text` の暫定行形式であり、`AISNAP-8` が前提とする
//! `GET /ai/snapshot` の確定応答ではない。TASK-19・`AISNAP-6` の確定後に `snapshot_text` 側を
//! 差し替えれば本ハーネスも追従する。ref 解決は DOM からの再計算で、先行要素が省略されると
//! 不一致側（空配列）へ倒れる（fail-closed）。解決できないことはエラーにせず結果として記録する。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::retention_check::{FlatEntry, flatten, parse_ref, target_refs};
use crate::snapshot_text::render_snapshot;
use crate::tasks::{GOLDEN, Golden, Locator, TASKS, json_str};
use fandhe_browser_ai::data_leaf::{DataLeafKind, classify_data_leaf};
use fandhe_browser_ai::snapshot::{Snapshot, build_snapshot};
use fandhe_browser_core::dom::{Document, NodeId};
use fandhe_browser_core::parse::{ParseOptions, parse_document};
use fandhe_browser_core::query::query_selector_all_str;

/// 生成処理の失敗。
#[derive(Debug)]
pub enum GenerateError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        page: String,
        message: String,
    },
    Snapshot {
        page: String,
        message: String,
    },
    Selector {
        task: String,
        selector: String,
        message: String,
    },
    LocatorOutOfRange {
        task: String,
        selector: String,
        index: usize,
        found: usize,
    },
}

impl fmt::Display for GenerateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { page, message } => write!(f, "parse failed for {page}: {message}"),
            Self::Snapshot { page, message } => write!(f, "snapshot failed for {page}: {message}"),
            Self::Selector {
                task,
                selector,
                message,
            } => write!(f, "{task}: selector {selector:?} failed: {message}"),
            Self::LocatorOutOfRange {
                task,
                selector,
                index,
                found,
            } => write!(
                f,
                "{task}: selector {selector:?} matched {found} elements, index {index} out of range"
            ),
        }
    }
}

impl std::error::Error for GenerateError {}

/// 1 ページ分の簡約表現。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReducedPage {
    pub page: String,
    /// エージェントへ渡すテキスト（末尾改行 1 個）。
    pub text: String,
    /// snapshot が上限で打ち切られたか。
    pub truncated: bool,
}

/// 解決済みロケータ。`refs` は文書順・重複なし。解決不能は空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLocator {
    pub selector: String,
    pub index: usize,
    pub refs: Vec<String>,
}

/// 1 タスク分の解決結果（golden が持つ全ロケータ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTask {
    pub id: String,
    pub locators: Vec<ResolvedLocator>,
}

/// `TASKS` が参照するページ名（重複なし・昇順）。
pub fn task_pages() -> Vec<&'static str> {
    TASKS
        .iter()
        .map(|t| t.page)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn load(fixtures_dir: &Path, page: &str) -> Result<Document, GenerateError> {
    let path = fixtures_dir.join(format!("{page}.html"));
    let html = std::fs::read_to_string(&path).map_err(|source| GenerateError::Io {
        path: path.clone(),
        source,
    })?;
    parse_document(&html, &ParseOptions::default())
        .map(|p| p.document)
        .map_err(|e| GenerateError::Parse {
            page: page.to_owned(),
            message: e.to_string(),
        })
}

fn snapshot_of(page: &str, doc: &Document) -> Result<Snapshot, GenerateError> {
    build_snapshot(doc).map_err(|e| GenerateError::Snapshot {
        page: page.to_owned(),
        message: e.to_string(),
    })
}

/// 1 ページの簡約表現を生成する。
pub fn generate_page(fixtures_dir: &Path, page: &str) -> Result<ReducedPage, GenerateError> {
    let doc = load(fixtures_dir, page)?;
    let snap = snapshot_of(page, &doc)?;
    // 空の name 等で行末に空白が残る（例: `header: `）。意味を持たず、生成物が .editorconfig の
    // trim_trailing_whitespace 検査（lint-docs）に通らないため、行ごとに末尾空白だけを落とす。
    let notes = value_annotations(&doc, &flatten(&snap.tree));
    let mut text = render_snapshot(&snap)
        .lines()
        .map(|l| annotate_line(l.trim_end(), &notes))
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    Ok(ReducedPage {
        page: page.to_owned(),
        text,
        truncated: snap.truncated,
    })
}

/// 値注釈の本文の上限（文字数）。長文でもエージェント入力が肥大しないようにする。
const NOTE_MAX_CHARS: usize = 300;

/// 要素に対応する snapshot ref 文字列（文書順・重複なし。解決不能は空）。
fn refs_of(doc: &Document, node: NodeId, entries: &[FlatEntry]) -> Vec<String> {
    let cands = target_refs(doc, node, entries);
    let mut refs: Vec<String> = Vec::new();
    for e in entries {
        if let Some(r) = &e.r#ref
            && parse_ref(r).is_some_and(|p| cands.contains(&p))
            && !refs.contains(r)
        {
            refs.push(r.clone());
        }
    }
    refs
}

/// 簡約表現へ補う値注釈（ref → 表示文字列。`"本文"` と任意の `value="…"` 接尾辞）。
///
/// snapshot はデータ葉（価格クラス・引用・地の文クラス）の本文と `option` のラベル / value を
/// 持たない（値は ref で別途取得する前提。`AISNAP-3`）。エージェント評価（#121）が値抽出・
/// 選択肢の特定に回答できるよう、本ハーネスの出力に限り DOM から本文を補う（`snapshot_text` の
/// 暫定形式は変えない）。名前を持たない generic 行のみが対象。ref を一意に解決できた要素だけ補う。
fn value_annotations(doc: &Document, entries: &[FlatEntry]) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for id in doc.descendants(doc.root()) {
        if !doc.is_element(id) {
            continue;
        }
        let is_option = doc.local_name(id) == Some("option");
        let leaf = matches!(
            classify_data_leaf(doc, id),
            Some(DataLeafKind::PriceClass | DataLeafKind::ProseClass | DataLeafKind::Quote)
        );
        if !is_option && !leaf {
            continue;
        }
        let raw = doc.text_content(id).unwrap_or_default();
        let norm = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if norm.is_empty() {
            continue;
        }
        let shown: String = norm.chars().take(NOTE_MAX_CHARS).collect();
        let suffix = if is_option {
            doc.attribute(id, "value")
                .map(|v| format!(" value={}", json_str(v)))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let refs = refs_of(doc, id, entries);
        if let [r] = refs.as_slice() {
            out.insert(r.clone(), (shown, suffix));
        }
    }
    out
}

/// `- generic [ref]` 形式の行（名前なし）へ値注釈を挿入する。該当しない行はそのまま返す。
fn annotate_line(line: &str, notes: &BTreeMap<String, (String, String)>) -> String {
    let Some(rest) = line.trim_start().strip_prefix("- generic [") else {
        return line.to_owned();
    };
    let Some(end) = rest.find(']') else {
        return line.to_owned();
    };
    let Some(r) = rest.get(..end) else {
        return line.to_owned();
    };
    let Some((text, suffix)) = notes.get(r) else {
        return line.to_owned();
    };
    let indent = &line[..line.len() - line.trim_start().len()];
    let tail = rest.get(end + 1..).unwrap_or("");
    format!("{indent}- generic {} [{r}]{tail}{suffix}", json_str(text))
}

/// 全タスク対象ページの簡約表現を `task_pages()` の順で生成する。
pub fn generate_all(fixtures_dir: &Path) -> Result<Vec<ReducedPage>, GenerateError> {
    task_pages()
        .into_iter()
        .map(|p| generate_page(fixtures_dir, p))
        .collect()
}

/// golden が持つ全ロケータ（出現順）。
pub fn golden_locators(g: &Golden) -> Vec<Locator> {
    match g {
        Golden::Ref { any_of } => any_of.to_vec(),
        Golden::Value { source, .. } => vec![*source],
        Golden::Steps(steps) => steps.iter().map(|s| s.target).collect(),
    }
}

/// 全タスクの golden ロケータを ref へ解決する（`TASKS` と同順）。
pub fn resolve_golden(fixtures_dir: &Path) -> Result<Vec<ResolvedTask>, GenerateError> {
    let mut cache: BTreeMap<&str, (Document, Vec<FlatEntry>)> = BTreeMap::new();
    let mut out = Vec::with_capacity(TASKS.len());
    for t in &TASKS {
        if !cache.contains_key(t.page) {
            let doc = load(fixtures_dir, t.page)?;
            let snap = snapshot_of(t.page, &doc)?;
            cache.insert(t.page, (doc, flatten(&snap.tree)));
        }
        let Some((doc, entries)) = cache.get(t.page) else {
            continue;
        };
        let golden = GOLDEN.iter().find(|(id, _)| *id == t.id).map(|(_, g)| g);
        let mut locators = Vec::new();
        for l in golden.map(golden_locators).unwrap_or_default() {
            let found = query_selector_all_str(doc, doc.root(), l.selector).map_err(|e| {
                GenerateError::Selector {
                    task: t.id.to_owned(),
                    selector: l.selector.to_owned(),
                    message: e.to_string(),
                }
            })?;
            let node =
                found
                    .get(l.index)
                    .copied()
                    .ok_or_else(|| GenerateError::LocatorOutOfRange {
                        task: t.id.to_owned(),
                        selector: l.selector.to_owned(),
                        index: l.index,
                        found: found.len(),
                    })?;
            let refs = refs_of(doc, node, entries);
            locators.push(ResolvedLocator {
                selector: l.selector.to_owned(),
                index: l.index,
                refs,
            });
        }
        out.push(ResolvedTask {
            id: t.id.to_owned(),
            locators,
        });
    }
    Ok(out)
}

/// `golden-refs.json`（採点側。エージェントへ渡さない）の内容。2 スペースインデント・LF・末尾改行。
pub fn render_golden_refs_json(tasks: &[ResolvedTask]) -> String {
    let items: Vec<String> = tasks
        .iter()
        .map(|t| {
            let locs: Vec<String> = t
                .locators
                .iter()
                .map(|l| {
                    let refs: Vec<String> = l.refs.iter().map(|r| json_str(r)).collect();
                    format!(
                        "      {{\"selector\": {}, \"index\": {}, \"refs\": [{}]}}",
                        json_str(&l.selector),
                        l.index,
                        refs.join(", ")
                    )
                })
                .collect();
            format!(
                "  {{\n    \"id\": {},\n    \"locators\": [\n{}\n    ]\n  }}",
                json_str(&t.id),
                locs.join(",\n")
            )
        })
        .collect();
    format!("[\n{}\n]\n", items.join(",\n"))
}

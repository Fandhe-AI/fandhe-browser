//! パース済み DOM から実行対象の `<script>` を文書順に集めるモジュール
//! （TASK-109・MS-6・Issue #774・ビヘイビア `JS-4`・`JS-6`）。
//!
//! 呼び出し元は、ページスクリプトランナー（[`crate::page_runner`]。#780・#781）。
//! 本モジュールは **収集だけ** を担い、
//! スクリプトの評価・ネットワークアクセスは一切行わない。
//!
//! # 収集規則
//!
//! - 対象は HTML 名前空間の `<script>`（前順 = 文書順）。`<template>` の template
//!   contents は [`Document::descendants`] に含まれないため集まらない。HTML の
//!   `<noscript>` 配下は、既定のパース設定（`scripting_enabled: false`）で要素として
//!   組み立てられるため、祖先判定で明示的に除外する。ただし head 直下の `<noscript>` 内の
//!   `<script>` は、パーサーが noscript を閉じて head の子にするため除外できない
//!   （既知の制約。`scripting_enabled` を切り替えたパースは本タスクの範囲外）。
//! - `type` は WHATWG の JavaScript MIME type essence 一覧（`text/javascript` 等）、
//!   未指定、空のとき classic として収集する。`module` は収集せず
//!   [`ScriptDiagnosticKind::SkippedModule`]、その他は
//!   [`ScriptDiagnosticKind::SkippedUnsupportedType`] を診断に残す（`JS-4`。未知の型は
//!   実行しない側に倒す）。
//! - `src` は inline 本文より優先し、値は未検証の生の文字列として保持する。
//! - 収集件数（既定 [`DEFAULT_MAX_PAGE_SCRIPTS`]）と inline 1 件のバイト数
//!   （既定 [`DEFAULT_MAX_INLINE_SCRIPT_BYTES`]）に上限を設け、本文を確保する前に判定する
//!   （`JS-6`）。診断の記録も [`MAX_SCRIPT_DIAGNOSTICS`] 件で頭打ちにする。
//!
//! # 責務外・未実装（実装済みを装わない。REPAIR-3）
//!
//! - スクリプトの評価、`src` の URL 検証・取得（`Fetcher` 経由のみ。`JS-8`）、総バイト上限、
//!   DOMContentLoaded / load の発火（いずれも [`crate::page_runner`] の担当。#780・#781 で実装済み）。
//! - SVG の `<script>`、`language` 属性、`type=module`・import map の実行、
//!   `crossorigin`・`integrity`・`referrerpolicy`、`charset`、`document.write` で
//!   挿入されるスクリプト。
//! - 設定ファイルからの上限値の読み込み（TASK-112）と、可観測性（`OperationRecorder`）
//!   の計装（対応する `OperationKind` が無い）。

use crate::dom::{Document, HTML_NAMESPACE_URI, NodeData, NodeId};
use crate::selector::html_local_name_eq;

/// 収集する `<script>`（classic）件数の既定上限（`JS-6`）。
pub const DEFAULT_MAX_PAGE_SCRIPTS: usize = 64;
/// inline `<script>` 1 件の本文バイト数の既定上限（`JS-6`）。
pub const DEFAULT_MAX_INLINE_SCRIPT_BYTES: usize = 1024 * 1024;
/// 記録する診断の最大件数。超過分は [`CollectedScripts::dropped_diagnostics`] に数える。
pub const MAX_SCRIPT_DIAGNOSTICS: usize = 256;

/// classic script として扱う MIME type essence（WHATWG HTML の JavaScript MIME type）。
const JS_MIME_ESSENCES: &[&str] = &[
    "text/javascript",
    "application/javascript",
    "application/ecmascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
];

/// 収集の上限設定。設定ファイルへの配線は TASK-112 の担当。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptCollectionOptions {
    max_scripts: usize,
    max_inline_bytes: usize,
}

impl Default for ScriptCollectionOptions {
    fn default() -> Self {
        Self {
            max_scripts: DEFAULT_MAX_PAGE_SCRIPTS,
            max_inline_bytes: DEFAULT_MAX_INLINE_SCRIPT_BYTES,
        }
    }
}

impl ScriptCollectionOptions {
    /// 収集件数の上限を変更する。
    pub fn with_max_scripts(mut self, max_scripts: usize) -> Self {
        self.max_scripts = max_scripts;
        self
    }

    /// inline 1 件のバイト上限を変更する。
    pub fn with_max_inline_bytes(mut self, max_inline_bytes: usize) -> Self {
        self.max_inline_bytes = max_inline_bytes;
        self
    }

    /// 収集件数の上限。
    pub fn max_scripts(&self) -> usize {
        self.max_scripts
    }

    /// inline 1 件のバイト上限。
    pub fn max_inline_bytes(&self) -> usize {
        self.max_inline_bytes
    }
}

/// `<script>` の内容の出どころ。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptSource {
    /// inline 本文（子の Text ノードを連結したもの）。
    Inline(String),
    /// `src` 属性の生の値。trim・URL 解析・scheme 検証は行っていない未検証の値で、
    /// 取得時は #781 が `Fetcher` 経由で検証する（`JS-8`）。
    External(String),
}

/// 収集した実行対象の `<script>` 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptEntry {
    node: NodeId,
    index: usize,
    source: ScriptSource,
    is_async: bool,
    is_defer: bool,
    is_nomodule: bool,
    type_attr: Option<String>,
}

impl ScriptEntry {
    /// 対応する DOM ノード（`document.currentScript` 用）。
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// 対象 `<script>` の文書順の通し番号（0 始まり）。収集されなかったものにも
    /// 番号を振るため、診断の `index` と同じ体系になる。
    pub fn index(&self) -> usize {
        self.index
    }

    /// 内容の出どころ。
    pub fn source(&self) -> &ScriptSource {
        &self.source
    }

    /// `async` 属性の有無（並びは文書順のまま。`JS-4`）。
    pub fn is_async(&self) -> bool {
        self.is_async
    }

    /// `defer` 属性の有無（並びは文書順のまま。`JS-4`）。
    pub fn is_defer(&self) -> bool {
        self.is_defer
    }

    /// `nomodule` 属性の有無。module を実行しない本実装では収集対象に含める。
    pub fn is_nomodule(&self) -> bool {
        self.is_nomodule
    }

    /// 生の `type` 属性値。
    pub fn type_attr(&self) -> Option<&str> {
        self.type_attr.as_deref()
    }
}

/// 診断の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptDiagnosticKind {
    /// `type=module` をスキップした。
    SkippedModule,
    /// JavaScript 以外の `type` をスキップした。
    SkippedUnsupportedType,
    /// 件数上限を超えた分を収集しなかった。
    ScriptLimitExceeded,
    /// inline 本文がバイト上限を超えたため収集しなかった。
    InlineScriptTooLarge,
    /// `src=""`（HTML 仕様ではエラー扱いで実行しない）。
    EmptySrc,
}

impl ScriptDiagnosticKind {
    /// ログ・診断で使う安定した表記。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SkippedModule => "skipped_module",
            Self::SkippedUnsupportedType => "skipped_unsupported_type",
            Self::ScriptLimitExceeded => "script_limit_exceeded",
            Self::InlineScriptTooLarge => "inline_script_too_large",
            Self::EmptySrc => "empty_src",
        }
    }
}

/// 収集時の診断 1 件。メッセージに入力の本文・属性値は含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptDiagnostic {
    kind: ScriptDiagnosticKind,
    index: Option<usize>,
    node: Option<NodeId>,
    count: Option<usize>,
    message: String,
}

impl ScriptDiagnostic {
    /// 種別。
    pub fn kind(&self) -> ScriptDiagnosticKind {
        self.kind
    }

    /// 対象 `<script>` の通し番号。
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// 対象ノード。
    pub fn node(&self) -> Option<NodeId> {
        self.node
    }

    /// 数値情報（上限超過時は超過件数、inline 超過時はバイト数）。
    pub fn count(&self) -> Option<usize> {
        self.count
    }

    /// 英語の説明文。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// [`collect_page_scripts`] の結果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CollectedScripts {
    entries: Vec<ScriptEntry>,
    diagnostics: Vec<ScriptDiagnostic>,
    dropped_diagnostics: usize,
    seen_scripts: usize,
}

impl CollectedScripts {
    /// 実行対象を文書順に並べたもの。
    pub fn entries(&self) -> &[ScriptEntry] {
        &self.entries
    }

    /// 診断（文書順。最大 [`MAX_SCRIPT_DIAGNOSTICS`] 件）。
    pub fn diagnostics(&self) -> &[ScriptDiagnostic] {
        &self.diagnostics
    }

    /// 記録上限を超えて捨てた診断の件数。
    pub fn dropped_diagnostics(&self) -> usize {
        self.dropped_diagnostics
    }

    /// 対象 `<script>` の総数（収集されなかったものを含む）。
    pub fn seen_scripts(&self) -> usize {
        self.seen_scripts
    }

    /// 通常の診断を追加する。最後の 1 枠は上限超過診断のために予約する。
    fn push_diagnostic(&mut self, diagnostic: ScriptDiagnostic) {
        if self.diagnostics.len() < MAX_SCRIPT_DIAGNOSTICS.saturating_sub(1) {
            self.diagnostics.push(diagnostic);
        } else {
            self.dropped_diagnostics = self.dropped_diagnostics.saturating_add(1);
        }
    }
}

/// `type` 属性の判定結果。
enum ScriptType {
    Classic,
    Module,
    Unsupported,
}

fn classify_type(type_attr: Option<&str>) -> ScriptType {
    let Some(raw) = type_attr else {
        return ScriptType::Classic;
    };
    let value = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    if value.is_empty()
        || JS_MIME_ESSENCES
            .iter()
            .any(|m| value.eq_ignore_ascii_case(m))
    {
        ScriptType::Classic
    } else if value.eq_ignore_ascii_case("module") {
        ScriptType::Module
    } else {
        ScriptType::Unsupported
    }
}

fn is_html_element(document: &Document, id: NodeId, name: &str) -> bool {
    document.namespace_url(id) == Some(HTML_NAMESPACE_URI)
        && document
            .local_name(id)
            .is_some_and(|n| html_local_name_eq(n, name))
}

/// 子の Text ノードの合計バイト数（連結せずに数える。確保前の上限検査用）。
fn inline_text_len(document: &Document, id: NodeId) -> usize {
    document
        .children(id)
        .filter_map(|c| match document.node_data(c) {
            Some(NodeData::Text { contents }) => Some(contents.len()),
            _ => None,
        })
        .fold(0usize, |a, n| a.saturating_add(n))
}

fn inline_text(document: &Document, id: NodeId, len: usize) -> String {
    let mut text = String::with_capacity(len);
    for child in document.children(id) {
        if let Some(NodeData::Text { contents }) = document.node_data(child) {
            text.push_str(contents);
        }
    }
    text
}

fn diagnostic(
    kind: ScriptDiagnosticKind,
    index: Option<usize>,
    node: Option<NodeId>,
    count: Option<usize>,
    message: String,
) -> ScriptDiagnostic {
    ScriptDiagnostic {
        kind,
        index,
        node,
        count,
        message,
    }
}

/// `document` から実行対象の `<script>` を文書順に集める（`JS-4`・`JS-6`）。
///
/// 失敗しない。不正なケースはすべて診断に変え、1 件の悪性スクリプトでページ全体を
/// 失敗させない。ネットワークアクセス・評価は行わない。
pub fn collect_page_scripts(
    document: &Document,
    options: &ScriptCollectionOptions,
) -> CollectedScripts {
    let mut out = CollectedScripts::default();
    let mut exceeded = 0usize;
    let mut first_exceeded: Option<(usize, NodeId)> = None;

    // 前順 DFS。noscript 配下かどうかを子へ引き継ぎ、祖先走査を避けて全体を線形時間にする。
    // 歩数は node_count で打ち切る（子リンクが壊れていても停止する）。
    let mut stack: Vec<(NodeId, bool)> = document
        .children(document.root())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|c| (c, false))
        .collect();
    let mut remaining_steps = document.node_count();
    while let Some((id, in_noscript)) = stack.pop() {
        if remaining_steps == 0 {
            break;
        }
        remaining_steps -= 1;
        let in_noscript = in_noscript || is_html_element(document, id, "noscript");
        let children: Vec<NodeId> = document.children(id).collect();
        stack.extend(children.into_iter().rev().map(|c| (c, in_noscript)));
        if in_noscript || !is_html_element(document, id, "script") {
            continue;
        }
        let index = out.seen_scripts;
        out.seen_scripts = out.seen_scripts.saturating_add(1);

        let type_attr = document.attribute(id, "type");
        match classify_type(type_attr) {
            ScriptType::Module => {
                out.push_diagnostic(diagnostic(
                    ScriptDiagnosticKind::SkippedModule,
                    Some(index),
                    Some(id),
                    None,
                    "module script skipped".to_string(),
                ));
                continue;
            }
            ScriptType::Unsupported => {
                out.push_diagnostic(diagnostic(
                    ScriptDiagnosticKind::SkippedUnsupportedType,
                    Some(index),
                    Some(id),
                    None,
                    "script with non-JavaScript type skipped".to_string(),
                ));
                continue;
            }
            ScriptType::Classic => {}
        }

        // 本文の確保より前に件数上限を判定する。
        if out.entries.len() >= options.max_scripts {
            exceeded = exceeded.saturating_add(1);
            if first_exceeded.is_none() {
                first_exceeded = Some((index, id));
            }
            continue;
        }

        let source = match document.attribute(id, "src") {
            Some("") => {
                out.push_diagnostic(diagnostic(
                    ScriptDiagnosticKind::EmptySrc,
                    Some(index),
                    Some(id),
                    None,
                    "script has empty src attribute".to_string(),
                ));
                continue;
            }
            Some(src) => ScriptSource::External(src.to_string()),
            None => {
                let len = inline_text_len(document, id);
                if len > options.max_inline_bytes {
                    out.push_diagnostic(diagnostic(
                        ScriptDiagnosticKind::InlineScriptTooLarge,
                        Some(index),
                        Some(id),
                        Some(len),
                        format!(
                            "inline script of {len} bytes exceeds limit {}",
                            options.max_inline_bytes
                        ),
                    ));
                    continue;
                }
                ScriptSource::Inline(inline_text(document, id, len))
            }
        };

        out.entries.push(ScriptEntry {
            node: id,
            index,
            source,
            is_async: document.attribute(id, "async").is_some(),
            is_defer: document.attribute(id, "defer").is_some(),
            is_nomodule: document.attribute(id, "nomodule").is_some(),
            type_attr: type_attr.map(str::to_string),
        });
    }

    if let Some((index, node)) = first_exceeded {
        // 予約枠のため通常の記録上限の判定を通さず追加する。
        out.diagnostics.push(diagnostic(
            ScriptDiagnosticKind::ScriptLimitExceeded,
            Some(index),
            Some(node),
            Some(exceeded),
            format!(
                "script limit {} exceeded: {exceeded} scripts not collected",
                options.max_scripts
            ),
        ));
        // 文書順の契約を保つため、通し番号順に並べ直す（安定ソート）。
        out.diagnostics
            .sort_by_key(|d| d.index.unwrap_or(usize::MAX));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{ParseOptions, parse_document};

    fn collect_with(html: &str, options: &ScriptCollectionOptions) -> CollectedScripts {
        let parsed = parse_document(html, &ParseOptions::default()).expect("parse");
        collect_page_scripts(&parsed.document, options)
    }

    fn collect(html: &str) -> CollectedScripts {
        collect_with(html, &ScriptCollectionOptions::default())
    }

    fn inline(e: &ScriptEntry) -> &str {
        match e.source() {
            ScriptSource::Inline(s) => s,
            _ => "<external>",
        }
    }

    /// JS-6: 上限超過診断も文書順（index 昇順）に並ぶ。
    #[test]
    fn js_6_limit_diagnostic_is_in_document_order() {
        let r = collect_with(
            "<script>1</script><script>2</script><script type=module></script>",
            &ScriptCollectionOptions::default().with_max_scripts(1),
        );
        let idx: Vec<Option<usize>> = r.diagnostics().iter().map(|d| d.index()).collect();
        assert_eq!(idx, [Some(1), Some(2)]);
    }

    /// JS-6: 深い入れ子でも祖先走査せず noscript 配下を除外できる。
    #[test]
    fn js_6_deep_nesting_with_noscript_is_linear_and_excluded() {
        let mut html = String::from("<body><noscript><script>x</script></noscript>");
        html.push_str(&"<div>".repeat(2000));
        html.push_str(&"<script></script>".repeat(2000));
        let r = collect(&html);
        assert_eq!(r.seen_scripts(), 2000);
    }

    /// JS-4: head → body、入れ子の順に文書順で集める。
    #[test]
    fn js_4_collects_in_document_order_head_then_body() {
        let r = collect(
            "<html><head><script>1</script></head><body><div><script>2</script></div><script>3</script></body></html>",
        );
        let bodies: Vec<&str> = r.entries().iter().map(inline).collect();
        assert_eq!(bodies, ["1", "2", "3"]);
        let idx: Vec<usize> = r.entries().iter().map(|e| e.index()).collect();
        assert_eq!(idx, [0, 1, 2]);
    }

    /// JS-4: inline は本文、src は生の属性値（trim しない）を保持する。
    #[test]
    fn js_4_inline_keeps_text_and_src_keeps_raw_attr() {
        let r = collect("<script>a=1</script><script src=\" ./x.js?q=1 \"></script>");
        assert_eq!(
            r.entries()[0].source(),
            &ScriptSource::Inline("a=1".to_string())
        );
        assert_eq!(
            r.entries()[1].source(),
            &ScriptSource::External(" ./x.js?q=1 ".to_string())
        );
    }

    /// JS-4: src は inline 本文より優先する。
    #[test]
    fn js_4_src_takes_precedence_over_inline() {
        let r = collect("<script src=\"a.js\">ignored()</script>");
        assert_eq!(r.entries().len(), 1);
        assert_eq!(
            r.entries()[0].source(),
            &ScriptSource::External("a.js".to_string())
        );
    }

    /// JS-4: `src=""` は収集せず診断に残す。
    #[test]
    fn js_4_empty_src_is_diagnosed() {
        let r = collect("<script src=\"\"></script>");
        assert_eq!(r.entries().len(), 0);
        assert_eq!(r.diagnostics().len(), 1);
        assert_eq!(r.diagnostics()[0].kind(), ScriptDiagnosticKind::EmptySrc);
        assert_eq!(r.diagnostics()[0].index(), Some(0));
    }

    /// JS-4: type ごとの判定。
    #[test]
    fn js_4_type_classification_matrix() {
        let r = collect(concat!(
            "<script>a</script>",
            "<script type=\"\">b</script>",
            "<script type=\"text/javascript\">c</script>",
            "<script type=\"APPLICATION/JAVASCRIPT\">d</script>",
            "<script type=\" text/javascript \">e</script>",
            "<script type=\"text/ecmascript\">f</script>",
            "<script type=\"module\">g</script>",
            "<script type=\"MODULE\">h</script>",
            "<script type=\"application/json\">{}</script>",
            "<script type=\"text/template\">t</script>",
            "<script type=\"text/javascript; charset=utf-8\">i</script>",
        ));
        let bodies: Vec<&str> = r.entries().iter().map(inline).collect();
        assert_eq!(bodies, ["a", "b", "c", "d", "e", "f"]);
        let kinds: Vec<(ScriptDiagnosticKind, Option<usize>)> = r
            .diagnostics()
            .iter()
            .map(|d| (d.kind(), d.index()))
            .collect();
        assert_eq!(
            kinds,
            [
                (ScriptDiagnosticKind::SkippedModule, Some(6)),
                (ScriptDiagnosticKind::SkippedModule, Some(7)),
                (ScriptDiagnosticKind::SkippedUnsupportedType, Some(8)),
                (ScriptDiagnosticKind::SkippedUnsupportedType, Some(9)),
                (ScriptDiagnosticKind::SkippedUnsupportedType, Some(10)),
            ]
        );
    }

    /// JS-4: async / defer はフラグとして持つだけで並びを変えない。
    #[test]
    fn js_4_async_defer_flags_kept_and_order_unchanged() {
        let r = collect(
            "<script defer src=\"a.js\"></script><script async src=\"b.js\"></script><script src=\"c.js\"></script>",
        );
        let flags: Vec<(bool, bool)> = r
            .entries()
            .iter()
            .map(|e| (e.is_defer(), e.is_async()))
            .collect();
        assert_eq!(flags, [(true, false), (false, true), (false, false)]);
        assert_eq!(
            r.entries()[0].source(),
            &ScriptSource::External("a.js".to_string())
        );
        assert_eq!(
            r.entries()[2].source(),
            &ScriptSource::External("c.js".to_string())
        );
    }

    /// JS-4: nomodule は収集しフラグを持つ。
    #[test]
    fn js_4_nomodule_is_collected_with_flag() {
        let r = collect("<script nomodule>x</script>");
        assert_eq!(r.entries().len(), 1);
        assert!(r.entries()[0].is_nomodule());
    }

    /// JS-4: template 内の script は集めない。
    #[test]
    fn js_4_template_scripts_are_not_collected() {
        let r = collect("<template><script>x</script></template><script>y</script>");
        let bodies: Vec<&str> = r.entries().iter().map(inline).collect();
        assert_eq!(bodies, ["y"]);
        assert_eq!(r.seen_scripts(), 1);
    }

    /// JS-4: noscript 内の script は集めない。
    #[test]
    fn js_4_noscript_scripts_are_not_collected() {
        // head 直下の `<noscript>` 内の script は、パーサー（scripting 無効）が noscript を
        // 閉じて head の子として扱うため対象外にできない。body 内の入れ子で検証する。
        let r = collect("<body><noscript><script>x</script></noscript><script>y</script>");
        let bodies: Vec<&str> = r.entries().iter().map(inline).collect();
        assert_eq!(bodies, ["y"]);
        assert_eq!(r.seen_scripts(), 1);
    }

    /// JS-4: SVG の script は対象外。
    #[test]
    fn js_4_svg_script_is_ignored() {
        let r = collect("<svg><script>x</script></svg>");
        assert_eq!(r.entries().len(), 0);
        assert_eq!(r.seen_scripts(), 0);
    }

    /// JS-4: 空文書。
    #[test]
    fn js_4_empty_document() {
        let r = collect("");
        assert_eq!(r.entries().len(), 0);
        assert_eq!(r.diagnostics().len(), 0);
        assert_eq!(r.seen_scripts(), 0);
    }

    fn n_scripts(n: usize) -> String {
        "<script>x</script>".repeat(n)
    }

    /// JS-6: 64 件は全て収集し、診断なし。
    #[test]
    fn js_6_script_limit_boundary_64() {
        let r = collect(&n_scripts(64));
        assert_eq!(r.entries().len(), 64);
        assert!(r.diagnostics().is_empty());
    }

    /// JS-6: 65 件目は集めず、超過件数 1 を診断に残す。
    #[test]
    fn js_6_script_limit_boundary_65() {
        let r = collect(&n_scripts(65));
        assert_eq!(r.entries().len(), 64);
        assert_eq!(r.diagnostics().len(), 1);
        let d = &r.diagnostics()[0];
        assert_eq!(d.kind(), ScriptDiagnosticKind::ScriptLimitExceeded);
        assert_eq!(d.count(), Some(1));
        assert_eq!(d.index(), Some(64));
    }

    /// JS-6: 上限は変更できる。
    #[test]
    fn js_6_script_limit_is_configurable() {
        let opts = ScriptCollectionOptions::default().with_max_scripts(2);
        let r = collect_with(&n_scripts(5), &opts);
        assert_eq!(r.entries().len(), 2);
        assert_eq!(r.diagnostics()[0].count(), Some(3));
    }

    /// JS-6: 非 JS は件数上限の枠を消費しない。
    #[test]
    fn js_6_non_js_scripts_do_not_consume_limit() {
        let html = format!(
            "{}<script>x</script>",
            "<script type=\"application/json\">{}</script>".repeat(70)
        );
        let r = collect(&html);
        assert_eq!(r.entries().len(), 1);
        assert_eq!(r.diagnostics().len(), 70);
    }

    /// JS-6: inline のバイト上限の境界。
    #[test]
    fn js_6_inline_bytes_boundary() {
        let ok = format!("<script>{}</script>", "a".repeat(1024 * 1024));
        assert_eq!(collect(&ok).entries().len(), 1);
        let big = format!("<script>{}</script>", "a".repeat(1024 * 1024 + 1));
        let r = collect(&big);
        assert_eq!(r.entries().len(), 0);
        assert_eq!(
            r.diagnostics()[0].kind(),
            ScriptDiagnosticKind::InlineScriptTooLarge
        );
        assert_eq!(r.diagnostics()[0].count(), Some(1048577));
    }

    /// JS-6: 診断の記録は上限で頭打ち（最後の 1 枠は上限超過診断用に予約）。
    #[test]
    fn js_6_diagnostics_are_capped() {
        let html = "<script type=\"text/x\"></script>".repeat(300);
        let r = collect(&html);
        assert_eq!(r.diagnostics().len(), MAX_SCRIPT_DIAGNOSTICS - 1);
        assert_eq!(r.dropped_diagnostics(), 300 - (MAX_SCRIPT_DIAGNOSTICS - 1));
    }

    /// JS-6: 上限超過の診断は記録上限でも必ず残る。
    #[test]
    fn js_6_limit_diagnostic_survives_cap() {
        let html = format!(
            "{}{}",
            "<script type=\"text/x\"></script>".repeat(300),
            n_scripts(DEFAULT_MAX_PAGE_SCRIPTS + 1)
        );
        let r = collect(&html);
        assert_eq!(r.diagnostics().len(), MAX_SCRIPT_DIAGNOSTICS);
        assert!(
            r.diagnostics()
                .iter()
                .any(|d| d.kind() == ScriptDiagnosticKind::ScriptLimitExceeded)
        );
    }
}

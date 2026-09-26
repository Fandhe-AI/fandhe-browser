//! parse: `fetch`（TASK-24.2・#36）が取得した HTML 文字列を構文解析し、
//! `dom`（TASK-24.5・#39）の arena（[`crate::dom::Document`]）を構築するモジュール
//! （TASK-24.4・#38・ビヘイビア `CORE-1`）。
//!
//! # 依存の経緯
//!
//! Issue #35（2026-09-25 承認済み）の決定により、`scraper`（推移的に
//! `selectors`/`cssparser` の MPL-2.0 を core の既定ビルドへ持ち込む）は
//! 不採用とし、`html5ever = "=0.40.1"` を単体で使い `TreeSink`（DOM 構築）と
//! セレクタ照合（#41・#418）を自作する方針とした。`markup5ever`・`tendril` 等の
//! 推移的依存は直接依存に追加せず、`html5ever::*` の再エクスポート経由でのみ
//! 使う（`Cargo.toml` の `[dependencies]` を参照）。
//!
//! # エラーポリシー（受入基準: 不正な入力に対して panic せず `Result::Err` を
//! 返す経路を持つこと）
//!
//! WHATWG HTML のパースはエラー回復が前提で、html5ever はどんな入力でも
//! 木を返す設計のため、次の 3 系統に分けて扱う。
//!
//! 1. **必ず `Err` を返す入力**: 不正な UTF-8（[`parse_document_bytes`]）・
//!    サイズ上限超過（[`ParseOptions::max_input_bytes`]）・ノード数上限超過
//!    （[`ParseOptions::max_nodes`]）・`TreeSink` の内部不変条件違反。
//! 2. **回復可能なパースエラー**: 既定の [`ParseErrorPolicy::Recover`] では
//!    `Ok` を返し、[`ParseDiagnostics`] に件数・メッセージを記録する。
//! 3. **[`ParseErrorPolicy::Strict`]**: パースエラーが 1 件以上あれば `Err` を
//!    返す（オプトイン）。
//!
//! # 呼び出し文脈
//!
//! `fetch`（#36）が取得した HTML 文字列・バイト列を受け取り、`dom`（TASK-24.5・
//! #39）が提供する走査 API・要素名アクセサ（[`crate::dom::Document::local_name`]
//! 等）・`query`（#41）が使う arena を構築する前段を担う。
//!
//! ## 本モジュールの範囲外（将来仕様。REPAIR-3: 実装済みを装わない）
//!
//! - 文字コード検出（ビヘイビア `CORE-5` (7)・`TASK-25`・`MS-3`）
//! - Shadow DOM（ビヘイビア `CORE-5` (5)・`TASK-25`・`MS-3`。
//!   `allow_declarative_shadow_roots` を常に `false` として無効化）
//! - `<option>` の選択値解決（ビヘイビア `CORE-5` (4)・`TASK-25`・`MS-3`）

use crate::dom::{Attribute, Document, Node, NodeData, NodeId, QuirksMode};
use crate::error::{Error, ParseError, Result};
use html5ever::interface::{ElemName, ElementFlags, NodeOrText, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{Attribute as HtmlAttribute, LocalName, Namespace, ParseOpts, QualName};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};

/// ルートノード（[`html5ever::interface::TreeSink::get_document`]）の ID。
/// arena（[`ArenaSink::new`]）は常にこの位置に [`NodeData::Document`] を
/// 確保する。
const ROOT_ID: NodeId = NodeId::new(0);

/// arena が最初から確保しておくノード数（[`ROOT_ID`] のみ）。
const RESERVED_NODE_COUNT: usize = 1;

/// ノード数上限超過後に発行する「オーバーフローハンドル」の初期値
/// （降順に払い出す）。
///
/// 上限超過後も html5ever の tree builder は `create_element` 等の呼び出しを
/// 止めない（`TreeSink` にエラーを伝える手段が無いため）。ここで**固定の
/// 番兵 ID を使い回す**設計は、tree builder が異なる論理要素に対して同じ
/// `Handle` を受け取ることになり、`same_node` が異なる要素同士を「同じ
/// ノード」と誤判定して tree builder 内部の不変条件（開いている要素の
/// スタック管理等）を壊し、html5ever 自身が `panic!` する（実測済み。
/// `<table>`・`<template>`・adoption agency を含む入力を `max_nodes: 3` で
/// パースすると `no current element` で panic した）。
///
/// 対策として、上限超過後も呼び出しごとに一意なオーバーフローハンドルを
/// 発行する（実ノードは作らない。arena の実インデックスは `nodes.len()` から
/// 単調増加するため、`usize::MAX` から降順に払い出せば実インデックスと
/// 衝突しない）。オーバーフローハンドルへの読み取り系メソッド呼び出しは
/// arena に実体が無いため必ず `None` になるが、`limit_exceeded` が立っている
/// 間はこれを「想定内の縮退」として扱い、`poison`（内部不変条件違反）とは
/// 区別する。
const FIRST_OVERFLOW_ID: usize = usize::MAX;

/// [`parse_document`]・[`parse_document_bytes`] の挙動を制御するオプション。
///
/// `#[non_exhaustive]` のため、crate 外からは [`ParseOptions::default`] と
/// `with_*` ビルダーメソッドで構築する（REPAIR-4: 戻り値・引数は将来拡張できる
/// 構造にする）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ParseOptions {
    max_input_bytes: usize,
    max_nodes: usize,
    max_recorded_errors: usize,
    scripting_enabled: bool,
    error_policy: ParseErrorPolicy,
}

/// 入力サイズ上限の既定値（32 MiB）。
///
/// スクレイピング対象の実サイトの HTML は概ね数 MB に収まる一方、
/// 悪意ある・壊れたレスポンスによる無制限確保を防ぐため、余裕を持たせつつ
/// 上限を設ける（security.md「無制限リソース確保」対策）。
pub const DEFAULT_MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// ノード数上限の既定値。
///
/// 深いネスト・adoption agency による要素複製で無制限にノードが増えるのを防ぐ。
pub const DEFAULT_MAX_NODES: usize = 1_000_000;

/// 記録するパースエラーメッセージの件数上限の既定値。
///
/// 診断メッセージが無制限に蓄積してメモリを圧迫しないための上限。
pub const DEFAULT_MAX_RECORDED_ERRORS: usize = 100;

impl Default for ParseOptions {
    fn default() -> Self {
        ParseOptions {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_nodes: DEFAULT_MAX_NODES,
            max_recorded_errors: DEFAULT_MAX_RECORDED_ERRORS,
            // 本 crate は現時点で JS を実行しない（`js_stub`。`JS-2`・`TASK-30`・
            // `MS-3` で差し替え予定）。スクレイピング用途では `<noscript>` の
            // 中身を要素として取れる方が有用なため、html5ever の既定（true）とは
            // 逆に false とする。JS 統合時（`TASK-30`・`MS-3`）に見直す。
            scripting_enabled: false,
            error_policy: ParseErrorPolicy::Recover,
        }
    }
}

impl ParseOptions {
    /// 入力サイズ上限（バイト数）を設定する。
    pub fn with_max_input_bytes(mut self, max_input_bytes: usize) -> Self {
        self.max_input_bytes = max_input_bytes;
        self
    }

    /// ノード数上限を設定する。
    ///
    /// `0` はここではエラーにならない（ビルダーメソッドは `Self` を返す
    /// 非 `Result` 契約のため）。`0` を指定した状態で
    /// [`parse_document`]・[`parse_document_bytes`] を呼ぶと
    /// `Err(ParseError::InvalidMaxNodes)` になる（arena は `Document`
    /// ルート用に最低 1 ノードを要するため。REPAIR-6 P1 レビュー指摘）。
    pub fn with_max_nodes(mut self, max_nodes: usize) -> Self {
        self.max_nodes = max_nodes;
        self
    }

    /// 記録するパースエラーメッセージ件数の上限を設定する。
    pub fn with_max_recorded_errors(mut self, max_recorded_errors: usize) -> Self {
        self.max_recorded_errors = max_recorded_errors;
        self
    }

    /// スクリプト実行が有効な文脈としてパースするかどうかを設定する
    /// （`<noscript>` の扱いに影響する。html5ever `TreeBuilderOpts` 参照）。
    pub fn with_scripting_enabled(mut self, scripting_enabled: bool) -> Self {
        self.scripting_enabled = scripting_enabled;
        self
    }

    /// 回復可能なパースエラーの扱いを設定する。
    pub fn with_error_policy(mut self, error_policy: ParseErrorPolicy) -> Self {
        self.error_policy = error_policy;
        self
    }
}

/// 回復可能なパースエラー（不正な HTML だが木は構築できる場合）の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParseErrorPolicy {
    /// パースエラーがあっても `Ok` を返し、[`ParseDiagnostics`] に記録する
    /// （既定）。
    #[default]
    Recover,
    /// パースエラーが 1 件以上あれば `Err(Error::Parse(ParseError::Malformed
    /// { .. }))` を返す。
    Strict,
}

/// パース中に検出した回復可能なエラーの診断情報。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ParseDiagnostics {
    /// 検出したパースエラーの総件数（`u64` で saturating に加算。
    /// `ParseOptions::max_recorded_errors` を超えてもここは減らない）。
    pub error_count: u64,
    /// 先頭 `ParseOptions::max_recorded_errors` 件までのメッセージ
    /// （英語。html5ever が返す静的文字列）。
    pub messages: Vec<String>,
}

/// [`parse_document`]・[`parse_document_bytes`] の成功時の戻り値。
#[derive(Debug)]
#[non_exhaustive]
pub struct ParsedDocument {
    /// 構築した DOM ドキュメント。
    pub document: Document,
    /// 回復可能なパースエラーの診断情報（[`ParseErrorPolicy::Recover`] 時）。
    pub diagnostics: ParseDiagnostics,
}

/// HTML 文字列をパースし、`dom::Document` を構築する。
///
/// `fetch`（#36）がデコード済みの文字列を渡す想定の経路。バイト列から直接
/// パースする場合は [`parse_document_bytes`] を使う。
///
/// # エラー
///
/// [`parse` モジュールのドキュメント](self) の「エラーポリシー」を参照。
pub fn parse_document(input: &str, options: &ParseOptions) -> Result<ParsedDocument> {
    if options.max_nodes == 0 {
        return Err(Error::Parse(ParseError::InvalidMaxNodes { requested: 0 }));
    }
    if input.len() > options.max_input_bytes {
        return Err(Error::Parse(ParseError::InputTooLarge {
            len: input.len(),
            limit: options.max_input_bytes,
        }));
    }

    let sink = ArenaSink::new(options);
    let parse_opts = ParseOpts {
        tree_builder: TreeBuilderOpts {
            scripting_enabled: options.scripting_enabled,
            // `exact_errors: false`（html5ever の既定）: 詳細なエラー位置情報は
            // 性能ペナルティを伴い、CORE-1 の受入基準（Err 経路の有無・診断件数）
            // には不要なため既定のままにする。
            exact_errors: false,
            ..Default::default()
        },
        ..Default::default()
    };

    // `TendrilSink::one` は入力全体を処理して `finish()` を呼ぶ。
    // `ArenaSink::Output` は `Result<ParsedDocument>` そのものなので、
    // ここでの変換は不要（`finish` が優先順位（poison > ノード上限 >
    // strict 時のパースエラー）に従って `Result` を組み立てる）。
    html5ever::driver::parse_document(sink, parse_opts).one(input)
}

/// バイト列を UTF-8 として厳密に検証してから [`parse_document`] を呼ぶ。
///
/// 文字コード検出（`CORE-5` (7)）は行わない。UTF-8 以外のバイト列は
/// `Err(ParseError::InvalidUtf8 { .. })` になる。
///
/// # エラー
///
/// [`parse` モジュールのドキュメント](self) の「エラーポリシー」を参照。
pub fn parse_document_bytes(input: &[u8], options: &ParseOptions) -> Result<ParsedDocument> {
    // オプション検証（`max_nodes`）を入力検証より先に行う。`parse_document`
    // 側でも同じ検査をするが、`InvalidUtf8`/`InputTooLarge` を先に返すと
    // 「同じ `options` で `parse_document` を呼ぶと `InvalidMaxNodes` に
    // なるのに、このエントリポイントでは別のエラーが先に返る」という
    // 呼び出し元にとって非直感的な分岐が生まれるため、ここでも明示的に
    // チェックする（REPAIR-6 P1 レビュー指摘）。
    if options.max_nodes == 0 {
        return Err(Error::Parse(ParseError::InvalidMaxNodes { requested: 0 }));
    }
    if input.len() > options.max_input_bytes {
        return Err(Error::Parse(ParseError::InputTooLarge {
            len: input.len(),
            limit: options.max_input_bytes,
        }));
    }

    match std::str::from_utf8(input) {
        Ok(text) => parse_document(text, options),
        Err(utf8_error) => Err(Error::Parse(ParseError::InvalidUtf8 {
            valid_up_to: utf8_error.valid_up_to(),
        })),
    }
}

/// html5ever `TreeSink::elem_name` の戻り値。
///
/// html5ever 標準の arena サンプル（`examples/arena.rs`）は
/// `&'a QualName`（借用）を返すが、本実装は `RefCell<Vec<Node>>` 越しに
/// ノードへアクセスするため、借用をそのまま返すと呼び出し元（tree builder）が
/// それを保持したまま他の `TreeSink` メソッドを呼んだ場合に `try_borrow_mut`
/// が競合しうる。atom（`Namespace`/`LocalName`）を clone して切り離すことで、
/// `elem_name` 呼び出し完了時点で借用を確実に解放する（atom は参照カウント式で
/// clone コストが低い）。
#[derive(Debug)]
struct OwnedElemName(QualName);

impl OwnedElemName {
    fn empty() -> Self {
        OwnedElemName(QualName::new(
            None,
            Namespace::from(""),
            LocalName::from(""),
        ))
    }
}

impl ElemName for OwnedElemName {
    fn ns(&self) -> &Namespace {
        &self.0.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.0.local
    }
}

/// パース中に蓄積する診断情報（`ArenaSink` 内部の可変状態）。
#[derive(Debug, Default)]
struct DiagnosticsState {
    error_count: u64,
    messages: Vec<String>,
}

/// html5ever の `TreeSink` を実装し、`fetch` が取得した HTML から
/// `dom::Document`（arena）を構築する。
///
/// `TreeSink` の全メソッドは `&self` を取る契約のため、内部可変性
/// （`RefCell`/`Cell`）で状態を持つ。html5ever は各メソッドを同期的・非再入で
/// 呼び出す契約だが、`TreeSink` 契約外の呼び出し順序（実装バグ・将来の
/// html5ever バージョン差異）に備えて `try_borrow`/`try_borrow_mut` を使い、
/// 失敗時は panic せず `poison` を立てて `finish()` で
/// `Err(ParseError::Internal)` に変換する（coding-rust.md「外部入力の経路では
/// panic させない」。HTML は外部入力であり、本 sink はその処理系そのもの）。
struct ArenaSink {
    nodes: RefCell<Vec<Node>>,
    quirks_mode: Cell<QuirksMode>,
    diagnostics: RefCell<DiagnosticsState>,
    /// `try_borrow`/`try_borrow_mut` の失敗、または契約外のハンドル
    /// （要素でないノードへの `elem_name` 呼び出し等）を検出した場合に立つ。
    poison: Cell<bool>,
    /// ノード数が `max_nodes` に達した場合に立つ。以後のミューテーションは
    /// no-op になり、`finish()` で `Err(NodeLimitExceeded)` に変換する。
    limit_exceeded: Cell<bool>,
    /// 上限超過後に払い出す次のオーバーフローハンドル（[`FIRST_OVERFLOW_ID`]
    /// から降順）。実ノードを持たないため、対応する arena エントリは存在しない。
    next_overflow_id: Cell<usize>,
    /// 直近に解決した「兄弟ノード → その親の `children` 内での位置」を 1 件だけ
    /// 覚える 1 スロットキャッシュ（`(sibling, position)`）。
    ///
    /// foster parenting（`<table>` 直前へテキストを繰り返し挿入する経路。
    /// [`Self::insert_before`]・`append_before_sibling` の `find_prev`）は
    /// 呼び出しのたびに同じ `<table>` ノードを `sibling` として位置探索するため、
    /// 外部 HTML の兄弟数に応じて呼び出し回数分の線形探索が重なり二乗時間になる
    /// （P1 レビュー指摘。PR #431
    /// <https://github.com/Fandhe-AI/fandhe-browser/pull/431#discussion_r4110398582>）。
    /// 直近 1 件をここに残すことで、同じ `sibling` への 2 回目以降の探索を
    /// O(1) に落とす。`ArenaSink` はパース専用の内部型で、完成後に外へ返す
    /// `Document`（`dom.rs` の Send + Sync 契約）には含まれないため、
    /// `Node` 側に `Cell` を持たせずに済む。キャッシュが古い（他の変更で
    /// ずれた）場合は参照時に不一致を検知して線形探索へフォールバックするため、
    /// 返す位置の正しさ自体は損なわれない。
    last_sibling_pos: Cell<Option<(NodeId, usize)>>,
    max_nodes: usize,
    max_recorded_errors: usize,
    strict: bool,
}

impl ArenaSink {
    /// [`ROOT_ID`]（Document）のみを確保した arena を持つ sink を作る。
    fn new(options: &ParseOptions) -> Self {
        let nodes = vec![Node {
            parent: None,
            children: Vec::new(),
            data: NodeData::Document,
        }];

        ArenaSink {
            nodes: RefCell::new(nodes),
            quirks_mode: Cell::new(QuirksMode::NoQuirks),
            diagnostics: RefCell::new(DiagnosticsState::default()),
            poison: Cell::new(false),
            limit_exceeded: Cell::new(false),
            next_overflow_id: Cell::new(FIRST_OVERFLOW_ID),
            last_sibling_pos: Cell::new(None),
            // `options.max_nodes == 0` は `parse_document`/`parse_document_bytes`
            // が `Err(ParseError::InvalidMaxNodes)` として呼び出し前に拒否する
            // ため、ここに到達する値は常に 1 以上（REPAIR-6 P1 レビュー指摘：
            // 以前はここで黙って `RESERVED_NODE_COUNT` へ引き上げており、
            // 公開 API が受け取った上限と実際の適用上限が乖離していた）。
            // `.max(RESERVED_NODE_COUNT)` は不変条件が壊れた場合の多重防御
            // として残す（`ArenaSink::new` を将来 `parse_document` 以外から
            // 直接呼ぶ経路が増えても Document すら確保できない状態にはしない）。
            max_nodes: options.max_nodes.max(RESERVED_NODE_COUNT),
            max_recorded_errors: options.max_recorded_errors,
            strict: matches!(options.error_policy, ParseErrorPolicy::Strict),
        }
    }

    /// 一意なオーバーフローハンドルを 1 つ払い出す（[`FIRST_OVERFLOW_ID`] の
    /// コメント参照）。呼び出しごとに異なる値を返すため、tree builder が
    /// 異なる論理要素を「同じノード」と誤認しない。
    fn next_overflow_id(&self) -> NodeId {
        let id = self.next_overflow_id.get();
        // 実ノード数（`max_nodes` は現実的にはメモリ上限に遠く及ばない）が
        // `usize::MAX` に達することはないため、通常この減算が 0 を下回ることはない。
        self.next_overflow_id.set(id.wrapping_sub(1));
        NodeId::new(id)
    }

    /// 新規ノードを 1 つ確保し、その ID を返す。ノード数上限に達している、
    /// または借用に失敗した場合は一意なオーバーフローハンドル
    /// （[`Self::next_overflow_id`]）を返し、対応するフラグを立てる
    /// （panic しない）。
    fn try_create_node(&self, data: NodeData) -> NodeId {
        if self.poison.get() || self.limit_exceeded.get() {
            return self.next_overflow_id();
        }
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return self.next_overflow_id();
            }
        };
        if nodes.len() >= self.max_nodes {
            drop(nodes);
            self.limit_exceeded.set(true);
            return self.next_overflow_id();
        }
        let id = NodeId::new(nodes.len());
        nodes.push(Node {
            parent: None,
            children: Vec::new(),
            data,
        });
        id
    }

    /// `child` を arena 内の別ノードから detach する（元の親の `children` から
    /// 取り除き、`parent` を `None` にする）。
    fn detach(nodes: &mut [Node], child: NodeId) {
        let old_parent = nodes.get(child.index()).and_then(|n| n.parent);
        if let Some(parent_id) = old_parent
            && let Some(parent) = nodes.get_mut(parent_id.index())
        {
            parent.children.retain(|&c| c != child);
        }
        if let Some(node) = nodes.get_mut(child.index()) {
            node.parent = None;
        }
    }

    /// `sibling` が `parent` の `children` 内で占める位置を返す。
    ///
    /// [`Self::last_sibling_pos`] にキャッシュされた `(sibling, pos)` が
    /// `parent.children[pos] == sibling` を満たせばそれを O(1) で使い、
    /// 未設定・他ノードの挿入や削除でずれて不一致になっていればその場で
    /// 線形探索してキャッシュを更新する（フィールドのドキュメントコメント
    /// 参照。P1 レビュー指摘。PR #431
    /// <https://github.com/Fandhe-AI/fandhe-browser/pull/431#discussion_r4110398582>）。
    /// キャッシュが古い場合でも線形探索へフォールバックするだけで、返す位置の
    /// 正しさ自体は損なわれない。
    fn sibling_position(&self, parent: &Node, sibling: NodeId) -> Option<usize> {
        let cached = self.last_sibling_pos.get();
        let pos = cached
            .filter(|&(cached_sibling, pos)| {
                cached_sibling == sibling && parent.children.get(pos) == Some(&sibling)
            })
            .map(|(_, pos)| pos)
            .or_else(|| parent.children.iter().position(|&c| c == sibling))?;
        self.last_sibling_pos.set(Some((sibling, pos)));
        Some(pos)
    }

    /// `child` を（既存の親から detach したうえで）`parent` の最後の子として
    /// 付け直す。
    fn append_child(nodes: &mut [Node], parent: NodeId, child: NodeId) {
        Self::detach(nodes, child);
        if let Some(parent_node) = nodes.get_mut(parent.index()) {
            parent_node.children.push(child);
        }
        if let Some(child_node) = nodes.get_mut(child.index()) {
            child_node.parent = Some(parent);
        }
    }

    /// `new_node` を（既存の親から detach したうえで）`sibling` の直前へ挿入する。
    /// `sibling` に親がない場合（html5ever の契約上発生しない想定外経路）は
    /// 何もしない。
    fn insert_before(&self, nodes: &mut [Node], sibling: NodeId, new_node: NodeId) {
        Self::detach(nodes, new_node);
        let Some(parent_id) = nodes.get(sibling.index()).and_then(|n| n.parent) else {
            return;
        };
        let insert_pos = nodes
            .get(parent_id.index())
            .and_then(|parent| self.sibling_position(parent, sibling));
        if let Some(parent) = nodes.get_mut(parent_id.index()) {
            match insert_pos {
                Some(pos) => parent.children.insert(pos, new_node),
                None => parent.children.push(new_node),
            }
        }
        if let Some(new_node_ref) = nodes.get_mut(new_node.index()) {
            new_node_ref.parent = Some(parent_id);
        }
        // `sibling` は挿入により実際に 1 つ後ろへずれた場合のみキャッシュを
        // 更新する（`insert_pos` が見つからず末尾へ push した場合、`sibling`
        // は別の親配下にいるため動いていない。次回参照時は不一致を検知して
        // 再探索にフォールバックする）。
        if let Some(pos) = insert_pos {
            self.last_sibling_pos.set(Some((sibling, pos + 1)));
        }
    }

    /// `append`/`append_before_sibling` に共通する「隣接テキストノードへの
    /// 連結、なければ新規作成」ロジック。`find_prev`（連結対象を探す）と
    /// `place`（新規ノードを配置する）を呼び出し元から渡す。
    fn append_or_merge_text(
        &self,
        nodes: &mut Vec<Node>,
        text: StrTendril,
        find_prev: impl FnOnce(&[Node]) -> Option<NodeId>,
        place: impl FnOnce(&mut [Node], NodeId),
    ) {
        if let Some(prev_id) = find_prev(nodes)
            && let Some(NodeData::Text { contents }) =
                nodes.get_mut(prev_id.index()).map(|n| &mut n.data)
        {
            contents.push_str(&text);
            return;
        }
        if nodes.len() >= self.max_nodes {
            self.limit_exceeded.set(true);
            return;
        }
        let id = NodeId::new(nodes.len());
        nodes.push(Node {
            parent: None,
            children: Vec::new(),
            data: NodeData::Text {
                contents: text.to_string(),
            },
        });
        place(nodes, id);
    }

    /// `node` の子をすべて `new_parent` の末尾へまとめて付け替える
    /// （[`TreeSink::reparent_children`] 実装から呼ばれる）。`append_child`
    /// を子ごとに繰り返すと、各回の [`Self::detach`] が元の親（`node`）の
    /// `children` 全体を `retain` で線形走査するため、子数 N に対し
    /// O(N^2) になる（REPAIR-6 P1 レビュー指摘）。本関数は `node.children`
    /// を丸ごと取り出して親ポインタだけを一括更新するため、子数に比例する
    /// （O(N)）処理になる。
    fn move_all_children(nodes: &mut [Node], node: NodeId, new_parent: NodeId) {
        let children = match nodes.get_mut(node.index()) {
            Some(n) => std::mem::take(&mut n.children),
            None => return,
        };
        if children.is_empty() {
            return;
        }
        for &child in &children {
            if let Some(child_node) = nodes.get_mut(child.index()) {
                child_node.parent = Some(new_parent);
            }
        }
        if let Some(parent_node) = nodes.get_mut(new_parent.index()) {
            parent_node.children.extend(children);
        }
    }
}

impl TreeSink for ArenaSink {
    type Handle = NodeId;
    type Output = Result<ParsedDocument>;
    type ElemName<'a> = OwnedElemName;

    fn finish(self) -> Result<ParsedDocument> {
        // 優先順位: 内部不変条件違反 > ノード数上限 > Strict 時のパースエラー。
        // 上限超過は途中で打ち切った不完全な木を意味し、Strict の
        // Malformed より重大度が高いと判断する。
        if self.poison.get() {
            return Err(Error::Parse(ParseError::Internal {
                message: "TreeSink invariant violated (borrow conflict or unexpected handle)"
                    .to_string(),
            }));
        }
        if self.limit_exceeded.get() {
            return Err(Error::Parse(ParseError::NodeLimitExceeded {
                limit: self.max_nodes,
            }));
        }

        let diagnostics_state = self.diagnostics.into_inner();
        let diagnostics = ParseDiagnostics {
            error_count: diagnostics_state.error_count,
            messages: diagnostics_state.messages,
        };

        if self.strict && diagnostics.error_count > 0 {
            return Err(Error::Parse(ParseError::Malformed {
                error_count: diagnostics.error_count,
                first_message: diagnostics.messages.first().cloned(),
            }));
        }

        Ok(ParsedDocument {
            document: Document {
                nodes: self.nodes.into_inner(),
                root: ROOT_ID,
                quirks_mode: self.quirks_mode.get(),
            },
            diagnostics,
        })
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        let mut diagnostics = match self.diagnostics.try_borrow_mut() {
            Ok(diagnostics) => diagnostics,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        diagnostics.error_count = diagnostics.error_count.saturating_add(1);
        if diagnostics.messages.len() < self.max_recorded_errors {
            diagnostics.messages.push(msg.into_owned());
        }
    }

    fn get_document(&self) -> NodeId {
        ROOT_ID
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> OwnedElemName {
        let nodes = match self.nodes.try_borrow() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return OwnedElemName::empty();
            }
        };
        match nodes.get(target.index()) {
            Some(node) => match &node.data {
                NodeData::Element { name, .. } => OwnedElemName(name.clone()),
                _ => {
                    // html5ever の契約上「要素以外に elem_name が呼ばれることは
                    // ない」が、契約外の呼び出しでも panic は許されない
                    // （coding-rust.md）ため、空の名前を返しつつ内部不変条件
                    // 違反として記録する。
                    drop(nodes);
                    self.poison.set(true);
                    OwnedElemName::empty()
                }
            },
            None if self.limit_exceeded.get() => {
                // ノード数上限超過後に払い出したオーバーフローハンドル
                // （`Self::next_overflow_id`）には対応する arena エントリが
                // 無いため必ず `None` になる。これは想定内の縮退であり、
                // 内部不変条件違反（poison）とは区別する。
                OwnedElemName::empty()
            }
            None => {
                drop(nodes);
                self.poison.set(true);
                OwnedElemName::empty()
            }
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<HtmlAttribute>,
        flags: ElementFlags,
    ) -> NodeId {
        let dom_attrs = attrs
            .into_iter()
            .map(|attr| Attribute {
                name: attr.name,
                value: attr.value.to_string(),
            })
            .collect();
        let template_contents = if flags.template {
            Some(self.try_create_node(NodeData::DocumentFragment))
        } else {
            None
        };
        self.try_create_node(NodeData::Element {
            name,
            attrs: dom_attrs,
            template_contents,
            mathml_annotation_xml_integration_point: flags.mathml_annotation_xml_integration_point,
        })
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.try_create_node(NodeData::Comment {
            contents: text.to_string(),
        })
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> NodeId {
        self.try_create_node(NodeData::ProcessingInstruction {
            target: target.to_string(),
            data: data.to_string(),
        })
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        if self.poison.get() || self.limit_exceeded.get() {
            return;
        }
        let parent = *parent;
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        match child {
            NodeOrText::AppendText(text) => {
                self.append_or_merge_text(
                    &mut nodes,
                    text,
                    |nodes| {
                        nodes
                            .get(parent.index())
                            .and_then(|n| n.children.last().copied())
                    },
                    |nodes, id| Self::append_child(nodes, parent, id),
                );
            }
            NodeOrText::AppendNode(child_id) => {
                Self::append_child(&mut nodes, parent, child_id);
            }
        }
    }

    fn append_before_sibling(&self, sibling: &NodeId, child: NodeOrText<NodeId>) {
        if self.poison.get() || self.limit_exceeded.get() {
            return;
        }
        let sibling = *sibling;
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        match child {
            NodeOrText::AppendText(text) => {
                self.append_or_merge_text(
                    &mut nodes,
                    text,
                    |nodes| {
                        let parent_id = nodes.get(sibling.index()).and_then(|n| n.parent)?;
                        let parent = nodes.get(parent_id.index())?;
                        let pos = self.sibling_position(parent, sibling)?;
                        pos.checked_sub(1)
                            .and_then(|i| parent.children.get(i).copied())
                    },
                    |nodes, id| self.insert_before(nodes, sibling, id),
                );
            }
            NodeOrText::AppendNode(child_id) => {
                self.insert_before(&mut nodes, sibling, child_id);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        if self.poison.get() || self.limit_exceeded.get() {
            return;
        }
        let has_parent = {
            let nodes = match self.nodes.try_borrow() {
                Ok(nodes) => nodes,
                Err(_) => {
                    self.poison.set(true);
                    return;
                }
            };
            nodes.get(element.index()).and_then(|n| n.parent).is_some()
        };
        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        if self.poison.get() || self.limit_exceeded.get() {
            return;
        }
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        if nodes.len() >= self.max_nodes {
            drop(nodes);
            self.limit_exceeded.set(true);
            return;
        }
        let id = NodeId::new(nodes.len());
        nodes.push(Node {
            parent: None,
            children: Vec::new(),
            data: NodeData::Doctype {
                name: name.to_string(),
                public_id: public_id.to_string(),
                system_id: system_id.to_string(),
            },
        });
        Self::append_child(&mut nodes, ROOT_ID, id);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        let nodes = match self.nodes.try_borrow() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return *target;
            }
        };
        match nodes.get(target.index()) {
            Some(node) => match &node.data {
                NodeData::Element {
                    template_contents: Some(contents),
                    ..
                } => *contents,
                _ => {
                    drop(nodes);
                    self.poison.set(true);
                    *target
                }
            },
            // オーバーフローハンドル（elem_name の同種の分岐を参照）。
            None if self.limit_exceeded.get() => *target,
            None => {
                drop(nodes);
                self.poison.set(true);
                *target
            }
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks_mode.set(mode);
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<HtmlAttribute>) {
        // 「上限超過後のミューテーションは no-op」という契約（parse モジュールの
        // doc コメント参照）を属性追加にも適用する。
        if self.poison.get() || self.limit_exceeded.get() {
            return;
        }
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        match nodes.get_mut(target.index()).map(|n| &mut n.data) {
            Some(NodeData::Element {
                attrs: existing, ..
            }) => {
                // 属性名の集合を作ってから照合する。候補ごとに `existing` を
                // `iter().any` で線形探索すると、重複した `<html>` タグ等で
                // 大量の属性候補が渡された場合に属性数 N に対し O(N^2) になる
                // （REPAIR-6 P1 レビュー指摘）。`HashSet` により属性数に
                // ほぼ比例する処理にする。
                let mut existing_names: std::collections::HashSet<QualName> =
                    existing.iter().map(|e| e.name.clone()).collect();
                for attr in attrs {
                    if existing_names.insert(attr.name.clone()) {
                        existing.push(Attribute {
                            name: attr.name,
                            value: attr.value.to_string(),
                        });
                    }
                }
            }
            _ => {
                drop(nodes);
                self.poison.set(true);
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        if self.poison.get() {
            return;
        }
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        Self::detach(&mut nodes, *target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        if self.poison.get() {
            return;
        }
        let mut nodes = match self.nodes.try_borrow_mut() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return;
            }
        };
        Self::move_all_children(&mut nodes, *node, *new_parent);
    }

    fn is_mathml_annotation_xml_integration_point(&self, target: &NodeId) -> bool {
        let nodes = match self.nodes.try_borrow() {
            Ok(nodes) => nodes,
            Err(_) => {
                self.poison.set(true);
                return false;
            }
        };
        matches!(
            nodes.get(target.index()).map(|n| &n.data),
            Some(NodeData::Element {
                mathml_annotation_xml_integration_point: true,
                ..
            })
        )
    }

    fn allow_declarative_shadow_roots(&self, _intended_parent: &NodeId) -> bool {
        // Shadow DOM は本 crate のスコープ外（`CORE-5` (5)）のため、宣言的
        // shadow root の付与を常に拒否する。
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::NodeData as Data;

    fn text_of(doc: &Document, id: NodeId) -> Option<String> {
        match &doc.node(id)?.data {
            Data::Text { contents } => Some(contents.clone()),
            _ => None,
        }
    }

    fn element_local_name(doc: &Document, id: NodeId) -> Option<String> {
        match &doc.node(id)?.data {
            Data::Element { name, .. } => Some(name.local.to_string()),
            _ => None,
        }
    }

    fn find_child_element(doc: &Document, parent: NodeId, local_name: &str) -> Option<NodeId> {
        let node = doc.node(parent)?;
        node.children
            .iter()
            .copied()
            .find(|&id| element_local_name(doc, id).as_deref() == Some(local_name))
    }

    /// CORE-1: 整形式の HTML が期待どおりの `html > head, body > p` 構造と
    /// テキストを持つ。
    #[test]
    fn core_1_parses_well_formed_html_into_expected_structure() {
        let input = "<!DOCTYPE html><html><head></head><body><p>hi</p></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("整形式なので成功する");
        assert_eq!(parsed.diagnostics.error_count, 0);

        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").expect("html 要素がある");
        let body = find_child_element(doc, html, "body").expect("body 要素がある");
        let p = find_child_element(doc, body, "p").expect("p 要素がある");
        let text_id = doc.node(p).expect("p ノード").children[0];
        assert_eq!(text_of(doc, text_id).as_deref(), Some("hi"));
    }

    /// CORE-1: DOCTYPE 欠落など回復可能なパースエラーは Recover（既定）では
    /// `Ok` を返し、木は回復する（`p` の中に `div` を書いた不正なネストは
    /// html5ever が `p` を自動的に閉じて `div` を `body` の子にする）。
    #[test]
    fn core_1_recover_policy_returns_ok_with_diagnostics_for_malformed_html() {
        let input = "<p>a<div>b</div>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("Recover では Ok");
        assert!(parsed.diagnostics.error_count > 0);

        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").expect("html は補完される");
        let body = find_child_element(doc, html, "body").expect("body は補完される");
        let body_node = doc.node(body).expect("body ノード");
        // `<p>` の中に `<div>` は入れられないため、html5ever は `p` を閉じてから
        // `div` を body の直接の子として配置する。
        assert_eq!(body_node.children.len(), 2);
        assert_eq!(
            element_local_name(doc, body_node.children[0]).as_deref(),
            Some("p")
        );
        assert_eq!(
            element_local_name(doc, body_node.children[1]).as_deref(),
            Some("div")
        );
    }

    /// CORE-1: 同じ入力を Strict ポリシーでパースすると `Err(Malformed)` になる。
    #[test]
    fn core_1_strict_policy_returns_err_for_malformed_html() {
        let input = "<p>a<div>b</div>";
        let options = ParseOptions::default().with_error_policy(ParseErrorPolicy::Strict);
        let err = parse_document(input, &options).expect_err("Strict では Err");
        match err {
            Error::Parse(ParseError::Malformed { error_count, .. }) => {
                assert!(error_count > 0);
            }
            other => panic!("Malformed を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: 不正な UTF-8 バイト列は `InvalidUtf8` を返す。
    #[test]
    fn core_1_rejects_invalid_utf8_bytes() {
        let bytes: &[u8] = &[0xff, 0xfe];
        let err = parse_document_bytes(bytes, &ParseOptions::default()).expect_err("不正な UTF-8");
        match err {
            Error::Parse(ParseError::InvalidUtf8 { valid_up_to }) => {
                assert_eq!(valid_up_to, 0);
            }
            other => panic!("InvalidUtf8 を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: 入力サイズが上限を 1 バイトでも超えると `InputTooLarge`。
    /// 上限ちょうどは成功する。
    #[test]
    fn core_1_enforces_input_size_limit() {
        let options = ParseOptions::default().with_max_input_bytes(10);
        let exactly_at_limit = "0123456789";
        assert_eq!(exactly_at_limit.len(), 10);
        assert!(parse_document(exactly_at_limit, &options).is_ok());

        let over_limit = "01234567890";
        assert_eq!(over_limit.len(), 11);
        let err = parse_document(over_limit, &options).expect_err("上限超過");
        match err {
            Error::Parse(ParseError::InputTooLarge { len, limit }) => {
                assert_eq!(len, 11);
                assert_eq!(limit, 10);
            }
            other => panic!("InputTooLarge を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: ノード数上限を小さくすると `NodeLimitExceeded` になる。
    #[test]
    fn core_1_enforces_node_limit() {
        let options = ParseOptions::default().with_max_nodes(5);
        let input = "<!DOCTYPE html><html><head></head><body><p>hi</p></body></html>";
        let err = parse_document(input, &options).expect_err("ノード数上限超過");
        match err {
            Error::Parse(ParseError::NodeLimitExceeded { limit }) => {
                assert_eq!(limit, 5);
            }
            other => panic!("NodeLimitExceeded を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: `with_max_nodes(0)` は黙って `1` へ引き上げず
    /// `Err(InvalidMaxNodes)` になる（公開 API が受け取った上限と実際の
    /// 適用上限の乖離を防ぐ。REPAIR-6 P1 レビュー指摘）。
    #[test]
    fn core_1_rejects_zero_max_nodes() {
        let options = ParseOptions::default().with_max_nodes(0);
        let input = "<p>hi</p>";
        let err = parse_document(input, &options).expect_err("max_nodes(0) は Err");
        match err {
            Error::Parse(ParseError::InvalidMaxNodes { requested }) => {
                assert_eq!(requested, 0);
            }
            other => panic!("InvalidMaxNodes を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: `parse_document_bytes` でも `max_nodes(0)` は
    /// `InvalidMaxNodes` になる（`InvalidUtf8` より優先。両エントリポイントで
    /// 同じ `options` を渡したときの分岐を一貫させる。REPAIR-6 P1 レビュー
    /// 指摘）。
    #[test]
    fn core_1_rejects_zero_max_nodes_bytes_before_invalid_utf8() {
        let options = ParseOptions::default().with_max_nodes(0);
        let invalid_utf8: &[u8] = &[0xff, 0xfe];
        let err = parse_document_bytes(invalid_utf8, &options).expect_err("max_nodes(0) は Err");
        match err {
            Error::Parse(ParseError::InvalidMaxNodes { requested }) => {
                assert_eq!(requested, 0);
            }
            other => panic!("InvalidMaxNodes を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: `<template>`（`get_template_contents` 経由）・`<table>`
    /// （foster parenting）・adoption agency（`<b><i></b></i>`）・重複属性の
    /// いずれの構造でノード数上限に達しても、内部不変条件違反（`Internal`）に
    /// 誤判定されず `NodeLimitExceeded` になることを確認する
    /// （`get_template_contents`・`add_attrs_if_missing` が上限超過後の
    /// 番兵ノードに対して呼ばれても poison を立てないことの回帰テスト）。
    #[test]
    fn core_1_node_limit_exceeded_takes_priority_over_internal_for_various_structures() {
        for input in [
            "<template>x</template>",
            "<table>x</table>",
            "<b><i></b></i>",
            r#"<html a="1"><html a="2">"#,
        ] {
            let options = ParseOptions::default().with_max_nodes(3);
            let err = parse_document(input, &options)
                .expect_err(&format!("{input}: ノード数上限超過で Err になるはず"));
            assert!(
                matches!(
                    err,
                    Error::Parse(ParseError::NodeLimitExceeded { limit: 3 })
                ),
                "{input}: NodeLimitExceeded を期待したが {err:?} だった"
            );
        }
    }

    /// CORE-1: adoption agency アルゴリズム（`<b><i></b></i>`）により、`i` が
    /// 複製され `b` の外側にも配置される。
    #[test]
    fn core_1_adoption_agency_duplicates_misnested_element() {
        let input = "<!DOCTYPE html><html><body><b>1<i>2</b>3</i></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("Recover では Ok");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").unwrap();
        let body = find_child_element(doc, html, "body").unwrap();
        let body_node = doc.node(body).unwrap();

        // adoption agency により `<i>` が複製されるため、`body` 直下には
        // `b` と、複製された `i` の 2 要素が並ぶ。
        assert_eq!(body_node.children.len(), 2);
        let b = body_node.children[0];
        assert_eq!(element_local_name(doc, b).as_deref(), Some("b"));
        let outer_i = body_node.children[1];
        assert_eq!(element_local_name(doc, outer_i).as_deref(), Some("i"));

        let b_node = doc.node(b).unwrap();
        // `b` の子は "1" と、内側に複製された `i`（中身は "2"）。
        assert_eq!(text_of(doc, b_node.children[0]).as_deref(), Some("1"));
        let inner_i = b_node.children[1];
        assert_eq!(element_local_name(doc, inner_i).as_deref(), Some("i"));
        let inner_i_node = doc.node(inner_i).unwrap();
        assert_eq!(text_of(doc, inner_i_node.children[0]).as_deref(), Some("2"));

        // 外側に複製された `i` の子は "3"。
        let outer_i_node = doc.node(outer_i).unwrap();
        assert_eq!(text_of(doc, outer_i_node.children[0]).as_deref(), Some("3"));
    }

    /// CORE-1: 隣接するテキストは 1 つのテキストノードに連結される
    /// （`<p>` の子はコメントで分断されない限り 1 つのテキストノードになる）。
    #[test]
    fn core_1_merges_adjacent_text_nodes() {
        // 文字参照 `&amp;` はトークナイザ内部で 2 つのテキストチャンクに
        // 分割されて tree builder へ渡されるため、隣接テキスト連結の
        // 確認に使える。
        let input = "<p>foo&amp;bar</p>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("整形式");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").unwrap();
        let body = find_child_element(doc, html, "body").unwrap();
        let p = find_child_element(doc, body, "p").unwrap();
        let p_node = doc.node(p).unwrap();
        assert_eq!(p_node.children.len(), 1);
        assert_eq!(text_of(doc, p_node.children[0]).as_deref(), Some("foo&bar"));
    }

    /// CORE-1: DOCTYPE ありは Doctype ノードと NoQuirks を持つ。
    #[test]
    fn core_1_doctype_present_yields_no_quirks_mode() {
        let input = "<!DOCTYPE html><html><head></head><body></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("整形式");
        let doc = &parsed.document;
        assert_eq!(doc.quirks_mode(), QuirksMode::NoQuirks);
        let doctype_id = doc.node(doc.root()).unwrap().children[0];
        match &doc.node(doctype_id).unwrap().data {
            Data::Doctype { name, .. } => assert_eq!(name, "html"),
            other => panic!("Doctype を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: DOCTYPE なしは Quirks モードになり、Doctype ノードを持たない。
    #[test]
    fn core_1_missing_doctype_yields_quirks_mode() {
        let input = "<html><head></head><body></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("Recover では Ok");
        let doc = &parsed.document;
        assert_eq!(doc.quirks_mode(), QuirksMode::Quirks);
        let root_children = &doc.node(doc.root()).unwrap().children;
        assert!(
            root_children
                .iter()
                .all(|&id| !matches!(doc.node(id).unwrap().data, Data::Doctype { .. }))
        );
    }

    /// CORE-1: コメントノードの内容が一致する。
    #[test]
    fn core_1_comment_contents_preserved() {
        let input = "<!--hello--><html><head></head><body></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("Recover では Ok");
        let doc = &parsed.document;
        let comment_id = doc.node(doc.root()).unwrap().children[0];
        match &doc.node(comment_id).unwrap().data {
            Data::Comment { contents } => assert_eq!(contents, "hello"),
            other => panic!("Comment を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: `<template>` の子は要素自身の `children` ではなく
    /// `template_contents` 側に入る。
    #[test]
    fn core_1_template_children_go_to_template_contents() {
        let input = "<!DOCTYPE html><html><head><template><span>x</span></template></head><body></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("整形式");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").unwrap();
        let head = find_child_element(doc, html, "head").unwrap();
        let template = find_child_element(doc, head, "template").unwrap();
        let template_node = doc.node(template).unwrap();
        assert!(template_node.children.is_empty());
        match &template_node.data {
            Data::Element {
                template_contents: Some(contents),
                ..
            } => {
                let span = find_child_element(doc, *contents, "span").expect("span は contents 側");
                let text_id = doc.node(span).unwrap().children[0];
                assert_eq!(text_of(doc, text_id).as_deref(), Some("x"));
            }
            other => panic!("template_contents を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: `<table>` 直下のテキストは foster parenting により
    /// table の前に配置される。
    #[test]
    fn core_1_foster_parenting_moves_text_before_table() {
        let input = "<!DOCTYPE html><html><body><table>text</table></body></html>";
        let parsed = parse_document(input, &ParseOptions::default()).expect("Recover では Ok");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").unwrap();
        let body = find_child_element(doc, html, "body").unwrap();
        let body_node = doc.node(body).unwrap();
        assert_eq!(text_of(doc, body_node.children[0]).as_deref(), Some("text"));
        assert_eq!(
            element_local_name(doc, body_node.children[1]).as_deref(),
            Some("table")
        );
    }

    /// CORE-1: `<table>` の直前に多数の兄弟要素が存在する状態で `<table>` 直下へ
    /// テキストが foster parenting されても、探索位置キャッシュ
    /// （`ArenaSink::last_sibling_pos`）を挟まない場合と同じ木構造になる
    /// （P1 レビュー指摘の回帰防止。PR #431
    /// <https://github.com/Fandhe-AI/fandhe-browser/pull/431#discussion_r4110398582>）。
    /// `<table>` を `sibling` とする `insert_before`／位置探索は、`<table>` の
    /// 手前に多数の兄弟（`<p>` 64 個）がある場合ほど線形探索のコストが高く、
    /// キャッシュが正しく機能しないと結果が壊れる経路を通る。
    #[test]
    fn core_1_foster_parenting_before_table_with_many_preceding_siblings() {
        let preceding: String = (0..64).map(|i| format!("<p>p{i}</p>")).collect();
        let input =
            format!("<!DOCTYPE html><html><body>{preceding}<table>text</table></body></html>");
        let parsed = parse_document(&input, &ParseOptions::default()).expect("Recover では Ok");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").unwrap();
        let body = find_child_element(doc, html, "body").unwrap();
        let body_node = doc.node(body).unwrap();
        // 64 個の `<p>` の直後に、foster parenting されたテキストノードが
        // `<table>` の直前へ挿入され、その後に `<table>` 自身が続く。
        assert_eq!(body_node.children.len(), 64 + 1 + 1);
        for i in 0..64 {
            assert_eq!(
                element_local_name(doc, body_node.children[i]).as_deref(),
                Some("p")
            );
        }
        assert_eq!(
            text_of(doc, body_node.children[64]).as_deref(),
            Some("text")
        );
        let table = body_node.children[65];
        assert_eq!(element_local_name(doc, table).as_deref(), Some("table"));
    }

    /// CORE-1: 重複する `<html>` の属性は first-wins（`add_attrs_if_missing`
    /// の契約どおり最初の値を残す）。
    #[test]
    fn core_1_duplicate_root_attributes_keep_first_value() {
        let input = r#"<html lang="en"><html lang="fr"><head></head><body></body></html>"#;
        let parsed = parse_document(input, &ParseOptions::default()).expect("Recover では Ok");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").unwrap();
        match &doc.node(html).unwrap().data {
            Data::Element { attrs, .. } => {
                assert_eq!(attrs.len(), 1);
                assert_eq!(attrs[0].value, "en");
            }
            other => panic!("Element を期待したが {other:?} だった"),
        }
    }

    /// CORE-1: 数千段の `<div>` ネストでもスタックオーバーフローせずに
    /// パース・drop が完了する（arena は `Vec` フラット確保で再帰しないため）。
    #[test]
    fn core_1_deeply_nested_divs_do_not_overflow_stack() {
        const DEPTH: usize = 20_000;
        let mut input = String::with_capacity(DEPTH * 5);
        for _ in 0..DEPTH {
            input.push_str("<div>");
        }
        let options = ParseOptions::default().with_max_nodes(usize::MAX);
        let parsed = parse_document(&input, &options).expect("深いネストでも成功する");
        // ROOT + SENTINEL + html/head/body + DEPTH 個の div。
        assert!(parsed.document.node_count() >= DEPTH);
    }

    /// CORE-1: `parse_error` のメッセージ件数は `max_recorded_errors` で
    /// 打ち切られるが、`error_count` はそれを超えて数える。
    #[test]
    fn core_1_parse_error_messages_are_capped() {
        // 重複する `<html>` の属性を大量に発生させ、パースエラーを多発させる。
        let mut input = String::from("<html");
        for _ in 0..10 {
            input.push_str(r#" lang="en" lang="en" lang="en""#);
        }
        input.push_str("><head></head><body></body></html>");
        let options = ParseOptions::default().with_max_recorded_errors(3);
        let parsed = parse_document(&input, &options).expect("Recover では Ok");
        assert_eq!(parsed.diagnostics.messages.len(), 3);
        assert!(parsed.diagnostics.error_count >= 3);
    }

    /// CORE-1: `Document`・`ParsedDocument`・`Error` が `Send + Sync + 'static`
    /// を満たす（`StrTendril`・`RefCell` を保持しないため）。
    #[test]
    fn core_1_parsed_document_is_send_sync_static() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<Document>();
        assert_bounds::<ParsedDocument>();
        assert_bounds::<Error>();
    }

    /// CORE-1: 空文字列は `Ok`（html/head/body が補完される）。
    #[test]
    fn core_1_empty_input_is_ok_with_completed_structure() {
        let parsed = parse_document("", &ParseOptions::default()).expect("空文字列でも成功する");
        let doc = &parsed.document;
        let html = find_child_element(doc, doc.root(), "html").expect("html が補完される");
        find_child_element(doc, html, "head").expect("head が補完される");
        find_child_element(doc, html, "body").expect("body が補完される");
    }
}

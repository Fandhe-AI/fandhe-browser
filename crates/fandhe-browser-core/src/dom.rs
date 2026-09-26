//! dom: `parse` が構築した木構造の DOM 表現（arena 方式）を担うモジュール。
//!
//! `parse` モジュール（TASK-24.4・#38）の出力を保持する型定義のみを置く。
//! ノードの走査 API（親子・兄弟を辿るイテレータ等）や属性アクセサは
//! TASK-24.5（Issue #39）が担当するため、本 PR では追加しない
//! （REPAIR-3: 実装済みを装わない）。`query` モジュール（TASK-24.7・#41）や
//! 上位 crate（`fandhe-browser-ai` の簡約 DOM 生成等）は #39 が追加する
//! 走査 API 経由で本モジュールの型を参照する想定。
//!
//! # 設計上の要点
//!
//! - ノードは `Vec<Node>`（arena）に平坦に格納し、親子関係は [`NodeId`]
//!   （`Vec` へのインデックス）で表現する。参照カウント（`Rc`）や内部可変性
//!   （`RefCell`）を持たないため [`Document`] は `Send + Sync` になり、
//!   マルチスレッド環境（将来の並列スクレイピング）へ持ち出せる
//!   （TASK-24.5 の前提。#39 で `Send + Sync` のコンパイル時テストを追加）。
//! - `Vec` が平坦に確保されるため、`Drop` 時の解放は再帰しない
//!   （数万段のネストでもスタックオーバーフローしない。security.md
//!   「無制限リソース確保」対策の一部）。
//! - 属性値・テキストは html5ever の `StrTendril`（`Send` でない）ではなく
//!   `String` に変換して保持する（Issue #35 決定・PoC-5 で判明した
//!   `scraper::Html` の `!Send` 問題の再発防止）。

/// arena（[`Document::nodes`]）内のノードを指すインデックス。
///
/// 添字アクセス（`nodes[id]`）ではなく [`Document::node`] 等の `get` 系
/// メソッド経由でのみ参照する（coding-rust.md「外部入力の経路では添字アクセスを
/// 使わない」。HTML は外部入力であり、`parse` モジュールが構築するノード数は
/// `ParseOptions::max_nodes` で上限管理されるが、念のため境界チェックを徹底する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub(crate) usize);

/// HTML 要素の属性 1 つ分。
///
/// html5ever の `Attribute`（値が `StrTendril`）とは異なり、値を `String` に
/// 変換済みで保持する（本モジュールの設計上の要点を参照）。
#[derive(Debug, Clone)]
pub struct Attribute {
    /// 属性名（名前空間込み）。
    pub name: html5ever::QualName,
    /// 属性値。
    pub value: String,
}

/// ノード 1 つが持つ種別ごとのデータ。
///
/// `parse`（#38）の `ArenaSink`（`TreeSink` 実装）が html5ever のトークン列から
/// 構築する。`#[non_exhaustive]` とすることで、後続タスクでのバリアント追加を
/// 非破壊にする（REPAIR-4・coding-rust.md「公開 API」）。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NodeData {
    /// ドキュメントのルートノード（[`Document::root`] が指す）。
    Document,
    /// `<!DOCTYPE ...>` 宣言。
    Doctype {
        /// DOCTYPE 名（通常は `"html"`）。
        name: String,
        /// 公開識別子（多くの場合空文字列）。
        public_id: String,
        /// システム識別子（多くの場合空文字列）。
        system_id: String,
    },
    /// 要素ノード（`<div>` 等）。
    Element {
        /// 要素名（名前空間込み）。公開アクセサの形（atom で返すか `&str` で
        /// 返すか）は走査 API を追加する #39 に委ねる。
        name: html5ever::QualName,
        /// 属性一覧。html5ever の契約（`add_attrs_if_missing` は
        /// 既存属性名を優先する）に従い、重複する属性名は最初の 1 つのみを残す。
        attrs: Vec<Attribute>,
        /// `<template>` 要素の template contents（別ドキュメントフラグメントの
        /// ノード）。`<template>` 以外では常に `None`。
        template_contents: Option<NodeId>,
        /// MathML の `annotation-xml` 要素が HTML integration point かどうか
        /// （html5ever の `TreeSink::is_mathml_annotation_xml_integration_point`
        /// が参照する）。
        mathml_annotation_xml_integration_point: bool,
    },
    /// テキストノード。html5ever の契約により、隣接するテキストは 1 ノードへ
    /// 連結済み（`parse` の `ArenaSink::append` 等が担う）。
    Text {
        /// テキスト内容。
        contents: String,
    },
    /// コメントノード（`<!-- ... -->`）。
    Comment {
        /// コメント内容。
        contents: String,
    },
    /// 処理命令ノード（XML 由来。HTML では稀）。
    ProcessingInstruction {
        /// ターゲット名。
        target: String,
        /// データ本体。
        data: String,
    },
    /// `<template>` の template contents 用に確保される、どの親も持たない
    /// ドキュメントフラグメントのルート。
    DocumentFragment,
}

/// arena 内のノード 1 つ。親子関係は [`NodeId`] で表現する。
#[derive(Debug, Clone)]
pub struct Node {
    /// 親ノードの ID。ルートノード・detach 済みノードは `None`。
    pub parent: Option<NodeId>,
    /// 子ノードの ID（出現順）。
    pub children: Vec<NodeId>,
    /// ノード種別ごとのデータ。
    pub data: NodeData,
}

/// ドキュメントの quirks mode。
///
/// html5ever の `QuirksMode`（tendril 等の `!Send` な内部可変性を持たない
/// 素の enum）をそのまま再エクスポートする。独自の enum を再定義して
/// 変換コードを増やすより、html5ever（本 crate が唯一許可された HTML パーサー
/// 依存。Issue #35 決定）の型をそのまま公開 API に使う方が保守コストが低い。
pub type QuirksMode = html5ever::interface::QuirksMode;

/// `parse::parse_document`（#38）が構築する DOM ドキュメント全体。
///
/// `Rc`・`RefCell`・`StrTendril` を含まないため `Send + Sync` になる
/// （#39 が静的アサーションで検証する前提）。
#[derive(Debug)]
pub struct Document {
    /// 全ノードを格納する arena。インデックス 0 は常に [`NodeData::Document`]
    /// （[`Document::root`] と一致）。
    pub(crate) nodes: Vec<Node>,
    /// ルートノードの ID（常に `NodeId(0)`）。
    pub(crate) root: NodeId,
    /// パース時に決定した quirks mode。
    pub(crate) quirks_mode: QuirksMode,
}

impl Document {
    /// ルートノード（[`NodeData::Document`]）の ID を返す。
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// quirks mode を返す。
    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks_mode
    }

    /// 総ノード数（`ParseOptions::max_nodes` の検証などに使う）。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// `id` が指すノードへの参照を返す。範囲外なら `None`
    /// （添字アクセスではなく `get` 経由。coding-rust.md）。
    ///
    /// 本格的な走査 API（親子・兄弟を辿るイテレータ等）は TASK-24.5（#39）が
    /// 追加する。ここでは `parse` のユニットテストが構築結果を検査するための
    /// 最小限のアクセサのみを提供する。
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.0)
    }
}

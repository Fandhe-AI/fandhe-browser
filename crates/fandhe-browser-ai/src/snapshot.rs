//! `core::dom::Document` から、方式 B（役割ベースアクセシビリティツリー）の
//! 簡約表現 `Snapshot`/`Node` を構築するモジュール（`AISNAP-1`・`TASK-11`・`MS-2`）。
//!
//! 呼び出し文脈: 将来 `fandhe-browser-cli` 層の配線を経由して、
//! `fandhe-browser-cdp` の `/ai/snapshot` ルータから利用される想定
//! （`TASK-19`・`AISNAP-6`/`AISNAP-7`）。本 crate（`ai`）は `cdp` に
//! 直接依存しない（crate 間の許可依存。lib.rs のドキュメンテーションコメント
//! を参照）。
//!
//! # スタブについて
//!
//! TASK-11.2（`AISNAP-1`・Issue #71）で公開型 `Snapshot`/`Node` を定義した。
//! state の算出ロジックは実装済み（[`state`] モジュール）。accessible
//! name はネイティブのラベル付け分のみ実装済み（[`name`] モジュール・
//! [`name::compute_name`]。TASK-11.4.2・Issue #545）。DOM からのツリー
//! 構築は TASK-11.7（Issue #76）で実装済み（[`build`] モジュール・
//! [`build_snapshot`]）。段階的に以下の Issue で実装した：
//!
//! - role（役割）算出: TASK-11.3（`AISNAP-1`・Issue #72）。骨格と button・
//!   link・heading・table・list 等の代表要素は TASK-11.3.1（Issue #541）で
//!   実装済み（[`role`] モジュール・[`role::compute_role`]）。`input[type]`
//!   の対応表は TASK-11.3.2（Issue #542）で実装済み（`list` による
//!   `combobox` 化は未対応）。`select`・`header`/`footer`
//!   は TASK-11.3.3（Issue #543）で実装済み。`aside`（complementary）と
//!   `th` の表文脈による降格は担当未割り当て。
//!   form・section・img の名前依存の昇格は担当未割り当て（role.rs の doc 参照）
//! - accessible name（アクセシブルネーム）算出: TASK-11.4（Issue #73）を
//!   3 分割。ARIA 属性（`aria-labelledby`/`aria-label`）: TASK-11.4.1
//!   （Issue #544）で実装済み。HTML ネイティブのラベル付け（`alt`・`title`・
//!   `value`・`placeholder`・submit/reset/image の既定ラベル・
//!   `label[for]`・label による包含）: TASK-11.4.2（Issue #545）で実装済み。
//!   子孫テキスト（name from content）・優先順位統合・文書ルートの
//!   `<title>`: TASK-11.4.3（Issue #546）で実装済み
//! - state（状態）算出: TASK-11.5（Issue #74）で実装済み（[`state`] モジュール・
//!   [`state::compute_state`]）。[`build_snapshot`] から呼び出される（TASK-11.7・Issue #76）
//! - ref（role + name シグネチャによる再特定要求。`AISNAP-10`）: TASK-11.6（Issue #75）で
//!   生成器を実装済み（[`element_ref`]・[`RefAllocator`]）。木への組み込みは [`build_snapshot`]（TASK-11.7・Issue #76）で実装済み
//! - DOM から `Snapshot` へのツリー構築統合（[`role::compute_role`]・
//!   [`state::compute_state`] の呼び出し組み込みを含む）: TASK-11.7（Issue #76）で
//!   実装済み（[`build_snapshot`]。generic の折り畳み等の簡約は未実装）
//! - データ葉（`isDataLeaf`）の判定結果の反映: TASK-13.3（`AISNAP-3`・Issue #88）で
//!   実装済み（[`Node::data_leaf`]。算出は [`crate::data_leaf::classify_data_leaf`]。
//!   印を付けるだけで、簡約・剪定への利用は後続タスク）
//! - 表・一覧の圧縮戦略の統合: TASK-12.5（`AISNAP-2`・Issue #83）で実装済み
//!   （[`Node::table`]・[`TableSummary`]。規則的な `table`・`ul`・`ol` は
//!   [`build_snapshot`] が子孫を展開せず、ヘッダ・圧縮行・超過行数を持つ 1 ノードへ
//!   置き換える。圧縮した行・項目の中の操作要素（リンク等）は現状 ref を持たない）
//! - ユニットテスト一式: TASK-11.8（Issue #77）で実装済み（代表フィクスチャ 3 種の
//!   結合テスト `tests/snapshot.rs`）

pub mod build;
pub mod element_ref;
pub mod name;
pub mod role;
pub mod state;
pub use crate::data_leaf::DataLeafKind;
pub use build::{MAX_TREE_DEPTH, SnapshotError, build_snapshot};
pub use element_ref::{ElementRef, RefAllocator, RefError, ref_signature};
pub use name::{AccessibleName, NameIndex, NameSource, compute_name, compute_name_with_index};
pub use role::{ComputedRole, RoleSource, compute_role};
pub use state::{CheckedState, State, compute_state};

/// 方式 B 簡約ツリーの 1 ノード（`AISNAP-1`・`TASK-11`・`MS-2`）。
///
/// DOM の要素 1 つに対応する、AI エージェント向けの簡約表現。各フィールドの
/// 算出は後続タスクが担う（本 Issue TASK-11.2 では型の形だけを定義する）。
///
/// - `role`: ARIA role のトークン（例: `"document"`・`"heading"`・`"button"`）。
///   役割の種類は多く将来も増えるため `String` とし、enum 化しない。
///   算出ロジックは [`role::compute_role`]。骨格と代表要素は TASK-11.3.1
///   （Issue #541）で実装済みだが、[`build_snapshot`] が本フィールドへ反映する
///   （TASK-11.7・Issue #76）
/// - `name`: accessible name。空文字列は「名前なし」を表す。
///   算出は TASK-11.4（Issue #73）が担う。ネイティブのラベル付け分は
///   [`name::compute_name`]（TASK-11.4.2・Issue #545）、ARIA 属性分は
///   同関数の優先順位（`aria-labelledby` → `aria-label` → ネイティブ）で
///   実装済み（TASK-11.4.1・Issue #544）。子孫テキストによる命名・
///   文書ルートの `<title>` は TASK-11.4.3（Issue #546）で実装済み
/// - `r#ref`: role + name シグネチャによる再特定要求（`AISNAP-10`）。
///   `None` は ref を振らないノード（例: document ルート）を表す。
///   値の形式は `e<16hex>[v<n>][-n]`（[`RefAllocator`] が発行。TASK-11.6・Issue #75・
///   `AISNAP-10`）。木への割り当ては [`build_snapshot`]（TASK-11.7）が担う
/// - `children`: DOM の親子関係に対応する子ノード。構築は
///   [`build_snapshot`]（TASK-11.7・Issue #76）
/// - `state`: 要素の状態（`disabled`・`checked`）。算出は
///   [`state::compute_state`]（TASK-11.5・Issue #74）が担う。`Node::new` の
///   既定値は `State::default()`（`disabled: false`・`checked: None`）
/// - `data_leaf`: 非インタラクティブなデータ値（表セル・価格クラス要素）と判定した
///   根拠。`None` はデータ葉でない。判定は [`crate::data_leaf::classify_data_leaf`]
///   （`AISNAP-3`・TASK-13.3・Issue #88）。role や ref には影響させない
///   （ref の安定性。`AISNAP-10`）
///
/// `#[non_exhaustive]` により、今後のフィールド追加は破壊的変更にならない。
///
/// - `table`: 規則的な表・一覧を圧縮した内容（`AISNAP-2`・TASK-12.5・Issue #83）。
///   `Some` のノードは子孫を展開せず（`children` は空）、ヘッダ・圧縮行・超過行数を
///   [`TableSummary`] に持つ。`None` は圧縮していないノード
///
/// 呼び出し文脈: [`build_snapshot`]（TASK-11.7・Issue #76）が `core` の DOM から構築し、
/// 将来は `cli` 層の配線を経由して `cdp` の `/ai/snapshot` から使われる
/// （TASK-19・`AISNAP-6`）。
///
/// 不安全な設計への対処: `children` は再帰構造で、derive した
/// `Drop`/`PartialEq`/`Debug`/`Clone` も再帰する。[`build_snapshot`] は反復で構築し、
/// 深さを [`MAX_TREE_DEPTH`] に制限してスタックオーバーフローを防ぐ。手動で
/// 深い木を組み立てる場合は呼び出し側で深さに注意すること。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Node {
    /// ARIA role のトークン。算出は TASK-11.3（Issue #72）。
    pub role: String,
    /// accessible name。算出は TASK-11.4（Issue #73）。ネイティブ分は
    /// [`name::compute_name`]（TASK-11.4.2・Issue #545）で実装済み。
    pub name: String,
    /// role + name シグネチャによる再特定要求（`AISNAP-10`）。
    /// 形式は `e<16hex>[v<n>][-n]`。生成は [`RefAllocator`]（TASK-11.6・Issue #75）。
    pub r#ref: Option<String>,
    /// DOM の親子関係に対応する子ノード。構築は [`build_snapshot`]（TASK-11.7・Issue #76）。
    pub children: Vec<Node>,
    /// 要素の状態（`disabled`・`checked`）。算出は
    /// [`state::compute_state`]（TASK-11.5・Issue #74）。
    pub state: State,
    /// データ葉と判定した根拠。`None` はデータ葉でない。
    /// 判定は [`crate::data_leaf::classify_data_leaf`]（`AISNAP-3`・TASK-13.3・Issue #88）。
    pub data_leaf: Option<DataLeafKind>,
    /// 規則的な表・一覧の圧縮結果。`None` は圧縮していないノード
    /// （`AISNAP-2`・TASK-12.5・Issue #83）。
    pub table: Option<TableSummary>,
    /// 展開した表・一覧（操作要素等を含み全体を圧縮できないもの）のうち、優先保持
    /// （`AISNAP-12`・TASK-16.4）の対象外で、かつ失われる内容を持たない通常行を
    /// 子ノードの代わりに 1 行文字列へ畳んだもの（文書順）。ref・state を持つ行
    /// （リンク・ボタン等を含む行）と優先保持で選ばれた行は畳まず `children` に残す。
    /// 空は畳んだ行なし。行の内容は省略せず文字列で保持する（セルの切り詰めは
    /// [`FoldedRow::truncated`] で通知）。
    ///
    /// 契約（`AISNAP-2`・`AISNAP-12`）: 畳んだ行は ref・`data_leaf` を持たない
    /// （圧縮表の「データ行は ref を持たない」を展開表の通常行へ広げる）。ref を持つべき行は
    /// `children` に残る。元の順序は [`FoldedRow::index`] で復元する。
    pub folded_rows: Vec<FoldedRow>,
}

/// 展開した表・一覧で 1 行文字列へ畳んだ通常行（`AISNAP-12`・TASK-16.4）。
///
/// [`Node::folded_rows`] の要素。ref・`data_leaf` は持たない（`AISNAP-2` の
/// 「データ行は ref を持たない」と同じ契約）。
///
/// `index` は、表・一覧の表示対象データ行（ヘッダ行・`tfoot` 行・非表示行を除く）を
/// 文書順に数えたときの 0 起点の位置。展開側（`children` 配下）に残る表示対象データ行は、
/// 畳んだ行が使っていない index を文書順に昇順で占める。したがって両者を index で
/// 統合すれば元の文書順を復元できる。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FoldedRow {
    /// 表示対象データ行の中での元の位置（0 起点）。
    pub index: usize,
    /// 1 行表現（セルを `" | "` で連結）。
    pub text: String,
    /// いずれかのセルが上限で切り詰められたか。
    pub truncated: bool,
}

impl FoldedRow {
    /// 位置・行文字列・切り詰め有無を指定して作る。
    pub fn new(index: usize, text: impl Into<String>, truncated: bool) -> Self {
        Self {
            index,
            text: text.into(),
            truncated,
        }
    }
}

/// 圧縮した表・一覧のヘッダ 1 セル（`AISNAP-2`・`AISNAP-10`・TASK-12.5）。
///
/// ヘッダ行のみ個別 ref を持つ（データ行は圧縮のため ref を持たない）。
/// spec の想定 JSON ではヘッダを文字列配列で描くが、JSON への写像は
/// TASK-19・`AISNAP-6` で扱う。
///
/// データ葉分類（`AISNAP-3`・TASK-13.3）も [`Node`] と同じく [`HeaderCell::data_leaf`]
/// に型として保持する。圧縮の有無で同じ `th` の分類が変わらないことを型で保証する契約で、
/// 利用側（テスト・後続の JSON 写像）が値を補完する必要はない（Issue #625）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeaderCell {
    /// role トークン（通常 `columnheader`）。
    pub role: String,
    /// accessible name。
    pub name: String,
    /// 再特定要求（形式は [`Node::r#ref`] と同じ）。
    pub r#ref: String,
    /// データ葉と判定した根拠。`None` はデータ葉でない。
    ///
    /// `build_snapshot` が元セル（`td`/`th`）に対して
    /// [`crate::data_leaf::classify_data_leaf`] で算出する。ヘッダセルは `td`/`th` に
    /// 限られるため現状は常に `Some(DataLeafKind::TableCell)`。展開時の
    /// [`Node::data_leaf`] と同一規則で、role・ref には影響しない（`AISNAP-10`）。
    pub data_leaf: Option<DataLeafKind>,
}

impl HeaderCell {
    /// role・name・ref を指定して作る。`data_leaf` は `None`（[`Self::with_data_leaf`] で設定）。
    pub fn new(role: impl Into<String>, name: impl Into<String>, r#ref: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            name: name.into(),
            r#ref: r#ref.into(),
            data_leaf: None,
        }
    }

    /// データ葉の判定根拠を設定する（[`Node::with_data_leaf`] と対）。
    #[must_use]
    pub fn with_data_leaf(mut self, kind: DataLeafKind) -> Self {
        self.data_leaf = Some(kind);
        self
    }
}

/// 圧縮した 1 データ行（セルを `" | "` で連結した文字列。`AISNAP-2`・TASK-12.5）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TableRow {
    /// 1 行表現。
    pub text: String,
    /// いずれかのセルが上限（文字数・走査量）で切り詰められたか。
    pub truncated: bool,
}

impl TableRow {
    /// 行文字列と切り詰め有無を指定して作る。
    pub fn new(text: impl Into<String>, truncated: bool) -> Self {
        Self {
            text: text.into(),
            truncated,
        }
    }
}

/// 規則的な表・一覧の圧縮表現（`AISNAP-2`・TASK-12.5・Issue #83）。
///
/// [`build_snapshot`] が `compress_table` の部品（構造検出・ヘッダ ref 付与・行圧縮）から
/// 組み立てる。呼び出し元は将来の `/ai/snapshot`（TASK-19）。
///
/// 既知の制約（実装済みを装わない。REPAIR-3）:
/// - 非表示のヘッダセルは `header` から除かれるため、`header` と `rows` の列位置は
///   対応しないことがある
/// - `tfoot` 行は `rows` にも `truncated_rows` にも含めない
/// - セル内の `" | "` はエスケープしない
/// - 圧縮した行・項目の中のリンク等の操作要素は ref を持たない
/// - 行の省略は `truncated_rows`、セルの切り詰めは [`TableRow::truncated`] で通知し、
///   [`Snapshot::truncated`] は立てない
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TableSummary {
    /// ヘッダセル（ヘッダ行が無い表・一覧では空）。
    pub header: Vec<HeaderCell>,
    /// 優先保持（`AISNAP-12`）で選んだ最大 20 行の圧縮行（文書順）。
    pub rows: Vec<TableRow>,
    /// 保持されず省略した表示対象の行数。
    pub truncated_rows: usize,
}

impl TableSummary {
    /// ヘッダ・行・超過行数を指定して作る。
    pub fn new(header: Vec<HeaderCell>, rows: Vec<TableRow>, truncated_rows: usize) -> Self {
        Self {
            header,
            rows,
            truncated_rows,
        }
    }
}

impl Node {
    /// role・name を指定して `Node` を作る（`ref` は `None`、`children` は空、
    /// `state` は `State::default()`、`data_leaf` は `None`）。
    pub fn new(role: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            name: name.into(),
            r#ref: None,
            children: Vec::new(),
            state: State::default(),
            data_leaf: None,
            table: None,
            folded_rows: Vec::new(),
        }
    }

    /// `ref` を設定した `Node` を返す（ビルダー）。
    #[must_use]
    pub fn with_ref(mut self, r: impl Into<String>) -> Self {
        self.r#ref = Some(r.into());
        self
    }

    /// `state` を設定した `Node` を返す（ビルダー）。
    #[must_use]
    pub fn with_state(mut self, state: State) -> Self {
        self.state = state;
        self
    }

    /// `data_leaf` に根拠 `kind` を設定した `Node` を返す（ビルダー。`AISNAP-3`）。
    #[must_use]
    pub fn with_data_leaf(mut self, kind: DataLeafKind) -> Self {
        self.data_leaf = Some(kind);
        self
    }

    /// `table` に圧縮結果を設定した `Node` を返す（ビルダー。`AISNAP-2`）。
    #[must_use]
    pub fn with_table(mut self, table: TableSummary) -> Self {
        self.table = Some(table);
        self
    }

    /// `children` を設定した `Node` を返す（ビルダー）。
    #[must_use]
    pub fn with_children(mut self, children: Vec<Node>) -> Self {
        self.children = children;
        self
    }

    /// `children` へ 1 ノードを追加する。
    pub fn push_child(&mut self, child: Node) {
        self.children.push(child);
    }
}

/// 方式 B 簡約ツリー全体を表す型（`AISNAP-1`・`TASK-11`・`MS-2`）。
///
/// `tree` は spec の想定 JSON のキー `tree` に対応する DOM のルートノード。
///
/// `url` フィールドは本 Issue では追加しない。URL は `core::dom::Document`
/// ではなく `AppState`（`last_url`）が持つため、`/ai/snapshot` ルータを
/// 実装する TASK-19（`AISNAP-6`・`MS-4`）で追加する。
///
/// serde の derive や JSON への変換は本 Issue では実装しない（serde は
/// workspace に無く、依存の追加はユーザー承認が必要。dependency-policy.md）。
/// JSON への写像は TASK-19・`AISNAP-6` の範囲。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Snapshot {
    /// DOM のルートに対応するノード。構築は [`build_snapshot`]（TASK-11.7・Issue #76）。
    pub tree: Node,
    /// 深さ上限（[`MAX_TREE_DEPTH`]）を超えるサブツリーを省略したか。
    /// 黙って捨てないための印（`AISNAP-1`）。
    pub truncated: bool,
}

impl Snapshot {
    /// ルートノードを指定して `Snapshot` を作る。
    pub fn new(tree: Node) -> Self {
        Self {
            tree,
            truncated: false,
        }
    }

    /// `truncated` を設定した `Snapshot` を返す（ビルダー）。
    #[must_use]
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CheckedState, DataLeafKind, HeaderCell, Node, Snapshot, State, TableRow, TableSummary,
    };

    /// `AISNAP-1`（TASK-11.2・Issue #71、`state` の既定値は TASK-11.5・
    /// Issue #74）: `Node::new` が role・name を設定し、`ref` は `None`、
    /// `children` は空、`state` は `State::default()` になること。
    #[test]
    fn aisnap_1_node_new_sets_role_name_and_defaults() {
        let node = Node::new("heading", "Example Domain");
        assert_eq!(node.role, "heading");
        assert_eq!(node.name, "Example Domain");
        assert_eq!(node.r#ref, None);
        assert_eq!(node.children.len(), 0);
        assert_eq!(node.state, State::default());
        assert_eq!(node.data_leaf, None);
        assert_eq!(node.table, None);
    }

    /// `AISNAP-3`（Issue #625）: `HeaderCell::new` は `data_leaf` を `None` にする。
    #[test]
    fn aisnap_3_header_cell_new_defaults_data_leaf_none() {
        let h = HeaderCell::new("columnheader", "名前", "e1");
        assert_eq!(h.data_leaf, None);
    }

    /// `AISNAP-3`（Issue #625）: `with_data_leaf` は `data_leaf` だけを設定する。
    #[test]
    fn aisnap_3_header_cell_with_data_leaf_sets_kind() {
        let h =
            HeaderCell::new("columnheader", "名前", "e1").with_data_leaf(DataLeafKind::TableCell);
        assert_eq!(h.data_leaf, Some(DataLeafKind::TableCell));
        assert_eq!(h.role, "columnheader");
        assert_eq!(h.name, "名前");
        assert_eq!(h.r#ref, "e1");
    }

    /// `AISNAP-2`（TASK-12.5・Issue #83）: `with_table` が `table` だけを設定すること。
    #[test]
    fn aisnap_2_node_with_table_sets_summary() {
        let summary = TableSummary::new(
            vec![HeaderCell::new("columnheader", "名前", "e1")],
            vec![TableRow::new("太郎", false)],
            3,
        );
        let node = Node::new("table", "").with_table(summary.clone());
        assert_eq!(node.table, Some(summary));
        assert_eq!(node.r#ref, None);
        assert_eq!(node.children.len(), 0);
    }

    /// `AISNAP-3`（TASK-13.3・Issue #88）: `with_data_leaf` が `data_leaf` だけを
    /// 設定し、他のフィールドの既定値は変えないこと。
    #[test]
    fn aisnap_3_node_with_data_leaf_sets_kind() {
        let node = Node::new("cell", "80").with_data_leaf(DataLeafKind::TableCell);
        assert_eq!(node.data_leaf, Some(DataLeafKind::TableCell));
        assert_eq!(node.r#ref, None);
        assert_eq!(node.children.len(), 0);
        assert_eq!(node.state, State::default());
    }

    /// `AISNAP-1`（TASK-11.5・Issue #74）: `with_state` が `state` を
    /// 設定し、他のフィールドの既定値は変えないこと。
    #[test]
    fn aisnap_1_node_with_state_sets_state() {
        let state = State::default()
            .with_disabled(true)
            .with_checked(Some(CheckedState::Checked));
        let node = Node::new("checkbox", "同意する").with_state(state.clone());
        assert_eq!(node.state, state);
        assert_eq!(node.r#ref, None);
        assert_eq!(node.children.len(), 0);
    }

    /// `AISNAP-1`（TASK-11.2・Issue #71）: `with_ref` が `ref` を設定すること。
    #[test]
    fn aisnap_1_node_with_ref_sets_ref() {
        let node = Node::new("link", "More information...").with_ref("e1");
        assert_eq!(node.r#ref, Some("e1".to_string()));
    }

    /// `AISNAP-1`（TASK-11.2・Issue #71）: `push_child` と `with_children` が
    /// 同じ木構造を作り、子ノードを `get()` で具体値まで検証できること。
    #[test]
    fn aisnap_1_node_children_nest() {
        let heading = Node::new("heading", "Example Domain").with_ref("e1");
        let link = Node::new("link", "More information...").with_ref("e2");

        let mut via_push = Node::new("document", "Example Domain");
        via_push.push_child(heading.clone());
        via_push.push_child(link.clone());

        let via_builder =
            Node::new("document", "Example Domain").with_children(vec![heading, link]);

        assert_eq!(via_push, via_builder);
        assert_eq!(via_push.children.len(), 2);

        let first = via_push.children.first().expect("先頭の子ノードが存在する");
        assert_eq!(first.role, "heading");
        assert_eq!(first.r#ref, Some("e1".to_string()));

        let second = via_push
            .children
            .get(1)
            .expect("2 番目の子ノードが存在する");
        assert_eq!(second.role, "link");
        assert_eq!(second.r#ref, Some("e2".to_string()));
    }

    /// `AISNAP-1`（TASK-11.2・Issue #71）: `Snapshot::new` が `tree` を
    /// そのまま保持し、spec 例と同じ形（ルートの role が `"document"`、
    /// ref が `None`）になること。
    #[test]
    fn aisnap_1_snapshot_new_holds_tree() {
        let tree = Node::new("document", "Example Domain")
            .with_children(vec![Node::new("heading", "Example Domain").with_ref("e1")]);

        let snapshot = Snapshot::new(tree.clone());

        assert_eq!(snapshot.tree, tree);
        assert_eq!(snapshot.tree.role, "document");
        assert_eq!(snapshot.tree.r#ref, None);
    }

    /// `AISNAP-1`（TASK-11.2・Issue #71）: `Node` を clone したものが
    /// 元と等しいこと。
    #[test]
    fn aisnap_1_node_clone_equals_original() {
        let node = Node::new("button", "Submit").with_ref("e3");
        let cloned = node.clone();
        assert_eq!(node, cloned);
    }
}

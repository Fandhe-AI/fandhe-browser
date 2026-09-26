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
//! role・name・state・ref の算出ロジックと、DOM からのツリー構築はまだ
//! 実装しない（実装済みを装わない。REPAIR-3）。段階的に以下の Issue で
//! 実装する：
//!
//! - role（役割）算出: TASK-11.3（Issue #72）
//! - accessible name（アクセシブルネーム）算出: TASK-11.4（Issue #73）
//! - state（状態）算出: TASK-11.5（Issue #74）
//! - ref（role + name シグネチャによる再特定要求。`AISNAP-10`）: TASK-11.6（Issue #75）
//! - DOM から `Snapshot` へのツリー構築統合: TASK-11.7（Issue #76）
//! - ユニットテスト一式: TASK-11.8（Issue #77）

/// 方式 B 簡約ツリーの 1 ノード（`AISNAP-1`・`TASK-11`・`MS-2`）。
///
/// DOM の要素 1 つに対応する、AI エージェント向けの簡約表現。各フィールドの
/// 算出は後続タスクが担う（本 Issue TASK-11.2 では型の形だけを定義する）。
///
/// - `role`: ARIA role のトークン（例: `"document"`・`"heading"`・`"button"`）。
///   役割の種類は多く将来も増えるため `String` とし、enum 化しない。
///   算出は TASK-11.3（Issue #72）が担う
/// - `name`: accessible name。空文字列は「名前なし」を表す。
///   算出は TASK-11.4（Issue #73）が担う
/// - `r#ref`: role + name シグネチャによる再特定要求（`AISNAP-10`）。
///   `None` は ref を振らないノード（例: document ルート）を表す。
///   値の形式（シグネチャ方式・同名要素の一意化）は TASK-11.6（Issue #75・
///   `AISNAP-10`）が決める
/// - `children`: DOM の親子関係に対応する子ノード。構築は
///   TASK-11.7（Issue #76）が担う
///
/// `state`（状態）フィールドは本 Issue では追加しない。TASK-11.5
/// （Issue #74）が追加する。`#[non_exhaustive]` により、フィールドの追加は
/// 破壊的変更にならない。
///
/// 拡張方針: 表・一覧類型のノードには TASK-12（`AISNAP-2`）で `header`
/// （ヘッダ列）・`rows`（圧縮 1 行表現の行）・`truncated_rows`（省略した
/// 行数）を追加する予定である。実現方法（専用構造体を `Option` で持たせる
/// 案など）は TASK-12 で決める。
///
/// 呼び出し文脈: TASK-11.7（Issue #76）が `core` の DOM から構築し、
/// 将来は `cli` 層の配線を経由して `cdp` の `/ai/snapshot` から使われる
/// （TASK-19・`AISNAP-6`）。
///
/// 不安全な設計への申し送り（TASK-11.7 向け）: `children` は再帰構造の
/// ため、極端に深い DOM をそのままツリーにすると、構築時の再帰呼び出しや
/// 破棄（`Drop`）でスタックオーバーフローを起こしうる。構築側で深さの
/// 上限、またはスタックを使わない構築・破棄を検討すること。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Node {
    /// ARIA role のトークン。算出は TASK-11.3（Issue #72）。
    pub role: String,
    /// accessible name。算出は TASK-11.4（Issue #73）。
    pub name: String,
    /// role + name シグネチャによる再特定要求（`AISNAP-10`）。
    /// 算出は TASK-11.6（Issue #75）。
    pub r#ref: Option<String>,
    /// DOM の親子関係に対応する子ノード。構築は TASK-11.7（Issue #76）。
    pub children: Vec<Node>,
}

impl Node {
    /// role・name を指定して `Node` を作る（`ref` は `None`、`children` は空）。
    pub fn new(role: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            name: name.into(),
            r#ref: None,
            children: Vec::new(),
        }
    }

    /// `ref` を設定した `Node` を返す（ビルダー）。
    #[must_use]
    pub fn with_ref(mut self, r: impl Into<String>) -> Self {
        self.r#ref = Some(r.into());
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
    /// DOM のルートに対応するノード。構築は TASK-11.7（Issue #76）。
    pub tree: Node,
}

impl Snapshot {
    /// ルートノードを指定して `Snapshot` を作る。
    pub fn new(tree: Node) -> Self {
        Self { tree }
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, Snapshot};

    /// `AISNAP-1`（TASK-11.2・Issue #71）: `Node::new` が role・name を
    /// 設定し、`ref` は `None`、`children` は空になること。
    #[test]
    fn aisnap_1_node_new_sets_role_name_and_defaults() {
        let node = Node::new("heading", "Example Domain");
        assert_eq!(node.role, "heading");
        assert_eq!(node.name, "Example Domain");
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

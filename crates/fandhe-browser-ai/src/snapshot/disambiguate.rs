//! 同じ `(role, name)` を持つ ref 保持要素へ出現順の区別子 ` #n` を付ける後段パス
//! （`AISNAP-10`・TASK-17・MS-2・Issue #1221。設計は `docs/design/reference-disambiguation.md` の案 A2）。
//!
//! `build.rs` の `build_snapshot_inner` が木の構築（残余縮約・折り畳みを含む）を終えた直後に
//! [`apply_disambiguation`] を呼ぶ。文書順（ノード → 表ヘッダ → 行内コントロール → 子）で
//! 同じキーの 2 件目以降の name にだけ ` #2`・` #3`… を付け、先頭は素の name のまま残す。
//! ref・accname（ref シグネチャ）・role・state 等は一切変えない。参照安定性の測定
//! （`reference_stability`）が、変化前後で同じ `(role, name)` の候補を区別できるようにするのが目的。
//!
//! # 既知の制約（REPAIR-3。将来仕様は後続 Issue）
//!
//! - ページ由来の name と区別子付き name の衝突（元から `alpha #2` がある等）の解消は未対応
//!   （Issue #1222）。base の切り詰めで新たに衝突する場合も同じ。
//! - 同じ `(role, name)` の要素が対象より前に挿入されると出現番号がずれる（ref の `-n` と同じ
//!   原理的な限界）。
//! - 重複時の [`super::Node::name`] はアクセシブルネームそのものではなくなる（出力 name の補完）。

use std::collections::HashMap;

use super::name::{MAX_DATA_LEAF_VALUE_CHARS, MAX_NAME_CHARS};
use super::{MAX_TREE_DEPTH, Node};

/// 重複判定の入力 1 件（文書順）。`name` は ref 発行時の出力 name（`RefHolder` と同じ定義）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RefEntry<'a> {
    pub(super) r#ref: &'a str,
    pub(super) role: &'a str,
    pub(super) name: &'a str,
}

/// [`suffixed_name`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(super) struct SuffixedName {
    pub(super) name: String,
    /// 区別子を収めるため base を切り詰めたか。
    pub(super) truncated: bool,
}

/// [`apply_disambiguation`] の結果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub(super) struct DisambiguationOutcome {
    /// 区別子を付けた要素の `(ref, 出現番号)`（文書順）。
    pub(super) renamed: Vec<(String, usize)>,
    /// base の切り詰めが起きたか（`Snapshot::truncated` へ伝える）。
    pub(super) truncated: bool,
}

/// 文書順の入力から、2 件目以降の要素の `ref → 出現番号` を返す純関数。O(要素数)。
pub(super) fn disambiguate_names(entries: &[RefEntry<'_>]) -> HashMap<String, usize> {
    let mut counts: HashMap<(&str, &str), usize> = HashMap::new();
    let mut out = HashMap::new();
    for e in entries {
        let n = counts.entry((e.role, e.name)).or_insert(0);
        *n += 1;
        if *n >= 2 {
            out.insert(e.r#ref.to_string(), *n);
        }
    }
    out
}

/// base に ` #n`（base が空なら `#n`）を付ける。文字数（char）が上限を超える場合は base を切り詰める。
///
/// 上限は `max(MAX_NAME_CHARS, base の文字数)` を `MAX_DATA_LEAF_VALUE_CHARS` で頭打ちにした値
/// （通常の name は 120、データ葉の値は 300 の既存上限の内に収める）。
pub(super) fn suffixed_name(base: &str, n: usize) -> SuffixedName {
    let suffix = if base.is_empty() {
        format!("#{n}")
    } else {
        format!(" #{n}")
    };
    let base_chars = base.chars().count();
    let limit = MAX_NAME_CHARS
        .max(base_chars)
        .min(MAX_DATA_LEAF_VALUE_CHARS);
    let suffix_chars = suffix.chars().count();
    if base_chars + suffix_chars <= limit {
        return SuffixedName {
            name: format!("{base}{suffix}"),
            truncated: false,
        };
    }
    let keep = limit.saturating_sub(suffix_chars);
    let cut: String = base.chars().take(keep).collect();
    SuffixedName {
        name: format!("{cut}{suffix}"),
        truncated: true,
    }
}

/// 木を先行順（ノード → ヘッダ → 行内コントロール → 子）に走査し、ref 保持要素ごとに
/// `(role, ref, name)` を `f` へ渡す。明示スタックで反復する（再帰しない）。
fn walk_ref_items(tree: &mut Node, f: &mut impl FnMut(&str, &str, &mut String)) {
    let mut stack: Vec<(&mut Node, usize)> = vec![(tree, 0)];
    while let Some((node, depth)) = stack.pop() {
        if let Some(r) = node.r#ref.as_deref() {
            f(&node.role, r, &mut node.name);
        }
        if let Some(table) = node.table.as_mut() {
            for h in table.header.iter_mut() {
                f(&h.role, &h.r#ref, &mut h.name);
            }
            for row in table.rows.iter_mut() {
                for c in row.controls.iter_mut() {
                    f(&c.role, &c.r#ref, &mut c.name);
                }
            }
        }
        // 構築側が深さを制限済み。手組みの木でも走査を有界にする保険。
        if depth <= MAX_TREE_DEPTH + 1 {
            for c in node.children.iter_mut().rev() {
                stack.push((c, depth + 1));
            }
        }
    }
}

/// 確定した木へ区別子を付ける。`keys` は `ref → (role, ref 発行時の出力 name)`。
///
/// `keys` に無い ref（到達しない想定）は、現在の role・name をキーとして使う安全側の扱い。
pub(super) fn apply_disambiguation(
    tree: &mut Node,
    keys: &HashMap<String, (String, String)>,
) -> DisambiguationOutcome {
    let mut collected: Vec<(String, String, String)> = Vec::new();
    walk_ref_items(tree, &mut |role, r, name| {
        let (k_role, k_name) = match keys.get(r) {
            Some((kr, kn)) => (kr.clone(), kn.clone()),
            None => (role.to_string(), name.clone()),
        };
        collected.push((r.to_string(), k_role, k_name));
    });
    let entries: Vec<RefEntry<'_>> = collected
        .iter()
        .map(|(r, role, name)| RefEntry {
            r#ref: r,
            role,
            name,
        })
        .collect();
    let numbers = disambiguate_names(&entries);
    let mut outcome = DisambiguationOutcome::default();
    if numbers.is_empty() {
        return outcome;
    }
    walk_ref_items(tree, &mut |_role, r, name| {
        if let Some(&n) = numbers.get(r) {
            let s = suffixed_name(name, n);
            outcome.truncated |= s.truncated;
            *name = s.name;
            outcome.renamed.push((r.to_string(), n));
        }
    });
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{HeaderCell, RowControl, TableRow, TableSummary};

    fn link(name: &str, r: &str) -> Node {
        let mut n = Node::new("link", name);
        n.r#ref = Some(r.to_string());
        n
    }

    fn root(children: Vec<Node>) -> Node {
        let mut n = Node::new("document", "");
        n.children = children;
        n
    }

    fn keys_of(tree: &Node) -> HashMap<String, (String, String)> {
        let mut t = tree.clone();
        let mut m = HashMap::new();
        walk_ref_items(&mut t, &mut |role, r, name| {
            m.insert(r.to_string(), (role.to_string(), name.clone()));
        });
        m
    }

    /// AISNAP-10・TASK-17（#1221）: 先頭は素の name、2 件目以降に ` #n`。ref は不変。
    #[test]
    fn aisnap_10_three_same_keys_get_occurrence_suffix() {
        let mut t = root(vec![
            link("alpha", "e1"),
            link("alpha", "e1-2"),
            link("alpha", "e1-3"),
        ]);
        let keys = keys_of(&t);
        let out = apply_disambiguation(&mut t, &keys);
        let names: Vec<_> = t.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "alpha #2", "alpha #3"]);
        let refs: Vec<_> = t
            .children
            .iter()
            .filter_map(|c| c.r#ref.as_deref())
            .collect();
        assert_eq!(refs, vec!["e1", "e1-2", "e1-3"]);
        assert_eq!(
            out.renamed,
            vec![("e1-2".to_string(), 2), ("e1-3".to_string(), 3)]
        );
        assert!(!out.truncated);
    }

    /// AISNAP-10: role が違えば別グループ。
    #[test]
    fn aisnap_10_different_role_is_separate_group() {
        let mut b = Node::new("button", "alpha");
        b.r#ref = Some("e2".into());
        let mut t = root(vec![link("alpha", "e1"), b]);
        let keys = keys_of(&t);
        apply_disambiguation(&mut t, &keys);
        assert_eq!(t.children[0].name, "alpha");
        assert_eq!(t.children[1].name, "alpha");
    }

    /// AISNAP-10: ref を持たないノードは対象外。
    #[test]
    fn aisnap_10_nodes_without_ref_are_ignored() {
        let mut t = root(vec![Node::new("link", "alpha"), Node::new("link", "alpha")]);
        apply_disambiguation(&mut t, &HashMap::new());
        assert_eq!(t.children[0].name, "alpha");
        assert_eq!(t.children[1].name, "alpha");
    }

    /// AISNAP-10: ノード → ヘッダ → controls → children の文書順で番号が振られる。
    #[test]
    fn aisnap_10_document_order_node_header_controls_children() {
        let mut table = link("alpha", "t0");
        let header = HeaderCell::new("link", "alpha", "h1".to_string());
        let ctl = RowControl::new("link", "alpha", "c1".to_string());
        let row = TableRow::new("r".to_string(), false).with_controls(vec![ctl]);
        table.table = Some(TableSummary::new(vec![header], vec![row], 0));
        table.children = vec![link("alpha", "k1")];
        let mut t = root(vec![table]);
        let keys = keys_of(&t);
        apply_disambiguation(&mut t, &keys);
        let tbl = &t.children[0];
        let ts = tbl.table.as_ref().expect("table");
        assert_eq!(tbl.name, "alpha");
        assert_eq!(ts.header[0].name, "alpha #2");
        assert_eq!(ts.rows[0].controls[0].name, "alpha #3");
        assert_eq!(tbl.children[0].name, "alpha #4");
    }

    /// AISNAP-10: 空の name は先頭に空白を付けず `#2`。
    #[test]
    fn aisnap_10_empty_name_gets_bare_suffix() {
        let mut t = root(vec![link("", "e1"), link("", "e1-2")]);
        let keys = keys_of(&t);
        apply_disambiguation(&mut t, &keys);
        assert_eq!(t.children[0].name, "");
        assert_eq!(t.children[1].name, "#2");
    }

    /// AISNAP-10: 長さの上限は char 単位で守り、切り詰めたら truncated を返す。
    #[test]
    fn aisnap_10_suffix_respects_char_limit() {
        let long = "あ".repeat(120);
        let s = suffixed_name(&long, 2);
        assert!(s.truncated);
        assert_eq!(s.name.chars().count(), 120);
        assert!(s.name.ends_with(" #2"));
        let leaf = "x".repeat(300);
        let s = suffixed_name(&leaf, 12);
        assert!(s.truncated);
        assert_eq!(s.name.chars().count(), 300);
        let ok = suffixed_name("alpha", 2);
        assert_eq!(ok.name, "alpha #2");
        assert!(!ok.truncated);
    }

    /// AISNAP-10: name 以外（ref・role・state・data_leaf 等）は付与の前後で同一。
    #[test]
    fn aisnap_10_only_name_changes() {
        let mut t = root(vec![link("alpha", "e1"), link("alpha", "e1-2")]);
        let before = t.clone();
        let keys = keys_of(&t);
        apply_disambiguation(&mut t, &keys);
        let mut after = t.clone();
        after.children[1].name = "alpha".into();
        assert_eq!(after, before);
    }
}

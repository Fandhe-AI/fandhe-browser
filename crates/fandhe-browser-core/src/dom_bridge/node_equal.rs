//! `isEqualNode` op の実体: DOM 標準の「node equals」による 2 ノードの構造比較
//! （TASK-108・Issue #1227・ビヘイビア `JS-5`・`JS-6`）。
//!
//! 呼び出し元は [`super`]（`dom_bridge` の `execute` の `isEqualNode` 分岐）で、shim の `dom.js` が
//! `Node.prototype.isEqualNode` として公開する。React / Next.js のクライアントが差分検出に使う。
//! `Document` を読むだけで状態を変えず、診断も記録しない。
//!
//! # 判定規則（DOM 標準）
//!
//! - 種別が同じで、種別ごとのデータが等しい。Element は名前空間・prefix・local name が一致し、
//!   属性の件数が同じで、各属性が名前空間・local name・値で一致する（順序と属性の prefix は無視）。
//!   Doctype は name・public_id・system_id、Text / Comment は内容、処理命令は target・data。
//! - 子の件数が同じで、同じ添字の子どうしがすべて等しい（`template_contents` は子ではないので比べない）。
//!
//! # 安全性（`JS-6`）
//!
//! 再帰せず明示スタックで走査し、反復回数を `node_count` で打ち切る（リンクが壊れていても有限。
//! 上限到達・範囲外 ID・未知の種別は false で fail-closed）。

use crate::dom::{Document, NodeData, NodeId};

/// `a` と `b` が DOM 標準の意味で等しければ true。
pub(super) fn nodes_equal(doc: &Document, a: NodeId, b: NodeId) -> bool {
    let mut stack = vec![(a, b)];
    let mut budget = doc.node_count();
    while let Some((x, y)) = stack.pop() {
        if budget == 0 {
            return false;
        }
        budget -= 1;
        if x == y {
            continue;
        }
        let (Some(dx), Some(dy)) = (doc.node_data(x), doc.node_data(y)) else {
            return false;
        };
        if !data_equal(dx, dy) {
            return false;
        }
        let xs: Vec<NodeId> = doc.children(x).collect();
        let ys: Vec<NodeId> = doc.children(y).collect();
        if xs.len() != ys.len() {
            return false;
        }
        stack.extend(xs.into_iter().zip(ys));
    }
    true
}

/// ノード単体（子を除く）の等価判定。未知の種別は false。
fn data_equal(a: &NodeData, b: &NodeData) -> bool {
    match (a, b) {
        (NodeData::Document, NodeData::Document)
        | (NodeData::DocumentFragment, NodeData::DocumentFragment) => true,
        (
            NodeData::Doctype {
                name: n1,
                public_id: p1,
                system_id: s1,
            },
            NodeData::Doctype {
                name: n2,
                public_id: p2,
                system_id: s2,
            },
        ) => n1 == n2 && p1 == p2 && s1 == s2,
        (
            NodeData::Element {
                name: n1,
                attrs: a1,
                ..
            },
            NodeData::Element {
                name: n2,
                attrs: a2,
                ..
            },
        ) => {
            n1.ns == n2.ns
                && n1.prefix == n2.prefix
                && n1.local == n2.local
                && a1.len() == a2.len()
                && a1.iter().all(|x| {
                    a2.iter().any(|y| {
                        x.name.ns == y.name.ns && x.name.local == y.name.local && x.value == y.value
                    })
                })
        }
        (NodeData::Text { contents: c1 }, NodeData::Text { contents: c2 })
        | (NodeData::Comment { contents: c1 }, NodeData::Comment { contents: c2 }) => c1 == c2,
        (
            NodeData::ProcessingInstruction {
                target: t1,
                data: d1,
            },
            NodeData::ProcessingInstruction {
                target: t2,
                data: d2,
            },
        ) => t1 == t2 && d1 == d2,
        _ => false,
    }
}
